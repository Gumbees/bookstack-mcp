//! Browser-based settings UI for the Hive memory flow.
//!
//! Auth flow:
//! 1. User visits GET /settings → no cookie → redirect to /authorize?return_to=/settings
//! 2. /authorize validates the BookStack token and (if return_to is /settings)
//!    creates a settings session, sets a cookie, redirects to /settings.
//! 3. /settings reads the cookie, looks up credentials in the session store,
//!    renders the form.
//! 4. POST /settings parses the form, persists to user_settings, redirects.
//!
//! Sessions are kept in memory; on restart users must re-authenticate. The cookie
//! is HttpOnly, SameSite=Lax, 8h TTL.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;
use zeroize::Zeroize;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::settings::{hash_token_id, UserSettings};

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

/// Periodically evict expired settings sessions. Spawn once at startup.
pub fn spawn_settings_cleanup(store: SettingsSessionStore) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let mut sessions = store.write().await;
            sessions.retain(|_, s| s.created_at.elapsed() < SETTINGS_SESSION_TTL);
        }
    });
}

/// Issue a fresh settings session and return the cookie value.
/// Called from oauth.rs after successful token validation when return_to=/settings.
pub async fn issue_settings_session(
    store: &SettingsSessionStore,
    token_id: &str,
    token_secret: &str,
) -> String {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut sessions = store.write().await;
    // Cap session count to prevent unbounded growth
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

/// Build the Set-Cookie header value for the settings session.
pub fn build_session_cookie(session_id: &str) -> String {
    format!(
        "{name}={id}; Path=/settings; HttpOnly; SameSite=Lax; Max-Age={ttl}; Secure",
        name = SETTINGS_COOKIE_NAME,
        id = session_id,
        ttl = SETTINGS_SESSION_TTL.as_secs(),
    )
}

/// Resolve cookie → (token_id, token_secret). Returns None if missing/expired.
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
    let (token_id, token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&code_challenge=&code_challenge_method=&return_to=/settings").into_response(),
    };

    // Load existing settings
    let token_id_hash = hash_token_id(&token_id);
    let settings = match state.db.get_user_settings(&token_id_hash).await {
        Ok(Some(s)) => s,
        Ok(None) => UserSettings::default(),
        Err(e) => {
            eprintln!("Settings: load failed: {e}");
            UserSettings::default()
        }
    };

    // Fetch BookStack lists in parallel for the dropdowns. If any fail, the
    // form falls back to a text input — never fatal.
    let client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let (shelves_res, books_res, chapters_res) = tokio::join!(
        client.list_shelves(500, 0),
        client.list_books(500, 0),
        client.list_chapters(500, 0),
    );

    let shelves = extract_named_list(shelves_res.ok().as_ref());
    let books = extract_named_list(books_res.ok().as_ref());
    let chapters = extract_named_list(chapters_res.ok().as_ref());

    Html(render_settings_page(&settings, &shelves, &books, &chapters)).into_response()
}

#[derive(Deserialize)]
pub struct SettingsForm {
    pub label: Option<String>,
    pub role: Option<String>,
    pub ai_identity_ouid: Option<String>,
    pub ai_identity_book_id: Option<String>,
    pub ai_identity_page_id: Option<String>,
    pub ai_identity_name: Option<String>,
    pub ai_subagents_chapter_id: Option<String>,
    pub ai_connections_chapter_id: Option<String>,
    pub ai_opportunities_chapter_id: Option<String>,
    pub ai_hive_shelf_id: Option<String>,
    pub ai_collage_book_id: Option<String>,
    pub ai_shared_collage_book_id: Option<String>,
    pub ai_hive_journal_book_id: Option<String>,
    pub ai_activity_chapter_id: Option<String>,
    pub user_id: Option<String>,
    pub user_identity_page_id: Option<String>,
    pub user_journal_book_id: Option<String>,
    pub semantic_against_journal: Option<String>,
    pub semantic_against_collage: Option<String>,
    pub semantic_against_shared_collage: Option<String>,
    pub semantic_against_user_journal: Option<String>,
    pub semantic_against_full_kb: Option<String>,
    pub use_follow_up_remember_agent: Option<String>,
    pub recent_journal_count: Option<String>,
    pub active_collage_count: Option<String>,
    /// Comma- or whitespace-separated list of BookStack page IDs.
    pub system_prompt_page_ids: Option<String>,
}

