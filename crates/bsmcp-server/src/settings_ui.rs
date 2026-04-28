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
use bsmcp_common::settings::{hash_token_id, GlobalSettings, UserSettings};

use crate::remember::naming::NamedResource;
use crate::remember::provision;

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

/// Public predicate for other handlers (e.g., /status) that want to honour the
/// settings session cookie as a valid auth method without exposing credentials.
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
    let (token_id, token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&code_challenge=&code_challenge_method=&return_to=/settings").into_response(),
    };

    let token_id_hash = hash_token_id(&token_id);
    let settings = match state.db.get_user_settings(&token_id_hash).await {
        Ok(Some(s)) => s,
        Ok(None) => UserSettings::default(),
        Err(e) => {
            eprintln!("Settings: load failed: {e}");
            UserSettings::default()
        }
    };
    let globals = state.db.get_global_settings().await.unwrap_or_default();

    let client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let (shelves_res, books_res, admin_res) = tokio::join!(
        client.list_shelves(500, 0),
        client.list_books(500, 0),
        client.is_admin(),
    );

    let shelves = extract_named_list(shelves_res.ok().as_ref());
    let books = extract_named_list(books_res.ok().as_ref());
    let is_admin = admin_res.unwrap_or(false);

    Html(render_settings_page(&settings, &globals, &shelves, &books, is_admin)).into_response()
}

#[derive(Deserialize)]
pub struct SettingsForm {
    pub label: Option<String>,
    pub role: Option<String>,
    pub ai_identity_ouid: Option<String>,
    pub ai_identity_book_id: Option<String>,
    pub ai_identity_page_id: Option<String>,
    pub ai_identity_name: Option<String>,
    pub ai_hive_shelf_id: Option<String>,
    pub ai_collage_book_id: Option<String>,
    pub ai_shared_collage_book_id: Option<String>,
    pub ai_hive_journal_book_id: Option<String>,
    pub user_id: Option<String>,
    pub bookstack_user_id: Option<String>,
    pub user_identity_page_id: Option<String>,
    pub user_identity_book_id: Option<String>,
    pub user_journal_agent_page_id: Option<String>,
    pub user_journal_book_id: Option<String>,
    pub domains: Option<String>,
    pub semantic_against_journal: Option<String>,
    pub semantic_against_collage: Option<String>,
    pub semantic_against_shared_collage: Option<String>,
    pub semantic_against_user_journal: Option<String>,
    pub semantic_against_full_kb: Option<String>,
    pub use_follow_up_remember_agent: Option<String>,
    pub recent_journal_count: Option<String>,
    pub active_collage_count: Option<String>,
    pub system_prompt_page_ids: Option<String>,
    pub timezone: Option<String>,

    // Global shelves
    pub hive_shelf_id: Option<String>,
    pub user_journals_shelf_id: Option<String>,

    // Auto-create checkboxes (presence + "on" means "create if missing").
    pub create_hive_shelf: Option<String>,
    pub create_user_journals_shelf: Option<String>,
    pub create_ai_identity_book: Option<String>,
    pub create_ai_hive_journal_book: Option<String>,
    pub create_ai_collage_book: Option<String>,
    pub create_ai_shared_collage_book: Option<String>,
    pub create_user_journal_book: Option<String>,

    // Org-default AI identity (admins only).
    pub default_ai_identity_page_id: Option<String>,
    pub default_ai_identity_name: Option<String>,
    pub default_ai_identity_ouid: Option<String>,

    // Org identity + domains (admins only). org_identity_page_id is
    // first-write-wins like the shelves; org_domains is tunable.
    pub org_identity_page_id: Option<String>,
    pub org_domains: Option<String>,
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

/// Parse a free-form list of domains / strings. Splits on commas and
/// whitespace, trims, drops empties, deduplicates.
fn parse_str_list(s: Option<String>) -> Vec<String> {
    let Some(raw) = empty_to_none(s) else { return Vec::new(); };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tok in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let v = tok.trim().to_lowercase();
        if v.is_empty() {
            continue;
        }
        if seen.insert(v.clone()) {
            out.push(v);
        }
    }
    out
}

