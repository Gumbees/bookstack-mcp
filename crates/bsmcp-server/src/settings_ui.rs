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

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use tokio::sync::RwLock;
use zeroize::Zeroize;

use bsmcp_common::settings::{hash_token_id, GlobalSettings, KbScope, UserSettings};

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
    format!(
        "{name}={id}; Path=/settings; HttpOnly; SameSite=Lax; Max-Age={ttl}; Secure",
        name = SETTINGS_COOKIE_NAME,
        id = session_id,
        ttl = SETTINGS_SESSION_TTL.as_secs(),
    )
}

async fn resolve_session(
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
    resolve_session(headers, store).await.is_some()
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
    let (token_id, _token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
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

    Html(render_settings_page(&settings, &globals)).into_response()
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
    Form(form): Form<SettingsForm>,
) -> Response {
    let (token_id, token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
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
    s.split([',', '\n'])
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

fn render_settings_page(s: &UserSettings, g: &GlobalSettings) -> String {
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
  <label>user_journals_shelf_id <input type="number" name="user_journals_shelf_id" value="{user_journals_shelf_id}"></label>

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
    )
}
