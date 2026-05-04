//! `remember/events` — future-scheduled calendar items living on monthly
//! pages inside the user's per-user Journal book.
//!
//! Layout (inside `resolve_user_journal_book` for the calling user):
//!   - Singleton chapter: `Events`
//!   - Monthly pages: `{YYYY-MM}-Events` (the page month matches the
//!     event's scheduled month — events scheduled for July 15 land on
//!     the July page, even if you create them in May)
//!   - Page seed: `## 📅 Scheduled` section header
//!
//! Bullet shape (no checkbox — events are "scheduled" or "cancelled",
//! and cancellation removes the bullet entirely):
//!
//! ```text
//! - {scheduled_at YYYY-MM-DD HH:MM TZ} — {title} _[{label}: {natural_key}]_
//! ```
//!
//! `event_id` = `{label}:{natural_key}`; same shape as reminders. Three
//! actions: `create`, `list`, `cancel`. Past events stay on the page —
//! they're a record. Auto-prune of expired events is deferred (out of
//! scope for the v1.0.0 surface).

use chrono::{Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::resolvers::{
    normalize_agent_name, resolve_events_chapter, resolve_events_monthly_page,
    resolve_user_journal_book, slugify_natural_key, ResolverError,
};
use super::{Context, DispatchResult};

const PAGE_MARKDOWN_FIELD: &str = "markdown";
const SCHEDULED_HEADER: &str = "## 📅 Scheduled";

pub async fn create(ctx: &Context) -> DispatchResult {
    let title = body_required_str(ctx, "title")?;
    if title.trim().is_empty() {
        return Err((
            ErrorCode::InvalidArgument,
            "`title` must not be empty".to_string(),
        ));
    }
    let scheduled_at_raw = body_required_str(ctx, "scheduled_at")?;
    let label = parse_label(ctx)?;
    let natural_key = ctx
        .body
        .get("natural_key")
        .and_then(|v| v.as_str())
        .map(slugify_natural_key)
        .unwrap_or_else(|| slugify_natural_key(&title));

    let user_tz = parse_user_tz(ctx);
    let scheduled = parse_scheduled_at(&scheduled_at_raw, &user_tz)?;
    let scheduled_stamp = format_scheduled(&scheduled, &user_tz);

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

    let chapter_id = resolve_events_chapter(book_id, &ctx.client)
        .await
        .map_err(resolver_to_envelope)?;

    let scheduled_local = scheduled.with_timezone(&user_tz);
    let (page_id, _was_created) = resolve_events_monthly_page(
        chapter_id,
        scheduled_local.year(),
        scheduled_local.month(),
        &ctx.client,
    )
    .await
    .map_err(resolver_to_envelope)?;

    let mut markdown = load_page_markdown(ctx, page_id).await?;
    let bullet = format!(
        "- {scheduled_stamp} — {title} _[{label}: {natural_key}]_",
        title = title.trim()
    );
    insert_into_scheduled_section(&mut markdown, &bullet);
    save_page(ctx, page_id, &markdown).await?;

    let event_id = format!("{label}:{natural_key}");
    Ok(json!({
        "event_id": event_id,
        "label": label,
        "natural_key": natural_key,
        "scheduled_at": scheduled_stamp,
        "page_id": page_id,
        "chapter_id": chapter_id,
        "book_id": book_id,
    }))
}

pub async fn list(ctx: &Context) -> DispatchResult {
    let label_filter = ctx
        .body
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let user_tz = parse_user_tz(ctx);
    let now_local = Utc::now().with_timezone(&user_tz);
    let (year, month) = (now_local.year(), now_local.month());

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
    let chapter_id = resolve_events_chapter(book_id, &ctx.client)
        .await
        .map_err(resolver_to_envelope)?;
    let (page_id, _was_created) =
        resolve_events_monthly_page(chapter_id, year, month, &ctx.client)
            .await
            .map_err(resolver_to_envelope)?;
    let markdown = load_page_markdown(ctx, page_id).await?;

    let items: Vec<Value> = parse_events(&markdown)
        .into_iter()
        .filter(|e| {
            label_filter
                .as_deref()
                .map(|l| e.label == l)
                .unwrap_or(true)
        })
        .map(|e| {
            json!({
                "event_id": format!("{}:{}", e.label, e.natural_key),
                "label": e.label,
                "natural_key": e.natural_key,
                "scheduled_at": e.scheduled_at,
                "title": e.title,
            })
        })
        .collect();

    Ok(json!({
        "items": items,
        "page_id": page_id,
        "chapter_id": chapter_id,
        "book_id": book_id,
    }))
}

pub async fn cancel(ctx: &Context) -> DispatchResult {
    let event_id = body_required_str(ctx, "event_id")?;
    let (label, natural_key) = split_event_id(&event_id)?;

    let user_tz = parse_user_tz(ctx);
    let now_local = Utc::now().with_timezone(&user_tz);

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
    let chapter_id = resolve_events_chapter(book_id, &ctx.client)
        .await
        .map_err(resolver_to_envelope)?;
    let (page_id, _was_created) = resolve_events_monthly_page(
        chapter_id,
        now_local.year(),
        now_local.month(),
        &ctx.client,
    )
    .await
    .map_err(resolver_to_envelope)?;

    let mut markdown = load_page_markdown(ctx, page_id).await?;
    let id_marker = format!("_[{label}: {natural_key}]_");
    let removed = remove_event_bullet(&mut markdown, &id_marker);
    if !removed {
        return Err((
            ErrorCode::InvalidArgument,
            format!(
                "event_id `{event_id}` not found on this month's page (cross-month cancel is out of scope)"
            ),
        ));
    }
    save_page(ctx, page_id, &markdown).await?;

    Ok(json!({
        "event_id": event_id,
        "page_id": page_id,
        "chapter_id": chapter_id,
        "book_id": book_id,
    }))
}

// ---------- shared helpers ----------

async fn load_page_markdown(ctx: &Context, page_id: i64) -> Result<String, (ErrorCode, String)> {
    let page = ctx
        .client
        .get_page(page_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_page failed: {e}")))?;
    let markdown = page
        .get(PAGE_MARKDOWN_FIELD)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(if markdown.trim().is_empty() {
        super::resolvers::events_seed()
    } else {
        markdown
    })
}

async fn save_page(ctx: &Context, page_id: i64, markdown: &str) -> Result<(), (ErrorCode, String)> {
    ctx.client
        .update_page(page_id, &json!({ "markdown": markdown }))
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("update_page failed: {e}")))?;
    Ok(())
}