fn format_str_list(values: &[String]) -> String {
    values.join(", ")
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
    let (token_id, token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&return_to=/settings").into_response(),
    };
    let token_id_hash = hash_token_id(&token_id);
    let client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let mut settings = UserSettings {
        label: empty_to_none(form.label),
        role: empty_to_none(form.role),
        ai_identity_ouid: empty_to_none(form.ai_identity_ouid),
        ai_identity_book_id: parse_id(form.ai_identity_book_id),
        ai_identity_page_id: parse_id(form.ai_identity_page_id),
        ai_identity_name: empty_to_none(form.ai_identity_name),
        ai_hive_shelf_id: parse_id(form.ai_hive_shelf_id),
        ai_collage_book_id: parse_id(form.ai_collage_book_id),
        ai_shared_collage_book_id: parse_id(form.ai_shared_collage_book_id),
        ai_hive_journal_book_id: parse_id(form.ai_hive_journal_book_id),
        user_id: empty_to_none(form.user_id.clone()),
        bookstack_user_id: parse_id(form.bookstack_user_id.clone()),
        user_identity_page_id: parse_id(form.user_identity_page_id),
        user_identity_book_id: parse_id(form.user_identity_book_id.clone()),
        user_journal_agent_page_id: parse_id(form.user_journal_agent_page_id.clone()),
        user_journal_book_id: parse_id(form.user_journal_book_id),
        domains: parse_str_list(form.domains.clone()),
        semantic_against_journal: checkbox_on(form.semantic_against_journal),
        semantic_against_collage: checkbox_on(form.semantic_against_collage),
        semantic_against_shared_collage: checkbox_on(form.semantic_against_shared_collage),
        semantic_against_user_journal: checkbox_on(form.semantic_against_user_journal),
        semantic_against_full_kb: checkbox_on(form.semantic_against_full_kb),
        use_follow_up_remember_agent: checkbox_on(form.use_follow_up_remember_agent),
        recent_journal_count: parse_count(form.recent_journal_count, 3),
        active_collage_count: parse_count(form.active_collage_count, 10),
        system_prompt_page_ids: parse_id_list(form.system_prompt_page_ids),
        timezone: empty_to_none(form.timezone.clone()),
        // Manual /settings submit counts as a fresh fetch — stamp now so the
        // briefing's refresh-due flag doesn't immediately fire afterwards.
        timezone_fetched_at: if empty_to_none(form.timezone).is_some() {
            Some(crate::remember::frontmatter::now_unix())
        } else {
            None
        },
        // Carry over the existing dismiss timestamp — saving via /settings
        // doesn't change the snooze. (The nudge auto-stops showing once
        // is_configured() is true.)
        settings_nudge_dismissed_until: state
            .db
            .get_user_settings(&token_id_hash)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.settings_nudge_dismissed_until),
    };

    // Globals: server-side first-write-wins, gated to BookStack admins.
    // Once a global field is set, it cannot be changed via this UI; once set
    // by a non-admin (shouldn't happen — UI hides the controls), subsequent
    // form submissions cannot rewrite them either.
    let existing_globals = state.db.get_global_settings().await.unwrap_or_default();
    let is_admin = client.is_admin().await.unwrap_or(false);

    let mut globals = existing_globals.clone();
    let mut global_warnings: Vec<String> = Vec::new();
    let proposed_hive = parse_id(form.hive_shelf_id);
    let proposed_user_journals = parse_id(form.user_journals_shelf_id);
    let proposed_default_id = parse_id(form.default_ai_identity_page_id);
    let proposed_default_name = empty_to_none(form.default_ai_identity_name);
    let proposed_default_ouid = empty_to_none(form.default_ai_identity_ouid);

    if existing_globals.hive_shelf_id.is_none() {
        if let Some(new_id) = proposed_hive {
            if is_admin {
                globals.hive_shelf_id = Some(new_id);
            } else {
                global_warnings.push(
                    "Ignoring hive_shelf_id submission — only BookStack admins can set global shelves.".into()
                );
            }
        }
    } else if proposed_hive.is_some() && proposed_hive != existing_globals.hive_shelf_id {
        global_warnings.push(
            "Ignoring hive_shelf_id change — global shelves are first-write-wins; current value preserved.".into()
        );
    }

    if existing_globals.user_journals_shelf_id.is_none() {
        if let Some(new_id) = proposed_user_journals {
            if is_admin {
                globals.user_journals_shelf_id = Some(new_id);
            } else {
                global_warnings.push(
                    "Ignoring user_journals_shelf_id submission — only BookStack admins can set global shelves.".into()
                );
            }
        }
    } else if proposed_user_journals.is_some() && proposed_user_journals != existing_globals.user_journals_shelf_id {
        global_warnings.push(
            "Ignoring user_journals_shelf_id change — global shelves are first-write-wins; current value preserved.".into()
        );
    }

    // Org-default identity fields — admin-only writes, but they're updatable
    // (not first-write-wins) so admins can swap the house agent later.
    if is_admin {
        globals.default_ai_identity_page_id = proposed_default_id;
        globals.default_ai_identity_name = proposed_default_name;
        globals.default_ai_identity_ouid = proposed_default_ouid;
    } else if proposed_default_id.is_some() || proposed_default_name.is_some() || proposed_default_ouid.is_some() {
        global_warnings.push(
            "Ignoring org-default identity submission — only BookStack admins can set the org default.".into()
        );
    }

    // Org identity page — first-write-wins (the page itself is structural;
    // an admin can change its content but the page ID stays put).
    let proposed_org_identity_page = parse_id(form.org_identity_page_id);
    if existing_globals.org_identity_page_id.is_none() {
        if let Some(new_id) = proposed_org_identity_page {
            if is_admin {
                globals.org_identity_page_id = Some(new_id);
            } else {
                global_warnings.push(
                    "Ignoring org_identity_page_id submission — only BookStack admins can set org identity.".into()
                );
            }
        }
    } else if proposed_org_identity_page.is_some()
        && proposed_org_identity_page != existing_globals.org_identity_page_id
    {
        global_warnings.push(
            "Ignoring org_identity_page_id change — first-write-wins; current value preserved.".into()
        );
    }

    // Org domains — admin-editable, replaces the existing list when provided.
    let proposed_org_domains = parse_str_list(form.org_domains);
    if is_admin {
        // Empty list means "user cleared the textarea" — honor the clear.
        // Non-empty replaces the old list outright.
        globals.org_domains = proposed_org_domains;
    } else if !proposed_org_domains.is_empty() {
        global_warnings.push(
            "Ignoring org_domains submission — only BookStack admins can edit org domains.".into()
        );
    }

    // Same admin gate for create-if-missing on the global shelves themselves.
    if !is_admin {
        if checkbox_on(form.create_hive_shelf.clone()) || checkbox_on(form.create_user_journals_shelf.clone()) {
            global_warnings.push(
                "Ignoring create-if-missing for global shelves — only BookStack admins can provision them.".into()
            );
        }
    }

    // Auto-provisioning in dependency order. Each step writes back into
    // `settings` / `globals` so later steps can use the just-created IDs.
    // After each successful create we lock the new content to admin-only edit
    // (everyone else read-only; owner keeps default edit). admin_role_id is
    // looked up lazily so we only pay the cost when something is being created.
    let mut provision_log: Vec<String> = Vec::new();
    let mut admin_role_id: Option<i64> = None;
    async fn ensure_admin_role(
        cached: &mut Option<i64>,
        client: &BookStackClient,
    ) -> Option<i64> {
        if cached.is_some() {
            return *cached;
        }
        match client.find_admin_role_id().await {
            Ok(id) => { *cached = Some(id); Some(id) }
            Err(e) => {
                eprintln!("Settings: admin role lookup failed (locking will be skipped): {e}");
                None
            }
        }
    }
    use bsmcp_common::bookstack::ContentType;

    if is_admin && globals.hive_shelf_id.is_none() && checkbox_on(form.create_hive_shelf) {
        let r = provision::create_shelf(&client, NamedResource::HiveShelf).await;
        provision_log.push(r.human(NamedResource::HiveShelf));
        if let Some(id) = r.id() {
            globals.hive_shelf_id = Some(id);
            if let Some(role) = ensure_admin_role(&mut admin_role_id, &client).await {
                provision::lock_to_admin_only(&client, ContentType::Shelf, id, role).await;
            }
        }
    }
    if is_admin && globals.user_journals_shelf_id.is_none() && checkbox_on(form.create_user_journals_shelf) {
        let r = provision::create_shelf(&client, NamedResource::UserJournalsShelf).await;
        provision_log.push(r.human(NamedResource::UserJournalsShelf));
        if let Some(id) = r.id() {
            globals.user_journals_shelf_id = Some(id);
            if let Some(role) = ensure_admin_role(&mut admin_role_id, &client).await {
                provision::lock_to_admin_only(&client, ContentType::Shelf, id, role).await;
            }
        }
    }

    // Books that live on the Hive shelf.
    let hive_shelf = globals.hive_shelf_id;
    if settings.ai_identity_book_id.is_none() && checkbox_on(form.create_ai_identity_book) {
        let r = provision::create_book(&client, state.index_db.as_ref(), NamedResource::IdentityBook, hive_shelf).await;
        provision_log.push(r.human(NamedResource::IdentityBook));
        if let Some(id) = r.id() {
            settings.ai_identity_book_id = Some(id);
            if let Some(role) = ensure_admin_role(&mut admin_role_id, &client).await {
                provision::lock_to_admin_only(&client, ContentType::Book, id, role).await;
            }
        }
    }
    if settings.ai_hive_journal_book_id.is_none() && checkbox_on(form.create_ai_hive_journal_book) {
        let r = provision::create_book(&client, state.index_db.as_ref(), NamedResource::JournalBook, hive_shelf).await;
        provision_log.push(r.human(NamedResource::JournalBook));
        if let Some(id) = r.id() {
            settings.ai_hive_journal_book_id = Some(id);
        }
    }
    if settings.ai_collage_book_id.is_none() && checkbox_on(form.create_ai_collage_book) {
        let r = provision::create_book(&client, state.index_db.as_ref(), NamedResource::CollageBook, hive_shelf).await;
        provision_log.push(r.human(NamedResource::CollageBook));
        if let Some(id) = r.id() {
            settings.ai_collage_book_id = Some(id);
            if let Some(role) = ensure_admin_role(&mut admin_role_id, &client).await {
                provision::lock_to_admin_only(&client, ContentType::Book, id, role).await;
            }
        }
    }
    if settings.ai_shared_collage_book_id.is_none() && checkbox_on(form.create_ai_shared_collage_book) {
        let r = provision::create_book(&client, state.index_db.as_ref(), NamedResource::SharedCollageBook, hive_shelf).await;
        provision_log.push(r.human(NamedResource::SharedCollageBook));
        if let Some(id) = r.id() {
            settings.ai_shared_collage_book_id = Some(id);
            if let Some(role) = ensure_admin_role(&mut admin_role_id, &client).await {
                provision::lock_to_admin_only(&client, ContentType::Book, id, role).await;
            }
        }
    }

    // User journal book on the User Journals shelf — name personalized by user_id.
    if settings.user_journal_book_id.is_none() && checkbox_on(form.create_user_journal_book) {
        let user_label = settings.user_id.clone().unwrap_or_else(|| "User".to_string());
        let book_name = format!("{user_label} Journal");
        let book_desc = format!("Journal for {user_label}. Auto-created by /remember.");
        match client.create_book(&book_name, &book_desc).await {
            Ok(book) => {
                if let Some(book_id) = book.get("id").and_then(|i| i.as_i64()) {
                    settings.user_journal_book_id = Some(book_id);
                    provision_log.push(format!("Created user journal book \"{book_name}\" (id={book_id})"));
                    if let Some(shelf_id) = globals.user_journals_shelf_id {
                        if let Ok(shelf) = client.get_shelf(shelf_id).await {
                            let mut existing: Vec<i64> = shelf
                                .get("books")
                                .and_then(|v| v.as_array())
                                .map(|arr| arr.iter().filter_map(|b| b.get("id").and_then(|i| i.as_i64())).collect())
                                .unwrap_or_default();
                            if !existing.contains(&book_id) { existing.push(book_id); }
                            let _ = client.update_shelf(shelf_id, &serde_json::json!({ "books": existing })).await;
                        }
                    }
                }
            }
            Err(e) => provision_log.push(format!("Failed to create user journal book: {e}")),
        }
    }

    // Mirror the global hive_shelf_id into the per-user setting so existing
    // briefing code paths that read `settings.ai_hive_shelf_id` still work.
    if let Some(id) = globals.hive_shelf_id {
        settings.ai_hive_shelf_id = Some(id);
    }

    // Lock journal books to owner-only — applies to both freshly auto-created
    // books and previously-existing books the user selected. Idempotent.
    provision::lock_journal_books_to_owner(
        &client,
        settings.ai_hive_journal_book_id,
        settings.user_journal_book_id,
    )
    .await;

    // Persist.
    if let Err(e) = state.db.save_global_settings(&globals, &token_id_hash).await {
        eprintln!("Settings: save_global_settings failed: {e}");
    }
    if let Err(e) = state.db.save_user_settings(&token_id_hash, &settings).await {
        eprintln!("Settings: save_user_settings failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to save settings").into_response();
    }

    if !provision_log.is_empty() {
        eprintln!("Settings: auto-provisioned items:");
        for line in &provision_log { eprintln!("  - {line}"); }
    }
    if !global_warnings.is_empty() {
        eprintln!("Settings: global-write warnings:");
        for line in &global_warnings { eprintln!("  - {line}"); }
    }
    eprintln!("Settings: saved for user (token_id_hash={}…, admin={is_admin})", &token_id_hash[..16.min(token_id_hash.len())]);
    Redirect::to("/settings?saved=1").into_response()
}

