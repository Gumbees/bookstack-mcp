//! `remember/journal` — append-only structured journal entries on the
//! user's per-user Journal book.
//!
//! Layout (inside `resolve_user_journal_book` for the calling user):
//!   - Monthly chapter:  `{YYYY-MM}-{name}`
//!   - Daily page:       `{YYYY-MM-DD}-{name}` inside that chapter
//!   - Each `write` appends a new section to the daily page:
//!         ## YYYY-MM-DD HH:MM:SS TZ
//!         {content}
//!
//! `name` is the user's first name (entry_type=user) or a normalized agent
//! name (entry_type=agent), so two different agents writing on the same day
//! land on two different daily pages — easy to skim by author.
//!
//! `read` returns the daily page's full markdown body. It's a passive query:
//! if the page doesn't exist it returns `{exists: false, content: null}`
//! without bootstrapping anything, so dashboards / drive-by reads don't
//! create empty pages.

use chrono::{Datelike, NaiveDate, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::resolvers::{
    journal_page_name, normalize_agent_name, resolve_first_name, resolve_journal_chapter,
    resolve_journal_page, resolve_user_journal_book, ResolverError,
};
use super::{Context, DispatchResult};

/// Page-content field returned by BookStack `GET /api/pages/{id}` for
/// markdown-editor pages. Mirrors `identity::PAGE_MARKDOWN_FIELD`.
const PAGE_MARKDOWN_FIELD: &str = "markdown";

pub async fn write(ctx: &Context) -> DispatchResult {
    let entry = parse_entry(ctx)?;
    let content = ctx
        .body
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                ErrorCode::InvalidArgument,
                "Missing required argument: content (string)".to_string(),
            )
        })?
        .to_string();
    if content.trim().is_empty() {
        return Err((
            ErrorCode::InvalidArgument,
            "`content` must not be empty".to_string(),
        ));
    }

    let mut settings = ctx.settings.clone();
    let globals = ctx
        .db
        .get_global_settings()
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_global_settings failed: {e}")))?;

    let book_id = resolve_user_journal_book(
        &ctx.token_id_hash,
        &mut settings,
        &ctx.client,
        ctx.db.clone(),
        &globals,
    )
    .await
    .map_err(resolver_to_envelope)?;

    let name = resolve_entry_name(&entry, &ctx.token_id_hash, &mut settings, ctx).await?;

    let (now_local, tz_label) = local_now(&settings);
    let date = NaiveDate::from_ymd_opt(now_local.year(), now_local.month(), now_local.day())
        .ok_or_else(|| (
            ErrorCode::InternalError,
            format!(
                "computed local date out of range: y={} m={} d={}",
                now_local.year(),
                now_local.month(),
                now_local.day()
            ),
        ))?;

    // Probe BEFORE find-or-create so we can honestly report
    // `was_chapter_created`. One extra `get_book` round-trip (via
    // find_chapter_in_book), but the write path already makes several;
    // the observability is worth it.
    let chapter_name = super::resolvers::journal_chapter_name(
        now_local.year(),
        now_local.month(),
        &name,
    );
    let chapter_existed = ctx
        .client
        .find_chapter_in_book(book_id, &chapter_name)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("find_chapter_in_book failed: {e}")))?
        .is_some();

    let chapter_id = resolve_journal_chapter(
        book_id,
        now_local.year(),
        now_local.month(),
        &name,
        &ctx.client,
    )
    .await
    .map_err(resolver_to_envelope)?;
    let was_chapter_created = !chapter_existed;

    let (page_id, was_page_created) =
        resolve_journal_page(chapter_id, date, &name, &ctx.client)
            .await
            .map_err(resolver_to_envelope)?;

    let section_heading = format_section_heading(&now_local, &tz_label);
    let section = format!("## {section_heading}\n\n{content}\n\n");

    // Read current body, append, write back. BookStack's `update_page`
    // is a wholesale replacement (no native append endpoint), so we
    // must round-trip the existing body.
    let page = ctx
        .client
        .get_page(page_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_page failed: {e}")))?;
    let existing = page
        .get(PAGE_MARKDOWN_FIELD)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let new_body = append_section(&existing, &section);
    let bytes_appended = section.len();

    let updated = ctx
        .client
        .update_page(
            page_id,
            &json!({
                "markdown": new_body,
            }),
        )
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("update_page failed: {e}")))?;
    let resolved_chapter = updated
        .get("chapter_id")
        .and_then(Value::as_i64)
        .unwrap_or(chapter_id);

    Ok(json!({
        "page_id": page_id,
        "chapter_id": resolved_chapter,
        "book_id": book_id,
        "section_heading": section_heading,
        "bytes_appended": bytes_appended,
        "was_chapter_created": was_chapter_created,
        "was_page_created": was_page_created,
    }))
}

