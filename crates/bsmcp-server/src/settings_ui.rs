//! Browser-based settings UI.
//!
//! v0.8.0: minimal text-input form covering the surviving fields. Typeahead
//! pickers backed by `precision_search` ship in a follow-up commit on this
//! branch (depends on #22).
//!
//! Auth flow:
//! 1. User visits GET /settings → no cookie → redirect to /authorize?return_to=/settings
//! 2. /authorize validates the BookStack token and (if return_to is /settings)
//!    creates a settings session, sets a cookie, redirects to /settings.
//! 3. /settings reads the cookie, looks up credentials, renders the form.
//! 4. POST /settings parses the form, persists, redirects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{RawForm, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use tokio::sync::RwLock;
use zeroize::Zeroize;

use bsmcp_common::settings::{hash_token_id, GlobalSettings, KbScope, UserSettings};

use crate::mcp;
use crate::sse::AppState;

pub const SETTINGS_COOKIE_NAME: &str = "bsmcp_settings_session";
pub const SETTINGS_SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

pub struct SettingsSession {
    pub token_id: String,
    pub token_secret: String,
    pub created_at: Instant,
}

impl Drop for SettingsSession {
    fn drop(&mut self) {
        self.token_id.zeroize();
        self.token_secret.zeroize();
    }
}

pub type SettingsSessionStore = Arc<RwLock<HashMap<String, SettingsSession>>>;

pub fn new_settings_store() -> SettingsSessionStore {
    Arc::new(RwLock::new(HashMap::new()))
}

pub fn spawn_settings_cleanup(store: SettingsSessionStore) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut sessions = store.write().await;
            sessions.retain(|_, s| s.created_at.elapsed() < SETTINGS_SESSION_TTL);
        }
    });
}

pub async fn issue_settings_session(
    store: &SettingsSessionStore,
    token_id: &str,
    token_secret: &str,
) -> String {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut sessions = store.write().await;
    if sessions.len() >= 1000 {
        sessions.retain(|_, s| s.created_at.elapsed() < SETTINGS_SESSION_TTL);
    }
    sessions.insert(
        session_id.clone(),
        SettingsSession {
            token_id: token_id.to_string(),
            token_secret: token_secret.to_string(),
            created_at: Instant::now(),
        },
    );
    session_id
}

pub fn build_session_cookie(session_id: &str) -> String {
    // Path=/ so a single cookie covers both /settings (admin) and /setup/user
    // (the onboarding wizard, sub-PR 2.4e). Earlier revisions scoped this
    // narrowly to /settings; widening it to / is safe because the cookie is
    // HttpOnly + SameSite=Lax + Secure, and the only consumers of the
    // bsmcp_settings_session cookie are first-party browser routes on this
    // server.
    format!(
        "{name}={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}; Secure",
        name = SETTINGS_COOKIE_NAME,
        id = session_id,
        ttl = SETTINGS_SESSION_TTL.as_secs(),
    )
}

/// Look up the (token_id, token_secret) for the cookie attached to the
/// incoming request. Returns `None` if the cookie is absent, points at a
/// missing/expired session, or fails to parse. Public so sibling modules
/// (notably `setup_ui` for the onboarding wizard) can share the same
/// session store without duplicating the cookie-parsing logic.
pub async fn resolve_session_creds(
    headers: &HeaderMap,
    store: &SettingsSessionStore,
) -> Option<(String, String)> {
    let cookie_header = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())?;
    let session_id = cookie_header
        .split(';')
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(&format!("{}=", SETTINGS_COOKIE_NAME)))?;

    let sessions = store.read().await;
    let session = sessions.get(session_id)?;
    if session.created_at.elapsed() >= SETTINGS_SESSION_TTL {
        return None;
    }
    Some((session.token_id.clone(), session.token_secret.clone()))
}

