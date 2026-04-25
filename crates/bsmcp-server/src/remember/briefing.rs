//! `/remember/v1/briefing/read` — the reconstitution dossier.
//!
//! Replaces the multi-call AI-driven bootstrap with a single structured pull.
//! All KB fetches run in parallel. Sections whose settings are missing are
//! silently omitted (Null in the response) — the call never fails because
//! some optional pointer is unset.

use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;

use super::envelope::ErrorCode;
use super::{frontmatter, singletons, Context, Outcome};

pub async fn read(ctx: &Context) -> Outcome {
    let user_prompt = ctx.body_str("user_prompt").unwrap_or_default();
    let recent_count = ctx
        .body
        .get("recent_journal_count")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(ctx.settings.recent_journal_count.max(1));
    let active_count = ctx
        .body
        .get("active_collage_count")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(ctx.settings.active_collage_count.max(1));

    // Identity manifest (page) + user manifest (page) — fetch in parallel.
    let identity_fut = fetch_optional_page(&ctx.client, ctx.settings.ai_identity_page_id);
    let user_fut = fetch_optional_page(&ctx.client, ctx.settings.user_identity_page_id);

    // Subagents (chapter listing).
    let subagents_fut = async {
        if let Some(chapter_id) = ctx.settings.ai_subagents_chapter_id {
            singletons::list_pages_in_chapter(chapter_id, ctx).await.unwrap_or_default()
        } else {
            Vec::new()
        }
    };

    // Recent journals (newest pages in the journal book).
    let recent_journals_fut = list_recent_pages(
        ctx.settings.ai_hive_journal_book_id,
        recent_count,
        &ctx.client,
    );

    // Active collage (newest topic pages).
    let active_collage_fut = list_recent_pages(
        ctx.settings.ai_collage_book_id,
        active_count,
        &ctx.client,
    );
    let shared_collage_fut = list_recent_pages(
        ctx.settings.ai_shared_collage_book_id,
        active_count,
        &ctx.client,
    );

    // Semantic search fan-out — one per configured target.
    let prompt_for_semantic = user_prompt.clone();
    let semantic_fut = async {
        if prompt_for_semantic.is_empty() {
            return SemanticSlice::default();
        }
        let Some(sem) = &ctx.semantic else {
            return SemanticSlice::default();
        };
        let mut slice = SemanticSlice::default();

        // Use a single semantic search and partition results by book/chapter.
        let raw = match sem
            .search(&prompt_for_semantic, 40, 0.40, true, false, &ctx.client)
            .await
        {
            Ok(v) => v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default(),
            Err(e) => {
                eprintln!("Briefing: semantic search failed: {e}");
                return slice;
            }
        };

        if ctx.settings.semantic_against_journal {
            slice.journal_matches = filter_by_book(&raw, ctx.settings.ai_hive_journal_book_id, 5);
        }
        if ctx.settings.semantic_against_collage {
            slice.collage_matches = filter_by_book(&raw, ctx.settings.ai_collage_book_id, 5);
        }
        if ctx.settings.semantic_against_shared_collage {
            slice.shared_collage_matches = filter_by_book(&raw, ctx.settings.ai_shared_collage_book_id, 5);
        }
        if ctx.settings.semantic_against_user_journal {
            slice.user_journal_matches = filter_by_book(&raw, ctx.settings.user_journal_book_id, 5);
        }
        if ctx.settings.semantic_against_full_kb {
            slice.kb_matches = raw.iter().take(10).cloned().collect();
        }
        slice
    };

    // Always-on context pages (writing style, communication prefs, etc).
    let system_prompt_fut = fetch_system_prompt_pages(
        &ctx.client,
        &ctx.settings.system_prompt_page_ids,
    );

    let (identity, user_page, subagents, recent_journals, active_collage, shared_collage, semantic, system_prompt) = tokio::join!(
        identity_fut,
        user_fut,
        subagents_fut,
        recent_journals_fut,
        active_collage_fut,
        shared_collage_fut,
        semantic_fut,
        system_prompt_fut,
    );

    Outcome::ok(json!({
        "identity": match identity {
            Some(p) => json!({
                "ouid": ctx.settings.ai_identity_ouid,
                "name": ctx.settings.ai_identity_name.clone()
                    .or_else(|| p.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())),
                "manifest": {
                    "page_id": ctx.settings.ai_identity_page_id,
                    "markdown": frontmatter::strip(p.get("markdown").and_then(|v| v.as_str()).unwrap_or("")),
                    "url": p.get("url").cloned().unwrap_or(Value::Null),
                }
            }),
            None => Value::Null,
        },
        "user": match user_page {
            Some(p) => json!({
                "user_id": ctx.settings.user_id,
                "identity_page": {
                    "page_id": ctx.settings.user_identity_page_id,
                    "markdown": frontmatter::strip(p.get("markdown").and_then(|v| v.as_str()).unwrap_or("")),
                    "url": p.get("url").cloned().unwrap_or(Value::Null),
                }
            }),
            None => json!({
                "user_id": ctx.settings.user_id,
                "identity_page": Value::Null,
            }),
        },
        "subagents": subagents,
        "journal_recent": recent_journals,
        "journal_semantic_matches": semantic.journal_matches,
        "collage_active": active_collage,
        "collage_semantic_matches": semantic.collage_matches,
        "shared_collage_active": shared_collage,
        "shared_collage_semantic_matches": semantic.shared_collage_matches,
        "user_journal_semantic_matches": semantic.user_journal_matches,
        "kb_semantic_matches": if ctx.settings.semantic_against_full_kb { json!(semantic.kb_matches) } else { Value::Null },
        "system_prompt_additions": system_prompt,
        "config": {
            "label": ctx.settings.label,
            "role": ctx.settings.role,
            "shelf_id": ctx.settings.ai_hive_shelf_id,
            "use_follow_up_remember_agent": ctx.settings.use_follow_up_remember_agent,
        },
    }))
}

