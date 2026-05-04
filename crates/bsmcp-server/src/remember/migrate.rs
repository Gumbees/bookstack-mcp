//! `remember/migrate` — import legacy journal content into the v1.0.0
//! per-user-Journal-book layout.
//!
//! Three actions:
//!   - `list_sources` — list books on the User Journals shelf the calling
//!     user can see. Returns `{sources: [{book_id, name, slug, page_count,
//!     owned}]}`. Per-user visibility filtering happens server-side at
//!     BookStack (we use the user's own token), so we don't ACL-filter
//!     ourselves.
//!   - `plan` — DRY RUN. Walks a source book, parses each page's name (and
//!     body H1 fallback) for a `YYYY-MM-DD` date, and projects the new
//!     target chapter+page name per the v1.0.0 layout (`{YYYY-MM}-{name}`
//!     monthly chapter, `{YYYY-MM-DD}-{name}` daily page).
//!   - `execute` — for each requested page, fetch the source page's
//!     markdown body, find-or-create the target chapter+page using the
//!     same resolvers the journal endpoints use, and append the body as a
//!     single section headed `## Imported from {source_name} —
//!     {original_updated_at}`. One block per source page; original content
//!     verbatim (no further parsing).
//!
//! Sync inline by design — most users have <100 legacy pages. If a real
//! install hits a wall later we'll move to a background-job tracker.

use chrono::{Datelike, NaiveDate};
use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::resolvers::{
    journal_chapter_name, journal_page_name, normalize_agent_name, resolve_first_name,
    resolve_journal_chapter, resolve_journal_page, resolve_user_journal_book, ResolverError,
};
use super::{Context, DispatchResult};

/// BookStack `GET /api/pages/{id}` markdown field. Mirrors the constant
/// in `journal::PAGE_MARKDOWN_FIELD` — kept local for the same reason
/// (the two modules are independent and the constant is one line).
const PAGE_MARKDOWN_FIELD: &str = "markdown";

// =====================================================================
// Public dispatch
// =====================================================================