pub async fn read(ctx: &Context) -> DispatchResult {
    let entry = parse_entry(ctx)?;

    let mut settings = ctx.settings.clone();

    // Passive: don't auto-create the journal book on a read. If it
    // hasn't been created yet (no prior `write`), the user has no
    // journal entries to read — short-circuit with exists=false rather
    // than spawning a private book + permission-scrub round-trip just
    // because someone polled the daily page.
    let cached_book_id = settings.user_journal_book_id;
    let book_id = if let Some(id) = cached_book_id {
        // Verify the cached book still exists; clear-cache + re-treat as
        // missing if it was deleted out from under us.
        match ctx.client.get_book(id).await {
            Ok(_) => Some(id),
            Err(e) if e.contains("404") => None,
            Err(e) => {
                return Err((
                    ErrorCode::InternalError,
                    format!("get_book({id}) failed: {e}"),
                ))
            }
        }
    } else {
        None
    };
    let Some(book_id) = book_id else {
        return Ok(json!({
            "exists": false,
            "content": null,
            "page_id": null,
            "chapter_id": null,
            "book_id": null,
            "reason": "no journal book yet (call action=write to create)",
        }));
    };

    let name = resolve_entry_name(&entry, &ctx.token_id_hash, &mut settings, ctx).await?;

    // Date defaults to today in user TZ; explicit `date` overrides.
    let (now_local, _tz_label) = local_now(&settings);
    let today = NaiveDate::from_ymd_opt(now_local.year(), now_local.month(), now_local.day())
        .ok_or_else(|| (
            ErrorCode::InternalError,
            "computed local date out of range".to_string(),
        ))?;
    let date = match ctx.body.get("date").and_then(|v| v.as_str()) {
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| (
            ErrorCode::InvalidArgument,
            format!("Invalid `date` `{s}`: must be YYYY-MM-DD ({e})"),
        ))?,
        None => today,
    };

    // Find chapter — passive: don't create.
    let chapter_name = super::resolvers::journal_chapter_name(
        date.year(),
        date.month(),
        &name,
    );
    let chapter = ctx
        .client
        .find_chapter_in_book(book_id, &chapter_name)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("find_chapter_in_book failed: {e}")))?;
    let Some(chapter) = chapter else {
        return Ok(json!({
            "exists": false,
            "content": null,
            "page_id": null,
            "chapter_id": null,
            "book_id": book_id,
            "date": date.format("%Y-%m-%d").to_string(),
            "name": name,
        }));
    };
    let chapter_id = chapter.get("id").and_then(|v| v.as_i64()).ok_or_else(|| (
        ErrorCode::InternalError,
        "chapter response missing id".to_string(),
    ))?;

    let page_name = journal_page_name(date, &name);
    let page = ctx
        .client
        .find_page_in_chapter(chapter_id, &page_name)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("find_page_in_chapter failed: {e}")))?;
    let Some(page) = page else {
        return Ok(json!({
            "exists": false,
            "content": null,
            "page_id": null,
            "chapter_id": chapter_id,
            "book_id": book_id,
            "date": date.format("%Y-%m-%d").to_string(),
            "name": name,
        }));
    };
    let page_id = page.get("id").and_then(|v| v.as_i64()).ok_or_else(|| (
        ErrorCode::InternalError,
        "page response missing id".to_string(),
    ))?;

    // The find_page_in_chapter listing doesn't carry the markdown body —
    // fetch it explicitly.
    let full = ctx
        .client
        .get_page(page_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_page failed: {e}")))?;
    let content = full
        .get(PAGE_MARKDOWN_FIELD)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "exists": true,
        "content": content,
        "page_id": page_id,
        "chapter_id": chapter_id,
        "book_id": book_id,
        "date": date.format("%Y-%m-%d").to_string(),
        "name": name,
    }))
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

