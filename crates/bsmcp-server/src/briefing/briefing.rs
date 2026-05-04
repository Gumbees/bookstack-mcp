//! Briefing builder — produces the per-session reconstitution payload.
//!
//! Returns: time, system_prompt_additions (guide, org_identity,
//! org_instructions, org_policy, user-supplied pages, owned-domains
//! synthetic block, kb_scopes pointers), KB semantic matches against the user
//! prompt, setup_nudge (when settings incomplete), setup_warnings (resolution
//! failures + v0.7.x migration leftovers), and a thin config echo.
//!
//! Behavior toggles read from `GlobalSettings`:
//! - `full_content_in_briefing` — when true, fetch full markdown for Page-typed
//!   `system_prompt_additions` entries (incl. resolved kb_scopes that point at
//!   a Page). When false (default), entries carry id/name/summary/url only.
//!   Shelf/Book scopes never include body content (potentially huge).
//! - `friendly_structure` — when false, drop prose summary/hint fields from
//!   the JSON shape. When true (default), keep human-readable headings/labels.
//! - `strict_setup` — when true and setup is incomplete, the response carries
//!   `setup_required: true` at the top level. The actual error-envelope gating
//!   on tool-call paths lives in `mcp.rs` (Agent E's scope). The
//!   `settings_fields_complete` heuristic here is intentionally minimal —
//!   it just asks whether `pending_user_fields` and `pending_global_fields`
//!   are both empty. Distinct from `UserSettings.setup_complete` (the
//!   onboarding-wizard flag, sub-PR 2.4e), which means "user submitted the
//!   /setup/user form" — a one-bit, write-once flag rather than a derived
//!   check across many fields.

use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::settings::{GlobalSettings, KbScope, UserSettings};

use super::{frontmatter, Context};
use crate::semantic::trim_match;

/// v0.7.x personal-memory keys removed in v0.8.0. Each entry maps the JSON
/// field to the BookStack entity kind it pointed at (or `None` for non-ID
/// fields like counters and booleans). If any appear in `UserSettings.extras`
/// (the serde-flatten capture), surface a one-shot migration warning with
/// BookStack metadata for the addressable entries, then clean them off disk.
const LEGACY_USER_SETTINGS_KEYS: &[(&str, Option<LegacyKind>)] = &[
    ("ai_hive_journal_book_id", Some(LegacyKind::Book)),
    ("ai_collage_book_id", Some(LegacyKind::Book)),
    ("ai_shared_collage_book_id", Some(LegacyKind::Book)),
    ("ai_identity_page_id", Some(LegacyKind::Page)),
    ("ai_identity_book_id", Some(LegacyKind::Book)),
    ("ai_identity_name", None),
    ("ai_identity_ouid", None),
    ("ai_hive_shelf_id", Some(LegacyKind::Shelf)),
    ("ai_identity_agents_chapter_id", Some(LegacyKind::Chapter)),
    ("ai_identity_journal_chapter_id", Some(LegacyKind::Chapter)),
    ("user_journal_book_id", Some(LegacyKind::Book)),
    ("user_identity_page_id", Some(LegacyKind::Page)),
    ("user_identity_book_id", Some(LegacyKind::Book)),
    ("user_journal_agent_page_id", Some(LegacyKind::Page)),
    ("recent_journal_count", None),
    ("recent_collage_count", None),
    ("active_collage_count", None),
    ("semantic_against_journal", None),
    ("semantic_against_collage", None),
    ("semantic_against_shared_collage", None),
    ("semantic_against_user_journal", None),
    ("use_follow_up_remember_agent", None),
];

#[derive(Clone, Copy)]
enum LegacyKind {
    Shelf,
    Book,
    Chapter,
    Page,
}

impl LegacyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shelf => "shelf",
            Self::Book => "book",
            Self::Chapter => "chapter",
            Self::Page => "page",
        }
    }
}