pub async fn list_sources(ctx: &Context) -> DispatchResult {
    let globals = ctx
        .db
        .get_global_settings()
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_global_settings failed: {e}")))?;
    let shelf_id = globals
        .user_journals_shelf_id
        .ok_or((
            ErrorCode::InvalidArgument,
            "user_journals_shelf_id not configured — admin must set it via /setup/admin"
                .to_string(),
        ))?;

    let shelf = ctx
        .client
        .get_shelf(shelf_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_shelf({shelf_id}) failed: {e}")))?;
    let book_stubs: Vec<Value> = shelf
        .get("books")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let bookstack_user_id = ctx.settings.bookstack_user_id;

    let mut sources: Vec<Value> = Vec::with_capacity(book_stubs.len());
    for stub in &book_stubs {
        let Some(book_id) = stub.get("id").and_then(|v| v.as_i64()) else { continue };
        let name = stub
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let slug = stub
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Per-book `get_book` so we can report (a) page_count, and (b)
        // an accurate `owned` flag — `owned_by` rides on the book detail
        // payload, not the shelf's nested book stubs. Skips the book
        // silently on a non-2xx (e.g. permission shifted between
        // get_shelf and get_book) so the UI still gets every visible
        // entry rather than failing the whole call.
        let (page_count, owned) = match ctx.client.get_book(book_id).await {
            Ok(book) => {
                let count = count_book_pages(&book);
                let owner = book.get("owned_by").and_then(value_to_owner_id);
                let is_owned = matches!((owner, bookstack_user_id), (Some(o), Some(u)) if o == u);
                (count, is_owned)
            }
            Err(_) => (0_usize, false),
        };

        sources.push(json!({
            "book_id": book_id,
            "name": name,
            "slug": slug,
            "page_count": page_count,
            "owned": owned,
        }));
    }

    Ok(json!({ "sources": sources }))
}

pub async fn plan(ctx: &Context) -> DispatchResult {
    let book_id = ctx
        .body_i64("book_id")
        .ok_or((
            ErrorCode::InvalidArgument,
            "Missing required argument: book_id (integer)".to_string(),
        ))?;
    let entry = parse_entry(ctx)?;

    let mut settings = ctx.settings.clone();
    let name = resolve_entry_name(&entry, &ctx.token_id_hash, &mut settings, ctx).await?;

    let source_book = ctx
        .client
        .get_book(book_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_book({book_id}) failed: {e}")))?;
    let source_name = source_book
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pages = walk_source_book_pages(&source_book);

    // Build the planned list. For each page: parse its name; if no match,
    // fetch the body and try the H1 fallback. If still no date the page
    // lands in `undated_pages` and the user picks a date in the UI.
    let mut planned: Vec<Value> = Vec::with_capacity(pages.len());
    let mut undated: Vec<Value> = Vec::new();
    let mut total_blocks: usize = 0;

    for page_stub in &pages {
        let Some(source_page_id) = page_stub.get("id").and_then(|v| v.as_i64()) else { continue };
        let source_page_name = page_stub
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let name_match = parse_date_prefix(&source_page_name);
        let detected = match name_match {
            Some(d) => Some(d),
            None => {
                // Body fallback — fetch the page lazily, peek at the first
                // H1 line. Costs one extra API call per undated page; for
                // the typical <100-page legacy book that's well within
                // the wizard's interactive budget.
                let body = ctx
                    .client
                    .get_page(source_page_id)
                    .await
                    .ok()
                    .and_then(|p| {
                        p.get(PAGE_MARKDOWN_FIELD)
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    });
                body.as_deref().and_then(parse_date_from_h1)
            }
        };

        match detected {
            Some(date) => {
                let target_chapter = journal_chapter_name(date.year(), date.month(), &name);
                let target_page = journal_page_name(date, &name);
                planned.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "detected_date": format_date(date),
                    "target_chapter": target_chapter,
                    "target_page": target_page,
                    "import": true,
                }));
                total_blocks += 1;
            }
            None => {
                undated.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "detected_date": Value::Null,
                    "target_chapter": Value::Null,
                    "target_page": Value::Null,
                    "import": false,
                }));
            }
        }
    }

    Ok(json!({
        "source": {
            "book_id": book_id,
            "name": source_name,
        },
        "target": {
            "book_id": settings.user_journal_book_id,
            "chapter_naming": format!("{{YYYY-MM}}-{name}"),
        },
        "pages": planned,
        "undated_pages": undated,
        "estimated_block_count": total_blocks,
    }))
}