fn body_required_str(ctx: &Context, key: &str) -> Result<String, (ErrorCode, String)> {
    ctx.body
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            (
                ErrorCode::InvalidArgument,
                format!("Missing required argument: {key} (string)"),
            )
        })
}

fn parse_label(ctx: &Context) -> Result<String, (ErrorCode, String)> {
    let raw = ctx
        .body
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "user".to_string());
    if raw == "user" {
        return Ok("user".to_string());
    }
    normalize_agent_name(&raw).ok_or_else(|| {
        (
            ErrorCode::InvalidArgument,
            format!("Invalid `label`: `{raw}` — must be `user` or a normalized agent name"),
        )
    })
}

fn parse_user_tz(ctx: &Context) -> Tz {
    ctx.settings
        .timezone
        .as_deref()
        .and_then(|t| t.parse().ok())
        .unwrap_or(chrono_tz::UTC)
}

/// Accept a few common `scheduled_at` shapes:
///   - `YYYY-MM-DD HH:MM TZ` (the canonical render shape — TZ as IANA short)
///   - `YYYY-MM-DDTHH:MM:SS±HH:MM` (RFC 3339 with offset)
///   - `YYYY-MM-DD HH:MM` (assume user's timezone)
///   - `YYYY-MM-DD` (assume midnight in user's timezone)
fn parse_scheduled_at(
    raw: &str,
    user_tz: &Tz,
) -> Result<chrono::DateTime<FixedOffset>, (ErrorCode, String)> {
    let s = raw.trim();

    // RFC 3339 with offset.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt);
    }

    // `YYYY-MM-DD HH:MM TZ` where TZ is e.g. EDT, PST, UTC, or an IANA name.
    if let Some((datetime_part, tz_part)) = s.rsplit_once(' ') {
        if let Ok(naive) = NaiveDateTime::parse_from_str(datetime_part, "%Y-%m-%d %H:%M") {
            if let Some(dt) = naive_in_named_tz(naive, tz_part, user_tz) {
                return Ok(dt);
            }
        }
    }

    // `YYYY-MM-DD HH:MM` — assume user's TZ.
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Ok(in_user_tz(naive, user_tz));
    }

    // `YYYY-MM-DD` — midnight in user's TZ.
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let naive = NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        return Ok(in_user_tz(naive, user_tz));
    }

    Err((
        ErrorCode::InvalidArgument,
        format!(
            "`scheduled_at` must be `YYYY-MM-DD`, `YYYY-MM-DD HH:MM`, `YYYY-MM-DD HH:MM TZ`, \
             or RFC 3339 (got `{raw}`)"
        ),
    ))
}