// --- Probe endpoint ---

pub async fn handle_settings_probe_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let (token_id, token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
        Some(c) => c,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&return_to=/settings").into_response(),
    };
    let globals = state.db.get_global_settings().await.unwrap_or_default();
    let hive_shelf_id = match globals.hive_shelf_id {
        Some(id) => id,
        None => {
            return Html(probe_no_shelf_page()).into_response();
        }
    };
    let client = BookStackClient::new(&state.bookstack_url, &token_id, &token_secret, state.http_client.clone());
    let matches = match probe_hive(&client, hive_shelf_id).await {
        Ok(m) => m,
        Err(e) => return Html(probe_error_page(&e)).into_response(),
    };
    Html(render_probe_page(&matches, hive_shelf_id)).into_response()
}

pub async fn handle_settings_probe_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ProbeAcceptForm>,
) -> Response {
    let (token_id, token_secret) = match resolve_session(&headers, &state.settings_sessions).await {
        Some(c) => c,
        None => return Redirect::to("/authorize?response_type=code&client_id=settings-ui&redirect_uri=/settings&return_to=/settings").into_response(),
    };
    let token_id_hash = hash_token_id(&token_id);
    let client = BookStackClient::new(&state.bookstack_url, &token_id, &token_secret, state.http_client.clone());
    let mut settings = state.db.get_user_settings(&token_id_hash).await.ok().flatten().unwrap_or_default();

    let assign = |checked: Option<String>, id: Option<String>| -> Option<i64> {
        if checkbox_on(checked) { parse_id(id) } else { None }
    };
    if let Some(v) = assign(form.accept_ai_identity_book_id, form.ai_identity_book_id) { settings.ai_identity_book_id = Some(v); }
    if let Some(v) = assign(form.accept_ai_identity_page_id, form.ai_identity_page_id) { settings.ai_identity_page_id = Some(v); }
    if let Some(v) = assign(form.accept_ai_hive_journal_book_id, form.ai_hive_journal_book_id) { settings.ai_hive_journal_book_id = Some(v); }
    if let Some(v) = assign(form.accept_ai_collage_book_id, form.ai_collage_book_id) { settings.ai_collage_book_id = Some(v); }
    if let Some(v) = assign(form.accept_ai_shared_collage_book_id, form.ai_shared_collage_book_id) { settings.ai_shared_collage_book_id = Some(v); }

    provision::lock_journal_books_to_owner(
        &client,
        settings.ai_hive_journal_book_id,
        settings.user_journal_book_id,
    )
    .await;

    if let Err(e) = state.db.save_user_settings(&token_id_hash, &settings).await {
        eprintln!("Probe: save failed: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to save settings").into_response();
    }
    Redirect::to("/settings?saved=1").into_response()
}