pub async fn has_valid_session(
    headers: &HeaderMap,
    store: &SettingsSessionStore,
) -> bool {
    resolve_session_creds(headers, store).await.is_some()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// --- Handlers ---

pub async fn handle_settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&code_challenge=&code_challenge_method=&return_to=/settings").into_response(),
    };

    let token_id_hash = hash_token_id(&token_id);
    let settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let globals = state.db.get_global_settings().await.unwrap_or_default();

    // Best-effort: resolve the User Journals shelf name so the form can show
    // "Currently set to: <name> (<id>)". Failures are silent — the form still
    // renders without the summary line.
    let user_journals_shelf_summary = match globals.user_journals_shelf_id {
        Some(id) => {
            let bs_client = bsmcp_common::bookstack::BookStackClient::new(
                &state.bookstack_url,
                &token_id,
                &token_secret,
                state.http_client.clone(),
            );
            bs_client
                .get_shelf(id)
                .await
                .ok()
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                .map(|name| format!(" Currently set to: {} ({})", name, id))
                .unwrap_or_default()
        }
        None => String::new(),
    };

    let tool_defaults_section = render_tool_defaults_section(&globals, &settings);
    Html(render_settings_page(
        &settings,
        &globals,
        &user_journals_shelf_summary,
        &tool_defaults_section,
    ))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct SettingsForm {
    // Per-user
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub bookstack_user_id: Option<String>,
    #[serde(default)]
    pub domains: Option<String>,
    #[serde(default)]
    pub system_prompt_page_ids: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub semantic_against_full_kb: Option<String>,

    // Globals (admin-only — server checks before persisting)
    #[serde(default)]
    pub guide_page_id: Option<String>,
    #[serde(default)]
    pub org_identity_page_id: Option<String>,
    #[serde(default)]
    pub policies_scope_type: Option<String>,
    #[serde(default)]
    pub policies_scope_id: Option<String>,
    #[serde(default)]
    pub sops_scope_type: Option<String>,
    #[serde(default)]
    pub sops_scope_id: Option<String>,
    #[serde(default)]
    pub best_practices_scope_type: Option<String>,
    #[serde(default)]
    pub best_practices_scope_id: Option<String>,
    #[serde(default)]
    pub org_domains: Option<String>,
    #[serde(default)]
    pub org_required_instructions_page_ids: Option<String>,
    #[serde(default)]
    pub org_ai_usage_policy_page_ids: Option<String>,
    #[serde(default)]
    pub friendly_structure: Option<String>,
    #[serde(default)]
    pub full_content_in_briefing: Option<String>,
    #[serde(default)]
    pub strict_setup: Option<String>,
    #[serde(default)]
    pub hive_shelf_id: Option<String>,
    #[serde(default)]
    pub user_journals_shelf_id: Option<String>,
}