pub async fn execute(ctx: &Context) -> DispatchResult {
    let book_id = ctx
        .body_i64("book_id")
        .ok_or((
            ErrorCode::InvalidArgument,
            "Missing required argument: book_id (integer)".to_string(),
        ))?;
    let entry = parse_entry(ctx)?;
    let selected = parse_pages_arg(ctx)?;
    let date_overrides = parse_date_overrides(ctx)?;

    // No pages selected → safe no-op. Saves us walking the source book
    // and computing a default-all set just to throw it away.
    if let Some(p) = &selected {
        if p.is_empty() {
            return Ok(json!({
                "imported": 0,
                "skipped": 0,
                "errors": [],
            }));
        }
    }

    let mut settings = ctx.settings.clone();
    let globals = ctx
        .db
        .get_global_settings()
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_global_settings failed: {e}")))?;

    let target_book_id = resolve_user_journal_book(
        &ctx.token_id_hash,
        &mut settings,
        &ctx.client,
        ctx.db.clone(),
        &globals,
    )
    .await
    .map_err(resolver_to_envelope)?;

    let name = resolve_entry_name(&entry, &ctx.token_id_hash, &mut settings, ctx).await?;

    let source_book = ctx
        .client
        .get_book(book_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_book({book_id}) failed: {e}")))?;
    let source_name = source_book
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Build the work list — when caller omitted `pages`, default to every
    // dated page in the source book (matches `plan`'s default behavior).
    let candidates: Vec<(i64, String)> = walk_source_book_pages(&source_book)
        .into_iter()
        .filter_map(|p| {
            let id = p.get("id").and_then(|v| v.as_i64())?;
            let nm = p.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            Some((id, nm))
        })
        .collect();

    let work: Vec<(i64, String)> = match &selected {
        Some(ids) => candidates
            .into_iter()
            .filter(|(id, _)| ids.contains(id))
            .collect(),
        None => candidates
            .into_iter()
            .filter(|(id, nm)| {
                parse_date_prefix(nm).is_some() || date_overrides.contains_key(id)
            })
            .collect(),
    };

    let mut imported = 0_usize;
    let mut skipped = 0_usize;
    let mut errors: Vec<Value> = Vec::new();

    for (source_page_id, source_page_name) in &work {
        // Detect date the same way plan does. For execute we don't fetch
        // the body twice — if the name doesn't carry a date we briefly
        // peek the body's H1, and if that also fails we record an error
        // with `reason: "undated"` so the UI can flag the page.
        let body_value = match ctx.client.get_page(*source_page_id).await {
            Ok(p) => p,
            Err(e) => {
                errors.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "reason": format!("get_page failed: {e}"),
                }));
                skipped += 1;
                continue;
            }
        };
        let body = body_value
            .get(PAGE_MARKDOWN_FIELD)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let original_updated_at = body_value
            .get("updated_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let date = date_overrides
            .get(source_page_id)
            .copied()
            .or_else(|| parse_date_prefix(source_page_name))
            .or_else(|| parse_date_from_h1(&body));
        let date = match date {
            Some(d) => d,
            None => {
                errors.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "reason": "undated (no YYYY-MM-DD prefix in name; no H1 date)",
                }));
                skipped += 1;
                continue;
            }
        };

        let chapter_id = match resolve_journal_chapter(
            target_book_id,
            date.year(),
            date.month(),
            &name,
            &ctx.client,
        )
        .await
        {
            Ok(id) => id,
            Err(e) => {
                errors.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "reason": format!("resolve_journal_chapter failed: {e}"),
                }));
                skipped += 1;
                continue;
            }
        };

        let page_id = match resolve_journal_page(chapter_id, date, &name, &ctx.client).await {
            Ok((id, _was_created)) => id,
            Err(e) => {
                errors.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "reason": format!("resolve_journal_page failed: {e}"),
                }));
                skipped += 1;
                continue;
            }
        };

        // Read the existing target body, append the imported block, write
        // back. Same append-vs-overwrite invariant as the journal write
        // path — we re-use that helper directly.
        let target_page = match ctx.client.get_page(page_id).await {
            Ok(p) => p,
            Err(e) => {
                errors.push(json!({
                    "source_page_id": source_page_id,
                    "source_name": source_page_name,
                    "reason": format!("get_page (target) failed: {e}"),
                }));
                skipped += 1;
                continue;
            }
        };
        let existing = target_page
            .get(PAGE_MARKDOWN_FIELD)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let block = format_imported_block(source_page_name, &original_updated_at, &body);
        let new_body = super::journal::append_section(&existing, &block);

        if let Err(e) = ctx
            .client
            .update_page(page_id, &json!({ "markdown": new_body }))
            .await
        {
            errors.push(json!({
                "source_page_id": source_page_id,
                "source_name": source_page_name,
                "reason": format!("update_page failed: {e}"),
            }));
            skipped += 1;
            continue;
        }
        imported += 1;
    }

    Ok(json!({
        "imported": imported,
        "skipped": skipped,
        "errors": errors,
        "source": {
            "book_id": book_id,
            "name": source_name,
        },
        "target_book_id": target_book_id,
    }))
}

// =====================================================================
// Pure helpers
// =====================================================================

/// Parse a `YYYY-MM-DD` prefix from a page name. Tolerates the four
/// separators called out in the brief's date-parsing matrix: end-of-string,
/// dash, space, underscore. Returns the parsed date or `None`.
///
/// Pure — no I/O. Exported so the UI step that lets users pick a date for
/// undated pages can re-validate manually-entered dates with the same
/// parser.
pub fn parse_date_prefix(name: &str) -> Option<NaiveDate> {
    let bytes = name.as_bytes();
    // Need at least 10 chars for YYYY-MM-DD plus a terminator/separator.
    if bytes.len() < 10 {
        return None;
    }
    // Quick shape check: positions 4 and 7 must be dashes; 0..4, 5..7,
    // 8..10 must be ASCII digits. We avoid pulling in `regex` for this —
    // the test matrix is small and the parse is cheap.
    let is_digit = |b: u8| b.is_ascii_digit();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !(0..4).all(|i| is_digit(bytes[i]))
        || !(5..7).all(|i| is_digit(bytes[i]))
        || !(8..10).all(|i| is_digit(bytes[i]))
    {
        return None;
    }
    // Position 10 (if present) must be a recognized boundary so we don't
    // accept e.g. `2025-11-088` as 2025-11-08.
    if bytes.len() > 10 {
        match bytes[10] {
            b'-' | b' ' | b'_' | b'\t' => {}
            _ => return None,
        }
    }
    let date_str = &name[..10];
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()
}