fn empty_to_none(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn parse_id(s: Option<String>) -> Option<i64> {
    empty_to_none(s).and_then(|v| v.parse().ok())
}

fn parse_count(s: Option<String>, default: usize) -> usize {
    empty_to_none(s)
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0 && n <= 100)
        .unwrap_or(default)
}

fn parse_id_list(s: Option<String>) -> Vec<i64> {
    let Some(raw) = empty_to_none(s) else { return Vec::new(); };
    raw.split(|c: char| !c.is_ascii_digit())
        .filter_map(|tok| tok.parse::<i64>().ok())
        .collect()
}

fn format_id_list(ids: &[i64]) -> String {
    ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
}

fn checkbox_on(s: Option<String>) -> bool {
    matches!(s.as_deref(), Some("on") | Some("true") | Some("1"))
}

pub async fn handle_settings_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SettingsForm>,
) -> Response {
    let (token_id, _token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&return_to=/settings").into_response(),
    };

    let settings = UserSettings {
        label: empty_to_none(form.label),
        role: empty_to_none(form.role),
        ai_identity_ouid: empty_to_none(form.ai_identity_ouid),
        ai_identity_book_id: parse_id(form.ai_identity_book_id),
        ai_identity_page_id: parse_id(form.ai_identity_page_id),
        ai_identity_name: empty_to_none(form.ai_identity_name),
        ai_subagents_chapter_id: parse_id(form.ai_subagents_chapter_id),
        ai_connections_chapter_id: parse_id(form.ai_connections_chapter_id),
        ai_opportunities_chapter_id: parse_id(form.ai_opportunities_chapter_id),
        ai_hive_shelf_id: parse_id(form.ai_hive_shelf_id),
        ai_collage_book_id: parse_id(form.ai_collage_book_id),
        ai_shared_collage_book_id: parse_id(form.ai_shared_collage_book_id),
        ai_hive_journal_book_id: parse_id(form.ai_hive_journal_book_id),
        ai_activity_chapter_id: parse_id(form.ai_activity_chapter_id),
        user_id: empty_to_none(form.user_id),
        user_identity_page_id: parse_id(form.user_identity_page_id),
        user_journal_book_id: parse_id(form.user_journal_book_id),
        semantic_against_journal: checkbox_on(form.semantic_against_journal),
        semantic_against_collage: checkbox_on(form.semantic_against_collage),
        semantic_against_shared_collage: checkbox_on(form.semantic_against_shared_collage),
        semantic_against_user_journal: checkbox_on(form.semantic_against_user_journal),
        semantic_against_full_kb: checkbox_on(form.semantic_against_full_kb),
        use_follow_up_remember_agent: checkbox_on(form.use_follow_up_remember_agent),
        recent_journal_count: parse_count(form.recent_journal_count, 3),
        active_collage_count: parse_count(form.active_collage_count, 10),
        system_prompt_page_ids: parse_id_list(form.system_prompt_page_ids),
    };

    let token_id_hash = hash_token_id(&token_id);
    if let Err(e) = state.db.save_user_settings(&token_id_hash, &settings).await {
        eprintln!("Settings: save failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to save settings").into_response();
    }

    eprintln!("Settings: saved for user (token_id_hash={}…)", &token_id_hash[..16.min(token_id_hash.len())]);
    Redirect::to("/settings?saved=1").into_response()
}

// --- Helpers ---

#[derive(Clone)]
struct NamedItem {
    id: i64,
    name: String,
}

fn extract_named_list(value: Option<&Value>) -> Vec<NamedItem> {
    let Some(v) = value else { return Vec::new(); };
    let Some(arr) = v.get("data").and_then(|d| d.as_array()) else { return Vec::new(); };
    let mut items: Vec<NamedItem> = arr
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(|i| i.as_i64())?;
            let name = item.get("name").and_then(|n| n.as_str())?.to_string();
            Some(NamedItem { id, name })
        })
        .collect();
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
}