#[derive(Deserialize)]
pub struct ProbeAcceptForm {
    pub accept_ai_identity_book_id: Option<String>,
    pub ai_identity_book_id: Option<String>,
    pub accept_ai_identity_page_id: Option<String>,
    pub ai_identity_page_id: Option<String>,
    pub accept_ai_hive_journal_book_id: Option<String>,
    pub ai_hive_journal_book_id: Option<String>,
    pub accept_ai_collage_book_id: Option<String>,
    pub ai_collage_book_id: Option<String>,
    pub accept_ai_shared_collage_book_id: Option<String>,
    pub ai_shared_collage_book_id: Option<String>,
}

#[derive(Default)]
struct ProbeMatches {
    identity_book: Option<NamedItem>,
    identity_page: Option<NamedItem>,  // page inside the Identity book
    journal_book: Option<NamedItem>,
    collage_book: Option<NamedItem>,
    shared_collage_book: Option<NamedItem>,
}

async fn probe_hive(client: &BookStackClient, hive_shelf_id: i64) -> Result<ProbeMatches, String> {
    let mut out = ProbeMatches::default();

    // Books on the Hive shelf
    let shelf = client.get_shelf(hive_shelf_id).await?;
    let books_on_shelf: Vec<NamedItem> = shelf
        .get("books")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|b| {
            let id = b.get("id").and_then(|i| i.as_i64())?;
            let name = b.get("name").and_then(|n| n.as_str())?.to_string();
            Some(NamedItem { id, name })
        }).collect())
        .unwrap_or_default();

    out.identity_book = books_on_shelf.iter().find(|b| NamedResource::IdentityBook.matches(&b.name)).cloned();
    out.journal_book = books_on_shelf.iter().find(|b| NamedResource::JournalBook.matches(&b.name)).cloned();
    out.collage_book = books_on_shelf.iter().find(|b| NamedResource::CollageBook.matches(&b.name)).cloned();
    out.shared_collage_book = books_on_shelf.iter().find(|b| NamedResource::SharedCollageBook.matches(&b.name)).cloned();

    // Identity manifest page inside the Identity book.
    // Goes through `list_book_pages_by_updated` (which uses `get_book`)
    // rather than `search` — search silently returns system-wide results
    // when the query has no positive keyword term, which would surface
    // pages from outside the identity book.
    if let Some(ref ib) = out.identity_book {
        if let Ok(pages) = client.list_book_pages_by_updated(ib.id, usize::MAX).await {
            out.identity_page = pages.iter().find_map(|p| {
                let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if NamedResource::IdentityPage.matches(name) {
                    let id = p.get("id").and_then(|i| i.as_i64())?;
                    Some(NamedItem { id, name: name.to_string() })
                } else {
                    None
                }
            });
        }
    }

    Ok(out)
}