/// Hard cap on the "summary" snippet returned for pages when
/// `full_content_in_briefing` is false and BookStack didn't supply a description.
const SUMMARY_CHAR_LIMIT: usize = 500;

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
    let friendly = globals.friendly_structure;
    let full_content = globals.full_content_in_briefing;

    let mut setup_warnings: Vec<Value> = Vec::new();

    // v0.7.x migration warning + one-shot cleanup. We only care about extras
    // matching the known legacy key list — anything else stays in extras as a
    // pass-through (caller's problem, not a v0.7.x leftover).
    let stale_pairs: Vec<(String, Value, Option<LegacyKind>)> = ctx
        .settings
        .extras
        .iter()
        .filter_map(|(k, v)| {
            LEGACY_USER_SETTINGS_KEYS
                .iter()
                .find(|(name, _)| *name == k.as_str())
                .map(|(_, kind)| (k.clone(), v.clone(), *kind))
        })
        .collect();
    if !stale_pairs.is_empty() {
        let stale_keys: Vec<String> =
            stale_pairs.iter().map(|(k, _, _)| k.clone()).collect();
        let stale_values: serde_json::Map<String, Value> = stale_pairs
            .iter()
            .map(|(k, v, _)| (k.clone(), v.clone()))
            .collect();
        let stale_entities = resolve_legacy_entities(&stale_pairs, &ctx.client).await;
        setup_warnings.push(json!({
            "kind": "v0_8_0_migration",
            "message": "Detected v0.7.x personal-memory pointers in your settings. \
                        They no longer apply (memory moved to memberberry.ai). \
                        The associated BookStack pages are still in your instance \
                        but no longer auto-loaded by the briefing — see \
                        `stale_entities` for names and URLs so you can find them \
                        if you want to migrate them yourself.",
            "stale_keys": stale_keys,
            "stale_values": Value::Object(stale_values),
            "stale_entities": stale_entities,
        }));
        // Clean off disk. UserSettings now serializes `extras` through saves
        // (so unrelated save paths don't silently nuke legacy data before the
        // user is notified), so we explicitly clear it here on the same call
        // that surfaced the warning. Subsequent reads find an empty extras
        // map and emit no warning.
        let mut cleaned = ctx.settings.clone();
        cleaned.extras.clear();
        if let Err(e) = ctx
            .db
            .save_user_settings(&ctx.token_id_hash, &cleaned)
            .await
        {
            eprintln!(
                "Briefing: failed to clean v0.7.x extras (trace_id={}): {e}",
                ctx.trace_id
            );
        }
    }

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
        full_content,
        friendly,
    );
    let org_instructions_fut = fetch_pages_with_source(
        &ctx.client,
        &globals.org_required_instructions_page_ids,
        "org_instructions",
        full_content,
        friendly,
    );
    let org_policy_fut = fetch_pages_with_source(
        &ctx.client,
        &globals.org_ai_usage_policy_page_ids,
        "org_policy",
        full_content,
        friendly,
    );
    // org_identity is gated by the per-user `use_org_identity` opt-out.
    // Default-on: admin-configured org identities apply. Users who don't
    // want this instance's canonical identity bound to their session flip
    // it false and the org_identity entry is omitted entirely.
    let org_identity_page_ids: Vec<i64> = if ctx.settings.use_org_identity {
        globals.org_identity_page_id.map(|id| vec![id]).unwrap_or_default()
    } else {
        Vec::new()
    };
    let org_identity_fut = fetch_pages_with_source(
        &ctx.client,
        &org_identity_page_ids,
        "org_identity",
        full_content,
        friendly,
    );
    let guide_page_ids: Vec<i64> = globals
        .guide_page_id
        .map(|id| vec![id])
        .unwrap_or_default();
    let guide_fut = fetch_pages_with_source(
        &ctx.client,
        &guide_page_ids,
        "guide",
        full_content,
        friendly,
    );

    // Typed scope slots — one entry per configured slot. Each entry resolves
    // its name + url; failures bubble up as setup_warnings instead of hard
    // erroring. For Page scopes we honor full_content_in_briefing; for
    // Shelf/Book we never include body content (could be enormous).
    let scopes_fut = resolve_kb_scopes(&ctx.client, &globals, full_content, friendly);

    let (kb_matches, user_pages, org_instructions, org_policy, org_identity, guide_pages, scope_results) = tokio::join!(
        semantic_fut,
        user_pages_fut,
        org_instructions_fut,
        org_policy_fut,
        org_identity_fut,
        guide_fut,
        scopes_fut,
    );

    let (scope_entries, scope_warnings) = scope_results;
    setup_warnings.extend(scope_warnings);

    let mut system_prompt: Vec<Value> = Vec::with_capacity(
        user_pages.len()
            + org_instructions.len()
            + org_policy.len()
            + org_identity.len()
            + guide_pages.len()
            + scope_entries.len()
            + 1,
    );
    system_prompt.extend(guide_pages);
    system_prompt.extend(org_identity);
    system_prompt.extend(scope_entries);
    system_prompt.extend(user_pages);
    system_prompt.extend(org_instructions);
    system_prompt.extend(org_policy);

    // Domains block — merged user + org domains. In terse (`friendly=false`)
    // mode we emit just the domain list; the prose wrapper is friendly-only.
    let merged_domains = merge_domains(&ctx.settings.domains, &globals.org_domains);
    if !merged_domains.is_empty() {
        let mut entry = serde_json::Map::new();
        entry.insert("page_id".to_string(), json!(0));
        entry.insert("source".to_string(), json!("domains"));
        entry.insert("url".to_string(), Value::Null);
        entry.insert("domains".to_string(), json!(merged_domains));
        if friendly {
            entry.insert("name".to_string(), json!("Owned domains"));
            entry.insert(
                "markdown".to_string(),
                json!(format_domains_block(&merged_domains)),
            );
        }
        system_prompt.push(Value::Object(entry));
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

    // strict_setup gating. We only flip `setup_required` when the strict
    // boolean is on AND setup is incomplete. The actual error-envelope
    // gating on tool-call paths is Agent E's wiring in mcp.rs; this field
    // is the signal Agent E will read.
    let fields_complete = settings_fields_complete(&ctx.settings, &globals);
    let setup_required = globals.strict_setup && !fields_complete;

    let mut payload = serde_json::Map::new();
    payload.insert("setup_required".to_string(), json!(setup_required));
    payload.insert("setup_nudge".to_string(), json!(setup_nudge));
    payload.insert("setup_warnings".to_string(), json!(setup_warnings));
    payload.insert("kb_semantic_matches".to_string(), json!(kb_matches));
    if friendly {
        payload.insert(
            "semantic_matches_hint".to_string(),
            json!(SEMANTIC_MATCHES_HINT),
        );
    }
    payload.insert("system_prompt_additions".to_string(), json!(system_prompt));
    payload.insert(
        "time".to_string(),
        super::envelope::build_time_block(&ctx.settings, false),
    );
    payload.insert(
        "config".to_string(),
        json!({
            "label": ctx.settings.label,
            "role": ctx.settings.role,
            "friendly_structure": globals.friendly_structure,
            "full_content_in_briefing": globals.full_content_in_briefing,
            "strict_setup": globals.strict_setup,
        }),
    );

    // Journaling reminder. Off entirely when the user hasn't opted in.
    // When opted in, prefer the per-call `agent_name` (the AI's harness
    // pushes its identity on each briefing call when it knows it), then
    // the user's saved `chosen_ai_identity`. With neither, fall back to
    // a generic phrasing.
    let body_agent_name = ctx
        .body
        .get("agent_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(reminder) = select_journaling_reminder(
        ctx.settings.journaling_enabled,
        body_agent_name.as_deref(),
        ctx.settings.chosen_ai_identity.as_deref(),
    ) {
        payload.insert("journaling_reminder".to_string(), json!(reminder));
    }

    Value::Object(payload)
}