fn render_select(name: &str, items: &[NamedItem], current: Option<i64>, allow_blank: bool) -> String {
    let mut html = String::new();
    html.push_str(&format!(r#"<select name="{}" id="{}">"#, html_escape(name), html_escape(name)));
    if allow_blank {
        let selected = if current.is_none() { " selected" } else { "" };
        html.push_str(&format!(r#"<option value=""{selected}>— none —</option>"#));
    }
    for item in items {
        let selected = if current == Some(item.id) { " selected" } else { "" };
        html.push_str(&format!(
            r#"<option value="{id}"{selected}>{name} ({id})</option>"#,
            id = item.id,
            name = html_escape(&item.name),
        ));
    }
    html.push_str("</select>");
    html
}

fn render_text(name: &str, value: Option<&str>, placeholder: &str) -> String {
    format!(
        r#"<input type="text" name="{name}" id="{name}" value="{value}" placeholder="{placeholder}">"#,
        name = html_escape(name),
        value = html_escape(value.unwrap_or("")),
        placeholder = html_escape(placeholder),
    )
}

fn render_id_input(name: &str, value: Option<i64>) -> String {
    format!(
        r#"<input type="text" inputmode="numeric" name="{name}" id="{name}" value="{value}" placeholder="page id">"#,
        name = html_escape(name),
        value = value.map(|v| v.to_string()).unwrap_or_default(),
    )
}

fn render_checkbox(name: &str, checked: bool, label: &str) -> String {
    let chk = if checked { " checked" } else { "" };
    format!(
        r#"<label class="cb"><input type="checkbox" name="{name}" id="{name}"{chk}> {label}</label>"#,
        name = html_escape(name),
        chk = chk,
        label = html_escape(label),
    )
}

fn render_settings_page(
    s: &UserSettings,
    shelves: &[NamedItem],
    books: &[NamedItem],
    chapters: &[NamedItem],
) -> String {
    let css = r#"
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #1a1a2e; color: #e0e0e0; padding: 2rem; }
.container { max-width: 720px; margin: 0 auto; }
h1 { font-size: 1.5rem; margin-bottom: 0.3rem; color: #fff; }
.subtitle { color: #888; font-size: 0.9rem; margin-bottom: 2rem; }
.card { background: #16213e; border-radius: 12px; padding: 1.5rem; margin-bottom: 1rem; box-shadow: 0 4px 16px rgba(0,0,0,0.2); }
h2 { font-size: 1rem; font-weight: 600; margin-bottom: 1rem; color: #f8fafc; border-bottom: 1px solid #2a3a5c; padding-bottom: 0.5rem; }
.field { margin-bottom: 1rem; }
.field label { display: block; font-size: 0.82rem; color: #aaa; margin-bottom: 0.3rem; }
.field .hint { font-size: 0.75rem; color: #64748b; margin-top: 0.2rem; }
input[type="text"], select { width: 100%; padding: 0.55rem 0.7rem; border: 1px solid #2a3a5c; border-radius: 6px; background: #0f1a30; color: #e0e0e0; font-size: 0.9rem; font-family: inherit; }
input:focus, select:focus { outline: none; border-color: #3498db; }
.cb { display: flex; align-items: center; gap: 0.5rem; font-size: 0.88rem; color: #e0e0e0; cursor: pointer; padding: 0.3rem 0; }
.cb input { width: auto; }
.row2 { display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; }
.actions { display: flex; gap: 0.75rem; align-items: center; }
button.primary { background: #2980b9; color: #fff; border: none; padding: 0.7rem 1.4rem; border-radius: 6px; font-size: 0.95rem; font-weight: 600; cursor: pointer; }
button.primary:hover { background: #3498db; }
a.reauth { color: #94a3b8; font-size: 0.85rem; text-decoration: none; margin-left: auto; }
a.reauth:hover { color: #cbd5e1; text-decoration: underline; }
.saved { background: #14532d; color: #4ade80; padding: 0.6rem 1rem; border-radius: 6px; margin-bottom: 1rem; font-size: 0.9rem; }
"#;

    let saved_banner = ""; // optional ?saved=1 banner — populated in caller if we add query parsing later

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Hive Memory Settings — BookStack MCP</title>
<style>{css}</style>
</head>
<body>
<div class="container">
<h1>Hive Memory Settings</h1>
<p class="subtitle">Configure where your AI agent's memory lives in this BookStack. Empty fields disable that part of the <code>remember</code> response — they don't break it.</p>
{saved_banner}
<form method="POST" action="/settings">

<div class="card">
<h2>Instance</h2>
<div class="row2">
<div class="field">
  <label for="label">Label</label>
  {label_input}
  <div class="hint">Free-form name for this BookStack (e.g., "DTC", "Bee's Roadhouse").</div>
</div>
<div class="field">
  <label for="role">Role</label>
  {role_input}
  <div class="hint">Free-form role hint (e.g., "work", "personal").</div>
</div>
</div>
</div>

<div class="card">
<h2>AI Identity</h2>
<div class="row2">
<div class="field">
  <label for="ai_identity_name">Display name</label>
  {ai_name_input}
  <div class="hint">e.g., "Pia", "Apis"</div>
</div>
<div class="field">
  <label for="ai_identity_ouid">OUID</label>
  {ai_ouid_input}
  <div class="hint">Stable identifier (ULID/UUID). Echoed back in the response.</div>
</div>
</div>
<div class="row2">
<div class="field">
  <label for="ai_hive_shelf_id">Hive shelf</label>
  {ai_shelf_select}
  <div class="hint">The shelf this Hive lives on (informational).</div>
</div>
<div class="field">
  <label for="ai_identity_book_id">Identity book</label>
  {ai_identity_book_select}
  <div class="hint">Container for the manifest page + Connections / Opportunities / Subagents chapters.</div>
</div>
</div>
<div class="field">
  <label for="ai_identity_page_id">Identity manifest page ID</label>
  {ai_page_input}
  <div class="hint">The page (inside the Identity book) that defines who the AI is.</div>
</div>
<p class="subtitle" style="margin: 1rem 0 0.5rem;">Chapters inside the Identity book — leave blank if not used:</p>
<div class="row2">
<div class="field">
  <label for="ai_subagents_chapter_id">Subagents chapter</label>
  {ai_subagents_select}
</div>
<div class="field">
  <label for="ai_connections_chapter_id">Connections chapter</label>
  {ai_connections_select}
  <div class="hint">People and agents the AI has met.</div>
</div>
</div>
<div class="field">
  <label for="ai_opportunities_chapter_id">Opportunities chapter</label>
  {ai_opportunities_select}
  <div class="hint">Financial / actionable items the AI tracks.</div>
</div>
</div>

<div class="card">
<h2>AI Journal &amp; Topics</h2>
<div class="row2">
<div class="field">
  <label for="ai_hive_journal_book_id">AI journal book</label>
  {ai_journal_select}
  <div class="hint">Daily entries organized by YYYY-MM chapters.</div>
</div>
<div class="field">
  <label for="ai_activity_chapter_id">Activity chapter (in Journal book)</label>
  {ai_activity_select}
  <div class="hint">Sits before the date chapters. Conversations, social events, etc.</div>
</div>
</div>
<div class="row2">
<div class="field">
  <label for="ai_collage_book_id">Topics / Collage book</label>
  {ai_collage_select}
</div>
<div class="field">
  <label for="ai_shared_collage_book_id">Shared collage book (optional)</label>
  {ai_shared_collage_select}
  <div class="hint">Cross-agent shared topics.</div>
</div>
</div>
</div>

<div class="card">
<h2>Your Identity</h2>
<div class="row2">
<div class="field">
  <label for="user_id">User ID</label>
  {user_id_input}
  <div class="hint">e.g., your email. Echoed back in the response.</div>
</div>
<div class="field">
  <label for="user_identity_page_id">Your identity page ID</label>
  {user_page_input}
</div>
</div>
<div class="field">
  <label for="user_journal_book_id">Your journal book</label>
  {user_journal_select}
</div>
</div>

<div class="card">
<h2>Semantic Search Targets</h2>
<p class="subtitle" style="margin-bottom: 0.75rem;">Which corpora to vector-search against the user's first message.</p>
{cb_journal}
{cb_collage}
{cb_shared_collage}
{cb_user_journal}
{cb_full_kb}
</div>

<div class="card">
<h2>Behavior</h2>
{cb_followup}
<div class="row2" style="margin-top: 1rem;">
<div class="field">
  <label for="recent_journal_count">Recent journal entries to include</label>
  <input type="text" inputmode="numeric" name="recent_journal_count" id="recent_journal_count" value="{recent_count}">
</div>
<div class="field">
  <label for="active_collage_count">Active collage entries to include</label>
  <input type="text" inputmode="numeric" name="active_collage_count" id="active_collage_count" value="{collage_count}">
</div>
</div>
<div class="field" style="margin-top: 1rem;">
  <label for="system_prompt_page_ids">Always-on context page IDs</label>
  <input type="text" name="system_prompt_page_ids" id="system_prompt_page_ids" value="{system_prompt_ids}" placeholder="e.g. 3281, 3299, 3402">
  <div class="hint">Page IDs whose full markdown is included in every briefing response. Best for SHORT durable context — writing style, communication preferences, formatting rules, ethical constraints. Long pages bloat every response. Comma- or space-separated.</div>
</div>
</div>

<div class="actions">
  <button type="submit" class="primary">Save settings</button>
  <a href="/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&return_to=/settings" class="reauth">Re-authenticate with new BookStack token →</a>
</div>

</form>
</div>
</body>
</html>"##,
        css = css,
        saved_banner = saved_banner,
        label_input = render_text("label", s.label.as_deref(), "DTC"),
        role_input = render_text("role", s.role.as_deref(), "work"),
        ai_name_input = render_text("ai_identity_name", s.ai_identity_name.as_deref(), "Pia"),
        ai_ouid_input = render_text("ai_identity_ouid", s.ai_identity_ouid.as_deref(), "019dc66e4dd87ea080ebf5d5e2985d91"),
        ai_page_input = render_id_input("ai_identity_page_id", s.ai_identity_page_id),
        ai_shelf_select = render_select("ai_hive_shelf_id", shelves, s.ai_hive_shelf_id, true),
        ai_subagents_select = render_select("ai_subagents_chapter_id", chapters, s.ai_subagents_chapter_id, true),
        ai_journal_select = render_select("ai_hive_journal_book_id", books, s.ai_hive_journal_book_id, true),
        ai_collage_select = render_select("ai_collage_book_id", books, s.ai_collage_book_id, true),
        ai_shared_collage_select = render_select("ai_shared_collage_book_id", books, s.ai_shared_collage_book_id, true),
        ai_identity_book_select = render_select("ai_identity_book_id", books, s.ai_identity_book_id, true),
        ai_connections_select = render_select("ai_connections_chapter_id", chapters, s.ai_connections_chapter_id, true),
        ai_opportunities_select = render_select("ai_opportunities_chapter_id", chapters, s.ai_opportunities_chapter_id, true),
        ai_activity_select = render_select("ai_activity_chapter_id", chapters, s.ai_activity_chapter_id, true),
        user_id_input = render_text("user_id", s.user_id.as_deref(), "you@example.com"),
        user_page_input = render_id_input("user_identity_page_id", s.user_identity_page_id),
        user_journal_select = render_select("user_journal_book_id", books, s.user_journal_book_id, true),
        cb_journal = render_checkbox("semantic_against_journal", s.semantic_against_journal, "AI journal"),
        cb_collage = render_checkbox("semantic_against_collage", s.semantic_against_collage, "Topics / collage"),
        cb_shared_collage = render_checkbox("semantic_against_shared_collage", s.semantic_against_shared_collage, "Shared collage"),
        cb_user_journal = render_checkbox("semantic_against_user_journal", s.semantic_against_user_journal, "User journal"),
        cb_full_kb = render_checkbox("semantic_against_full_kb", s.semantic_against_full_kb, "Full KB (expensive — opt in only)"),
        cb_followup = render_checkbox("use_follow_up_remember_agent", s.use_follow_up_remember_agent, "Run a follow-up reconstitution agent after the structured pull"),
        recent_count = s.recent_journal_count,
        collage_count = s.active_collage_count,
        system_prompt_ids = html_escape(&format_id_list(&s.system_prompt_page_ids)),
    )
}
