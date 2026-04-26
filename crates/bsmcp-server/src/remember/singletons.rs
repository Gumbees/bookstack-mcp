//! Singleton resources: whoami, user, config.
//!
//! These don't fit the collection model — each user has exactly one of each.
//! Reads pull straight from BookStack (or settings); writes update the
//! manifest page (or persist settings).

use serde_json::{json, Value};

use bsmcp_common::settings::{GlobalSettings, UserSettings};

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

// --- config (UserSettings + GlobalSettings) ---

pub async fn read_config(ctx: &Context) -> Outcome {
    let user_json = match serde_json::to_value(&ctx.settings) {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e.to_string(), None),
    };
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    let global_json = match serde_json::to_value(&globals) {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e.to_string(), None),
    };
    Outcome::ok(json!({
        "settings": user_json,
        "global_settings": global_json,
    }))
}

pub async fn write_config(ctx: &Context) -> Outcome {
    // Two optional sub-objects:
    //   - "settings"        : per-user UserSettings (anyone can write their own)
    //   - "global_settings" : instance-wide GlobalSettings (admins only,
    //                         server-side first-write-wins for set fields)
    let user_settings_arg = ctx.body.get("settings").cloned();
    let global_settings_arg = ctx.body.get("global_settings").cloned();

    if user_settings_arg.is_none() && global_settings_arg.is_none() {
        return Outcome::error(
            ErrorCode::InvalidArgument,
            "at least one of `settings` or `global_settings` is required",
            None,
        );
    }

    let mut warnings: Vec<super::envelope::RememberWarning> = Vec::new();
    let mut saved_user: Option<UserSettings> = None;
    let mut saved_globals: Option<GlobalSettings> = None;

    // --- per-user settings ---
    if let Some(raw) = user_settings_arg {
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
        saved_user = Some(new_settings);
    }

    // --- global settings (admin-only, first-write-wins) ---
    if let Some(raw) = global_settings_arg {
        let proposed: GlobalSettings = match serde_json::from_value(raw) {
            Ok(g) => g,
            Err(e) => {
                return Outcome::error(
                    ErrorCode::InvalidArgument,
                    format!("global_settings parse error: {e}"),
                    Some("global_settings"),
                );
            }
        };
        let is_admin = ctx.client.is_admin().await.unwrap_or(false);
        if !is_admin {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "global_settings can only be written by BookStack admins",
                Some("global_settings"),
            );
        }

        // Enforce first-write-wins server-side: only fields that are currently
        // null may be set; pre-existing values are preserved silently.
        let existing = ctx.db.get_global_settings().await.unwrap_or_default();
        let mut merged = existing.clone();
        if existing.hive_shelf_id.is_none() {
            if let Some(v) = proposed.hive_shelf_id {
                merged.hive_shelf_id = Some(v);
            }
        } else if proposed.hive_shelf_id.is_some()
            && proposed.hive_shelf_id != existing.hive_shelf_id
        {
            warnings.push(super::envelope::RememberWarning::new(
                "global_locked",
                "hive_shelf_id is already set; ignoring requested change (first-write-wins).",
            ));
        }
        if existing.user_journals_shelf_id.is_none() {
            if let Some(v) = proposed.user_journals_shelf_id {
                merged.user_journals_shelf_id = Some(v);
            }
        } else if proposed.user_journals_shelf_id.is_some()
            && proposed.user_journals_shelf_id != existing.user_journals_shelf_id
        {
            warnings.push(super::envelope::RememberWarning::new(
                "global_locked",
                "user_journals_shelf_id is already set; ignoring requested change (first-write-wins).",
            ));
        }

        if let Err(e) = ctx.db.save_global_settings(&merged, &ctx.token_id_hash).await {
            return Outcome::error(ErrorCode::InternalError, e, None);
        }
        saved_globals = Some(merged);
    }

    let mut outcome = Outcome::ok(json!({
        "action": "saved",
        "settings": saved_user,
        "global_settings": saved_globals,
    }));
    for w in warnings {
        outcome = outcome.with_warning(w);
    }
    outcome
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
