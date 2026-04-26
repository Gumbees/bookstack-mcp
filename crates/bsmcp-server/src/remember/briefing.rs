//! `/remember/v1/briefing/read` — the reconstitution dossier.
//!
//! Replaces the multi-call AI-driven bootstrap with a single structured pull.
//! All KB fetches run in parallel. Sections whose settings are missing are
//! silently omitted (Null in the response) — the call never fails because
//! some optional pointer is unset.

use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;

use super::envelope::ErrorCode;
use super::{frontmatter, Context, Outcome};

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

    // Resolve identity with org-default fallback. If the user hasn't set
    // their own ai_identity_page_id but the org has a default, use it.
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();
    let resolved = globals.resolve_identity(&ctx.settings);

    // Identity manifest (page) + user manifest (page) — fetch in parallel.
    let identity_fut = fetch_optional_page(&ctx.client, resolved.page_id);
    let user_fut = fetch_optional_page(&ctx.client, ctx.settings.user_identity_page_id);

    // Recent journals (newest pages in each journal book — both the AI's
    // reflection journal and the user's personal journal).
    let recent_journals_fut = list_recent_pages(
        ctx.settings.ai_hive_journal_book_id,
        recent_count,
        &ctx.client,
    );
    let recent_user_journal_fut = list_recent_pages(
        ctx.settings.user_journal_book_id,
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
    //
    // The query string we send to `sem.search` is the user's prompt prefixed
    // with a `[Context: ...]` block carrying current time, timezone, user
    // identity, and AI identity. Both halves of hybrid search benefit:
    //   - Embedding side: the query vector is enriched with date / user
    //     signal so prompts like "what was I working on yesterday" can match
    //     pages dated relative to *today*, not relative to whenever the
    //     embedding model was trained.
    //   - Keyword side: the user_id and identity name appear in pages that
    //     mention the same person, biasing relevance toward their content.
    let prompt_for_semantic = build_semantic_query(&user_prompt, ctx);
    let semantic_fut = async {
        if user_prompt.is_empty() {
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

    // Always-on context pages — three sources, all run in parallel:
    //   - user-configured (system_prompt_page_ids)
    //   - org-required instructions (admin-mandated page IDs)
    //   - org-required AI usage policy (admin-mandated page IDs)
    let user_pages_fut = fetch_pages_with_source(
        &ctx.client,
        &ctx.settings.system_prompt_page_ids,
        "user",
    );
    let org_instructions_fut = fetch_pages_with_source(
        &ctx.client,
        &globals.org_required_instructions_page_ids,
        "org_instructions",
    );
    let org_policy_fut = fetch_pages_with_source(
        &ctx.client,
        &globals.org_ai_usage_policy_page_ids,
        "org_policy",
    );

    let (identity, user_page, recent_journals, recent_user_journal, active_collage, shared_collage, semantic, user_pages, org_instructions, org_policy) = tokio::join!(
        identity_fut,
        user_fut,
        recent_journals_fut,
        recent_user_journal_fut,
        active_collage_fut,
        shared_collage_fut,
        semantic_fut,
        user_pages_fut,
        org_instructions_fut,
        org_policy_fut,
    );

    // Merge the three sources into one flat array. Each entry carries its
    // `source` field so callers can group/filter as needed.
    let mut system_prompt: Vec<Value> = Vec::with_capacity(
        user_pages.len() + org_instructions.len() + org_policy.len(),
    );
    system_prompt.extend(user_pages);
    system_prompt.extend(org_instructions);
    system_prompt.extend(org_policy);

    // Setup nudge — show when the user hasn't configured anything AND hasn't
    // snoozed the reminder. Suppressed once they save anything to /settings or
    // explicitly dismiss via remember_config.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let snoozed = ctx.settings.settings_nudge_dismissed_until.map(|t| now_unix < t).unwrap_or(false);
    let setup_nudge = if !ctx.settings.is_configured() && !snoozed {
        Some(json!({
            "show": true,
            "summary": "Your Hive memory settings aren't configured yet. Briefing is running on org defaults (where set) or empty sections.",
            "two_paths": {
                "ui": "Visit the MCP server's /settings page in a browser — fill in dropdowns or use 'Probe existing Hive' to auto-detect.",
                "mcp_guided": "Have the AI walk you through it via tool calls (recommended for chat-driven setups). See `suggested_workflow` below."
            },
            "suggested_workflow": [
                "1. Ask the user what they want: a fresh identity, or to adopt an existing agent/structure that's already in this BookStack.",
                "2. If existing: call `remember_directory action=read kind=identities` to see what's already on the global Hive shelf, and `remember_directory action=read kind=user_journals` for journals. If those return settings_not_configured, the global shelves themselves aren't set — surface that to the user (only an admin can fix).",
                "3. Use `search_content` with queries like '{type:book} Identity', '{type:book} Journal', '{type:book} Topics' to find candidate content that may be elsewhere in BookStack and should belong on the Hive shelf.",
                "4. For each match, propose to the user whether to (a) adopt it as-is by writing the ID into config, or (b) move it onto the Hive shelf first using `move_book_to_shelf` / `move_chapter` / `move_page`, then write the ID.",
                "5. For brand-new structure, use `remember_identity action=create name=...` to scaffold a full Identity book + manifest + standard chapters in one call.",
                "6. Save the resolved IDs with `remember_config action=write` and a `settings` object. The next briefing will reflect the new config and the nudge will stop showing."
            ],
            "key_tools": [
                "remember_directory  — discover what's on the global shelves",
                "search_content      — find existing candidates anywhere",
                "list_books / list_chapters / list_shelves — browse",
                "move_book_to_shelf / move_chapter / move_page — relocate",
                "remember_identity action=create — scaffold a new identity",
                "remember_config    action=write — persist the chosen IDs",
                "remember_config    action=dismiss_setup_nudge days=N — snooze this reminder"
            ],
            "settings_path": "/settings",
            "dismiss": {
                "tool": "remember_config",
                "action": "dismiss_setup_nudge",
                "default_days": 7,
                "example": "remember_config action=dismiss_setup_nudge days=14"
            }
        }))
    } else {
        None
    };

    Outcome::ok(json!({
        "setup_nudge": setup_nudge,
        "identity": match identity {
            Some(p) => json!({
                "ouid": resolved.ouid,
                "name": resolved.name.clone()
                    .or_else(|| p.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())),
                "using_org_default": resolved.using_default,
                "manifest": {
                    "page_id": resolved.page_id,
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
        "journal_recent": recent_journals,
        "journal_semantic_matches": semantic.journal_matches,
        "user_journal_recent": recent_user_journal,
        "user_journal_semantic_matches": semantic.user_journal_matches,
        "collage_active": active_collage,
        "collage_semantic_matches": semantic.collage_matches,
        "shared_collage_active": shared_collage,
        "shared_collage_semantic_matches": semantic.shared_collage_matches,
        "kb_semantic_matches": if ctx.settings.semantic_against_full_kb { json!(semantic.kb_matches) } else { Value::Null },
        "system_prompt_additions": system_prompt,
        "time": {
            "now_unix": frontmatter::now_unix(),
            "now_utc": frontmatter::now_iso_utc(),
            "timezone": ctx.settings.timezone.clone().unwrap_or_else(|| "UTC".to_string()),
            "timezone_source": if ctx.settings.timezone.is_some() { "user_settings" } else { "default_utc" },
        },
        "config": {
            "label": ctx.settings.label,
            "role": ctx.settings.role,
            "shelf_id": ctx.settings.ai_hive_shelf_id,
            "use_follow_up_remember_agent": ctx.settings.use_follow_up_remember_agent,
        },
    }))
}

/// Build the semantic-search query string by prefixing the user's prompt
/// with a `[Context: ...]` block carrying current time, timezone, user
/// identity, and AI identity. The whole string is used for both vector
/// embedding and the hybrid keyword pass.
fn build_semantic_query(user_prompt: &str, ctx: &Context) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("time={}", frontmatter::now_iso_utc()));
    if let Some(tz) = &ctx.settings.timezone {
        parts.push(format!("tz={tz}"));
    }
    if let Some(uid) = &ctx.settings.user_id {
        parts.push(format!("user={uid}"));
    }
    if let Some(name) = &ctx.settings.ai_identity_name {
        parts.push(format!("ai={name}"));
    }
    if parts.is_empty() {
        user_prompt.to_string()
    } else {
        format!("[Context: {}]\n{}", parts.join(", "), user_prompt)
    }
}

/// Fetch the markdown for every page in `page_ids` concurrently. Each result
/// is tagged with the given `source` so the AI knows where the content came
/// from (`user`, `org_instructions`, or `org_policy`).
///
/// **Invariant: no truncation.** System prompts and org policies are
/// load-bearing — every word matters. The body returned here is the full
/// page markdown with only the leading YAML frontmatter (provenance metadata)
/// stripped. Do not add length caps, summarization, or chunking. If the body
/// is too large for some downstream consumer, fix the consumer.
async fn fetch_pages_with_source(
    client: &BookStackClient,
    page_ids: &[i64],
    source: &'static str,
) -> Vec<Value> {
    if page_ids.is_empty() {
        return Vec::new();
    }
    let mut handles = Vec::with_capacity(page_ids.len());
    for &id in page_ids {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            (id, client.get_page(id).await)
        }));
    }
    let mut out = Vec::new();
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
                    "source": source,
                }));
            }
            Err(e) => {
                eprintln!("Briefing: {source} page {id} fetch failed: {e}");
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

/// Lists the most-recently-updated pages within a book, using the
/// `BookStackClient::list_book_pages_by_updated` helper. The helper sorts
/// by the page row's `updated_at` from BookStack's database — never from
/// markdown content. We just narrow the page row down to the four fields
/// the briefing surfaces.
async fn list_recent_pages(book_id: Option<i64>, limit: usize, client: &BookStackClient) -> Vec<Value> {
    let Some(book_id) = book_id else { return Vec::new(); };
    match client.list_book_pages_by_updated(book_id, limit).await {
        Ok(pages) => pages
            .into_iter()
            .map(|p| json!({
                "page_id": p.get("id").cloned().unwrap_or(Value::Null),
                "name": p.get("name").cloned().unwrap_or(Value::Null),
                "url": p.get("url").cloned().unwrap_or(Value::Null),
                "updated_at": p.get("updated_at").cloned().unwrap_or(Value::Null),
            }))
            .collect(),
        Err(e) => {
            eprintln!("Briefing: list_book_pages_by_updated({book_id}) failed: {e}");
            Vec::new()
        }
    }
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
