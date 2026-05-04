//! `remember/sessions` — capture AI sessions (forager, Claude Code
//! hooks, etc.) into BookStack pages, with a (`session_id` → page)
//! lookup table for fast list / read.
//!
//! BookStack layout (inside `resolve_user_journal_book` for the
//! calling user):
//!   - Per-agent chapter: `Sessions: {agent_name}`
//!   - Page per session: `{YYYY-MM-DD}-{title|session_id_short}`
//!   - Each `append` call adds a new markdown block to the bottom:
//!
//! ```text
//! ## Block N — YYYY-MM-DD HH:MM:SS TZ
//!
//! {content}
//! ```
//!
//! The `sessions` DB table indexes `session_id → page_id`, so resume
//! flows don't need to re-walk BookStack to find the right page.
//!
//! Auth/gating: writes (`append`) require `journaling_enabled = true`
//! on the calling user's settings — same primary/secondary instance
//! topology as `journal::write` and `identity::write`. Reads (`list`,
//! `read`) are unconditional for the calling user.
//!
//! Out of scope for the v1.0.0 surface (deferred to Phase 6+):
//!   - `search` action — needs cross-encoder rerank to be useful.

use chrono::{Datelike, NaiveDate, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use bsmcp_common::db::SessionRow;

use super::envelope::ErrorCode;
use super::resolvers::{
    normalize_agent_name, resolve_sessions_chapter, resolve_user_journal_book,
    session_page_name, ResolverError,
};
use super::{Context, DispatchResult};

const PAGE_MARKDOWN_FIELD: &str = "markdown";

pub async fn append(ctx: &Context) -> DispatchResult {
    if !ctx.settings.journaling_enabled {
        return Err((
            ErrorCode::Forbidden,
            "session writes not enabled on this instance — flip \
             `journaling_enabled = true` in /setup/user (or `user write`) \
             if you want this MCP to be a session capture target".to_string(),
        ));
    }

    let session_id = body_required_str(ctx, "session_id")?;
    if session_id.trim().is_empty() {
        return Err((
            ErrorCode::InvalidArgument,
            "`session_id` must not be empty".to_string(),
        ));
    }
    let agent_name_raw = body_required_str(ctx, "agent_name")?;
    let agent_name = normalize_agent_name(&agent_name_raw).ok_or_else(|| {
        (
            ErrorCode::InvalidArgument,
            format!(
                "Invalid `agent_name`: `{agent_name_raw}` — must be ASCII alphanumerics + dashes/underscores after normalization"
            ),
        )
    })?;
    let content = body_required_str(ctx, "content")?;
    if content.trim().is_empty() {
        return Err((
            ErrorCode::InvalidArgument,
            "`content` must not be empty".to_string(),
        ));
    }
    let title = ctx
        .body
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (now_local, tz_label) = local_now(ctx);

    // 1. Look up existing session row. If present we resume; otherwise
    //    we'll resolve a fresh page below.
    let existing = ctx
        .db
        .get_session(&session_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_session failed: {e}")))?;

    // Reject session_id collisions across users — prevents one token
    // from clobbering another's session by guessing an id.
    if let Some(ref row) = existing {
        if row.token_id_hash != ctx.token_id_hash {
            return Err((
                ErrorCode::Forbidden,
                "session_id is already bound to another user".to_string(),
            ));
        }
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

    let chapter_id = resolve_sessions_chapter(book_id, &agent_name, &ctx.client)
        .await
        .map_err(resolver_to_envelope)?;

    // 2. Resolve the BookStack page — existing session reuses its
    //    page; new session creates one with the seed.
    let (page_id, started_at, prior_block_count) = if let Some(row) = existing {
        (row.bookstack_page_id, row.started_at, row.block_count)
    } else {
        let date = NaiveDate::from_ymd_opt(now_local.year(), now_local.month(), now_local.day())
            .ok_or_else(|| {
                (
                    ErrorCode::InternalError,
                    format!(
                        "computed local date out of range: y={} m={} d={}",
                        now_local.year(),
                        now_local.month(),
                        now_local.day()
                    ),
                )
            })?;
        let page_name = session_page_name(date, title.as_deref(), &session_id);
        let seed = format!(
            "_session_id_: `{session_id}`\n\
             _agent_: `{agent_name}`\n\
             _started_: {now_local:?} {tz_label}\n",
        );
        let created = ctx
            .client
            .create_page(&json!({
                "chapter_id": chapter_id,
                "name": page_name,
                "markdown": seed,
            }))
            .await
            .map_err(|e| (ErrorCode::InternalError, format!("create_page failed: {e}")))?;
        let page_id = created
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                (
                    ErrorCode::InternalError,
                    "create_page response missing id".to_string(),
                )
            })?;
        (page_id, now_secs, 0)
    };

    // 3. Append a new block.
    let next_block = prior_block_count + 1;
    let timestamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
        now_local.year(),
        now_local.month(),
        now_local.day(),
        now_local.hour(),
        now_local.minute(),
        now_local.second(),
        tz_label
    );
    let block = format!(
        "\n\n## Block {next_block} — {timestamp}\n\n{}\n",
        content.trim_end()
    );
    // Read current body, append, write back. Mirrors the journal::write
    // append flow — BookStack has no native partial-append surface for
    // markdown pages.
    let current = ctx
        .client
        .get_page(page_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_page failed: {e}")))?;
    let body = current
        .get(PAGE_MARKDOWN_FIELD)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let new_body = format!("{body}{block}");
    ctx.client
        .update_page(page_id, &json!({ "markdown": new_body }))
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("update_page failed: {e}")))?;

    // 4. Upsert the session row.
    let row = SessionRow {
        session_id: session_id.clone(),
        token_id_hash: ctx.token_id_hash.clone(),
        agent_name: agent_name.clone(),
        bookstack_page_id: page_id,
        chapter_id,
        book_id,
        started_at,
        last_appended_at: now_secs,
        block_count: next_block,
        title: title.clone(),
    };
    ctx.db
        .upsert_session(&row)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("upsert_session failed: {e}")))?;

    Ok(json!({
        "session_id": session_id,
        "agent_name": agent_name,
        "page_id": page_id,
        "chapter_id": chapter_id,
        "book_id": book_id,
        "block_count": next_block,
        "started_at": started_at,
        "last_appended_at": now_secs,
        "title": title,
    }))
}