pub async fn handle_settings_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawForm(body): RawForm,
) -> Response {
    // Parse the urlencoded body into both:
    //   1. The typed `SettingsForm` (the long-standing per-user / per-global
    //      fields). serde_urlencoded handles this directly.
    //   2. A flat (key, value) list, so we can pluck out the
    //      dynamically-named `tool_default_<name>` checkboxes added in
    //      Phase 2.4d. serde_urlencoded can't deserialize variable-name
    //      fields into a typed struct, so the extra pass over the raw
    //      bytes is unavoidable.
    let body_str = std::str::from_utf8(&body).unwrap_or("");
    let form: SettingsForm = serde_urlencoded::from_str(body_str).unwrap_or_default();
    let raw_pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(body_str).unwrap_or_default();

    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&code_challenge=&code_challenge_method=&return_to=/settings").into_response(),
    };

    let token_id_hash = hash_token_id(&token_id);
    let mut settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Per-user fields
    settings.label = nonempty(&form.label);
    settings.role = nonempty(&form.role);
    settings.user_id = nonempty(&form.user_id);
    settings.bookstack_user_id = parse_optional_i64(&form.bookstack_user_id);
    settings.domains = parse_string_list(&form.domains);
    settings.system_prompt_page_ids = parse_id_list(&form.system_prompt_page_ids);
    settings.timezone = nonempty(&form.timezone);
    if settings.timezone.is_some() {
        settings.timezone_fetched_at = Some(now_unix());
    }
    settings.semantic_against_full_kb = checkbox(&form.semantic_against_full_kb);

    if let Err(e) = state.db.save_user_settings(&token_id_hash, &settings).await {
        return error_response(format!("Failed to save user settings: {e}"));
    }

    // Globals: only persist when the calling token is admin.
    let bs_client = bsmcp_common::bookstack::BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );
    let is_admin = bs_client.is_admin().await.unwrap_or(false);
    if is_admin {
        let mut globals = state.db.get_global_settings().await.unwrap_or_default();
        globals.guide_page_id = parse_optional_i64(&form.guide_page_id);
        globals.org_identity_page_id = parse_optional_i64(&form.org_identity_page_id);
        globals.policies_scope = parse_kb_scope(&form.policies_scope_type, &form.policies_scope_id);
        globals.sops_scope = parse_kb_scope(&form.sops_scope_type, &form.sops_scope_id);
        globals.best_practices_scope = parse_kb_scope(&form.best_practices_scope_type, &form.best_practices_scope_id);
        globals.org_domains = parse_string_list(&form.org_domains);
        globals.org_required_instructions_page_ids = parse_id_list(&form.org_required_instructions_page_ids);
        globals.org_ai_usage_policy_page_ids = parse_id_list(&form.org_ai_usage_policy_page_ids);
        globals.friendly_structure = checkbox(&form.friendly_structure);
        globals.full_content_in_briefing = checkbox(&form.full_content_in_briefing);
        globals.strict_setup = checkbox(&form.strict_setup);
        globals.hive_shelf_id = parse_optional_i64(&form.hive_shelf_id);
        globals.user_journals_shelf_id = parse_optional_i64(&form.user_journals_shelf_id);
        globals.tool_defaults = parse_tool_defaults(&raw_pairs);
        if let Err(e) = state.db.save_global_settings(&globals, &token_id_hash).await {
            return error_response(format!("Failed to save global settings: {e}"));
        }
    }

    Redirect::to("/settings").into_response()
}

/// Probe handler stubbed for v0.8.0 — returns 410 until the typed-slot
/// auto-discovery design is complete.
pub async fn handle_settings_probe_get(
    State(_state): State<AppState>,
    _headers: HeaderMap,
) -> Response {
    (StatusCode::GONE, Html("<p>Probe disabled in v0.8.0. Use <a href=\"/settings\">/settings</a> directly.</p>"))
        .into_response()
}

pub async fn handle_settings_probe_post(
    State(_state): State<AppState>,
    _headers: HeaderMap,
) -> Response {
    (StatusCode::GONE, "probe disabled in v0.8.0").into_response()
}

// --- Form parsing helpers ---

fn nonempty(v: &Option<String>) -> Option<String> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn parse_optional_i64(v: &Option<String>) -> Option<i64> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).and_then(|s| s.parse().ok())
}

fn parse_id_list(v: &Option<String>) -> Vec<i64> {
    let Some(s) = v.as_deref() else { return Vec::new(); };
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|t| t.trim().parse::<i64>().ok())
        .collect()
}

fn parse_string_list(v: &Option<String>) -> Vec<String> {
    let Some(s) = v.as_deref() else { return Vec::new(); };
    s.split(|c: char| c == ',' || c == '\n')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

fn parse_kb_scope(kind: &Option<String>, id: &Option<String>) -> Option<KbScope> {
    let id = parse_optional_i64(id)?;
    let kind = nonempty(kind)?;
    match kind.to_ascii_lowercase().as_str() {
        "shelf" => Some(KbScope::Shelf(id)),
        "book" => Some(KbScope::Book(id)),
        "page" => Some(KbScope::Page(id)),
        _ => None,
    }
}

fn checkbox(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("on") | Some("true") | Some("1"))
}

