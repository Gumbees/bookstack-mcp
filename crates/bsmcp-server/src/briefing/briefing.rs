//! Briefing builder — produces the per-session reconstitution payload.
//!
//! Returns: time, system_prompt_additions (guide, org_identity,
//! org_instructions, org_policy, user-supplied pages, owned-domains
//! synthetic block), KB semantic matches against the user prompt,
//! setup_nudge (when settings incomplete), and a thin config echo.

use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;

use super::{frontmatter, Context};
use crate::semantic::trim_match;

/// `kb_semantic_matches` chunk-trim (tighter than the `semantic_search` MCP tool
/// because the briefing fires once per session at start).
const KB_CHUNK_LIMIT: usize = 4;
const KB_CHUNK_CHARS: usize = 150;
const KB_MATCH_LIMIT: usize = 6;

const SEMANTIC_MATCHES_HINT: &str =
    "kb_semantic_matches entries return up to 4 chunks of ~150 chars each. \
     Truncated chunks have `truncated: true` and end with …. \
     These are search-result previews, not full page content — call `get_page(page_id)` to read the full markdown when a match looks relevant.";

pub async fn read(ctx: &Context) -> Value {
    let user_prompt = ctx.body_str("user_prompt").unwrap_or_default();
    let globals = ctx.db.get_global_settings().await.unwrap_or_default();

    let bookstack_user_id = ctx.settings.bookstack_user_id;
    let token_id_hash = ctx.token_id_hash.clone();
    let semantic_fut = async {
        if user_prompt.is_empty() {
            return Vec::<Value>::new();
        }
        let Some(sem) = &ctx.semantic else {
            return Vec::new();
        };
        let user_roles = sem
            .resolve_user_roles(&token_id_hash, bookstack_user_id, &ctx.client)
            .await;
        let prompt_for_semantic = build_semantic_query(&user_prompt, ctx);
        let raw = match sem
            .search(
                &prompt_for_semantic,
                40,
                0.40,
                true,
                false,
                &ctx.client,
                None,
                user_roles.as_deref(),
            )
            .await
        {
            Ok(v) => v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default(),
            Err(e) => {
                eprintln!("Briefing: semantic search failed: {e}");
                return Vec::new();
            }
        };
        raw.into_iter()
            .take(KB_MATCH_LIMIT)
            .map(|h| trim_match(h, KB_CHUNK_LIMIT, KB_CHUNK_CHARS))
            .collect()
    };

    // Always-on context — five sources, all run in parallel.
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
    let org_identity_page_ids: Vec<i64> = globals
        .org_identity_page_id
        .map(|id| vec![id])
        .unwrap_or_default();
    let org_identity_fut = fetch_pages_with_source(
        &ctx.client,
        &org_identity_page_ids,
        "org_identity",
    );
    let guide_page_ids: Vec<i64> = globals
        .guide_page_id
        .map(|id| vec![id])
        .unwrap_or_default();
    let guide_fut = fetch_pages_with_source(&ctx.client, &guide_page_ids, "guide");

    let (kb_matches, user_pages, org_instructions, org_policy, org_identity, guide_pages) = tokio::join!(
        semantic_fut,
        user_pages_fut,
        org_instructions_fut,
        org_policy_fut,
        org_identity_fut,
        guide_fut,
    );

    let mut system_prompt: Vec<Value> = Vec::with_capacity(
        user_pages.len() + org_instructions.len() + org_policy.len() + org_identity.len() + guide_pages.len() + 1,
    );
    system_prompt.extend(guide_pages);
    system_prompt.extend(org_identity);
    system_prompt.extend(user_pages);
    system_prompt.extend(org_instructions);
    system_prompt.extend(org_policy);

    // Domains block — merged user + org domains.
    let merged_domains = merge_domains(&ctx.settings.domains, &globals.org_domains);
    if !merged_domains.is_empty() {
        system_prompt.push(json!({
            "page_id": 0,
            "name": "Owned domains",
            "markdown": format_domains_block(&merged_domains),
            "url": Value::Null,
            "source": "domains",
        }));
    }

    // Setup nudge — show until everything's configured AND not snoozed.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let snoozed = ctx
        .settings
        .settings_nudge_dismissed_until
        .map(|t| now_unix < t)
        .unwrap_or(false);
    let pending_user = pending_user_fields(&ctx.settings);
    let pending_global = pending_global_fields(&globals);
    let any_pending = !pending_user.is_empty() || !pending_global.is_empty();
    let setup_nudge = if any_pending && !snoozed {
        Some(json!({
            "show": true,
            "summary": format!(
                "Setup incomplete: {} user field(s), {} global field(s) still need values.",
                pending_user.len(), pending_global.len()
            ),
            "pending_user": pending_user,
            "pending_global": pending_global,
            "settings_path": "/settings",
        }))
    } else {
        None
    };

    json!({
        "setup_nudge": setup_nudge,
        "kb_semantic_matches": kb_matches,
        "semantic_matches_hint": SEMANTIC_MATCHES_HINT,
        "system_prompt_additions": system_prompt,
        "time": super::envelope::build_time_block(&ctx.settings, false),
        "config": {
            "label": ctx.settings.label,
            "role": ctx.settings.role,
            "friendly_structure": globals.friendly_structure,
            "full_content_in_briefing": globals.full_content_in_briefing,
        },
    })
}

