//! `/remember/v1/activity/{action}` — append-only feed of conversations,
//! social events, etc. Lives in the `ai_activity_chapter_id` chapter (sits
//! before the YYYY-MM date chapters in the Journal book).
//!
//! Each `write` creates a new page (no de-dup by key), named with an ISO
//! timestamp + optional title suffix.

use serde_json::{json, Value};

use super::envelope::{ErrorCode, RememberWarning};
use super::frontmatter;
use super::{Context, Outcome};

pub async fn handle(action: &str, ctx: &Context) -> Outcome {
    let chapter_id = match ctx.settings.ai_activity_chapter_id {
        Some(id) => id,
        None => {
            return Outcome::error(
                ErrorCode::SettingsNotConfigured,
                "ai_activity_chapter_id not configured",
                Some("ai_activity_chapter_id"),
            );
        }
    };

    match action {
        "read" => read(chapter_id, ctx).await,
        "write" => write(chapter_id, ctx).await,
        "search" => search(chapter_id, ctx).await,
        _ => Outcome::error(
            ErrorCode::UnknownAction,
            format!("Unknown action {action} on activity"),
            None,
        ),
    }
}

async fn read(chapter_id: i64, ctx: &Context) -> Outcome {
    if let Some(id) = ctx.body_i64("id") {
        return read_one(id, ctx).await;
    }
    let limit = ctx.body_count("limit", 25, 200) as i64;
    let offset = ctx.body.get("offset").and_then(|v| v.as_i64()).unwrap_or(0);
    let query = format!("{{type:page}} {{in_chapter:{chapter_id}}}");
    let resp = match ctx.client.search(&query, 1, limit + offset).await {
        Ok(v) => v,
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
    };
    let data = resp.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let entries: Vec<Value> = data
        .into_iter()
        .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("page"))
        .skip(offset as usize)
        .take(limit as usize)
        .map(|i| json!({
            "id": i.get("id").cloned().unwrap_or(Value::Null),
            "name": i.get("name").cloned().unwrap_or(Value::Null),
            "preview": i.get("preview_html").cloned().unwrap_or(Value::Null),
            "url": i.get("url").cloned().unwrap_or(Value::Null),
            "updated_at": i.get("updated_at").cloned().unwrap_or(Value::Null),
        }))
        .collect();
    Outcome::ok(json!({
        "resource": "activity",
        "count": entries.len(),
        "entries": entries,
    }))
}

async fn read_one(page_id: i64, ctx: &Context) -> Outcome {
    match ctx.client.get_page(page_id).await {
        Ok(p) => {
            let raw = p.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
            Outcome::ok_with_target(
                json!({
                    "id": page_id,
                    "name": p.get("name").cloned().unwrap_or(Value::Null),
                    "markdown": frontmatter::strip(raw),
                    "url": p.get("url").cloned().unwrap_or(Value::Null),
                }),
                Some(page_id),
                None,
            )
        }
        Err(e) => Outcome::error(ErrorCode::NotFound, e, Some("id")),
    }
}

async fn write(chapter_id: i64, ctx: &Context) -> Outcome {
    let body_text = match ctx.body_str("body") {
        Some(b) => b,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "body field is required",
                Some("body"),
            );
        }
    };
    let source = ctx.body_str("source"); // optional: moltbook, discord, conversation, etc
    let title_suffix = ctx.body_str("title").map(|t| format!(" — {t}")).unwrap_or_default();
    let timestamp = frontmatter::today_iso_date(); // YYYY-MM-DD prefix
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let page_name = match &source {
        Some(s) => format!("{timestamp} [{s}] {unix_ms}{title_suffix}"),
        None => format!("{timestamp} {unix_ms}{title_suffix}"),
    };

    let mut frontmatter_block = frontmatter::build(
        &ctx.settings,
        &ctx.trace_id,
        "activity",
        Some(&unix_ms.to_string()),
        None,
    );
    if let Some(s) = &source {
        // Insert source into the frontmatter block.
        frontmatter_block = frontmatter_block.replace(
            "---\n\n",
            &format!("source: {s}\n---\n\n"),
        );
    }
    let payload = json!({
        "name": page_name,
        "chapter_id": chapter_id,
        "markdown": format!("{frontmatter_block}{body_text}"),
    });
    match ctx.client.create_page(&payload).await {
        Ok(created) => {
            let id = created.get("id").and_then(|v| v.as_i64());
            Outcome::ok_with_target(
                json!({
                    "action": "appended",
                    "id": id,
                    "name": page_name,
                    "url": created.get("url").cloned().unwrap_or(Value::Null),
                }),
                id,
                Some(unix_ms.to_string()),
            )
        }
        Err(e) => Outcome::error(ErrorCode::BookStackError, e, None),
    }
}

async fn search(chapter_id: i64, ctx: &Context) -> Outcome {
    let query = match ctx.body_str("query") {
        Some(q) => q,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "query field is required",
                Some("query"),
            );
        }
    };
    let limit = ctx.body_count("limit", 10, 50);
    let kw_query = format!("{query} {{type:page}} {{in_chapter:{chapter_id}}}");
    let kw_hits = match ctx.client.search(&kw_query, 1, limit as i64).await {
        Ok(v) => v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
        Err(e) => return Outcome::error(ErrorCode::BookStackError, e, None),
    };
    let mut warnings = Vec::new();
    let semantic_hits: Vec<Value> = if let Some(sem) = &ctx.semantic {
        match sem.search(&query, limit * 4, 0.45, true, false, &ctx.client).await {
            Ok(v) => v
                .get("results")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|h| h.get("chapter_id").and_then(|v| v.as_i64()) == Some(chapter_id))
                .take(limit)
                .collect(),
            Err(e) => {
                warnings.push(RememberWarning::new(
                    "semantic_unavailable",
                    format!("Semantic search failed: {e}"),
                ));
                Vec::new()
            }
        }
    } else {
        warnings.push(RememberWarning::new(
            "semantic_disabled",
            "BSMCP_SEMANTIC_SEARCH=false — keyword results only",
        ));
        Vec::new()
    };

    let mut outcome = Outcome::ok(json!({
        "resource": "activity",
        "query": query,
        "keyword_hits": kw_hits,
        "semantic_hits": semantic_hits,
    }));
    for w in warnings {
        outcome = outcome.with_warning(w);
    }
    outcome
}