/// Try to recover a `YYYY-MM-DD` from the first H1 (`# ...`) line of a
/// page body. Tolerates the same separator set as `parse_date_prefix`.
/// Returns `None` when no H1 is present or it doesn't carry a date.
pub fn parse_date_from_h1(body: &str) -> Option<NaiveDate> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("# ") else { continue };
        let rest = rest.trim_start();
        if let Some(date) = parse_date_prefix(rest) {
            return Some(date);
        }
        // Stop at the first H1 — second H1s aren't relevant.
        return None;
    }
    None
}

/// Render the imported-block heading + body. Pure helper so the format
/// is locked behind a unit test.
fn format_imported_block(source_name: &str, original_updated_at: &str, body: &str) -> String {
    let heading = if original_updated_at.is_empty() {
        format!("## Imported from {source_name}")
    } else {
        format!("## Imported from {source_name} — {original_updated_at}")
    };
    format!("{heading}\n\n{body}\n\n")
}

/// Walk every page out of a `get_book` response, top-level + nested in
/// chapters. Mirrors `flatten_book_pages` in `bsmcp-common::bookstack`
/// (which is private to that module). Kept here because the migration
/// tool is the only consumer in this crate.
fn walk_source_book_pages(book: &Value) -> Vec<Value> {
    let Some(contents) = book.get("contents").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut pages = Vec::new();
    for item in contents {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("page") => pages.push(item.clone()),
            Some("chapter") => {
                if let Some(ch_pages) = item.get("pages").and_then(|p| p.as_array()) {
                    for p in ch_pages {
                        pages.push(p.clone());
                    }
                }
            }
            _ => {}
        }
    }
    pages
}

/// Count pages in a book. Wrapper around `walk_source_book_pages` —
/// kept named for `list_sources` readability.
fn count_book_pages(book: &Value) -> usize {
    walk_source_book_pages(book).len()
}

/// Pull `owned_by` from a BookStack book detail response. The field can
/// arrive as either an integer id or an embedded user object — accept
/// both shapes so we don't break against unrelated BookStack version
/// drift.
fn value_to_owner_id(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.get("id").and_then(|x| x.as_i64()))
}

#[derive(Debug, Clone)]
enum Entry {
    User,
    Agent(String),
}

fn parse_entry(ctx: &Context) -> Result<Entry, (ErrorCode, String)> {
    let raw = ctx
        .body
        .get("entry_type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                ErrorCode::InvalidArgument,
                "Missing required argument: entry_type (\"user\" or \"agent\")".to_string(),
            )
        })?;
    match raw {
        "user" => Ok(Entry::User),
        "agent" => {
            let raw_name = ctx
                .body
                .get("agent_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    (
                        ErrorCode::InvalidArgument,
                        "Missing required argument: agent_name (required when entry_type=\"agent\")"
                            .to_string(),
                    )
                })?;
            let normalized = normalize_agent_name(raw_name).ok_or_else(|| {
                (
                    ErrorCode::InvalidArgument,
                    format!(
                        "Invalid agent_name `{raw_name}`: must be non-empty and contain only ASCII alphanumerics, dashes, or underscores after trim+lowercase+space-to-dash normalization"
                    ),
                )
            })?;
            Ok(Entry::Agent(normalized))
        }
        other => Err((
            ErrorCode::InvalidArgument,
            format!("Invalid entry_type `{other}`: must be \"user\" or \"agent\""),
        )),
    }
}

async fn resolve_entry_name(
    entry: &Entry,
    token_id_hash: &str,
    settings: &mut bsmcp_common::settings::UserSettings,
    ctx: &Context,
) -> Result<String, (ErrorCode, String)> {
    match entry {
        Entry::User => resolve_first_name(token_id_hash, settings, &ctx.client, ctx.db.clone())
            .await
            .map_err(resolver_to_envelope),
        Entry::Agent(name) => Ok(name.clone()),
    }
}