/// Phase 2.4d. Pull the per-tool admin defaults out of the raw form pairs.
/// The form emits a `tool_listed_<name>` marker for every tool present
/// (so a missing checkbox is distinguishable from a tool that wasn't
/// rendered) plus an optional `tool_default_<name>=on` for each checked
/// one. Returns an explicit on/off entry for every listed tool — absent
/// keys mean "no admin default set" and `is_tool_enabled` will fall
/// through to the on-by-default branch.
///
/// This keeps the on-disk shape clean: we don't need to write `true` for
/// every tool just to say "it's on by default." The map only ever
/// captures the admin's explicit overrides.
fn parse_tool_defaults(
    pairs: &[(String, String)],
) -> std::collections::HashMap<String, bool> {
    use std::collections::{HashMap, HashSet};

    let mut listed: HashSet<String> = HashSet::new();
    let mut on: HashSet<String> = HashSet::new();
    for (k, v) in pairs {
        if let Some(name) = k.strip_prefix("tool_listed_") {
            if !name.is_empty() {
                listed.insert(name.to_string());
            }
        } else if let Some(name) = k.strip_prefix("tool_default_") {
            if !name.is_empty() && matches!(v.as_str(), "on" | "true" | "1") {
                on.insert(name.to_string());
            }
        }
    }

    // Keep only "off" explicit overrides; leave "on" tools out of the
    // map entirely so the default-ON contract holds without any storage.
    // (If we wrote `true` for every listed tool we'd grow the map every
    // settings save with no semantic difference from leaving it empty.)
    listed
        .into_iter()
        .filter(|name| !on.contains(name))
        .map(|name| (name, false))
        .collect::<HashMap<_, _>>()
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn error_response(msg: String) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Html(html_escape(&msg))).into_response()
}

// --- HTML rendering ---

fn render_kb_scope_picker(name: &str, label: &str, scope: &Option<KbScope>) -> String {
    let (kind, id) = match scope {
        Some(KbScope::Shelf(id)) => ("shelf", *id),
        Some(KbScope::Book(id)) => ("book", *id),
        Some(KbScope::Page(id)) => ("page", *id),
        None => ("", 0),
    };
    let id_value = if id == 0 { String::new() } else { id.to_string() };
    format!(
        r#"
<div class="scope-picker">
  <label>{label}</label>
  <select name="{name}_type">
    <option value=""{none_sel}>(none)</option>
    <option value="shelf"{shelf_sel}>Shelf</option>
    <option value="book"{book_sel}>Book</option>
    <option value="page"{page_sel}>Page</option>
  </select>
  <input type="number" name="{name}_id" placeholder="ID" value="{id_value}" />
</div>"#,
        none_sel = if kind.is_empty() { " selected" } else { "" },
        shelf_sel = if kind == "shelf" { " selected" } else { "" },
        book_sel = if kind == "book" { " selected" } else { "" },
        page_sel = if kind == "page" { " selected" } else { "" },
    )
}

