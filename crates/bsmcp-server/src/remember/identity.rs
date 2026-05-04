//! `remember/identity` — read/write the per-user identity narrative
//! (single page) and per-user-per-agent AI identity narratives (one
//! chapter + one page per agent).
//!
//! Layout (inside the user's per-user Journal book — see
//! `resolve_user_journal_book`):
//!   - `User Identity` chapter -> `User Identity` page
//!   - `AI Identity: {agent_name}` chapter -> `AI Identity: {agent_name}` page
//!     (one chapter per agent the user works with)
//!
//! Pages are written DIRECTLY by the AI as raw markdown — the AI overwrites
//! the page body wholesale via `identity write`. Bootstrap fires on first
//! read or first write when the chapter/page is missing; it stamps a tiny
//! seed (name + email) and the AI replaces it on its first write.

use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::resolvers::{
    normalize_agent_name, resolve_ai_identity_page, resolve_user_identity_page, ResolverError,
};
use super::{Context, DispatchResult};

/// Page-content field returned by BookStack `GET /api/pages/{id}` for
/// markdown-editor pages. WYSIWYG pages would carry `html` instead — we
/// always create our identity pages with `markdown` so the editor field
/// settles to "markdown".
const PAGE_MARKDOWN_FIELD: &str = "markdown";

pub async fn read(ctx: &Context) -> DispatchResult {
    let target = parse_target(ctx)?;
    let mut settings = ctx.settings.clone();
    let globals = ctx
        .db
        .get_global_settings()
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_global_settings failed: {e}")))?;

    let (page_id, bootstrapped) = match target {
        Target::User => resolve_user_identity_page(
            &ctx.token_id_hash,
            &mut settings,
            &ctx.client,
            ctx.db.clone(),
            &globals,
        )
        .await
        .map_err(resolver_to_envelope)?,
        Target::Agent(name) => resolve_ai_identity_page(
            &name,
            &ctx.token_id_hash,
            &mut settings,
            &ctx.client,
            ctx.db.clone(),
            &globals,
        )
        .await
        .map_err(resolver_to_envelope)?,
    };

    // One get_page covers both chapter_id and content — saves the round
    // trip we'd otherwise pay to read each separately.
    let page = ctx
        .client
        .get_page(page_id)
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_page failed: {e}")))?;
    let chapter_id = page
        .get("chapter_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            (
                ErrorCode::InternalError,
                format!("page {page_id} response missing chapter_id field"),
            )
        })?;
    let content = page
        .get(PAGE_MARKDOWN_FIELD)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(json!({
        "content": content,
        "page_id": page_id,
        "chapter_id": chapter_id,
        "bootstrapped": bootstrapped,
    }))
}

pub async fn write(ctx: &Context) -> DispatchResult {
    let target = parse_target(ctx)?;
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

    let mut settings = ctx.settings.clone();
    let globals = ctx
        .db
        .get_global_settings()
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("get_global_settings failed: {e}")))?;

    let (page_id, _bootstrapped) = match &target {
        Target::User => resolve_user_identity_page(
            &ctx.token_id_hash,
            &mut settings,
            &ctx.client,
            ctx.db.clone(),
            &globals,
        )
        .await
        .map_err(resolver_to_envelope)?,
        Target::Agent(name) => resolve_ai_identity_page(
            name,
            &ctx.token_id_hash,
            &mut settings,
            &ctx.client,
            ctx.db.clone(),
            &globals,
        )
        .await
        .map_err(resolver_to_envelope)?,
    };
    let bytes_written = content.len();

    let updated = ctx
        .client
        .update_page(
            page_id,
            &json!({
                "markdown": content,
            }),
        )
        .await
        .map_err(|e| (ErrorCode::InternalError, format!("update_page failed: {e}")))?;
    let chapter_id = updated
        .get("chapter_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            (
                ErrorCode::InternalError,
                format!("update_page {page_id} response missing chapter_id field"),
            )
        })?;

    Ok(json!({
        "page_id": page_id,
        "chapter_id": chapter_id,
        "bytes_written": bytes_written,
    }))
}

enum Target {
    User,
    Agent(String),
}

fn parse_target(ctx: &Context) -> Result<Target, (ErrorCode, String)> {
    let raw = ctx
        .body
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                ErrorCode::InvalidArgument,
                "Missing required argument: target (\"user\" or \"agent\")".to_string(),
            )
        })?;
    match raw {
        "user" => Ok(Target::User),
        "agent" => {
            let raw_name = ctx
                .body
                .get("agent_name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    (
                        ErrorCode::InvalidArgument,
                        "Missing required argument: agent_name (required when target=\"agent\")"
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
            Ok(Target::Agent(normalized))
        }
        other => Err((
            ErrorCode::InvalidArgument,
            format!("Invalid target `{other}`: must be \"user\" or \"agent\""),
        )),
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
