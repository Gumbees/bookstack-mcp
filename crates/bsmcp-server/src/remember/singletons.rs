//! Singleton resources: whoami, user, config.
//!
//! These don't fit the collection model — each user has exactly one of each.
//! Reads pull straight from BookStack (or settings); writes update the
//! manifest page (or persist settings).

use serde_json::{json, Value};

use bsmcp_common::settings::UserSettings;

use super::envelope::ErrorCode;
use super::frontmatter;
use super::{Context, Outcome};

// --- whoami ---

pub async fn read_whoami(ctx: &Context) -> Outcome {
    let page_id = match ctx.settings.ai_identity_page_id {
        Some(id) => id,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "ai_identity_page_id not configured",
                Some("ai_identity_page_id"),
            );
        }
    };

    let page = match ctx.client.get_page(page_id).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(ErrorCode::NotFound, e, Some("ai_identity_page_id")),
    };

    // Subagent listing (if chapter configured) — best-effort, runs after the manifest
    // because we need it to assemble the full whoami picture.
    let subagents = if let Some(chapter_id) = ctx.settings.ai_subagents_chapter_id {
        match list_pages_in_chapter(chapter_id, ctx).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Remember: subagent list failed (non-fatal): {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let raw_md = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
    let body = frontmatter::strip(raw_md).to_string();

    Outcome::ok(json!({
        "ouid": ctx.settings.ai_identity_ouid,
        "name": ctx.settings.ai_identity_name.clone()
            .or_else(|| page.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())),
        "manifest": {
            "page_id": page_id,
            "name": page.get("name").cloned().unwrap_or(Value::Null),
            "markdown": body,
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "updated_at": page.get("updated_at").cloned().unwrap_or(Value::Null),
        },
        "shelf_id": ctx.settings.ai_hive_shelf_id,
        "identity_book_id": ctx.settings.ai_identity_book_id,
        "subagents_chapter_id": ctx.settings.ai_subagents_chapter_id,
        "subagents": subagents,
        "books": {
            "journal": ctx.settings.ai_hive_journal_book_id,
            "collage": ctx.settings.ai_collage_book_id,
            "shared_collage": ctx.settings.ai_shared_collage_book_id,
        },
        "chapters": {
            "subagents": ctx.settings.ai_subagents_chapter_id,
            "connections": ctx.settings.ai_connections_chapter_id,
            "opportunities": ctx.settings.ai_opportunities_chapter_id,
            "activity": ctx.settings.ai_activity_chapter_id,
        },
    }))
}

pub async fn write_whoami(ctx: &Context) -> Outcome {
    let page_id = match ctx.settings.ai_identity_page_id {
        Some(id) => id,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "ai_identity_page_id not configured — set the manifest page in /settings first",
                Some("ai_identity_page_id"),
            );
        }
    };
    let body = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required",
                Some("body"),
            );
        }
    };

    let frontmatter_block = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        "whoami",
        None,
        Some(page_id),
    );
    let payload = json!({ "markdown": format!("{frontmatter_block}{body}") });
    match ctx.client.update_page(page_id, &payload).await {
        Ok(updated) => Outcome::ok_with_target(
            json!({
                "action": "updated",
                "id": page_id,
                "name": updated.get("name").cloned().unwrap_or(Value::Null),
                "url": updated.get("url").cloned().unwrap_or(Value::Null),
                "updated_at": updated.get("updated_at").cloned().unwrap_or(Value::Null),
            }),
            Some(page_id),
            None,
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

// --- user ---

pub async fn read_user(ctx: &Context) -> Outcome {
    let page_id = match ctx.settings.user_identity_page_id {
        Some(id) => id,
        None => {
            // user_id alone is enough for a partial response.
            if ctx.settings.user_id.is_some() {
                return Outcome::ok(json!({
                    "user_id": ctx.settings.user_id,
                    "identity_page": Value::Null,
                    "journal_book_id": ctx.settings.user_journal_book_id,
                }));
            }
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "user_identity_page_id not configured",
                Some("user_identity_page_id"),
            );
        }
    };

    let page = match ctx.client.get_page(page_id).await {
        Ok(p) => p,
        Err(e) => return Outcome::error(ErrorCode::NotFound, e, Some("user_identity_page_id")),
    };
    let raw_md = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
    let body = frontmatter::strip(raw_md).to_string();

    Outcome::ok(json!({
        "user_id": ctx.settings.user_id,
        "identity_page": {
            "page_id": page_id,
            "name": page.get("name").cloned().unwrap_or(Value::Null),
            "markdown": body,
            "url": page.get("url").cloned().unwrap_or(Value::Null),
            "updated_at": page.get("updated_at").cloned().unwrap_or(Value::Null),
        },
        "journal_book_id": ctx.settings.user_journal_book_id,
    }))
}

pub async fn write_user(ctx: &Context) -> Outcome {
    let page_id = match ctx.settings.user_identity_page_id {
        Some(id) => id,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "user_identity_page_id not configured",
                Some("user_identity_page_id"),
            );
        }
    };
    let body = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required",
                Some("body"),
            );
        }
    };
    let frontmatter_block = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        "user",
        None,
        Some(page_id),
    );
    let payload = json!({ "markdown": format!("{frontmatter_block}{body}") });
    match ctx.client.update_page(page_id, &payload).await {
        Ok(updated) => Outcome::ok_with_target(
            json!({
                "action": "updated",
                "id": page_id,
                "url": updated.get("url").cloned().unwrap_or(Value::Null),
            }),
            Some(page_id),
            None,
        ),
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

// --- config (UserSettings) ---

pub async fn read_config(ctx: &Context) -> Outcome {
    let json_value = match serde_json::to_value(&ctx.settings) {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e.to_string(), None),
    };
    Outcome::ok(json_value)
}

pub async fn write_config(ctx: &Context) -> Outcome {
    // Body must be a UserSettings JSON object (partial = full replace).
    let raw = match ctx.body.get("settings") {
        Some(v) => v.clone(),
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "settings field (object) is required",
                Some("settings"),
            );
        }
    };
    let new_settings: UserSettings = match serde_json::from_value(raw) {
        Ok(s) => s,
        Err(e) => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                format!("settings parse error: {e}"),
                Some("settings"),
            );
        }
    };
    if let Err(e) = ctx.db.save_user_settings(&ctx.token_id_hash, &new_settings).await {
        return Outcome::error(ErrorCode::InternalError, e, None);
    }
    Outcome::ok(json!({
        "action": "saved",
        "settings": new_settings,
    }))
}

// --- helpers ---

pub(super) async fn list_pages_in_chapter(
    chapter_id: i64,
    ctx: &Context,
) -> Result<Vec<Value>, String> {
    let query = format!("{{type:page}} {{in_chapter:{chapter_id}}}");
    let resp = ctx.client.search(&query, 1, 200).await?;
    let data = resp.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let pages: Vec<Value> = data
        .into_iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("page"))
        .map(|item| {
            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
            json!({
                "id": item.get("id").cloned().unwrap_or(Value::Null),
                "name": name.clone(),
                "expected_local_path": format!("agents/{}.md", super::frontmatter::slugify(&name)),
                "url": item.get("url").cloned().unwrap_or(Value::Null),
                "updated_at": item.get("updated_at").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(pages)
}