fn resolver_to_envelope(err: ResolverError) -> (ErrorCode, String) {
    let code = match &err {
        ResolverError::MissingBookstackUserId | ResolverError::MissingShelfConfig => {
            ErrorCode::InvalidArgument
        }
        ResolverError::BookstackError(_) | ResolverError::DbError(_) => ErrorCode::InternalError,
    };
    (code, err.to_string())
}

/// Compute the user's local "now" plus a short timezone label.
///
/// The label is the IANA short abbreviation when chrono-tz exposes one
/// (e.g. "EDT"). When it doesn't (some zones return the IANA name), we
/// fall back to the numeric offset in `+HHMM` form so the heading stays
/// unambiguous. Settings without a timezone fall back to UTC.
fn local_now(
    settings: &bsmcp_common::settings::UserSettings,
) -> (chrono::DateTime<chrono::FixedOffset>, String) {
    let now = Utc::now();
    let tz: Tz = settings
        .timezone
        .as_deref()
        .and_then(|s| s.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC);
    let local_in_tz = tz.from_utc_datetime(&now.naive_utc());
    let offset = local_in_tz.offset().fix();
    let label = tz_label(local_in_tz.format("%Z").to_string(), offset);
    let local_fixed: chrono::DateTime<chrono::FixedOffset> = local_in_tz.with_timezone(&offset);
    (local_fixed, label)
}

/// Choose the heading TZ label. chrono-tz's `%Z` formatter returns the
/// abbreviation when one is registered (e.g. "EDT", "PST"); otherwise it
/// returns the IANA zone name (e.g. "America/New_York"), which is too
/// long for a heading. Detect that fall-through by looking for a `/` and
/// substitute the numeric offset (`+HHMM` / `-HHMM`).
fn tz_label(raw: String, offset: chrono::FixedOffset) -> String {
    if raw.is_empty() || raw.contains('/') {
        format_offset_compact(offset)
    } else {
        raw
    }
}

/// Format a chrono FixedOffset as `+HHMM` / `-HHMM`. chrono's built-in
/// `%z` matches but pulling it through a DateTime requires a full format
/// pass; this is cheaper and self-contained.
fn format_offset_compact(offset: chrono::FixedOffset) -> String {
    let total = offset.local_minus_utc(); // seconds
    let sign = if total >= 0 { '+' } else { '-' };
    let abs = total.unsigned_abs();
    let hours = abs / 3600;
    let mins = (abs % 3600) / 60;
    format!("{sign}{hours:02}{mins:02}")
}

/// Build the section heading body — `YYYY-MM-DD HH:MM:SS TZ`. Pure helper
/// so tests can assert format without timezone setup.
pub fn format_section_heading(
    now_local: &chrono::DateTime<chrono::FixedOffset>,
    tz_label: &str,
) -> String {
    format!(
        "{} {}",
        now_local.format("%Y-%m-%d %H:%M:%S"),
        tz_label
    )
}

