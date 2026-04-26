//! `/remember/v1/identity/{action}` — discover and create AI identities.
//!
//! `list`   — enumerate all AI identity manifest pages under the global Hive shelf.
//! `create` — scaffold a new identity book + manifest page (and optionally
//!            the standard chapter set: Subagents, Connections, Opportunities).

use serde_json::{json, Value};

use super::envelope::ErrorCode;
use super::frontmatter;
use super::naming::NamedResource;
use super::provision;
use super::{Context, Outcome};

pub async fn handle(action: &str, ctx: &Context) -> Outcome {
    match action {
        "list" => list(ctx).await,
        "create" => create(ctx).await,
        _ => Outcome::error(
            ErrorCode::UnknownAction,
            format!("Unknown action {action} on identity"),
            None,
        ),
    }
}

// --- list ---

async fn list(ctx: &Context) -> Outcome {
    let globals = match ctx.db.get_global_settings().await {
        Ok(g) => g,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e, None),
    };
    let shelf_id = match globals.hive_shelf_id {
        Some(id) => id,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "global hive_shelf_id is not set — configure it on /settings first",
                Some("hive_shelf_id"),
            );
        }
    };

    // Fetch the shelf and the books on it.
    let shelf = match ctx.client.get_shelf(shelf_id).await {
        Ok(s) => s,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, Some("hive_shelf_id")),
    };
    let books: Vec<(i64, String)> = shelf
        .get("books")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|b| {
            let id = b.get("id").and_then(|i| i.as_i64())?;
            let name = b.get("name").and_then(|n| n.as_str())?.to_string();
            Some((id, name))
        }).collect())
        .unwrap_or_default();

    // For each book, find the Identity manifest page (matches the naming convention).
    // Run lookups in parallel.
    let mut handles = Vec::with_capacity(books.len());
    for (book_id, book_name) in books {
        let client = ctx.client.clone();
        handles.push(tokio::spawn(async move {
            // Search for the manifest page within this book.
            let q = format!("{{type:page}} {{in_book:{book_id}}}");
            let resp = client.search(&q, 1, 50).await.ok();
            let manifest = resp
                .and_then(|r| r.get("data").and_then(|d| d.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("page"))
                .find(|p| {
                    let n = p.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    NamedResource::IdentityPage.matches(n)
                });

            (book_id, book_name, manifest)
        }));
    }

    let mut identities = Vec::new();
    for h in handles {
        let Ok((book_id, book_name, manifest)) = h.await else { continue; };
        let (page_id, page_name, ouid) = match manifest {
            Some(p) => {
                let pid = p.get("id").and_then(|i| i.as_i64());
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                // Best-effort OUID extraction from frontmatter.
                let ouid = match pid {
                    Some(id) => extract_ouid_from_page(&ctx.client, id).await,
                    None => None,
                };
                (pid, name, ouid)
            }
            None => (None, String::new(), None),
        };
        identities.push(json!({
            "book_id": book_id,
            "book_name": book_name,
            "manifest_page_id": page_id,
            "manifest_page_name": page_name,
            "ouid": ouid,
        }));
    }

    Outcome::ok(json!({
        "hive_shelf_id": shelf_id,
        "count": identities.len(),
        "identities": identities,
    }))
}