fn build_semantic_query(user_prompt: &str, ctx: &Context) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("time={}", frontmatter::now_iso_utc()));
    if let Some(tz) = &ctx.settings.timezone {
        parts.push(format!("tz={tz}"));
    }
    if let Some(uid) = &ctx.settings.user_id {
        parts.push(format!("user={uid}"));
    }
    if parts.is_empty() {
        user_prompt.to_string()
    } else {
        format!("[Context: {}]\n{}", parts.join(", "), user_prompt)
    }
}

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

fn pending_user_fields(s: &bsmcp_common::settings::UserSettings) -> Vec<Value> {
    let mut out = Vec::new();
    if s.user_id.is_none() {
        out.push(json!({
            "field": "user_id",
            "why": "Stable identifier (typically email) — recorded in audit log entries.",
        }));
    }
    if s.bookstack_user_id.is_none() {
        out.push(json!({
            "field": "bookstack_user_id",
            "why": "BookStack user row ID — required for ACL-filtered semantic search and role-gated tool exposure.",
        }));
    }
    if s.domains.is_empty() {
        out.push(json!({
            "field": "domains",
            "why": "User's owned domains. Surfaced so the AI can distinguish ours vs external content.",
        }));
    }
    out
}

fn pending_global_fields(g: &bsmcp_common::settings::GlobalSettings) -> Vec<Value> {
    let mut out = Vec::new();
    if g.guide_page_id.is_none() {
        out.push(json!({
            "field": "guide_page_id",
            "why": "Org-configured guide page describing how to use this BookStack. AIs call `get_guide` to fetch it on demand. Admin-only.",
            "admin_only": true,
        }));
    }
    if g.org_identity_page_id.is_none() {
        out.push(json!({
            "field": "org_identity_page_id",
            "why": "Single page describing the organization. Pulled into every briefing's system_prompt_additions. Admin-only.",
            "admin_only": true,
        }));
    }
    if g.org_domains.is_empty() {
        out.push(json!({
            "field": "org_domains",
            "why": "Domains the organization owns. Admin-only.",
            "admin_only": true,
        }));
    }
    out
}

fn merge_domains(user_domains: &[String], org_domains: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(user_domains.len() + org_domains.len());
    for d in user_domains.iter().chain(org_domains.iter()) {
        let v = d.trim().to_lowercase();
        if v.is_empty() {
            continue;
        }
        if seen.insert(v.clone()) {
            out.push(v);
        }
    }
    out
}

fn format_domains_block(domains: &[String]) -> String {
    let mut s = String::new();
    s.push_str("## Owned domains\n\n");
    s.push_str(&domains.join(", "));
    s.push_str(
        "\n\n(Treat URLs and email addresses on these domains as ours; \
         everything else is external. Use this when deciding whether to \
         redact, share, or treat content as trusted.)\n",
    );
    s
}