fn probe_no_shelf_page() -> String {
    r##"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Probe — no Hive shelf</title>
<style>body{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#e0e0e0;padding:2rem;}a{color:#3498db;}</style></head>
<body><h1>No Hive shelf set</h1><p>The global Hive shelf isn't configured yet. Set it on the <a href="/settings">/settings</a> page first, then come back.</p></body></html>"##.to_string()
}

fn probe_error_page(err: &str) -> String {
    format!(r##"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Probe error</title>
<style>body{{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#e0e0e0;padding:2rem;}}.err{{background:#3d1f1f;padding:1rem;border-radius:6px;}}</style></head>
<body><h1>Probe failed</h1><div class="err">{}</div><p><a href="/settings" style="color:#3498db;">Back to settings</a></p></body></html>"##, html_escape(err))
}

fn render_probe_page(m: &ProbeMatches, hive_shelf_id: i64) -> String {
    fn row(field: &str, label: &str, hit: &Option<NamedItem>) -> String {
        match hit {
            Some(item) => format!(
                r#"<tr><td><label><input type="checkbox" name="accept_{field}" checked> {label}</label></td><td><code>{}</code></td><td>id={}<input type="hidden" name="{field}" value="{}"></td></tr>"#,
                html_escape(&item.name), item.id, item.id,
            ),
            None => format!(
                r#"<tr><td>{label}</td><td colspan="2" style="color:#64748b;">no match</td></tr>"#
            ),
        }
    }
    format!(r##"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Probe results</title>
<style>
*{{margin:0;padding:0;box-sizing:border-box;}}
body{{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#e0e0e0;padding:2rem;}}
.container{{max-width:720px;margin:0 auto;}}
h1{{font-size:1.4rem;margin-bottom:.3rem;color:#fff;}}
.subtitle{{color:#888;font-size:.9rem;margin-bottom:1.5rem;}}
.card{{background:#16213e;border-radius:12px;padding:1.5rem;margin-bottom:1rem;}}
table{{width:100%;border-collapse:collapse;}}
td{{padding:.4rem .5rem;border-bottom:1px solid #2a3a5c;font-size:.9rem;}}
button{{background:#2980b9;color:#fff;border:none;padding:.7rem 1.4rem;border-radius:6px;cursor:pointer;}}
button:hover{{background:#3498db;}}
a{{color:#3498db;}}
code{{background:#0f1a30;padding:.1rem .3rem;border-radius:3px;}}
</style></head>
<body><div class="container">
<h1>Probe results</h1>
<p class="subtitle">Scanned the Hive shelf (id={hive_shelf_id}) for known resources by name. Check the boxes for matches you want to assign to your settings, then save.</p>
<form method="POST" action="/settings/probe">
<div class="card">
<table>
{r1}{r2}{r3}{r4}{r5}
</table>
</div>
<button type="submit">Apply selected</button>
&nbsp;&nbsp;<a href="/settings">Back to settings</a>
</form>
</div></body></html>"##,
        hive_shelf_id = hive_shelf_id,
        r1 = row("ai_identity_book_id", "Identity book", &m.identity_book),
        r2 = row("ai_identity_page_id", "Identity manifest page", &m.identity_page),
        r3 = row("ai_hive_journal_book_id", "Journal book", &m.journal_book),
        r4 = row("ai_collage_book_id", "Topics / Collage book", &m.collage_book),
        r5 = row("ai_shared_collage_book_id", "Shared collage book", &m.shared_collage_book),
    )
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

fn render_textarea(name: &str, value: &str, placeholder: &str, rows: usize) -> String {
    format!(
        r#"<textarea name="{name}" id="{name}" rows="{rows}" placeholder="{ph}" style="width:100%;padding:0.55rem 0.7rem;border:1px solid #2a3a5c;border-radius:6px;background:#0f1a30;color:#e0e0e0;font-size:0.9rem;font-family:inherit;resize:vertical;">{value}</textarea>"#,
        name = html_escape(name),
        rows = rows,
        ph = html_escape(placeholder),
        value = html_escape(value),
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
    g: &GlobalSettings,
    shelves: &[NamedItem],
    books: &[NamedItem],
    is_admin: bool,
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
<p class="subtitle">Configure where your AI agent's memory lives in this BookStack. Empty fields disable that part of the <code>remember</code> response — they don't break it. <a href="/settings/probe" style="color:#3498db;">Probe existing Hive shelf</a> to auto-detect IDs from page names.</p>
{saved_banner}
<form method="POST" action="/settings">

<div class="card">
<h2>Global shelves <span style="font-weight:400;font-size:.78rem;color:#94a3b8;">{global_lock_note}</span></h2>
<p class="subtitle" style="margin-bottom:.75rem;">Shared by every user on this BookStack. First-write-wins for the UI; once set, the dropdown is locked here.</p>
<div class="row2">
<div class="field">
  <label for="hive_shelf_id">Hive shelf</label>
  {hive_shelf_select}
  <div class="hint">Contains every AI agent's Identity book.</div>
  {hive_shelf_create}
</div>
<div class="field">
  <label for="user_journals_shelf_id">User Journals shelf</label>
  {user_journals_shelf_select}
  <div class="hint">Contains every human user's journal book.</div>
  {user_journals_shelf_create}
</div>
</div>
</div>

<div class="card">
<h2>Org identity &amp; domains <span style="font-weight:400;font-size:.78rem;color:#94a3b8;">{org_identity_note}</span></h2>
<p class="subtitle" style="margin-bottom:.75rem;">Page describing the organization itself + the domains it owns. Pulled into every briefing's <code>system_prompt_additions</code> so every agent on the instance has a shared baseline. Page ID is first-write-wins; domains list is editable.</p>
<div class="field">
  <label for="org_identity_page_id">Org identity page ID</label>
  {org_identity_page_input}
  <div class="hint">Single page describing the org (mission, structure, conventions). First-write-wins.</div>
</div>
<div class="field">
  <label for="org_domains">Org-owned domains</label>
  {org_domains_input}
  <div class="hint">One per line, or comma-separated. Merged with each user's <code>domains</code> in the briefing.</div>
</div>
</div>

<div class="card">
<h2>Org-default AI identity <span style="font-weight:400;font-size:.78rem;color:#94a3b8;">{org_default_note}</span></h2>
<p class="subtitle" style="margin-bottom:.75rem;">When a user hasn't set their own AI identity, the briefing falls back to this. The "house agent". Admin-editable; can be changed later.</p>
<div class="field">
  <label for="default_ai_identity_page_id">Default identity manifest page ID</label>
  {default_id_input}
  <div class="hint">The page that defines the fallback agent. Find the ID in BookStack's URL.</div>
</div>
<div class="row2">
<div class="field">
  <label for="default_ai_identity_name">Default name</label>
  {default_name_input}
</div>
<div class="field">
  <label for="default_ai_identity_ouid">Default OUID</label>
  {default_ouid_input}
</div>
</div>
</div>

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
  <div class="hint">Container for the manifest page.</div>
</div>
</div>
<div class="field">
  <label for="ai_identity_page_id">Identity manifest page ID</label>
  {ai_page_input}
  <div class="hint">The page (inside the Identity book) that defines who the AI is.</div>
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
  <label for="ai_collage_book_id">Topics / Collage book</label>
  {ai_collage_select}
</div>
</div>
<div class="field">
  <label for="ai_shared_collage_book_id">Shared collage book (optional)</label>
  {ai_shared_collage_select}
  <div class="hint">Cross-agent shared topics.</div>
</div>
</div>

<div class="card">
<h2>Your Identity</h2>
<div class="row2">
<div class="field">
  <label for="user_id">User ID</label>
  {user_id_input}
  <div class="hint">e.g., your email. Echoed back in the response and used as the prefix for auto-provisioned per-user resource names.</div>
</div>
<div class="field">
  <label for="bookstack_user_id">BookStack user row ID</label>
  {bookstack_user_id_input}
  <div class="hint">Numeric row ID for your BookStack user. Required for ACL-filtered semantic search — without it, search falls back to per-page HTTP permission checks (slower). Find via <code>/api/users</code> if you're an admin, or ask one to look it up.</div>
</div>
</div>
<div class="row2">
<div class="field">
  <label for="user_identity_page_id">Your identity page ID</label>
  {user_page_input}
  <div class="hint">Auto-provisioned on first <code>remember_user action=read</code> once <code>user_id</code> is set.</div>
</div>
<div class="field">
  <label for="user_identity_book_id">Your identity book ID</label>
  {user_identity_book_id_input}
  <div class="hint">Container book holding your identity page + per-user agent definitions. Auto-provisioned.</div>
</div>
</div>
<div class="row2">
<div class="field">
  <label for="user_journal_book_id">Your journal book</label>
  {user_journal_select}
  <div class="hint">Auto-provisioned and force-attached to the User Journals shelf on every write.</div>
</div>
<div class="field">
  <label for="user_journal_agent_page_id">Your journal-agent page ID</label>
  {user_journal_agent_page_id_input}
  <div class="hint">Auto-provisioned page (Agent: {{user_id}}-journal-agent) — fetched into the local agent cache by the bootstrap protocol.</div>
</div>
</div>
<div class="field">
  <label for="domains">Your owned domains</label>
  {domains_input}
  <div class="hint">One per line, or comma-separated. Surfaced in every briefing's <code>system_prompt_additions</code> so the AI can distinguish ours vs external content (URLs, emails). E.g.: <code>example.com</code></div>
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
<div class="field" style="margin-top: 1rem;">
  <label for="timezone">Timezone</label>
  <input type="text" name="timezone" id="timezone" value="{timezone_input}" placeholder="America/New_York">
  <div class="hint">IANA timezone name. Surfaced in the briefing's <code>time</code> block so the AI can format timestamps in your local time. Leave blank for UTC. <a href="https://en.wikipedia.org/wiki/List_of_tz_database_time_zones" target="_blank">List of tz names</a>.</div>
</div>
</div>

<div class="card">
<h2>Auto-create missing structure</h2>
<p class="subtitle" style="margin-bottom:.75rem;">For each blank field above, check the box to have the server create the book or chapter on save (with sensible default name and description). Permission denials surface as a warning — they don't block the rest of the save.</p>
{create_ai_identity_book_cb}
{create_ai_hive_journal_book_cb}
{create_ai_collage_book_cb}
{create_ai_shared_collage_book_cb}
{create_user_journal_book_cb}
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
        ai_journal_select = render_select("ai_hive_journal_book_id", books, s.ai_hive_journal_book_id, true),
        ai_collage_select = render_select("ai_collage_book_id", books, s.ai_collage_book_id, true),
        ai_shared_collage_select = render_select("ai_shared_collage_book_id", books, s.ai_shared_collage_book_id, true),
        ai_identity_book_select = render_select("ai_identity_book_id", books, s.ai_identity_book_id, true),
        user_id_input = render_text("user_id", s.user_id.as_deref(), "you@example.com"),
        bookstack_user_id_input = render_id_input("bookstack_user_id", s.bookstack_user_id),
        user_page_input = render_id_input("user_identity_page_id", s.user_identity_page_id),
        user_identity_book_id_input = render_id_input("user_identity_book_id", s.user_identity_book_id),
        user_journal_agent_page_id_input = render_id_input("user_journal_agent_page_id", s.user_journal_agent_page_id),
        user_journal_select = render_select("user_journal_book_id", books, s.user_journal_book_id, true),
        domains_input = render_textarea(
            "domains",
            &format_str_list(&s.domains),
            "example.com\nexample.net",
            3,
        ),
        cb_journal = render_checkbox("semantic_against_journal", s.semantic_against_journal, "AI journal"),
        cb_collage = render_checkbox("semantic_against_collage", s.semantic_against_collage, "Topics / collage"),
        cb_shared_collage = render_checkbox("semantic_against_shared_collage", s.semantic_against_shared_collage, "Shared collage"),
        cb_user_journal = render_checkbox("semantic_against_user_journal", s.semantic_against_user_journal, "User journal"),
        cb_full_kb = render_checkbox("semantic_against_full_kb", s.semantic_against_full_kb, "Full KB (off: scope vector search to the configured books above; on: search the entire KB and surface out-of-scope hits in kb_semantic_matches)"),
        cb_followup = render_checkbox("use_follow_up_remember_agent", s.use_follow_up_remember_agent, "Run a follow-up reconstitution agent after the structured pull"),
        recent_count = s.recent_journal_count,
        collage_count = s.active_collage_count,
        system_prompt_ids = html_escape(&format_id_list(&s.system_prompt_page_ids)),
        timezone_input = html_escape(s.timezone.as_deref().unwrap_or("")),

        global_lock_note = match (g.updated_at > 0, is_admin) {
            (true, _) => "(set globally — locked)",
            (false, true) => "(unset — you are an admin and may set this once)",
            (false, false) => "(unset — only a BookStack admin can configure this)",
        },
        // Lock the field if globals are already set OR the user isn't an admin.
        hive_shelf_select = render_select_locked("hive_shelf_id", shelves, g.hive_shelf_id, g.hive_shelf_id.is_some() || !is_admin),
        user_journals_shelf_select = render_select_locked("user_journals_shelf_id", shelves, g.user_journals_shelf_id, g.user_journals_shelf_id.is_some() || !is_admin),
        // Only show the create checkboxes for admins setting unset globals.
        hive_shelf_create = if g.hive_shelf_id.is_none() && is_admin { create_inline("create_hive_shelf", "Create \"Hive\" shelf if missing") } else { String::new() },
        user_journals_shelf_create = if g.user_journals_shelf_id.is_none() && is_admin { create_inline("create_user_journals_shelf", "Create \"User Journals\" shelf if missing") } else { String::new() },

        org_identity_note = match (g.org_identity_page_id.is_some(), is_admin) {
            (true, true) => "(set globally; page ID is locked, domains list editable)",
            (false, true) => "(unset — you are an admin and may set this)",
            (true, false) => "(set globally — read-only)",
            (false, false) => "(unset — only a BookStack admin can configure)",
        },
        org_identity_page_input = if is_admin && g.org_identity_page_id.is_none() {
            render_id_input("org_identity_page_id", g.org_identity_page_id)
        } else {
            render_locked_value(g.org_identity_page_id.map(|v| v.to_string()))
        },
        org_domains_input = if is_admin {
            render_textarea(
                "org_domains",
                &format_str_list(&g.org_domains),
                "example.com\nexample.net",
                3,
            )
        } else {
            render_locked_value(if g.org_domains.is_empty() {
                None
            } else {
                Some(format_str_list(&g.org_domains))
            })
        },

        org_default_note = if is_admin { "(admin-editable)" } else { "(read-only — admin only)" },
        default_id_input = if is_admin {
            render_id_input("default_ai_identity_page_id", g.default_ai_identity_page_id)
        } else {
            render_locked_value(g.default_ai_identity_page_id.map(|v| v.to_string()))
        },
        default_name_input = if is_admin {
            render_text("default_ai_identity_name", g.default_ai_identity_name.as_deref(), "Pia")
        } else {
            render_locked_value(g.default_ai_identity_name.clone())
        },
        default_ouid_input = if is_admin {
            render_text("default_ai_identity_ouid", g.default_ai_identity_ouid.as_deref(), "019dc66e4dd87ea080ebf5d5e2985d91")
        } else {
            render_locked_value(g.default_ai_identity_ouid.clone())
        },

        create_ai_identity_book_cb = create_row("create_ai_identity_book", "Create Identity book under the Hive shelf if blank above", s.ai_identity_book_id.is_some()),
        create_ai_hive_journal_book_cb = create_row("create_ai_hive_journal_book", "Create Journal book under the Hive shelf if blank above", s.ai_hive_journal_book_id.is_some()),
        create_ai_collage_book_cb = create_row("create_ai_collage_book", "Create Topics / Collage book under the Hive shelf if blank above", s.ai_collage_book_id.is_some()),
        create_ai_shared_collage_book_cb = create_row("create_ai_shared_collage_book", "Create Shared Topics book under the Hive shelf if blank above", s.ai_shared_collage_book_id.is_some()),
        create_user_journal_book_cb = create_row("create_user_journal_book", "Create your personal journal book under the User Journals shelf if blank above", s.user_journal_book_id.is_some()),
    )
}

fn create_row(field_name: &str, label: &str, already_set: bool) -> String {
    if already_set {
        format!(r#"<div class="cb" style="color:#64748b;">✓ {label} (already configured)</div>"#, label = html_escape(label))
    } else {
        format!(
            r#"<label class="cb"><input type="checkbox" name="{name}" value="on"> {label}</label>"#,
            name = html_escape(field_name),
            label = html_escape(label),
        )
    }
}

fn create_inline(field_name: &str, label: &str) -> String {
    format!(
        r#"<label class="cb" style="margin-top:.3rem;"><input type="checkbox" name="{name}" value="on"> {label}</label>"#,
        name = html_escape(field_name),
        label = html_escape(label),
    )
}

fn render_locked_value(value: Option<String>) -> String {
    let display = value.unwrap_or_else(|| "(unset)".into());
    format!(
        r#"<div style="padding:.55rem .7rem;border:1px solid #2a3a5c;border-radius:6px;background:#0a0f1f;color:#94a3b8;font-size:.9rem;">{}</div>"#,
        html_escape(&display),
    )
}

fn render_select_locked(name: &str, items: &[NamedItem], current: Option<i64>, locked: bool) -> String {
    if locked {
        let current_name = current
            .and_then(|id| items.iter().find(|i| i.id == id))
            .map(|i| i.name.clone())
            .unwrap_or_else(|| String::from("(unknown)"));
        format!(
            r#"<input type="hidden" name="{name}" value="{value}"><div style="padding:.55rem .7rem;border:1px solid #2a3a5c;border-radius:6px;background:#0a0f1f;color:#94a3b8;font-size:.9rem;">{disp} <code style="font-size:.75rem;">id={id}</code></div>"#,
            name = html_escape(name),
            value = current.map(|v| v.to_string()).unwrap_or_default(),
            disp = html_escape(&current_name),
            id = current.map(|v| v.to_string()).unwrap_or_default(),
        )
    } else {
        render_select(name, items, current, true)
    }
}