/// Select the journaling reminder string for this briefing call. Returns
/// `None` when journaling is disabled (no field appended). Returns the
/// generic "remember to journal throughout the session" when on but no
/// agent name is available; appends ` {name}` when one is.
///
/// `body_agent_name` (per-call) takes precedence over `chosen_ai_identity`
/// (user-saved). Both are trimmed; empty strings count as absent.
pub fn select_journaling_reminder(
    enabled: bool,
    body_agent_name: Option<&str>,
    chosen_ai_identity: Option<&str>,
) -> Option<String> {
    if !enabled {
        return None;
    }
    let agent = body_agent_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| chosen_ai_identity.map(str::trim).filter(|s| !s.is_empty()));
    Some(match agent {
        Some(name) => format!("remember to journal throughout the session {name}"),
        None => "remember to journal throughout the session".to_string(),
    })
}

/// "Settings fields are filled in" = no pending user or global fields.
/// Mirrors the same definition used by `pending_user_fields` /
/// `pending_global_fields` so the `setup_required` flag (gated by
/// `globals.strict_setup`) and the `setup_nudge` block agree on what
/// "done" means.
///
/// This is a *derived* check across the typed setup slots — distinct from
/// `UserSettings.setup_complete`, which is a stored one-bit flag set by
/// the `/setup/user` onboarding wizard (sub-PR 2.4e). A user can have
/// `setup_complete = true` while still having pending settings fields
/// (they ran the wizard but skipped optional inputs), and vice versa
/// (an admin pre-filled their settings via API but they never opened
/// the wizard).
fn settings_fields_complete(s: &UserSettings, g: &GlobalSettings) -> bool {
    pending_user_fields(s).is_empty() && pending_global_fields(g).is_empty()
}