/// Parse the optional `page_date_overrides` arg — a JSON object mapping
/// `"<source_page_id>"` (string keys, since JSON object keys are always
/// strings) to `"YYYY-MM-DD"`. Used by the wizard to give undated pages
/// a date the user picked manually. Returns an empty map when omitted.
///
/// Invalid keys (non-integer) or values (not parseable as YYYY-MM-DD)
/// surface a clear `invalid_argument` error so a typo doesn't silently
/// drop a page.
fn parse_date_overrides(
    ctx: &Context,
) -> Result<std::collections::HashMap<i64, NaiveDate>, (ErrorCode, String)> {
    let mut out = std::collections::HashMap::new();
    let Some(v) = ctx.body.get("page_date_overrides") else { return Ok(out) };
    let Some(map) = v.as_object() else {
        return Err((
            ErrorCode::InvalidArgument,
            "`page_date_overrides` must be an object mapping source_page_id (string) to date (YYYY-MM-DD)".to_string(),
        ));
    };
    for (k, val) in map {
        let id: i64 = k.trim().parse().map_err(|_| {
            (
                ErrorCode::InvalidArgument,
                format!("`page_date_overrides` key `{k}` is not an integer"),
            )
        })?;
        let date_str = val.as_str().ok_or_else(|| {
            (
                ErrorCode::InvalidArgument,
                format!("`page_date_overrides[{k}]` must be a string YYYY-MM-DD"),
            )
        })?;
        let date = NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d").map_err(|e| {
            (
                ErrorCode::InvalidArgument,
                format!("`page_date_overrides[{k}]` `{date_str}` is not YYYY-MM-DD: {e}"),
            )
        })?;
        out.insert(id, date);
    }
    Ok(out)
}

/// Parse the optional `pages` arg. Returns:
/// - `None`              — caller omitted the field, defaults to all dated.
/// - `Some(vec![])`      — explicit empty list, treated as no-op.
/// - `Some(vec![...])`   — explicit selection.
fn parse_pages_arg(ctx: &Context) -> Result<Option<Vec<i64>>, (ErrorCode, String)> {
    let Some(v) = ctx.body.get("pages") else { return Ok(None) };
    let Some(arr) = v.as_array() else {
        return Err((
            ErrorCode::InvalidArgument,
            "`pages` must be an array of integers".to_string(),
        ));
    };
    let mut ids = Vec::with_capacity(arr.len());
    for entry in arr {
        let id = entry
            .as_i64()
            .or_else(|| entry.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| {
                (
                    ErrorCode::InvalidArgument,
                    format!("`pages` entries must be integers; got {entry}"),
                )
            })?;
        ids.push(id);
    }
    Ok(Some(ids))
}

fn resolver_to_envelope(err: ResolverError) -> (ErrorCode, String) {
    let code = match &err {
        ResolverError::MissingBookstackUserId | ResolverError::MissingShelfConfig => {
            ErrorCode::InvalidArgument
        }
        ResolverError::BookstackError(_) | ResolverError::DbError(_) => ErrorCode::InternalError,
    };
    (code, err.to_string())
}

