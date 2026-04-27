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
    //
    // Note: `client_timezone` refresh is handled centrally by `dispatch`
    // before this handler runs — `ctx.settings` already reflects any newly-
    // pushed timezone. No per-handler logic needed here.
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

    // Semantic search fan-out.
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
    //
    // Scope:
    //   - When `semantic_against_full_kb` is true, run one unfiltered query
    //     across the entire embedded corpus and partition results by book.
    //     `kb_semantic_matches` then surfaces the top hits NOT in any
    //     configured book (so it complements the per-book sections instead
    //     of duplicating them).
    //   - When false (default), restrict the vector pass to the union of the
    //     user's configured books whose individual toggles are on. The
    //     candidate pool is naturally smaller, which proportionally shrinks
    //     the permission filter and per-result fan-out. `kb_semantic_matches`
    //     is returned as `{enabled: false, reason: ...}` so consumers know
    //     the user opted out rather than getting a misleading empty slot.
    let prompt_for_semantic = build_semantic_query(&user_prompt, ctx);
    let configured_book_ids = configured_semantic_book_ids(&ctx.settings);
    let full_kb = ctx.settings.semantic_against_full_kb;
    let configured_books_for_kb_exclusion = configured_book_ids.clone();
    let bookstack_user_id = ctx.settings.bookstack_user_id;
    let token_id_hash = ctx.token_id_hash.clone();
    let semantic_fut = async {
        if user_prompt.is_empty() {
            return SemanticSlice::default();
        }
        let Some(sem) = &ctx.semantic else {
            return SemanticSlice::default();
        };
        let mut slice = SemanticSlice::default();

        // Pass `None` when full_kb is on (search everything), or `Some(&ids)`
        // to scope the vector pass to the configured books. An empty Some(&[])
        // would mean "no books to search" — handled by sem.search treating
        // empty as full corpus, but we never get here without ids when full_kb
        // is off because of the early-return guard below.
        let book_filter: Option<&[i64]> = if full_kb {
            None
        } else {
            if configured_book_ids.is_empty() {
                // No books configured AND user disabled full_kb — nothing to
                // search. Return empty slice without burning an embedder call.
                return slice;
            }
            Some(configured_book_ids.as_slice())
        };

        // Resolve the user's BookStack roles for ACL filtering. None when
        // `bookstack_user_id` isn't configured; sem.search then falls through
        // to the existing HTTP per-page permission check.
        let user_roles = sem
            .resolve_user_roles(&token_id_hash, bookstack_user_id, &ctx.client)
            .await;

        let raw = match sem
            .search(
                &prompt_for_semantic,
                40,
                0.40,
                true,
                false,
                &ctx.client,
                book_filter,
                user_roles.as_deref(),
            )
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
        if full_kb {
            // KB matches = top hits NOT in any configured book, so they
            // genuinely complement the per-book sections instead of just
            // duplicating their first few entries.
            let exclude: std::collections::HashSet<i64> =
                configured_books_for_kb_exclusion.iter().copied().collect();
            slice.kb_matches = raw.iter()
                .filter(|h| {
                    let bid = h.get("book_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    !exclude.contains(&bid)
                })
                .take(10)
                .cloned()
                .collect();
        }
        slice
    };

    // Always-on context pages — four sources, all run in parallel:
    //   - user-configured (system_prompt_page_ids)
    //   - org-required instructions (admin-mandated page IDs)
    //   - org-required AI usage policy (admin-mandated page IDs)
    //   - org identity page (single admin-mandated page describing the org)
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

    let (identity, user_page, recent_journals, recent_user_journal, active_collage, shared_collage, semantic, user_pages, org_instructions, org_policy, org_identity) = tokio::join!(
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
        org_identity_fut,
    );

    // Merge sources into one flat array. Each entry carries its `source`
    // field so callers can group/filter as needed. Synthetic entries
    // (domains list, identity refresh nudge) get a stable virtual page_id
    // sentinel of 0 so consumers can branch on `page_id == 0` to skip
    // anything that isn't a real BookStack page.
    let mut system_prompt: Vec<Value> = Vec::with_capacity(
        user_pages.len() + org_instructions.len() + org_policy.len() + org_identity.len() + 2,
    );
    system_prompt.extend(org_identity);
    system_prompt.extend(user_pages);
    system_prompt.extend(org_instructions);
    system_prompt.extend(org_policy);

    // Domains block — merged user + org domains. Surfaced as a synthetic
    // system_prompt_additions entry so the AI's "owned vs external" check
    // is always in context, not buried in a config dump.
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

    // Identity refresh nudge — fires when the user's identity page hasn't
    // been updated in 30+ days. Skipped silently if no identity page is
    // configured, or the updated_at can't be parsed.
    if let Some(stale) = identity_refresh_block(&user_page, ctx.settings.user_identity_page_id) {
        system_prompt.push(stale);
    }

    // Setup nudge — show until everything's configured AND not snoozed. The
    // nudge now lists exactly which user + global fields are still missing
    // so the AI / user can address them one at a time instead of "your
    // settings aren't done."
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let snoozed = ctx.settings.settings_nudge_dismissed_until.map(|t| now_unix < t).unwrap_or(false);
    let pending_user = pending_user_fields(&ctx.settings);
    let pending_global = pending_global_fields(&globals);
    let any_pending = !pending_user.is_empty() || !pending_global.is_empty();
    let setup_nudge = if any_pending && !snoozed {
        Some(json!({
            "show": true,
            "summary": format!(
                "Setup incomplete: {} user field(s), {} global field(s) still need values. Briefing falls back where possible but some sections will be empty until configured.",
                pending_user.len(), pending_global.len()
            ),
            "pending_user": pending_user,
            "pending_global": pending_global,
            "two_paths": {
                "ui": "Visit the MCP server's /settings page in a browser — fill in dropdowns or use 'Probe existing Hive' to auto-detect.",
                "mcp_guided": "Have the AI walk you through it via tool calls (recommended for chat-driven setups). See `suggested_workflow` below."
            },
            "suggested_workflow": [
                "1. Per-user settings: ask the user what they want — fresh identity, or adopt an existing agent already in this BookStack.",
                "2. For existing structure: `remember_directory action=read kind=identities` lists Hive-shelf identities; `kind=user_journals` lists journals. settings_not_configured here means the global shelves aren't set (admin task).",
                "3. Discover candidates anywhere: `search_content` with queries like '{type:book} Identity', '{type:book} Journal', '{type:book} Topics'.",
                "4. Adopt or relocate: write IDs directly with `remember_config action=write`, OR move the books with `move_book_to_shelf` / `move_chapter` / `move_page` first.",
                "5. Brand-new structure: `remember_identity action=create name=...` scaffolds Identity book + manifest + chapters in one call.",
                "6. Domains + identity: fill in the user's `domains` array (their owned domains) — the AI uses it to decide what's ours vs external.",
                "7. Admin-only globals: org_identity_page_id and org_domains describe the org for every user on the instance. Admins set them once via /settings.",
                "8. After each save the next briefing reflects the new config; the nudge stops once nothing's pending."
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
        "kb_semantic_matches": kb_matches_envelope(ctx, &semantic.kb_matches),
        "system_prompt_additions": system_prompt,
        // `time` is now in `meta.time` on every remember response (not just
        // briefing). Kept here too — readers were already targeting
        // `data.time` and we don't want to break them in a patch release.
        "time": super::envelope::build_time_block(&ctx.settings, false),
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

/// Collect the book IDs that the user has configured AND has the matching
/// semantic toggle on for. Used as the vector-search scope when
/// `semantic_against_full_kb` is off — we only embed against books the user
/// actually wants surfaced. Order is intentional but irrelevant; duplicates
/// are deduped by sort+dedup.
fn configured_semantic_book_ids(s: &bsmcp_common::settings::UserSettings) -> Vec<i64> {
    let mut ids: Vec<i64> = Vec::with_capacity(4);
    if s.semantic_against_journal {
        if let Some(id) = s.ai_hive_journal_book_id { ids.push(id); }
    }
    if s.semantic_against_collage {
        if let Some(id) = s.ai_collage_book_id { ids.push(id); }
    }
    if s.semantic_against_shared_collage {
        if let Some(id) = s.ai_shared_collage_book_id { ids.push(id); }
    }
    if s.semantic_against_user_journal {
        if let Some(id) = s.user_journal_book_id { ids.push(id); }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Build the `kb_semantic_matches` response envelope. Always returns an
/// object so consumers can branch on `enabled` rather than checking for a
/// null payload — a null was ambiguous between "user opted out", "no
/// semantic backend", and "search ran but returned no out-of-scope hits".
fn kb_matches_envelope(ctx: &Context, results: &[Value]) -> Value {
    if !ctx.settings.semantic_against_full_kb {
        return json!({
            "enabled": false,
            "reason": "user_disabled",
            "detail": "User setting `semantic_against_full_kb` is false. \
                Vector search was scoped to the configured journal/collage/user_journal \
                books only. Enable in /settings to search across the entire knowledge base.",
            "results": [],
        });
    }
    if ctx.semantic.is_none() {
        return json!({
            "enabled": false,
            "reason": "semantic_backend_unavailable",
            "detail": "Semantic search is not configured on this server.",
            "results": [],
        });
    }
    json!({
        "enabled": true,
        "reason": null,
        "detail": "Top hits from the entire knowledge base, excluding pages already surfaced in per-book sections.",
        "results": results,
    })
}


/// Per-user fields the setup nudge wants populated. Each entry is a
/// `{field, why}` pair — `field` matches the UserSettings JSON key so the AI
/// can write directly via `remember_config action=write settings={...}`.
fn pending_user_fields(s: &bsmcp_common::settings::UserSettings) -> Vec<Value> {
    let mut out = Vec::new();
    if s.user_id.is_none() {
        out.push(json!({
            "field": "user_id",
            "why": "Stable identifier (typically email) — drives per-user resource naming and journal frontmatter.",
        }));
    }
    if s.ai_identity_page_id.is_none() {
        out.push(json!({
            "field": "ai_identity_page_id",
            "why": "AI agent's manifest page. The briefing falls back to org default if set, otherwise identity is empty.",
        }));
    }
    if s.ai_hive_journal_book_id.is_none() {
        out.push(json!({
            "field": "ai_hive_journal_book_id",
            "why": "AI's journal book. `remember_journal action=write` won't work without it.",
        }));
    }
    if s.user_journal_book_id.is_none() {
        out.push(json!({
            "field": "user_journal_book_id",
            "why": "User's personal journal. Auto-provisioned on first `remember_user action=read` once `user_id` is set.",
        }));
    }
    if s.user_identity_page_id.is_none() {
        out.push(json!({
            "field": "user_identity_page_id",
            "why": "User's identity manifest. Auto-provisioned on first `remember_user action=read` once `user_id` is set.",
        }));
    }
    if s.domains.is_empty() {
        out.push(json!({
            "field": "domains",
            "why": "User's owned domains (array of strings). Surfaced in system_prompt_additions so the AI can distinguish ours vs external content.",
        }));
    }
    if s.bookstack_user_id.is_none() {
        out.push(json!({
            "field": "bookstack_user_id",
            "why": "BookStack user row ID — required for ACL-based semantic search filtering. Without it the search falls back to per-page HTTP permission checks (slower).",
        }));
    }
    out
}

/// Global fields the setup nudge surfaces. Visible to all users in the
/// briefing response so anyone can flag missing globals to the admin, but
/// only an admin can actually persist them.
fn pending_global_fields(g: &bsmcp_common::settings::GlobalSettings) -> Vec<Value> {
    let mut out = Vec::new();
    if g.hive_shelf_id.is_none() {
        out.push(json!({
            "field": "hive_shelf_id",
            "why": "Shared shelf containing every AI agent's Identity book. Admin-only, first-write-wins.",
            "admin_only": true,
        }));
    }
    if g.user_journals_shelf_id.is_none() {
        out.push(json!({
            "field": "user_journals_shelf_id",
            "why": "Shared shelf containing each human user's journal book + identity book. Admin-only, first-write-wins.",
            "admin_only": true,
        }));
    }
    if g.org_identity_page_id.is_none() {
        out.push(json!({
            "field": "org_identity_page_id",
            "why": "Single page describing the organization (mission, structure, conventions). Pulled into every briefing's system_prompt_additions. Admin-only.",
            "admin_only": true,
        }));
    }
    if g.org_domains.is_empty() {
        out.push(json!({
            "field": "org_domains",
            "why": "Domains the organization owns. Pairs with org_identity to give every agent a shared baseline of 'where am I'. Admin-only.",
            "admin_only": true,
        }));
    }
    out
}

/// Merge user-owned and org-owned domains into a deduplicated list. Order:
/// user domains first (more specific to the calling user), then org-wide
/// domains the user hasn't already listed.
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

/// Render the domains list as a markdown block destined for system_prompt_additions.
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

/// Build a "refresh due" reminder block when the user's identity page is
/// older than 30 days. Returns `None` when the page is recent, missing, or
/// the timestamp can't be parsed.
fn identity_refresh_block(user_page: &Option<Value>, page_id: Option<i64>) -> Option<Value> {
    let pid = page_id?;
    let page = user_page.as_ref()?;
    let updated = page.get("updated_at").and_then(|v| v.as_str())?;
    let days = days_since_iso_date(updated)?;
    if days < 30 {
        return None;
    }
    let body = format!(
        "## Identity refresh due\n\n\
         The user's identity page (page {pid}) hasn't been updated in {days} days. \
         If you've learned anything about how this user works, what they care \
         about, or how to collaborate with them better, append or replace the \
         relevant section before the session ends.\n\n\
         Update via `remember_user action=write` with the full new body."
    );
    Some(json!({
        "page_id": 0,
        "name": "Identity refresh due",
        "markdown": body,
        "url": Value::Null,
        "source": "identity_refresh_due",
    }))
}

/// Parse the YYYY-MM-DD prefix of an ISO 8601 timestamp and return the
/// number of whole days between that date and today (UTC). Avoids pulling
/// in chrono for what's effectively a 30-day staleness check.
fn days_since_iso_date(iso: &str) -> Option<i64> {
    let date_prefix = iso.get(0..10)?;
    let mut parts = date_prefix.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    let then = ymd_to_unix_days(y, m, d)?;

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let now_days = now_secs / 86_400;
    Some(now_days - then)
}

/// Convert a (year, month, day) triple to days-since-Unix-epoch using the
/// civil-from-days algorithm (Hinnant 2014). Pure arithmetic; no calendar
/// libraries required.
fn ymd_to_unix_days(y: i64, m: u32, d: u32) -> Option<i64> {
    if m < 1 || m > 12 || d < 1 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

// Suppress unused-import warning when this module is built with other features.
#[allow(dead_code)]
fn _used(_: ErrorCode) {}