/// Phase 2.4d. Build the "Tool defaults (admin only)" section. Sources
/// the tool list from `mcp::all_tool_names()` so the form stays in sync
/// with whatever the server actually advertises — no hardcoded list.
/// Each row carries a hidden `tool_listed_<name>` marker so the POST
/// handler can distinguish "checkbox unchecked" from "tool not in form."
///
/// TODO(2.4e onboarding): the per-user override UI for these same tools
/// lives in the onboarding setup page, not here. Users will be able to
/// flip individual tools on or off (over the admin default) from there;
/// `UserSettings.tool_overrides` already wires it up end-to-end. Linked
/// from the "(your override: ...)" annotation on each row.
fn render_tool_defaults_section(g: &GlobalSettings, s: &UserSettings) -> String {
    let mut rows = String::new();
    for name in mcp::all_tool_names() {
        // Effective admin default: explicit value if set, else on.
        let admin_on = g.tool_defaults.get(&name).copied().unwrap_or(true);
        // Best-effort note when the calling user has overridden the
        // global default in their per-user settings. Visible only to
        // the user themselves (we don't render other users' overrides);
        // the user-facing override UI lands in sub-PR 2.4e on the
        // onboarding setup page.
        let user_override_note = match s.tool_overrides.get(&name) {
            Some(true) => " <span class=\"note\">(your override: ON)</span>",
            Some(false) => " <span class=\"note\">(your override: OFF)</span>",
            None => "",
        };
        let escaped_name = html_escape(&name);
        let checked = if admin_on { " checked" } else { "" };
        rows.push_str(&format!(
            "<label class=\"tool-row\"><input type=\"hidden\" name=\"tool_listed_{name}\" value=\"1\">\
             <input type=\"checkbox\" name=\"tool_default_{name}\"{checked}> \
             <code>{escaped_name}</code>{user_override_note}</label>\n",
            name = escaped_name,
            checked = checked,
            escaped_name = escaped_name,
            user_override_note = user_override_note,
        ));
    }

    format!(
        r#"
  <h2>Tool defaults (admin only)</h2>
  <p class="help">Per-tool admin default. Unchecked = disabled by default for all users (a user can still re-enable in their own settings via <code>/setup/user</code>). Checked = on. Tools not listed at all default ON.</p>
  <p class="help"><strong>Memory-protocol tools ship default OFF on a fresh install</strong> (<code>briefing</code>, <code>journal</code>, <code>identity</code>, <code>reminders</code>, <code>events</code>, <code>sessions</code>, <code>migrate</code>, <code>user</code>, <code>config</code>, <code>directory</code>, <code>session_event</code>, <code>dismiss_setup_nudge</code>) so the AI sees just KB CRUD + semantic search until you opt in. Toggle them on here for everyone, or let users enable per-account in <code>/setup/user</code>. Note: disabling <code>briefing</code> kills <code>meta.briefing</code> auto-injection on every tool response, so the AI loses session context — re-enable if you want the AI to know what time it is.</p>
  <div class="tool-defaults">
    {rows}
  </div>
"#,
        rows = rows
    )
}