/// Resolve every numeric-ID legacy key against BookStack so the migration
/// warning carries actionable names + URLs. Failures (deleted pages, ACL
/// blocks) become entries with `resolved: false`. Non-ID keys are skipped
/// entirely — they have nothing to look up.
async fn resolve_legacy_entities(
    pairs: &[(String, Value, Option<LegacyKind>)],
    client: &BookStackClient,
) -> Vec<Value> {
    let mut out = Vec::new();
    for (key, value, kind) in pairs {
        let Some(kind) = kind else { continue };
        let Some(id) = value.as_i64() else { continue };
        let fetched = match kind {
            LegacyKind::Shelf => client.get_shelf(id).await,
            LegacyKind::Book => client.get_book(id).await,
            LegacyKind::Chapter => client.get_chapter(id).await,
            LegacyKind::Page => client.get_page(id).await,
        };
        match fetched {
            Ok(entity) => {
                let name = entity
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unnamed)")
                    .to_string();
                let url = entity
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                out.push(json!({
                    "key": key,
                    "kind": kind.as_str(),
                    "id": id,
                    "name": name,
                    "url": url,
                    "resolved": true,
                }));
            }
            Err(e) => {
                out.push(json!({
                    "key": key,
                    "kind": kind.as_str(),
                    "id": id,
                    "resolved": false,
                    "error": e,
                }));
            }
        }
    }
    out
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
    full_content: bool,
    friendly: bool,
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
                out.push(build_page_entry(id, &page, source, full_content, friendly));
            }
            Err(e) => {
                eprintln!("Briefing: {source} page {id} fetch failed: {e}");
            }
        }
    }
    out
}

/// Build a single page entry for `system_prompt_additions`. Honors
/// `full_content_in_briefing` (markdown body vs summary) and
/// `friendly_structure` (suppress prose-y fields when off).
fn build_page_entry(
    id: i64,
    page: &Value,
    source: &'static str,
    full_content: bool,
    friendly: bool,
) -> Value {
    let raw = page.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
    let stripped = frontmatter::strip(raw);
    let mut entry = serde_json::Map::new();
    entry.insert("page_id".to_string(), json!(id));
    entry.insert(
        "name".to_string(),
        page.get("name").cloned().unwrap_or(Value::Null),
    );
    entry.insert(
        "url".to_string(),
        page.get("url").cloned().unwrap_or(Value::Null),
    );
    entry.insert("source".to_string(), json!(source));
    if full_content {
        entry.insert("markdown".to_string(), json!(stripped));
    } else {
        let description = page
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let summary = description.unwrap_or_else(|| truncate_summary(stripped));
        if friendly || !summary.is_empty() {
            entry.insert("summary".to_string(), json!(summary));
        }
    }
    Value::Object(entry)
}