fn naive_in_named_tz(
    naive: NaiveDateTime,
    tz_label: &str,
    user_tz: &Tz,
) -> Option<chrono::DateTime<FixedOffset>> {
    // First try IANA names (chrono_tz parses `America/New_York`).
    if let Ok(tz) = tz_label.parse::<Tz>() {
        let local = tz.from_local_datetime(&naive).single()?;
        return Some(local.with_timezone(&local.offset().fix()));
    }
    // Fall back: ignore the tz_label and use the user's timezone. The
    // canonical render will stamp it back as the user's IANA short, so
    // round-trips stay coherent. (Users typing `EDT` get treated as
    // their local zone, which is what they almost always mean.)
    let _ = tz_label;
    Some(in_user_tz(naive, user_tz))
}

fn in_user_tz(naive: NaiveDateTime, user_tz: &Tz) -> chrono::DateTime<FixedOffset> {
    let local = user_tz
        .from_local_datetime(&naive)
        .single()
        .unwrap_or_else(|| user_tz.from_utc_datetime(&naive));
    local.with_timezone(&local.offset().fix())
}

fn format_scheduled(dt: &chrono::DateTime<FixedOffset>, user_tz: &Tz) -> String {
    let in_tz = dt.with_timezone(user_tz);
    let abbrev = in_tz.format("%Z").to_string();
    let label = if abbrev.is_empty() || abbrev.starts_with(['+', '-']) {
        user_tz.name().to_string()
    } else {
        abbrev
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} {}",
        in_tz.year(),
        in_tz.month(),
        in_tz.day(),
        in_tz.hour(),
        in_tz.minute(),
        label
    )
}

fn split_event_id(id: &str) -> Result<(String, String), (ErrorCode, String)> {
    let (label, key) = id.split_once(':').ok_or_else(|| {
        (
            ErrorCode::InvalidArgument,
            format!("`event_id` must be `<label>:<natural_key>` (got `{id}`)"),
        )
    })?;
    if label.is_empty() || key.is_empty() {
        return Err((
            ErrorCode::InvalidArgument,
            format!("`event_id` malformed (got `{id}`)"),
        ));
    }
    Ok((label.to_string(), key.to_string()))
}

// ---------- markdown manipulation ----------

#[derive(Debug, Clone)]
struct ParsedEvent {
    scheduled_at: String,
    title: String,
    label: String,
    natural_key: String,
}

fn parse_events(markdown: &str) -> Vec<ParsedEvent> {
    let mut out = Vec::new();
    for line in markdown.lines() {
        let line = line.trim_end();
        // Skip checkbox bullets — events have no checkbox and we don't
        // want to confuse a stray reminder copied here.
        if line.starts_with("- [ ] ") || line.starts_with("- [x] ") {
            continue;
        }
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        if let Some(event) = parse_event_bullet(rest) {
            out.push(event);
        }
    }
    out
}

fn parse_event_bullet(rest: &str) -> Option<ParsedEvent> {
    let id_open = rest.rfind("_[")?;
    let head = &rest[..id_open];
    let tail = &rest[id_open..];
    let id_close = tail.find("]_")?;
    let id_inner = &tail[2..id_close];
    let (label, natural_key) = id_inner.split_once(": ")?;
    let head_trimmed = head.trim_end();
    let (scheduled_at, title) = head_trimmed.split_once(" — ")?;
    Some(ParsedEvent {
        scheduled_at: scheduled_at.trim().to_string(),
        title: title.trim().to_string(),
        label: label.trim().to_string(),
        natural_key: natural_key.trim().to_string(),
    })
}