async fn fetch_system_prompt_pages(client: &BookStackClient, ids: &[i64]) -> Vec<Value> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut handles = Vec::with_capacity(ids.len());
    for &id in ids {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            (id, client.get_page(id).await)
        }));
    }
    let mut out = Vec::with_capacity(ids.len());
    for h in handles {
        let Ok((id, result)) = h.await else { continue; };
        match result {
            Ok(page) => {
                let raw = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
                out.push(json!({
                    "page_id": id,
                    "name": page.get("name").cloned().unwrap_or(Value::Null),
                    "markdown": frontmatter::strip(raw),
                    "url": page.get("url").cloned().unwrap_or(Value::Null),
                }));
            }
            Err(e) => {
                eprintln!("Briefing: system_prompt_page_ids[{id}] fetch failed: {e}");
            }
        }
    }
    out
}

#[derive(Default)]
struct SemanticSlice {
    journal_matches: Vec<Value>,
    collage_matches: Vec<Value>,
    shared_collage_matches: Vec<Value>,
    user_journal_matches: Vec<Value>,
    kb_matches: Vec<Value>,
}

async fn fetch_optional_page(client: &BookStackClient, page_id: Option<i64>) -> Option<Value> {
    let id = page_id?;
    match client.get_page(id).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("Briefing: page {id} fetch failed: {e}");
            None
        }
    }
}

async fn list_recent_pages(book_id: Option<i64>, limit: usize, client: &BookStackClient) -> Vec<Value> {
    let Some(book_id) = book_id else { return Vec::new(); };
    let query = format!("{{type:page}} {{in_book:{book_id}}} {{updated_after:1970-01-01}}");
    let resp = match client.search(&query, 1, limit as i64).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Briefing: list_recent_pages({book_id}) failed: {e}");
            return Vec::new();
        }
    };
    let data = resp.get("data").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    data.into_iter()
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("page"))
        .take(limit)
        .map(|p| json!({
            "page_id": p.get("id").cloned().unwrap_or(Value::Null),
            "name": p.get("name").cloned().unwrap_or(Value::Null),
            "preview": p.get("preview_html").cloned().unwrap_or(Value::Null),
            "url": p.get("url").cloned().unwrap_or(Value::Null),
            "updated_at": p.get("updated_at").cloned().unwrap_or(Value::Null),
        }))
        .collect()
}

fn filter_by_book(hits: &[Value], book_id: Option<i64>, limit: usize) -> Vec<Value> {
    let Some(book_id) = book_id else { return Vec::new(); };
    hits.iter()
        .filter(|h| h.get("book_id").and_then(|v| v.as_i64()) == Some(book_id))
        .take(limit)
        .cloned()
        .collect()
}

// Suppress unused-import warning when this module is built with other features.
#[allow(dead_code)]
fn _used(_: ErrorCode) {}