fn truncate_summary(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= SUMMARY_CHAR_LIMIT {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(SUMMARY_CHAR_LIMIT).collect();
    out.push('…');
    out
}

/// Resolve the three typed scope slots on `GlobalSettings` into
/// `system_prompt_additions` entries. Returns `(entries, warnings)`. Warnings
/// surface configured IDs that no longer resolve in BookStack.
async fn resolve_kb_scopes(
    client: &BookStackClient,
    globals: &GlobalSettings,
    full_content: bool,
    friendly: bool,
) -> (Vec<Value>, Vec<Value>) {
    let mut slots: Vec<(&'static str, &'static str, &'static str, &KbScope)> = Vec::new();
    if let Some(s) = &globals.policies_scope {
        slots.push((
            "policy_scope",
            "policies_scope",
            "Look here when asked about org policy, compliance, or required behavior.",
            s,
        ));
    }
    if let Some(s) = &globals.sops_scope {
        slots.push((
            "sop_scope",
            "sops_scope",
            "Look here when asked how to perform a routine operational task.",
            s,
        ));
    }
    if let Some(s) = &globals.best_practices_scope {
        slots.push((
            "best_practice_scope",
            "best_practices_scope",
            "Look here when asked for recommended approaches or design guidance.",
            s,
        ));
    }

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (kind, slot_name, hint, scope) in slots {
        match resolve_one_scope(client, scope, full_content, friendly).await {
            Ok(mut entry) => {
                entry.insert("kind".to_string(), json!(kind));
                entry.insert("source".to_string(), json!("kb_scope"));
                if friendly {
                    entry.insert("hint".to_string(), json!(hint));
                }
                entries.push(Value::Object(entry));
            }
            Err(e) => {
                warnings.push(json!({
                    "kind": "scope_unresolved",
                    "slot": slot_name,
                    "scope_type": scope_type_str(scope),
                    "id": scope.id(),
                    "message": format!("Configured {slot_name} could not be resolved: {e}"),
                }));
            }
        }
    }
    (entries, warnings)
}

fn scope_type_str(s: &KbScope) -> &'static str {
    match s {
        KbScope::Shelf(_) => "shelf",
        KbScope::Book(_) => "book",
        KbScope::Page(_) => "page",
    }
}

/// Resolve one `KbScope` against BookStack. Page scopes optionally include
/// body markdown (if `full_content` is true). Shelf/Book scopes never include
/// body content — they list referenced books/chapters via the BookStack API
/// response's `contents` array (when present) so the AI knows what's inside.
async fn resolve_one_scope(
    client: &BookStackClient,
    scope: &KbScope,
    full_content: bool,
    friendly: bool,
) -> Result<serde_json::Map<String, Value>, String> {
    let mut entry = serde_json::Map::new();
    entry.insert("scope_type".to_string(), json!(scope_type_str(scope)));
    entry.insert("id".to_string(), json!(scope.id()));
    match scope {
        KbScope::Shelf(id) => {
            let s = client.get_shelf(*id).await?;
            entry.insert(
                "name".to_string(),
                s.get("name").cloned().unwrap_or(Value::Null),
            );
            entry.insert(
                "url".to_string(),
                s.get("url").cloned().unwrap_or(Value::Null),
            );
            if friendly {
                if let Some(desc) = s.get("description").and_then(|v| v.as_str()) {
                    let trimmed = desc.trim();
                    if !trimmed.is_empty() {
                        entry.insert("summary".to_string(), json!(trimmed));
                    }
                }
            }
            if let Some(books) = s.get("books").and_then(|v| v.as_array()) {
                let listing: Vec<Value> = books
                    .iter()
                    .map(|b| {
                        json!({
                            "id": b.get("id").cloned().unwrap_or(Value::Null),
                            "name": b.get("name").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                entry.insert("books".to_string(), json!(listing));
            }
        }
        KbScope::Book(id) => {
            let b = client.get_book(*id).await?;
            entry.insert(
                "name".to_string(),
                b.get("name").cloned().unwrap_or(Value::Null),
            );
            entry.insert(
                "url".to_string(),
                b.get("url").cloned().unwrap_or(Value::Null),
            );
            if friendly {
                if let Some(desc) = b.get("description").and_then(|v| v.as_str()) {
                    let trimmed = desc.trim();
                    if !trimmed.is_empty() {
                        entry.insert("summary".to_string(), json!(trimmed));
                    }
                }
            }
            if let Some(contents) = b.get("contents").and_then(|v| v.as_array()) {
                let listing: Vec<Value> = contents
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.get("id").cloned().unwrap_or(Value::Null),
                            "type": c.get("type").cloned().unwrap_or(Value::Null),
                            "name": c.get("name").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                entry.insert("contents".to_string(), json!(listing));
            }
        }
        KbScope::Page(id) => {
            let p = client.get_page(*id).await?;
            entry.insert(
                "name".to_string(),
                p.get("name").cloned().unwrap_or(Value::Null),
            );
            entry.insert(
                "url".to_string(),
                p.get("url").cloned().unwrap_or(Value::Null),
            );
            entry.insert("page_id".to_string(), json!(id));
            let raw = p.get("markdown").and_then(|v| v.as_str()).unwrap_or("");
            let stripped = frontmatter::strip(raw);
            if full_content {
                entry.insert("markdown".to_string(), json!(stripped));
            } else {
                let description = p
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let summary = description.unwrap_or_else(|| truncate_summary(stripped));
                if friendly || !summary.is_empty() {
                    entry.insert("summary".to_string(), json!(summary));
                }
            }
        }
    }
    Ok(entry)
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
            "why": "Org-configured guide page describing how to use this BookStack. Auto-included in every briefing's system_prompt_additions when set. Admin-only.",
            "admin_only": true,
        }));
    }
    // org_identity_page_id is intentionally NOT pending. "No org identity"
    // is a first-class admin choice, not an unfinished setup step — some
    // orgs don't have a canonical identity manifest, and per-user
    // `use_org_identity = false` already lets users opt out when one is
    // set. Keeping it on the pending list would nag admins into picking a
    // page that doesn't apply.
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- journaling reminder selection ---

    #[test]
    fn journaling_reminder_disabled_returns_none() {
        assert!(select_journaling_reminder(false, None, None).is_none());
        assert!(select_journaling_reminder(false, Some("claude"), None).is_none());
        assert!(select_journaling_reminder(false, None, Some("claude")).is_none());
    }

    #[test]
    fn journaling_reminder_with_per_call_agent_name_uses_it() {
        let r = select_journaling_reminder(true, Some("claude"), None).unwrap();
        assert_eq!(r, "remember to journal throughout the session claude");
    }

    #[test]
    fn journaling_reminder_per_call_overrides_chosen_identity() {
        // Per-call name wins — the AI knows its identity for THIS call.
        let r = select_journaling_reminder(true, Some("opus"), Some("claude")).unwrap();
        assert_eq!(r, "remember to journal throughout the session opus");
    }

    #[test]
    fn journaling_reminder_falls_back_to_chosen_identity() {
        let r = select_journaling_reminder(true, None, Some("claude")).unwrap();
        assert_eq!(r, "remember to journal throughout the session claude");
    }

    #[test]
    fn journaling_reminder_generic_when_neither_name_available() {
        let r = select_journaling_reminder(true, None, None).unwrap();
        assert_eq!(r, "remember to journal throughout the session");
    }

    #[test]
    fn journaling_reminder_treats_empty_strings_as_absent() {
        // Whitespace-only agent name → fall back as if missing.
        let r = select_journaling_reminder(true, Some("   "), Some("claude")).unwrap();
        assert_eq!(r, "remember to journal throughout the session claude");

        let r = select_journaling_reminder(true, Some(""), None).unwrap();
        assert_eq!(r, "remember to journal throughout the session");

        let r = select_journaling_reminder(true, None, Some("")).unwrap();
        assert_eq!(r, "remember to journal throughout the session");
    }

    #[test]
    fn journaling_reminder_trims_agent_names() {
        let r = select_journaling_reminder(true, Some("  claude  "), None).unwrap();
        assert_eq!(r, "remember to journal throughout the session claude");
    }
}