fn insert_into_scheduled_section(markdown: &mut String, bullet: &str) {
    if let Some(idx) = markdown.find(SCHEDULED_HEADER) {
        let after_header = idx + SCHEDULED_HEADER.len();
        let rest_after_header = &markdown[after_header..];
        let next_section_offset = rest_after_header
            .find("\n## ")
            .map(|n| after_header + n)
            .unwrap_or(markdown.len());
        let segment = markdown[after_header..next_section_offset].to_string();
        let new_segment = ensure_trailing_newline(&segment) + bullet + "\n";
        markdown.replace_range(after_header..next_section_offset, &new_segment);
    } else {
        if !markdown.ends_with('\n') {
            markdown.push('\n');
        }
        markdown.push_str(SCHEDULED_HEADER);
        markdown.push('\n');
        markdown.push('\n');
        markdown.push_str(bullet);
        markdown.push('\n');
    }
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn remove_event_bullet(markdown: &mut String, id_marker: &str) -> bool {
    let mut lines: Vec<String> = markdown.lines().map(|l| l.to_string()).collect();
    let before = lines.len();
    lines.retain(|l| {
        !(l.contains(id_marker) && l.starts_with("- ") && !l.starts_with("- [ ] ") && !l.starts_with("- [x] "))
    });
    if lines.len() == before {
        return false;
    }
    let trailing_newline = markdown.ends_with('\n');
    *markdown = lines.join("\n");
    if trailing_newline {
        markdown.push('\n');
    }
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_page() -> String {
        "## 📅 Scheduled\n\
         \n\
         - 2026-05-15 14:00 EDT — Quarterly review with Maggie _[user: q2-review]_\n\
         - 2026-05-20 09:00 EDT — Roadhouse Architecture sync _[pia: rh-arch-sync]_\n"
            .to_string()
    }

    #[test]
    fn parse_events_pulls_label_and_key() {
        let events = parse_events(&sample_page());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].label, "user");
        assert_eq!(events[0].natural_key, "q2-review");
        assert_eq!(events[0].title, "Quarterly review with Maggie");
        assert_eq!(events[1].label, "pia");
    }

    #[test]
    fn parse_events_ignores_checkbox_bullets() {
        let mixed = format!(
            "{}- [ ] 🔥 2026-05-04 09:00 EDT — leftover reminder _[user: stray]_\n",
            sample_page()
        );
        let events = parse_events(&mixed);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.natural_key != "stray"));
    }

    #[test]
    fn insert_into_scheduled_appends_under_header() {
        let mut md = sample_page();
        insert_into_scheduled_section(&mut md, "- 2026-06-01 10:00 EDT — June kickoff _[user: june-kick]_");
        let events = parse_events(&md);
        assert_eq!(events.len(), 3);
        assert!(md.contains("_[user: june-kick]_"));
    }

    #[test]
    fn remove_event_bullet_targets_only_dash_lines() {
        let mut md = sample_page();
        let removed = remove_event_bullet(&mut md, "_[user: q2-review]_");
        assert!(removed);
        assert_eq!(parse_events(&md).len(), 1);
        // Trying again is a no-op.
        let again = remove_event_bullet(&mut md, "_[user: q2-review]_");
        assert!(!again);
    }

    #[test]
    fn parse_scheduled_at_accepts_common_shapes() {
        let utc: Tz = chrono_tz::UTC;

        // YYYY-MM-DD HH:MM (no TZ) → user TZ.
        assert!(parse_scheduled_at("2026-07-01 10:30", &utc).is_ok());

        // YYYY-MM-DD only → midnight user TZ.
        assert!(parse_scheduled_at("2026-07-01", &utc).is_ok());

        // RFC 3339.
        assert!(parse_scheduled_at("2026-07-01T10:30:00+00:00", &utc).is_ok());

        // Garbage rejected.
        assert!(parse_scheduled_at("tomorrow at noon", &utc).is_err());
    }

    #[test]
    fn split_event_id_validates_shape() {
        assert!(split_event_id("user:q2-review").is_ok());
        assert!(split_event_id("missing-colon").is_err());
        assert!(split_event_id(":empty-label").is_err());
        assert!(split_event_id("empty-key:").is_err());
    }
}