/// Append a section to an existing page body, ensuring there's a blank
/// line between the prior content and the new section heading. Pure helper
/// so the append-vs-overwrite invariant has a unit test.
pub fn append_section(existing: &str, new_section: &str) -> String {
    if existing.is_empty() {
        return new_section.to_string();
    }
    let mut out = String::with_capacity(existing.len() + new_section.len() + 2);
    out.push_str(existing);
    if !existing.ends_with("\n\n") {
        if existing.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
    out.push_str(new_section);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_local(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32, offset_secs: i32) -> chrono::DateTime<chrono::FixedOffset> {
        let offset = chrono::FixedOffset::east_opt(offset_secs).unwrap();
        offset
            .with_ymd_and_hms(year, month, day, hour, min, sec)
            .unwrap()
    }

    #[test]
    fn section_heading_matches_brief_format() {
        // 2026-05-03 18:25:14 EDT — exactly the shape called out in the
        // brief.
        let dt = fixed_local(2026, 5, 3, 18, 25, 14, -4 * 3600);
        let h = format_section_heading(&dt, "EDT");
        assert_eq!(h, "2026-05-03 18:25:14 EDT");
    }

    #[test]
    fn section_heading_falls_back_to_numeric_offset() {
        // No abbreviation → numeric offset.
        let dt = fixed_local(2026, 5, 3, 18, 25, 14, 4 * 3600);
        let label = format_offset_compact(dt.offset().fix());
        let h = format_section_heading(&dt, &label);
        assert_eq!(h, "2026-05-03 18:25:14 +0400");
    }

    #[test]
    fn tz_label_uses_abbrev_when_present() {
        let offset = chrono::FixedOffset::east_opt(-4 * 3600).unwrap();
        assert_eq!(tz_label("EDT".to_string(), offset), "EDT");
        assert_eq!(tz_label("PST".to_string(), offset), "PST");
    }

    #[test]
    fn tz_label_falls_back_for_iana_name() {
        // chrono-tz returns "America/New_York" for some zones — too long
        // for a heading; we substitute the numeric offset.
        let offset = chrono::FixedOffset::east_opt(-5 * 3600).unwrap();
        assert_eq!(tz_label("America/New_York".to_string(), offset), "-0500");
    }

    #[test]
    fn tz_label_falls_back_for_empty() {
        let offset = chrono::FixedOffset::east_opt(0).unwrap();
        assert_eq!(tz_label(String::new(), offset), "+0000");
    }

    #[test]
    fn format_offset_compact_handles_signs() {
        assert_eq!(
            format_offset_compact(chrono::FixedOffset::east_opt(0).unwrap()),
            "+0000"
        );
        assert_eq!(
            format_offset_compact(chrono::FixedOffset::east_opt(-4 * 3600).unwrap()),
            "-0400"
        );
        assert_eq!(
            format_offset_compact(chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap()),
            "+0530"
        );
        assert_eq!(
            format_offset_compact(chrono::FixedOffset::east_opt(14 * 3600).unwrap()),
            "+1400"
        );
    }

    // --- append_section: append-vs-overwrite invariant ---

    #[test]
    fn append_section_seeds_when_existing_is_empty() {
        let new = "## 2026-05-03 18:25:14 EDT\n\nfirst entry\n\n";
        assert_eq!(append_section("", new), new);
    }

    #[test]
    fn append_section_preserves_prior_content() {
        // The append must keep the prior section verbatim and add the
        // new one BELOW. Two writes back-to-back: the second must not
        // overwrite the first.
        let prior = "## 2026-05-03 18:25:14 EDT\n\nfirst entry\n\n";
        let new = "## 2026-05-03 19:00:00 EDT\n\nsecond entry\n\n";
        let combined = append_section(prior, new);

        assert!(combined.contains("first entry"), "first entry must survive");
        assert!(combined.contains("second entry"), "second entry must appear");
        // Order matters: prior text comes BEFORE new text.
        let pi = combined.find("first entry").unwrap();
        let ni = combined.find("second entry").unwrap();
        assert!(pi < ni, "prior section must precede new section");
    }

    #[test]
    fn append_section_inserts_blank_line_when_existing_lacks_trailing_break() {
        let prior = "some prose without a final newline";
        let new = "## heading\n\nbody\n\n";
        let combined = append_section(prior, new);
        // The boundary must include a blank line so the "## heading" is
        // a real markdown header, not an inline `## heading`.
        assert!(
            combined.contains("\n\n## heading"),
            "expected blank-line separator before heading; got: {combined}"
        );
    }

    #[test]
    fn append_section_inserts_blank_line_when_existing_ends_with_single_newline() {
        let prior = "some prose\n";
        let new = "## heading\n\nbody\n\n";
        let combined = append_section(prior, new);
        assert!(
            combined.contains("\n\n## heading"),
            "expected blank-line separator before heading; got: {combined}"
        );
    }

    #[test]
    fn append_section_does_not_double_blank_when_existing_already_blank() {
        let prior = "section A\n\n";
        let new = "## heading\n\nbody\n\n";
        let combined = append_section(prior, new);
        assert!(
            !combined.contains("\n\n\n## heading"),
            "should not introduce a triple-newline; got: {combined}"
        );
        assert!(combined.contains("\n\n## heading"));
    }
}