pub async fn list(ctx: &Context) -> DispatchResult {
    let agent_filter = ctx
        .body
        .get("agent_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let limit = ctx
        .body
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .clamp(1, 500);

    let rows = ctx
        .db
        .list_sessions(
            &ctx.token_id_hash,
            agent_filter.as_deref(),
            limit,
        )
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("list_sessions failed: {e}")))?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "session_id": r.session_id,
                "agent_name": r.agent_name,
                "title": r.title,
                "page_id": r.bookstack_page_id,
                "chapter_id": r.chapter_id,
                "book_id": r.book_id,
                "started_at": r.started_at,
                "last_appended_at": r.last_appended_at,
                "block_count": r.block_count,
            })
        })
        .collect();

    Ok(json!({ "items": items }))
}

pub async fn read(ctx: &Context) -> DispatchResult {
    let session_id = body_required_str(ctx, "session_id")?;
    let row = ctx
        .db
        .get_session(&session_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_session failed: {e}")))?
        .ok_or_else(|| {
            (
                ErrorCode::InvalidArgument,
                format!("session_id `{session_id}` not found"),
            )
        })?;

    if row.token_id_hash != ctx.token_id_hash {
        return Err((
            ErrorCode::Forbidden,
            "session belongs to another user".to_string(),
        ));
    }

    let page = ctx
        .client
        .get_page(row.bookstack_page_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_page failed: {e}")))?;
    let markdown = page
        .get(PAGE_MARKDOWN_FIELD)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "session_id": row.session_id,
        "agent_name": row.agent_name,
        "title": row.title,
        "page_id": row.bookstack_page_id,
        "chapter_id": row.chapter_id,
        "book_id": row.book_id,
        "started_at": row.started_at,
        "last_appended_at": row.last_appended_at,
        "block_count": row.block_count,
        "content": markdown,
    }))
}

// ---------- helpers ----------

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

fn local_now(ctx: &Context) -> (chrono::DateTime<chrono::FixedOffset>, String) {
    let tz_str = ctx.settings.timezone.as_deref().unwrap_or("UTC");
    let tz: Tz = tz_str.parse().unwrap_or(chrono_tz::UTC);
    let now_utc = Utc::now();
    let in_tz = tz.from_utc_datetime(&now_utc.naive_utc());
    let abbrev = in_tz.format("%Z").to_string();
    let label = if abbrev.is_empty() || abbrev.starts_with(['+', '-']) {
        tz.name().to_string()
    } else {
        abbrev
    };
    (in_tz.with_timezone(&in_tz.offset().fix()), label)
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
    use super::super::resolvers::{session_page_name, sessions_chapter_name};

    #[test]
    fn chapter_name_includes_agent() {
        assert_eq!(sessions_chapter_name("pia"), "Sessions: pia");
        assert_eq!(sessions_chapter_name("forager"), "Sessions: forager");
    }

    #[test]
    fn page_name_uses_title_when_supplied() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let name = session_page_name(date, Some("Architecture review"), "01J7FK0…");
        assert_eq!(name, "2026-05-04-architecture-review");
    }

    #[test]
    fn page_name_falls_back_to_session_id_short() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let name = session_page_name(date, None, "01J7FK0G2H3KZ");
        // First 8 alphanumerics from the session_id.
        assert_eq!(name, "2026-05-04-01J7FK0G");
    }

    #[test]
    fn page_name_falls_back_when_title_blank() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let name = session_page_name(date, Some("   "), "ZZZZZZZZZZZZ");
        assert_eq!(name, "2026-05-04-ZZZZZZZZ");
    }
}