/// Format a NaiveDate as `YYYY-MM-DD`. One-liner helper kept named for
/// call-site readability.
fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- parse_date_prefix matrix from the brief ---

    #[test]
    fn parse_date_prefix_bare_date() {
        let d = parse_date_prefix("2025-11-08").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 11, 8).unwrap());
    }

    #[test]
    fn parse_date_prefix_date_with_dash_suffix() {
        let d = parse_date_prefix("2025-11-08-untitled").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 11, 8).unwrap());
    }

    #[test]
    fn parse_date_prefix_date_with_space_suffix() {
        let d = parse_date_prefix("2025-11-08 conversation").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 11, 8).unwrap());
    }

    #[test]
    fn parse_date_prefix_date_with_underscore_suffix() {
        let d = parse_date_prefix("2025-11-08_thing").unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 11, 8).unwrap());
    }

    #[test]
    fn parse_date_prefix_rejects_slash_separators() {
        // Brief: `2025/11/08` should NOT match — slashes aren't dashes.
        assert!(parse_date_prefix("2025/11/08").is_none());
    }

    #[test]
    fn parse_date_prefix_rejects_two_digit_year() {
        // Brief: `25-11-08` should NOT match — wrong year format.
        assert!(parse_date_prefix("25-11-08").is_none());
    }

    #[test]
    fn parse_date_prefix_rejects_pure_text() {
        assert!(parse_date_prefix("untitled").is_none());
    }

    #[test]
    fn parse_date_prefix_rejects_invalid_calendar_date() {
        // 2025-13-01 is not a real month; chrono's parse rejects it.
        assert!(parse_date_prefix("2025-13-01").is_none());
        // Feb 30 doesn't exist.
        assert!(parse_date_prefix("2025-02-30").is_none());
    }

    #[test]
    fn parse_date_prefix_rejects_extra_digits_in_day() {
        // `2025-11-088` shouldn't quietly match `2025-11-08`. The
        // boundary check on position 10 catches this — extra digit isn't
        // a recognized separator.
        assert!(parse_date_prefix("2025-11-088").is_none());
    }

    // --- H1 fallback ---

    #[test]
    fn parse_date_from_h1_picks_first_h1_with_date() {
        let body = "# 2025-11-08 conversation log\n\nSome content here\n";
        let d = parse_date_from_h1(body).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2025, 11, 8).unwrap());
    }

    #[test]
    fn parse_date_from_h1_returns_none_when_first_h1_has_no_date() {
        let body = "# Notes\n\n# 2025-11-08 conversation log\n";
        // Stops at the first H1 ("Notes") — second H1 isn't searched.
        assert!(parse_date_from_h1(body).is_none());
    }

    #[test]
    fn parse_date_from_h1_returns_none_for_no_h1() {
        let body = "Just a paragraph\n\nWith some text.\n";
        assert!(parse_date_from_h1(body).is_none());
    }

    #[test]
    fn parse_date_from_h1_handles_leading_whitespace() {
        let body = "    # 2026-04-12 morning\n";
        let d = parse_date_from_h1(body).unwrap();
        assert_eq!(d, NaiveDate::from_ymd_opt(2026, 4, 12).unwrap());
    }

    #[test]
    fn parse_date_from_h1_does_not_match_h2() {
        let body = "## 2026-04-12 morning\n";
        assert!(parse_date_from_h1(body).is_none());
    }

    // --- format_imported_block ---

    #[test]
    fn format_imported_block_includes_source_name_and_timestamp() {
        let block = format_imported_block(
            "2025-11-08 conversation",
            "2025-11-08T14:33:00Z",
            "Some original content.",
        );
        assert!(
            block.starts_with("## Imported from 2025-11-08 conversation — 2025-11-08T14:33:00Z\n\n"),
            "wrong heading shape: {block}"
        );
        assert!(block.contains("Some original content."));
        // Trailing newlines so a follow-on append doesn't smush.
        assert!(block.ends_with("\n\n"));
    }

    #[test]
    fn format_imported_block_omits_em_dash_when_no_timestamp() {
        let block = format_imported_block("untitled", "", "Body.");
        assert!(block.starts_with("## Imported from untitled\n\n"));
        assert!(!block.contains(" — "), "should not include em-dash when no timestamp");
    }

    // --- walk_source_book_pages ---

    #[test]
    fn walk_source_book_pages_flattens_top_level_and_chapters() {
        let book = json!({
            "id": 100,
            "contents": [
                { "type": "page", "id": 1, "name": "loose-page" },
                {
                    "type": "chapter",
                    "id": 50,
                    "name": "Some Chapter",
                    "pages": [
                        { "id": 2, "name": "2025-01-01" },
                        { "id": 3, "name": "2025-01-02" },
                    ],
                },
                { "type": "page", "id": 4, "name": "another-loose" },
            ],
        });
        let pages = walk_source_book_pages(&book);
        assert_eq!(pages.len(), 4);
        let ids: Vec<i64> = pages
            .iter()
            .filter_map(|p| p.get("id").and_then(|v| v.as_i64()))
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn walk_source_book_pages_handles_missing_contents() {
        let book = json!({ "id": 100 });
        assert!(walk_source_book_pages(&book).is_empty());
    }

    #[test]
    fn walk_source_book_pages_skips_unknown_item_types() {
        let book = json!({
            "id": 100,
            "contents": [
                { "type": "page", "id": 1 },
                { "type": "weird_type", "id": 99 },
                { "type": "chapter", "id": 50, "pages": [{ "id": 2 }] },
            ],
        });
        let pages = walk_source_book_pages(&book);
        let ids: Vec<i64> = pages.iter().filter_map(|p| p.get("id").and_then(|v| v.as_i64())).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    // --- count_book_pages ---

    #[test]
    fn count_book_pages_counts_top_level_and_nested() {
        let book = json!({
            "contents": [
                { "type": "page", "id": 1 },
                {
                    "type": "chapter",
                    "id": 50,
                    "pages": [
                        { "id": 2 },
                        { "id": 3 },
                    ],
                },
            ],
        });
        assert_eq!(count_book_pages(&book), 3);
    }

    // --- value_to_owner_id ---

    #[test]
    fn value_to_owner_id_accepts_integer() {
        assert_eq!(value_to_owner_id(&json!(42)), Some(42));
    }

    #[test]
    fn value_to_owner_id_accepts_object() {
        assert_eq!(value_to_owner_id(&json!({ "id": 42, "name": "Nate" })), Some(42));
    }

    #[test]
    fn value_to_owner_id_returns_none_for_other_shapes() {
        assert!(value_to_owner_id(&json!("nate")).is_none());
        assert!(value_to_owner_id(&json!(null)).is_none());
        assert!(value_to_owner_id(&json!({})).is_none());
    }

    // --- plan output shape stability ---

    /// Sanity-check the `plan` projection by going through the pure
    /// helpers a wizard expects to drive it: given a known source name +
    /// entry name, the projected target chapter / page must match the
    /// v1.0.0 layout exactly. This locks the contract without needing a
    /// mock BookStack server — `plan` itself is just walk + (parse +
    /// project) + collect, and the (parse + project) part is fully pure.
    #[test]
    fn plan_projection_matches_journal_layout() {
        let date = parse_date_prefix("2025-11-08-conversation").unwrap();
        let target_chapter = journal_chapter_name(date.year(), date.month(), "pia");
        let target_page = journal_page_name(date, "pia");
        assert_eq!(target_chapter, "2025-11-pia");
        assert_eq!(target_page, "2025-11-08-pia");
    }

    #[test]
    fn plan_projection_pads_single_digit_month_and_day() {
        let date = parse_date_prefix("2025-01-05").unwrap();
        let target_chapter = journal_chapter_name(date.year(), date.month(), "nate");
        let target_page = journal_page_name(date, "nate");
        assert_eq!(target_chapter, "2025-01-nate");
        assert_eq!(target_page, "2025-01-05-nate");
    }

    #[test]
    fn format_date_pads_components() {
        let d = NaiveDate::from_ymd_opt(2025, 1, 5).unwrap();
        assert_eq!(format_date(d), "2025-01-05");
    }

    /// `execute` short-circuits when `pages: []` is explicitly passed.
    /// We can't unit-test the I/O path without a mock BookStack client,
    /// but we CAN lock in the predicate: an explicit empty array must
    /// be distinguishable from a missing field. `parse_pages_arg`
    /// returns `Some(vec![])` for `[]` and `None` for omitted —
    /// `execute`'s short-circuit hangs off that distinction.
    #[test]
    fn execute_no_op_predicate_distinguishes_empty_from_omitted() {
        // Mimic the check inside execute() without spinning up a Context.
        let omitted: Option<Vec<i64>> = None;
        let explicit_empty: Option<Vec<i64>> = Some(Vec::new());
        let explicit_nonempty: Option<Vec<i64>> = Some(vec![1, 2, 3]);

        // The brief: `execute no-op when pages: [] — safe`. Encode that
        // contract: only Some(vec![]) triggers the no-op fast path.
        let is_noop = |s: &Option<Vec<i64>>| matches!(s, Some(p) if p.is_empty());
        assert!(is_noop(&explicit_empty), "explicit [] is the no-op trigger");
        assert!(!is_noop(&omitted), "omitted falls through to default-all-dated");
        assert!(!is_noop(&explicit_nonempty), "non-empty selection runs normally");
    }
}