fn render_settings_page(
    s: &UserSettings,
    g: &GlobalSettings,
    user_journals_shelf_summary: &str,
    tool_defaults_section: &str,
) -> String {
    let domains = s.domains.join(", ");
    let system_prompt = s.system_prompt_page_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let org_domains = g.org_domains.join(", ");
    let org_instructions = g.org_required_instructions_page_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let org_policy = g.org_ai_usage_policy_page_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");

    let policies = render_kb_scope_picker("policies_scope", "Policies scope", &g.policies_scope);
    let sops = render_kb_scope_picker("sops_scope", "SOPs scope", &g.sops_scope);
    let bp = render_kb_scope_picker("best_practices_scope", "Best practices scope", &g.best_practices_scope);

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>BookStack MCP Settings</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 720px; margin: 2em auto; padding: 0 1em; color: #222; }}
h1 {{ margin-bottom: 0.25em; }}
h2 {{ margin-top: 2em; border-bottom: 1px solid #ddd; padding-bottom: 0.2em; }}
.note {{ color: #666; font-size: 0.9em; }}
label {{ display: block; margin: 0.8em 0 0.3em; font-weight: 600; }}
input[type=text], input[type=number], textarea, select {{ padding: 0.5em; box-sizing: border-box; font-family: inherit; }}
input[type=text], textarea {{ width: 100%; }}
input[type=number] {{ width: 12em; }}
textarea {{ min-height: 4em; }}
.help {{ font-size: 0.85em; color: #555; margin-top: 0.2em; }}
button {{ margin-top: 1.5em; padding: 0.6em 1.2em; font-size: 1em; cursor: pointer; }}
.scope-picker {{ margin: 0.8em 0; display: grid; grid-template-columns: 1fr auto; gap: 0.5em; align-items: end; }}
.scope-picker label {{ grid-column: 1 / -1; }}
.tool-defaults {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 0.3em 1em; margin: 0.5em 0; }}
.tool-defaults .tool-row {{ display: flex; align-items: center; gap: 0.4em; font-weight: normal; margin: 0; }}
.tool-defaults code {{ font-size: 0.9em; }}
</style>
</head>
<body>
<h1>BookStack MCP Settings</h1>
<p class="note">v0.8.0 — typed setup slots. Typeahead pickers backed by precision_search are coming in a follow-up commit on this branch.</p>
<form method="post" action="/settings">
  <h2>Per-user</h2>
  <label>Label <input type="text" name="label" value="{label}"></label>
  <label>Role hint <input type="text" name="role" value="{role}"></label>
  <label>user_id <input type="text" name="user_id" value="{user_id}"></label>
  <p class="help">Stable identifier (typically email).</p>
  <label>bookstack_user_id <input type="number" name="bookstack_user_id" value="{bookstack_user_id}"></label>
  <p class="help">BookStack user row ID — enables ACL-filtered semantic search and role-gated tool exposure.</p>
  <label>Owned domains <textarea name="domains">{domains}</textarea></label>
  <p class="help">Comma- or newline-separated.</p>
  <label>system_prompt_page_ids <input type="text" name="system_prompt_page_ids" value="{system_prompt}"></label>
  <p class="help">Free-form fallback for the typed slots — page IDs always injected into the briefing.</p>
  <label>Timezone <input type="text" name="timezone" value="{timezone}"></label>
  <p class="help">IANA name like <code>America/New_York</code>.</p>
  <label><input type="checkbox" name="semantic_against_full_kb" {full_kb_checked}> Search the entire knowledge base (expensive)</label>

  <h2>Org-wide (admin-only)</h2>
  <p class="help">Non-admin saves silently drop the global fields below.</p>
  <label>guide_page_id <input type="number" name="guide_page_id" value="{guide_page_id}"></label>
  <p class="help">Page describing how to use this BookStack. Auto-included in every briefing's system_prompt_additions when set.</p>
  <label>org_identity_page_id <input type="number" name="org_identity_page_id" value="{org_identity_page_id}"></label>
  <p class="help">Page describing the organization. Pulled into every briefing.</p>
  {policies}
  {sops}
  {bp}
  <label>Org domains <textarea name="org_domains">{org_domains}</textarea></label>
  <label>org_required_instructions_page_ids <input type="text" name="org_required_instructions_page_ids" value="{org_instructions}"></label>
  <label>org_ai_usage_policy_page_ids <input type="text" name="org_ai_usage_policy_page_ids" value="{org_policy}"></label>
  <label><input type="checkbox" name="friendly_structure" {fs_checked}> friendly_structure</label>
  <label><input type="checkbox" name="full_content_in_briefing" {fc_checked}> full_content_in_briefing</label>
  <label><input type="checkbox" name="strict_setup" {ss_checked}> strict_setup (block tools until configured)</label>
  <label>hive_shelf_id <input type="number" name="hive_shelf_id" value="{hive_shelf_id}"></label>
  <label>User Journals shelf <input type="number" name="user_journals_shelf_id" value="{user_journals_shelf_id}"></label>
  <p class="help">BookStack shelf where each user's personal Journal book lives. Required to enable remember_user_journal / remember_agent_journal.{user_journals_shelf_summary}</p>
{tool_defaults_section}
  <button type="submit">Save</button>
</form>
</body>
</html>"#,
        label = html_escape(s.label.as_deref().unwrap_or("")),
        role = html_escape(s.role.as_deref().unwrap_or("")),
        user_id = html_escape(s.user_id.as_deref().unwrap_or("")),
        bookstack_user_id = s.bookstack_user_id.map(|i| i.to_string()).unwrap_or_default(),
        domains = html_escape(&domains),
        system_prompt = html_escape(&system_prompt),
        timezone = html_escape(s.timezone.as_deref().unwrap_or("")),
        full_kb_checked = if s.semantic_against_full_kb { "checked" } else { "" },
        guide_page_id = g.guide_page_id.map(|i| i.to_string()).unwrap_or_default(),
        org_identity_page_id = g.org_identity_page_id.map(|i| i.to_string()).unwrap_or_default(),
        policies = policies,
        sops = sops,
        bp = bp,
        org_domains = html_escape(&org_domains),
        org_instructions = html_escape(&org_instructions),
        org_policy = html_escape(&org_policy),
        fs_checked = if g.friendly_structure { "checked" } else { "" },
        fc_checked = if g.full_content_in_briefing { "checked" } else { "" },
        ss_checked = if g.strict_setup { "checked" } else { "" },
        hive_shelf_id = g.hive_shelf_id.map(|i| i.to_string()).unwrap_or_default(),
        user_journals_shelf_id = g.user_journals_shelf_id.map(|i| i.to_string()).unwrap_or_default(),
        user_journals_shelf_summary = html_escape(user_journals_shelf_summary),
        tool_defaults_section = tool_defaults_section,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn parse_tool_defaults_marks_unchecked_listed_tools_as_off() {
        // Form rendered three tools, user unchecked one.
        let pairs = vec![
            pair("tool_listed_briefing", "1"),
            pair("tool_default_briefing", "on"),
            pair("tool_listed_journal", "1"),
            pair("tool_default_journal", "on"),
            pair("tool_listed_identity", "1"),
            // identity intentionally not checked
        ];
        let map = parse_tool_defaults(&pairs);
        assert_eq!(map.get("identity"), Some(&false));
        // Checked tools left out (default-ON, no need to store true).
        assert!(!map.contains_key("briefing"));
        assert!(!map.contains_key("journal"));
    }

    #[test]
    fn parse_tool_defaults_ignores_unrelated_pairs() {
        let pairs = vec![
            pair("label", "DTC"),
            pair("guide_page_id", "42"),
            pair("tool_listed_journal", "1"),
            // No tool_default_journal — unchecked, so journal=false.
        ];
        let map = parse_tool_defaults(&pairs);
        assert_eq!(map, [("journal".to_string(), false)].into_iter().collect());
    }

    #[test]
    fn parse_tool_defaults_empty_when_no_listed_keys() {
        // No `tool_listed_*` keys = admin section absent. Result is empty
        // even if a stray `tool_default_*` shows up.
        let pairs = vec![pair("tool_default_journal", "on")];
        let map = parse_tool_defaults(&pairs);
        assert!(map.is_empty());
    }

    #[test]
    fn render_tool_defaults_section_lists_every_advertised_tool() {
        let g = GlobalSettings::default();
        let s = UserSettings::default();
        let html = render_tool_defaults_section(&g, &s);
        for name in mcp::all_tool_names() {
            assert!(
                html.contains(&format!("tool_listed_{name}")),
                "missing hidden marker for {name}"
            );
        }
    }

    #[test]
    fn render_tool_defaults_section_marks_admin_off_unchecked() {
        let mut g = GlobalSettings::default();
        // Use a known stable tool name from the catalog.
        g.tool_defaults.insert("search_content".to_string(), false);
        let s = UserSettings::default();
        let html = render_tool_defaults_section(&g, &s);

        // The search_content checkbox row should NOT carry " checked".
        // Find the row by its hidden marker, then walk backwards a
        // small window to confirm the checkbox is absent of "checked".
        let marker = "tool_listed_search_content";
        let pos = html.find(marker).expect("search_content row missing");
        // The label spans a single line; the checkbox lives within
        // ~200 chars after the marker.
        let window_end = (pos + 400).min(html.len());
        let window = &html[pos..window_end];
        assert!(
            !window.contains("type=\"checkbox\" name=\"tool_default_search_content\" checked"),
            "search_content should be unchecked when admin set it false"
        );
    }
}