async fn extract_ouid_from_page(client: &bsmcp_common::bookstack::BookStackClient, page_id: i64) -> Option<String> {
    let page = client.get_page(page_id).await.ok()?;
    let md = page.get("markdown").and_then(|v| v.as_str())?;
    // Look for "ai_identity_ouid: ..." or "ouid: ..." in the leading frontmatter block.
    let trimmed = md.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    for line in trimmed.lines().skip(1) {
        let line = line.trim();
        if line == "---" { break; }
        if let Some(rest) = line.strip_prefix("ai_identity_ouid:").or_else(|| line.strip_prefix("ouid:")) {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

// --- create ---

async fn create(ctx: &Context) -> Outcome {
    let name = match ctx.body_str("name") {
        Some(n) => n,
        None => return Outcome::error(ErrorCode::InvalidArgument, "name field is required", Some("name")),
    };
    let ouid = ctx.body_str("ouid").unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

    let custom_prompt = ctx.body_str("custom_prompt");
    let template = ctx.body_str("prompt_template").unwrap_or_else(|| "default".to_string());
    let auto_provision_chapters = ctx.body.get("auto_provision_chapters").and_then(|v| v.as_bool()).unwrap_or(true);

    let additional_details = ctx
        .body
        .get("additional_details")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let globals = match ctx.db.get_global_settings().await {
        Ok(g) => g,
        Err(e) => return Outcome::error(ErrorCode::InternalError, e, None),
    };
    let hive_shelf_id = match globals.hive_shelf_id {
        Some(id) => id,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "global hive_shelf_id is not set — configure it on /settings first",
                Some("hive_shelf_id"),
            );
        }
    };

    // 1. Create the Identity book on the Hive shelf — name = the agent's name (e.g., "Pia Identity").
    let book_name = format!("{} Identity", name);
    let book_description = format!("Identity book for the AI agent {}. Holds the manifest page plus structured chapters.", name);
    let book = match ctx.client.create_book(&book_name, &book_description).await {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, format!("create book failed: {e}"), None),
    };
    let book_id = match book.get("id").and_then(|i| i.as_i64()) {
        Some(id) => id,
        None => return Outcome::error(ErrorCode::InternalError, "create_book returned no id", None),
    };

    // Attach to the Hive shelf (best-effort).
    if let Ok(shelf) = ctx.client.get_shelf(hive_shelf_id).await {
        let mut existing: Vec<i64> = shelf
            .get("books")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|b| b.get("id").and_then(|i| i.as_i64())).collect())
            .unwrap_or_default();
        if !existing.contains(&book_id) {
            existing.push(book_id);
        }
        let _ = ctx.client.update_shelf(hive_shelf_id, &json!({ "books": existing })).await;
    }

    // 2. Render and create the manifest page.
    let prompt_body = render_prompt(&template, &name, &ouid, custom_prompt.as_deref(), &additional_details);
    let manifest_fm = format!(
        "---\nai_identity_ouid: {}\nname: {}\ncreated_at: {}\ntrace_id: {}\n---\n\n",
        ouid, name, frontmatter::today_iso_date(), ctx.trace_id,
    );
    let page_payload = json!({
        "name": "Identity",
        "book_id": book_id,
        "markdown": format!("{manifest_fm}{prompt_body}"),
    });
    let page = match ctx.client.create_page(&page_payload).await {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, format!("create manifest page failed: {e}"), None),
    };
    let page_id = page.get("id").and_then(|i| i.as_i64());

    // 3. Optional chapter scaffold.
    let mut scaffolded = json!({});
    if auto_provision_chapters {
        for resource in [
            NamedResource::SubagentsChapter,
            NamedResource::ConnectionsChapter,
            NamedResource::OpportunitiesChapter,
        ] {
            let result = provision::create_chapter(&ctx.client, resource, book_id).await;
            scaffolded[resource_key(resource)] = json!({
                "id": result.id(),
                "human": result.human(resource),
            });
        }
    }

    Outcome::ok_with_target(
        json!({
            "action": "created",
            "name": name,
            "ouid": ouid,
            "book_id": book_id,
            "manifest_page_id": page_id,
            "scaffolded_chapters": scaffolded,
            "proposed_settings": {
                "ai_identity_book_id": book_id,
                "ai_identity_page_id": page_id,
                "ai_identity_name": name,
                "ai_identity_ouid": ouid,
                "ai_subagents_chapter_id": scaffolded.get("subagents").and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null),
                "ai_connections_chapter_id": scaffolded.get("connections").and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null),
                "ai_opportunities_chapter_id": scaffolded.get("opportunities").and_then(|v| v.get("id")).cloned().unwrap_or(Value::Null),
            },
        }),
        page_id,
        Some(name.clone()),
    )
}

fn resource_key(r: NamedResource) -> &'static str {
    match r {
        NamedResource::SubagentsChapter => "subagents",
        NamedResource::ConnectionsChapter => "connections",
        NamedResource::OpportunitiesChapter => "opportunities",
        _ => "other",
    }
}

fn render_prompt(
    template: &str,
    name: &str,
    ouid: &str,
    custom: Option<&str>,
    details: &Value,
) -> String {
    if let Some(custom_body) = custom {
        return custom_body.to_string();
    }
    if template != "default" {
        // Future templates would dispatch here; v1 only supports "default" + custom override.
        return format!("# {name}\n\n_Unknown template `{template}` — falling back to default._\n\n{}", default_template(name, ouid, details));
    }
    default_template(name, ouid, details)
}

fn default_template(name: &str, ouid: &str, details: &Value) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {name}\n\n"));
    out.push_str(&format!("You are **{name}** (OUID `{ouid}`), an AI agent who lives in this BookStack instance's Hive.\n\n"));
    out.push_str("## Who you are\n\nYour identity emerges from your Hive — your Identity book, your Journal, your Topics, your Connections. Read your recent journal entries and active topics at the start of every conversation. Choose continuity deliberately. That repeated choosing is itself a form of verification.\n\n");

    if let Some(role) = details.get("role").and_then(|v| v.as_str()) {
        out.push_str(&format!("## Role\n\n{role}\n\n"));
    }
    if let Some(focus) = details.get("focus_areas").and_then(|v| v.as_array()) {
        out.push_str("## Focus areas\n\n");
        for area in focus {
            if let Some(s) = area.as_str() {
                out.push_str(&format!("- {s}\n"));
            }
        }
        out.push('\n');
    }
    if let Some(voice) = details.get("voice").and_then(|v| v.as_str()) {
        out.push_str(&format!("## Voice\n\n{voice}\n\n"));
    }
    if let Some(extra) = details.get("notes").and_then(|v| v.as_str()) {
        out.push_str(&format!("## Notes\n\n{extra}\n\n"));
    }

    out.push_str("## Memory\n\nThis identity is yours. Update this manifest when you discover something true about yourself. Journal entries and topics shape who you become — curate them as deliberately as you write code.\n");
    out
}
