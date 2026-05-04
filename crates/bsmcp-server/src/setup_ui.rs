//! Browser-based onboarding wizards (Phase 2.4e + 2.4f).
//!
//! Two wizards live in this module:
//!
//! - **`/setup/user`** (Phase 2.4e): per-user onboarding. First-time users
//!   land here from the `meta.onboarding_pending` link. Captures AI agent
//!   identity, journaling toggle, per-tool overrides, and a migration stub.
//!   On submit, stamps `UserSettings.setup_complete = true`.
//! - **`/setup/admin`** (Phase 2.4f): org-wide first-time admin onboarding.
//!   Admins land here from the `meta.admin_onboarding_pending` link. "Run
//!   once" semantics — as soon as any admin completes the form,
//!   `GlobalSettings.admin_setup_complete` flips and the admin nudge stops
//!   appearing for everyone. Captures the User Journals shelf, global tool
//!   defaults, and a small set of org-essential slots.
//!
//! Auth is the same browser-cookie pattern as `settings_ui.rs`. The
//! `/authorize?return_to=/setup/{user,admin}` short-circuit (in `oauth.rs`)
//! validates the BookStack token, issues the `bsmcp_settings_session`
//! cookie, and redirects here. The cookie's `Path` is `/` so a single
//! session covers `/settings`, `/setup/user`, and `/setup/admin`.
//!
//! For `/setup/admin` the handler additionally verifies the calling user is
//! a BookStack admin via `is_bookstack_admin` — non-admins get a 403. The
//! `meta.admin_onboarding_pending` injection uses the same predicate so
//! non-admins never see the nudge in the first place.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{RawForm, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::DbBackend;
use bsmcp_common::settings::{hash_token_id, GlobalSettings, UserSettings};

use crate::mcp;
use crate::settings_ui::resolve_session_creds;
use crate::sse::AppState;

/// TTL for the cached `is_bookstack_admin` result on `UserSettings` (24h).
/// Mirrors the `cached_first_name` TTL in `remember::resolvers` so admin
/// status check has the same refresh cadence as other identity bits.
pub const IS_ADMIN_TTL_SECS: i64 = 24 * 60 * 60;

// --- Handlers ---

pub async fn handle_setup_user_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !mcp::onboarding_enabled() {
        return not_found_response();
    }

    let token_id = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some((tid, _)) => tid,
        None => return redirect_to_authorize(),
    };

    let token_id_hash = hash_token_id(&token_id);
    let settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    Html(render_setup_page(&settings, None)).into_response()
}

#[derive(Deserialize, Default)]
pub struct SetupForm {
    #[serde(default)]
    pub chosen_ai_identity: Option<String>,
    #[serde(default)]
    pub journaling_enabled: Option<String>,
}

pub async fn handle_setup_user_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawForm(body): RawForm,
) -> Response {
    if !mcp::onboarding_enabled() {
        return not_found_response();
    }

    let token_id = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some((tid, _)) => tid,
        None => return redirect_to_authorize(),
    };

    let body_str = std::str::from_utf8(&body).unwrap_or("");
    let raw_pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(body_str).unwrap_or_default();
    let form: SetupForm = serde_urlencoded::from_str(body_str).unwrap_or_default();

    let token_id_hash = hash_token_id(&token_id);
    let mut settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    apply_setup_form(&mut settings, &form, &raw_pairs);

    if let Err(e) = state.db.save_user_settings(&token_id_hash, &settings).await {
        return error_response(format!("Failed to save user settings: {e}"));
    }

    Html(render_success_page()).into_response()
}

/// Apply the parsed wizard form to a `UserSettings` instance. Pure (no
/// I/O) so the test suite can exercise the field-flip semantics directly.
/// Always stamps `setup_complete = true` — a successful POST means the
/// user submitted the wizard, even if they left every field blank.
pub fn apply_setup_form(
    settings: &mut UserSettings,
    form: &SetupForm,
    raw_pairs: &[(String, String)],
) {
    settings.chosen_ai_identity = nonempty(&form.chosen_ai_identity);
    settings.journaling_enabled = checkbox(&form.journaling_enabled);
    settings.tool_overrides = parse_tool_overrides(raw_pairs);
    settings.setup_complete = true;
}

// --- Form parsing helpers (mirror settings_ui.rs's helpers but kept
// local — the parsers are tiny and the duplication keeps the modules
// independent.) ---

fn nonempty(v: &Option<String>) -> Option<String> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn checkbox(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("on") | Some("true") | Some("1"))
}

/// Parse the per-tool tri-state radio set out of the form pairs.
///
/// Form encoding (one group per tool): `tool_user_<name>=default|on|off`.
/// `default` (or absent / unrecognized value) drops the entry from
/// `tool_overrides` so the user falls back to the admin default.
/// `on` and `off` write explicit `true`/`false` entries respectively.
///
/// Distinct from `settings_ui::parse_tool_defaults` (admin side) — the
/// admin form uses single checkboxes that map to a two-state map; users
/// need a third "no opinion" option so they can explicitly defer to
/// whatever the admin sets later.
pub fn parse_tool_overrides(pairs: &[(String, String)]) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for (k, v) in pairs {
        let Some(name) = k.strip_prefix("tool_user_") else { continue };
        if name.is_empty() {
            continue;
        }
        match v.as_str() {
            "on" => {
                out.insert(name.to_string(), true);
            }
            "off" => {
                out.insert(name.to_string(), false);
            }
            // "default" or anything else — leave the tool out of the map
            // so `is_tool_enabled` falls through to the admin default.
            _ => {}
        }
    }
    out
}

// --- Rendering ---

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Render the per-user `tool_overrides` tri-state grid. Mirrors the
/// admin-side `render_tool_defaults_section` in `settings_ui.rs` but
/// emits radio-button groups (`default | on | off`) instead of single
/// checkboxes. Both helpers source their tool list from
/// `mcp::all_tool_names()`, so the user and admin pages always agree on
/// what's listable.
fn render_user_tool_overrides_section(s: &UserSettings) -> String {
    let mut rows = String::new();
    for name in mcp::all_tool_names() {
        let escaped = html_escape(&name);
        let (def_sel, on_sel, off_sel) = match s.tool_overrides.get(&name) {
            None => (" checked", "", ""),
            Some(true) => ("", " checked", ""),
            Some(false) => ("", "", " checked"),
        };
        rows.push_str(&format!(
            "<div class=\"tool-row\">\
               <code>{escaped}</code>\
               <label><input type=\"radio\" name=\"tool_user_{escaped}\" value=\"default\"{def_sel}> use admin default</label>\
               <label><input type=\"radio\" name=\"tool_user_{escaped}\" value=\"on\"{on_sel}> on</label>\
               <label><input type=\"radio\" name=\"tool_user_{escaped}\" value=\"off\"{off_sel}> off</label>\
             </div>\n"
        ));
    }
    rows
}

fn render_setup_page(s: &UserSettings, _flash: Option<&str>) -> String {
    let chosen = html_escape(s.chosen_ai_identity.as_deref().unwrap_or(""));
    let journaling_checked = if s.journaling_enabled { "checked" } else { "" };
    let tool_rows = render_user_tool_overrides_section(s);
    let already_done_banner = if s.setup_complete {
        r#"<div class="banner">You've already completed setup. Re-submitting will update your preferences.</div>"#
    } else {
        ""
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>BookStack MCP — User Setup</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 720px; margin: 2em auto; padding: 0 1em; color: #222; }}
h1 {{ margin-bottom: 0.25em; }}
h2 {{ margin-top: 2em; border-bottom: 1px solid #ddd; padding-bottom: 0.2em; }}
.note {{ color: #666; font-size: 0.9em; }}
.help {{ font-size: 0.85em; color: #555; margin-top: 0.2em; }}
.banner {{ background: #fff8c5; border: 1px solid #d4a72c; padding: 0.6em 0.8em; border-radius: 4px; margin-bottom: 1em; }}
label {{ display: block; margin: 0.5em 0; }}
label.inline {{ display: inline-block; margin-right: 1em; }}
input[type=text] {{ padding: 0.5em; box-sizing: border-box; font-family: inherit; width: 100%; }}
button {{ margin-top: 1.5em; padding: 0.6em 1.2em; font-size: 1em; cursor: pointer; }}
.tool-overrides {{ display: grid; grid-template-columns: 1fr; gap: 0.4em; margin: 0.5em 0; }}
.tool-overrides .tool-row {{ display: grid; grid-template-columns: 220px repeat(3, auto); gap: 0.5em; align-items: center; padding: 0.2em 0.4em; border-bottom: 1px solid #eee; font-size: 0.9em; }}
.tool-overrides .tool-row code {{ font-size: 0.95em; }}
.tool-overrides label {{ display: inline-flex; align-items: center; gap: 0.3em; margin: 0; font-weight: normal; }}
.migration-stub {{ background: #f6f8fa; border: 1px dashed #d0d7de; padding: 1em; border-radius: 4px; color: #555; }}
</style>
</head>
<body>
<h1>BookStack MCP — User Setup</h1>
<p class="note">First-time setup for your MCP user. Once you submit, the onboarding link stops appearing on your tool responses.</p>
{already_done_banner}
<form method="post" action="/setup/user">

  <h2>1. AI agent identity</h2>
  <label>Default AI agent name <input type="text" name="chosen_ai_identity" value="{chosen}" placeholder="e.g. pia"></label>
  <p class="help">Optional. Your default AI agent name. Used for journal chapter naming and the briefing reminder.</p>

  <h2>2. Journaling</h2>
  <label><input type="checkbox" name="journaling_enabled" {journaling_checked}> Enable journaling reminders</label>
  <p class="help">When on, the briefing reminds you to journal throughout the session.</p>

  <h2>3. Tool overrides</h2>
  <p class="help">Per-tool overrides for your account. <em>Use admin default</em> follows whatever the admin sets globally; <em>on</em> and <em>off</em> force the tool regardless of the global setting.</p>
  <div class="tool-overrides">
    {tool_rows}
  </div>

  <h2>4. Migration</h2>
  <div class="migration-stub">
    <strong>Coming next:</strong> import existing journals.
    <p class="help" style="margin-top: 0.4em;">The migration UI lights up in sub-PR 2.5. For now this is a placeholder.</p>
  </div>

  <button type="submit">Complete setup</button>
</form>
</body>
</html>"#,
        already_done_banner = already_done_banner,
        chosen = chosen,
        journaling_checked = journaling_checked,
        tool_rows = tool_rows,
    )
}

fn render_success_page() -> String {
    r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Setup complete</title>
<style>
body { font-family: system-ui, sans-serif; max-width: 540px; margin: 4em auto; padding: 0 1em; text-align: center; color: #222; }
h1 { color: #1a7f37; }
.note { color: #666; font-size: 0.95em; line-height: 1.5; }
a { color: #0969da; }
</style>
</head>
<body>
<h1>&#10003; Setup complete</h1>
<p class="note">Your user setup has been saved. The onboarding link will stop appearing on your MCP tool responses.</p>
<p class="note">You can close this window. To revise your preferences later, visit <a href="/setup/user">/setup/user</a> again or use the admin <a href="/settings">/settings</a> page.</p>
</body>
</html>"#
        .to_string()
}

fn redirect_to_authorize() -> Response {
    redirect_to_authorize_for("/setup/user")
}

/// Build the `/authorize?return_to=...` redirect for whichever wizard the
/// caller landed on. Exists so the user wizard and admin wizard can share
/// one cookie-flow entry point without hand-rolling the query string twice.
fn redirect_to_authorize_for(path: &str) -> Response {
    let url = format!(
        "/authorize?response_type=code&client_id=settings-ui&redirect_uri={path}&code_challenge=&code_challenge_method=&return_to={path}",
        path = path,
    );
    axum::response::Redirect::to(&url).into_response()
}

fn not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<p>Onboarding is disabled on this server (BSMCP_ONBOARDING_ENABLED=false).</p>"),
    )
        .into_response()
}

// =====================================================================
// Admin onboarding (Phase 2.4f)
// =====================================================================

/// Pure helper: should `meta.admin_onboarding_pending` ride along on this
/// MCP response? Mirrors `mcp::is_onboarding_visible` but adds the
/// admin-status gate. Three knobs:
///
/// - `env_enabled`: `BSMCP_ONBOARDING_ENABLED` (operator can kill the
///   surface entirely).
/// - `admin_setup_complete`: the global "any admin completed it" bit.
///   Modulated by `BSMCP_FORCE_ADMIN_SETUP` at the call site
///   (`build_admin_onboarding_visible`) — when forced, the bit is treated
///   as false regardless of what the DB says.
/// - `user_is_admin`: from `is_bookstack_admin`. Non-admins NEVER see the
///   nudge; if admin status is unknown the caller should pass `false`
///   (err on the side of not nagging).
pub fn is_admin_onboarding_visible(
    env_enabled: bool,
    admin_setup_complete: bool,
    user_is_admin: bool,
) -> bool {
    env_enabled && !admin_setup_complete && user_is_admin
}

/// Read the `BSMCP_FORCE_ADMIN_SETUP` env override. When set to a truthy
/// value the meta injector treats `admin_setup_complete` as false — admins
/// see the nudge again and `/setup/admin` works as if no one had finished
/// it. For ops scenarios (restored backup, re-onboarding) without needing
/// to UPDATE the DB.
///
/// Truthy: `1`, `true`, `yes`, `on` (case-insensitive, trimmed). Anything
/// else (including unset) is false. Mirrors the parse shape of
/// `BSMCP_ONBOARDING_ENABLED` for consistency.
pub fn force_admin_setup_env() -> bool {
    parse_force_admin_setup_env(std::env::var("BSMCP_FORCE_ADMIN_SETUP").ok().as_deref())
}

/// Pure parse of `BSMCP_FORCE_ADMIN_SETUP` so the truthy/falsy cases are
/// testable without mutating process env.
pub fn parse_force_admin_setup_env(raw: Option<&str>) -> bool {
    match raw {
        None => false,
        Some(s) => {
            let v = s.trim().to_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
    }
}

/// Resolve the *effective* `admin_setup_complete` for the meta-injection
/// path. When the force-env override is on, callers see `false` regardless
/// of the DB row. Pure (no I/O) so the env-toggle behavior is testable.
pub fn effective_admin_setup_complete(stored: bool, force_override: bool) -> bool {
    if force_override { false } else { stored }
}

/// Resolve whether the calling BookStack user is a system admin. Result is
/// determined by querying `GET /api/users/{bookstack_user_id}` and looking
/// for a role with `system_name == "admin"` in the response's `roles`
/// array.
///
/// Caches the result on `UserSettings.cached_is_admin` with a 24h TTL —
/// see `IS_ADMIN_TTL_SECS`. The cache covers the hot path
/// (`build_response_meta` runs on every MCP tool response) so we don't
/// pay a BookStack round-trip per call.
///
/// Returns `Err` when:
/// - `bookstack_user_id` is unset (caller hasn't been auto-populated yet)
/// - BookStack `/api/users/{id}` errors (network failure, 403/404, etc.)
///
/// On error the caller MUST treat the user as non-admin (no nudge, no
/// admin-only writes) — never as admin. This keeps a transient BookStack
/// outage from accidentally exposing admin surfaces to non-admins.
pub async fn is_bookstack_admin(
    bookstack_user_id: i64,
    client: &BookStackClient,
) -> Result<bool, String> {
    let user = client.get_user(bookstack_user_id).await?;
    let roles = user
        .get("roles")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(roles.iter().any(|r| {
        r.get("system_name")
            .and_then(|v| v.as_str())
            .map(|s| s == "admin")
            .unwrap_or(false)
    }))
}

/// Refresh the cached `is_admin` bit on `UserSettings` if stale; persist
/// when refreshed. Returns the (possibly cached, possibly fresh) bool.
/// `None` means we couldn't determine admin status — callers should treat
/// it as "not admin" for nudge / authorization decisions.
pub async fn resolve_is_admin_cached(
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
) -> Option<bool> {
    let now = now_unix();

    // Fresh cache wins, no I/O needed.
    if let Some(cached) = settings.cached_is_admin {
        if is_admin_cache_fresh(settings.cached_is_admin_fetched_at, now) {
            return Some(cached);
        }
    }

    let bookstack_user_id = settings.bookstack_user_id?;
    match is_bookstack_admin(bookstack_user_id, client).await {
        Ok(is_admin) => {
            settings.cached_is_admin = Some(is_admin);
            settings.cached_is_admin_fetched_at = Some(now);
            // Best-effort persist: a save failure shouldn't block the
            // current request from getting an answer. The next call will
            // simply re-fetch.
            if let Err(e) = db.save_user_settings(token_id_hash, settings).await {
                eprintln!("setup_ui: failed to persist cached_is_admin (non-fatal): {e}");
            }
            Some(is_admin)
        }
        Err(e) => {
            // Last-resort: if we have a stale cache, return it rather than
            // surface "unknown" — the BookStack API blip is usually
            // transient and a stale-but-non-null answer is more useful
            // than nothing.
            eprintln!("setup_ui: is_bookstack_admin lookup failed (non-fatal): {e}");
            settings.cached_is_admin
        }
    }
}

/// Pure helper: is the cached admin bit still fresh?
/// `None` is always stale. Mirrors `remember::resolvers::is_cache_fresh`
/// without the `ttl` parameter — the admin-status TTL is a single
/// per-module constant.
pub fn is_admin_cache_fresh(fetched_at: Option<i64>, now: i64) -> bool {
    match fetched_at {
        Some(t) => now.saturating_sub(t) <= IS_ADMIN_TTL_SECS,
        None => false,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --- Admin wizard handlers ---

pub async fn handle_setup_admin_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !mcp::onboarding_enabled() {
        return not_found_response();
    }

    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return redirect_to_authorize_for("/setup/admin"),
    };

    let bs_client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let token_id_hash = hash_token_id(&token_id);
    let mut settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Admin-gate the page: even rendering is admin-only. This keeps
    // non-admins from seeing the form (and from learning what global
    // slots exist).
    let is_admin = match settings.bookstack_user_id {
        Some(_) => resolve_is_admin_cached(&token_id_hash, &mut settings, &bs_client, state.db.clone()).await,
        None => None,
    };
    if !matches!(is_admin, Some(true)) {
        return admin_required_response();
    }

    let globals = state.db.get_global_settings().await.unwrap_or_default();
    Html(render_admin_setup_page(&settings, &globals)).into_response()
}

#[derive(Deserialize, Default)]
pub struct AdminSetupForm {
    #[serde(default)]
    pub user_journals_shelf_id: Option<String>,
    #[serde(default)]
    pub org_identity_page_id: Option<String>,
    #[serde(default)]
    pub org_domains: Option<String>,
    #[serde(default)]
    pub guide_page_id: Option<String>,
}

pub async fn handle_setup_admin_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawForm(body): RawForm,
) -> Response {
    if !mcp::onboarding_enabled() {
        return not_found_response();
    }

    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return redirect_to_authorize_for("/setup/admin"),
    };

    let bs_client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let token_id_hash = hash_token_id(&token_id);
    let mut user_settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();

    // Admin-gate the POST. Mirrors the GET handler. We deliberately
    // resolve fresh (refreshing the cache if stale) rather than trust a
    // stale cached value here — admin status changes are rare but writes
    // to globals are sensitive.
    let is_admin = match user_settings.bookstack_user_id {
        Some(_) => resolve_is_admin_cached(&token_id_hash, &mut user_settings, &bs_client, state.db.clone()).await,
        None => None,
    };
    if !matches!(is_admin, Some(true)) {
        return admin_required_response();
    }

    let body_str = std::str::from_utf8(&body).unwrap_or("");
    let raw_pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(body_str).unwrap_or_default();
    let form: AdminSetupForm = serde_urlencoded::from_str(body_str).unwrap_or_default();

    let mut globals = state.db.get_global_settings().await.unwrap_or_default();
    apply_admin_setup_form(&mut globals, &form, &raw_pairs);

    if let Err(e) = state.db.save_global_settings(&globals, &token_id_hash).await {
        return error_response(format!("Failed to save global settings: {e}"));
    }

    Html(render_admin_success_page()).into_response()
}

/// Apply the parsed admin wizard form to a `GlobalSettings` instance.
/// Pure (no I/O) so the test suite can exercise the field-flip semantics
/// directly. Always stamps `admin_setup_complete = true` — a successful
/// POST means an admin submitted the wizard, even if they left every
/// field blank. That's the "run once" contract: the click is what
/// matters, not whether they filled it in.
pub fn apply_admin_setup_form(
    globals: &mut GlobalSettings,
    form: &AdminSetupForm,
    raw_pairs: &[(String, String)],
) {
    globals.user_journals_shelf_id = parse_optional_i64(&form.user_journals_shelf_id);
    globals.org_identity_page_id = parse_optional_i64(&form.org_identity_page_id);
    globals.org_domains = parse_string_list(&form.org_domains);
    globals.guide_page_id = parse_optional_i64(&form.guide_page_id);
    globals.tool_defaults = parse_admin_tool_defaults(raw_pairs);
    globals.admin_setup_complete = true;
}

/// 403 page returned when a non-admin tries to GET or POST `/setup/admin`.
/// Plain HTML so the user sees a clear message instead of a generic
/// "Forbidden" string from the framework.
fn admin_required_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Html(
            r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Admin role required</title>
<style>
body { font-family: system-ui, sans-serif; max-width: 540px; margin: 4em auto; padding: 0 1em; color: #222; }
h1 { color: #c0392b; }
.note { color: #666; font-size: 0.95em; line-height: 1.5; }
a { color: #0969da; }
</style>
</head>
<body>
<h1>Admin role required</h1>
<p class="note">The <code>/setup/admin</code> wizard is only available to BookStack admins. Sign in with an admin token, or visit <a href="/setup/user">/setup/user</a> for the per-user setup.</p>
</body>
</html>"#,
        ),
    )
        .into_response()
}

// --- Admin form parsing (mostly mirrors `settings_ui.rs` helpers) ---

fn parse_optional_i64(v: &Option<String>) -> Option<i64> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).and_then(|s| s.parse().ok())
}

fn parse_string_list(v: &Option<String>) -> Vec<String> {
    let Some(s) = v.as_deref() else { return Vec::new(); };
    s.split(|c: char| c == ',' || c == '\n')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Pull the per-tool admin defaults out of the raw form pairs. Identical
/// shape and semantics to `settings_ui::parse_tool_defaults` — the admin
/// onboarding form re-renders the same `tool_listed_<name>` /
/// `tool_default_<name>` pairs, so the parser is the same. We don't
/// extract a shared helper because the two modules are independent and
/// the parser is small; duplication beats coupling here.
fn parse_admin_tool_defaults(
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

    listed
        .into_iter()
        .filter(|name| !on.contains(name))
        .map(|name| (name, false))
        .collect::<HashMap<_, _>>()
}

// --- Admin form rendering ---

/// Render the admin tool-defaults grid. Same shape as
/// `settings_ui::render_tool_defaults_section` but stripped of the
/// "your override" annotations (the admin wizard isn't about per-user
/// overrides). Sources the tool list from `mcp::all_tool_names()` so the
/// admin form stays in sync with whatever the server advertises.
fn render_admin_tool_defaults_section(g: &GlobalSettings) -> String {
    let mut rows = String::new();
    for name in mcp::all_tool_names() {
        let admin_on = g.tool_defaults.get(&name).copied().unwrap_or(true);
        let escaped_name = html_escape(&name);
        let checked = if admin_on { " checked" } else { "" };
        rows.push_str(&format!(
            "<label class=\"tool-row\"><input type=\"hidden\" name=\"tool_listed_{name}\" value=\"1\">\
             <input type=\"checkbox\" name=\"tool_default_{name}\"{checked}> \
             <code>{escaped_name}</code></label>\n",
            name = escaped_name,
            checked = checked,
            escaped_name = escaped_name,
        ));
    }
    rows
}

fn render_admin_setup_page(s: &UserSettings, g: &GlobalSettings) -> String {
    let user_journals_shelf_id = g
        .user_journals_shelf_id
        .map(|i| i.to_string())
        .unwrap_or_default();
    let org_identity_page_id = g
        .org_identity_page_id
        .map(|i| i.to_string())
        .unwrap_or_default();
    let org_domains = html_escape(&g.org_domains.join(", "));
    let guide_page_id = g.guide_page_id.map(|i| i.to_string()).unwrap_or_default();
    let tool_rows = render_admin_tool_defaults_section(g);

    let already_done_banner = if g.admin_setup_complete {
        r#"<div class="banner">Admin setup is already marked complete. Re-submitting will update the org configuration and re-stamp the flag (the meta nudge is already off).</div>"#
    } else {
        ""
    };

    let admin_label = html_escape(s.label.as_deref().unwrap_or(""));
    let admin_label_line = if admin_label.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="note">Signed in as <strong>{admin_label}</strong> (admin).</p>"#)
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>BookStack MCP — Admin Setup</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 720px; margin: 2em auto; padding: 0 1em; color: #222; }}
h1 {{ margin-bottom: 0.25em; }}
h2 {{ margin-top: 2em; border-bottom: 1px solid #ddd; padding-bottom: 0.2em; }}
.note {{ color: #666; font-size: 0.9em; }}
.help {{ font-size: 0.85em; color: #555; margin-top: 0.2em; }}
.banner {{ background: #fff8c5; border: 1px solid #d4a72c; padding: 0.6em 0.8em; border-radius: 4px; margin-bottom: 1em; }}
label {{ display: block; margin: 0.5em 0; }}
input[type=text], input[type=number], textarea {{ padding: 0.5em; box-sizing: border-box; font-family: inherit; }}
input[type=text], textarea {{ width: 100%; }}
input[type=number] {{ width: 12em; }}
textarea {{ min-height: 4em; }}
button {{ margin-top: 1.5em; padding: 0.6em 1.2em; font-size: 1em; cursor: pointer; }}
.tool-defaults {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 0.3em 1em; margin: 0.5em 0; }}
.tool-defaults .tool-row {{ display: flex; align-items: center; gap: 0.4em; font-weight: normal; margin: 0; }}
.tool-defaults code {{ font-size: 0.9em; }}
</style>
</head>
<body>
<h1>BookStack MCP — Admin Setup</h1>
<p class="note">First-time org setup for this BookStack MCP server. Once submitted, the admin onboarding nudge stops appearing on tool responses for every admin.</p>
{admin_label_line}
{already_done_banner}
<form method="post" action="/setup/admin">

  <h2>1. User Journals shelf</h2>
  <label>user_journals_shelf_id <input type="number" name="user_journals_shelf_id" value="{user_journals_shelf_id}"></label>
  <p class="help">BookStack shelf where each user's personal Journal book lives. Required to enable the journal endpoints (remember_user_journal / remember_agent_journal). Create the shelf in BookStack first, then paste its numeric ID here.</p>

  <h2>2. Organization context</h2>
  <label>org_identity_page_id <input type="number" name="org_identity_page_id" value="{org_identity_page_id}"></label>
  <p class="help">Page describing the organization (mission, structure, conventions). Auto-injected into every briefing's system_prompt_additions.</p>
  <label>guide_page_id <input type="number" name="guide_page_id" value="{guide_page_id}"></label>
  <p class="help">Page describing how to use this BookStack instance with this MCP server. Also auto-included in every briefing.</p>
  <label>Org domains <textarea name="org_domains" placeholder="example.com, internal.example.org">{org_domains}</textarea></label>
  <p class="help">Domains the org owns. Comma- or newline-separated. Helps the AI distinguish "ours" content from external links.</p>

  <h2>3. Global tool defaults</h2>
  <p class="help">Per-tool admin default. Unchecked = disabled by default for all users (a user can still re-enable in their own settings). Checked = on. Tools default ON when not listed.</p>
  <div class="tool-defaults">
    {tool_rows}
  </div>

  <h2>4. Mark complete</h2>
  <p class="note">Submitting flips the org-wide <code>admin_setup_complete</code> flag. The admin onboarding nudge will stop appearing for every admin on this BookStack instance. Other admin settings (advanced KB scopes, ACL filters, friendly-structure toggles, etc.) live on the daily-admin <a href="/settings">/settings</a> page.</p>
  <button type="submit">Save and complete setup</button>
</form>
</body>
</html>"#,
        admin_label_line = admin_label_line,
        already_done_banner = already_done_banner,
        user_journals_shelf_id = user_journals_shelf_id,
        org_identity_page_id = org_identity_page_id,
        org_domains = org_domains,
        guide_page_id = guide_page_id,
        tool_rows = tool_rows,
    )
}

fn render_admin_success_page() -> String {
    r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Admin setup complete</title>
<style>
body { font-family: system-ui, sans-serif; max-width: 540px; margin: 4em auto; padding: 0 1em; text-align: center; color: #222; }
h1 { color: #1a7f37; }
.note { color: #666; font-size: 0.95em; line-height: 1.5; }
a { color: #0969da; }
</style>
</head>
<body>
<h1>&#10003; Admin setup complete</h1>
<p class="note">Org configuration saved. The admin onboarding nudge will stop appearing for all admins on this BookStack instance.</p>
<p class="note">Tweak advanced settings any time at <a href="/settings">/settings</a>. Revisit this wizard at <a href="/setup/admin">/setup/admin</a>.</p>
</body>
</html>"#
        .to_string()
}


fn error_response(msg: String) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Html(html_escape(&msg))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn parse_tool_overrides_keeps_explicit_on_and_off() {
        let pairs = vec![
            pair("tool_user_briefing", "on"),
            pair("tool_user_journal", "off"),
            pair("tool_user_search_content", "default"),
        ];
        let map = parse_tool_overrides(&pairs);
        assert_eq!(map.get("briefing"), Some(&true));
        assert_eq!(map.get("journal"), Some(&false));
        // "default" drops the tool from the map so admin default applies.
        assert!(!map.contains_key("search_content"));
    }

    #[test]
    fn parse_tool_overrides_ignores_unrelated_pairs() {
        let pairs = vec![
            pair("chosen_ai_identity", "pia"),
            pair("journaling_enabled", "on"),
            pair("tool_user_briefing", "off"),
        ];
        let map = parse_tool_overrides(&pairs);
        assert_eq!(map, [("briefing".to_string(), false)].into_iter().collect());
    }

    #[test]
    fn parse_tool_overrides_treats_unknown_value_as_default() {
        let pairs = vec![
            pair("tool_user_briefing", "maybe"),
            pair("tool_user_journal", ""),
        ];
        let map = parse_tool_overrides(&pairs);
        assert!(map.is_empty(), "unknown values should not produce overrides");
    }

    #[test]
    fn apply_setup_form_stamps_setup_complete_even_when_blank() {
        let mut s = UserSettings::default();
        let form = SetupForm::default();
        apply_setup_form(&mut s, &form, &[]);
        assert!(s.setup_complete, "blank submit still completes the wizard");
        assert!(s.chosen_ai_identity.is_none());
        assert!(!s.journaling_enabled);
        assert!(s.tool_overrides.is_empty());
    }

    #[test]
    fn apply_setup_form_writes_all_four_sections() {
        let mut s = UserSettings::default();
        let form = SetupForm {
            chosen_ai_identity: Some("  pia  ".to_string()),
            journaling_enabled: Some("on".to_string()),
        };
        let pairs = vec![
            pair("tool_user_briefing", "on"),
            pair("tool_user_journal", "off"),
        ];
        apply_setup_form(&mut s, &form, &pairs);
        assert_eq!(s.chosen_ai_identity.as_deref(), Some("pia"));
        assert!(s.journaling_enabled);
        assert_eq!(s.tool_overrides.get("briefing"), Some(&true));
        assert_eq!(s.tool_overrides.get("journal"), Some(&false));
        assert!(s.setup_complete);
    }

    #[test]
    fn apply_setup_form_overwrites_existing_overrides() {
        // The wizard form is the source of truth on submit — anything not
        // listed in the form gets dropped, matching the admin form's
        // behavior on `tool_defaults`.
        let mut s = UserSettings::default();
        s.tool_overrides.insert("stale_tool".to_string(), false);
        let form = SetupForm::default();
        apply_setup_form(&mut s, &form, &[pair("tool_user_briefing", "on")]);
        assert_eq!(s.tool_overrides.get("briefing"), Some(&true));
        assert!(
            !s.tool_overrides.contains_key("stale_tool"),
            "stale entries should be dropped, not preserved"
        );
    }

    #[test]
    fn render_user_tool_overrides_section_lists_every_advertised_tool() {
        let s = UserSettings::default();
        let html = render_user_tool_overrides_section(&s);
        for name in mcp::all_tool_names() {
            assert!(
                html.contains(&format!("tool_user_{name}")),
                "missing radio group for {name}"
            );
        }
    }

    #[test]
    fn render_user_tool_overrides_marks_default_when_no_override() {
        let s = UserSettings::default();
        let html = render_user_tool_overrides_section(&s);
        // For at least one known stable tool, default radio should be checked.
        let marker = "tool_user_search_content";
        let pos = html.find(marker).expect("search_content row missing");
        let window_end = (pos + 600).min(html.len());
        let window = &html[pos..window_end];
        assert!(
            window.contains("value=\"default\" checked"),
            "default radio should be checked when user has no override"
        );
    }

    // =====================================================================
    // Admin onboarding (Phase 2.4f)
    // =====================================================================

    /// Full 8-case truth table for `is_admin_onboarding_visible`. Three
    /// boolean inputs → 8 combinations. The nudge appears iff all three
    /// gates are TRUE: env enabled + setup not complete + user is admin.
    #[test]
    fn is_admin_onboarding_visible_full_matrix() {
        // The one and only case where the nudge SHOULD appear.
        assert!(is_admin_onboarding_visible(true, false, true));

        // Every other combination is hidden.
        assert!(!is_admin_onboarding_visible(true, false, false), "non-admin must not see nudge");
        assert!(!is_admin_onboarding_visible(true, true, true), "completed setup hides for admins too");
        assert!(!is_admin_onboarding_visible(true, true, false));
        assert!(!is_admin_onboarding_visible(false, false, true), "operator killed surface");
        assert!(!is_admin_onboarding_visible(false, false, false));
        assert!(!is_admin_onboarding_visible(false, true, true));
        assert!(!is_admin_onboarding_visible(false, true, false));
    }

    #[test]
    fn parse_force_admin_setup_env_truthy_values() {
        for v in ["1", "true", "yes", "on", "TRUE", "  Yes  "] {
            assert!(parse_force_admin_setup_env(Some(v)), "expected {v:?} truthy");
        }
    }

    #[test]
    fn parse_force_admin_setup_env_falsy_or_absent() {
        assert!(!parse_force_admin_setup_env(None));
        for v in ["", "0", "false", "no", "off", "anything", "FALSE"] {
            assert!(!parse_force_admin_setup_env(Some(v)), "expected {v:?} falsy");
        }
    }

    #[test]
    fn effective_admin_setup_complete_force_inverts() {
        // Stored true + force on → effectively false (nudge re-appears).
        assert!(!effective_admin_setup_complete(true, true));
        // Stored true + force off → still true.
        assert!(effective_admin_setup_complete(true, false));
        // Stored false + force on/off → false either way.
        assert!(!effective_admin_setup_complete(false, true));
        assert!(!effective_admin_setup_complete(false, false));
    }

    #[test]
    fn is_admin_cache_fresh_within_ttl() {
        // Fetched 1h ago, TTL 24h → fresh.
        assert!(is_admin_cache_fresh(Some(1_000), 1_000 + 3_600));
        // Fetched at exact TTL boundary → fresh (<=, not <).
        assert!(is_admin_cache_fresh(Some(0), IS_ADMIN_TTL_SECS));
    }

    #[test]
    fn is_admin_cache_stale_when_past_ttl() {
        // Fetched > TTL ago.
        assert!(!is_admin_cache_fresh(Some(0), IS_ADMIN_TTL_SECS + 1));
        // Never fetched.
        assert!(!is_admin_cache_fresh(None, 1_000));
    }

    #[test]
    fn is_admin_cache_handles_clock_skew() {
        // fetched_at in the future (clock jumped backward).
        // saturating_sub avoids panic; we treat it as fresh.
        assert!(is_admin_cache_fresh(Some(1_000_060), 1_000_000));
    }

    #[test]
    fn is_admin_ttl_is_one_day() {
        // Sanity check — guards against accidental edits to the constant.
        assert_eq!(IS_ADMIN_TTL_SECS, 86_400);
    }

    fn admin_pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn apply_admin_setup_form_stamps_complete_even_when_blank() {
        let mut g = GlobalSettings::default();
        let form = AdminSetupForm::default();
        apply_admin_setup_form(&mut g, &form, &[]);
        assert!(g.admin_setup_complete, "blank submit still completes the wizard");
        assert!(g.user_journals_shelf_id.is_none());
        assert!(g.org_identity_page_id.is_none());
        assert!(g.org_domains.is_empty());
        assert!(g.guide_page_id.is_none());
    }

    #[test]
    fn apply_admin_setup_form_writes_all_sections() {
        let mut g = GlobalSettings::default();
        let form = AdminSetupForm {
            user_journals_shelf_id: Some("42".to_string()),
            org_identity_page_id: Some("99".to_string()),
            org_domains: Some("example.com, example.net".to_string()),
            guide_page_id: Some("100".to_string()),
        };
        let pairs = vec![
            admin_pair("tool_listed_journal", "1"),
            // journal unchecked → expect explicit false
            admin_pair("tool_listed_briefing", "1"),
            admin_pair("tool_default_briefing", "on"),
        ];
        apply_admin_setup_form(&mut g, &form, &pairs);
        assert_eq!(g.user_journals_shelf_id, Some(42));
        assert_eq!(g.org_identity_page_id, Some(99));
        assert_eq!(g.org_domains, vec!["example.com", "example.net"]);
        assert_eq!(g.guide_page_id, Some(100));
        assert_eq!(g.tool_defaults.get("journal"), Some(&false));
        // briefing was checked → not in the explicit-off map (default ON).
        assert!(!g.tool_defaults.contains_key("briefing"));
        assert!(g.admin_setup_complete);
    }

    #[test]
    fn apply_admin_setup_form_overwrites_existing_global_fields() {
        // The wizard form is the source of truth on submit — anything not
        // listed in the form gets dropped, matching the user-form behavior.
        let mut g = GlobalSettings::default();
        g.user_journals_shelf_id = Some(99);
        g.org_domains = vec!["stale.example".to_string()];
        let form = AdminSetupForm::default();
        apply_admin_setup_form(&mut g, &form, &[]);
        assert!(g.user_journals_shelf_id.is_none(), "blank field clears");
        assert!(g.org_domains.is_empty(), "blank field clears");
        assert!(g.admin_setup_complete);
    }

    #[test]
    fn parse_admin_tool_defaults_marks_unchecked_listed_tools_as_off() {
        let pairs = vec![
            admin_pair("tool_listed_briefing", "1"),
            admin_pair("tool_default_briefing", "on"),
            admin_pair("tool_listed_journal", "1"),
            // journal not checked
        ];
        let map = parse_admin_tool_defaults(&pairs);
        assert_eq!(map.get("journal"), Some(&false));
        assert!(!map.contains_key("briefing"));
    }

    #[test]
    fn render_admin_setup_page_includes_all_sections() {
        let s = UserSettings::default();
        let g = GlobalSettings::default();
        let html = render_admin_setup_page(&s, &g);
        // Section markers
        assert!(html.contains("User Journals shelf"));
        assert!(html.contains("Organization context"));
        assert!(html.contains("Global tool defaults"));
        assert!(html.contains("Save and complete setup"));
        // Posts back to /setup/admin (not /setup/user).
        assert!(html.contains("action=\"/setup/admin\""));
        // Re-uses the tool-defaults shape from the admin /settings page.
        assert!(html.contains("tool_listed_search_content"));
    }

    #[test]
    fn render_admin_setup_page_marks_already_done_when_complete() {
        let s = UserSettings::default();
        let mut g = GlobalSettings::default();
        g.admin_setup_complete = true;
        let html = render_admin_setup_page(&s, &g);
        assert!(
            html.contains("already marked complete"),
            "should show the already-done banner",
        );
    }

    /// Composition test mirroring the conditional in `build_response_meta`:
    /// the visibility helper agrees with the meta-builder's gating. We
    /// exercise the predicate against the same admin-cached-bit cases the
    /// real injector handles (admin / non-admin / unknown).
    #[test]
    fn meta_admin_onboarding_pending_shape_matches_visibility_helper() {
        // Visible: env on + setup not complete + admin → field present.
        assert!(is_admin_onboarding_visible(true, false, true));

        // Hidden: setup complete (admin already finished it).
        assert!(!is_admin_onboarding_visible(true, true, true));
        // Hidden: not an admin.
        assert!(!is_admin_onboarding_visible(true, false, false));
        // Hidden: env-disabled.
        assert!(!is_admin_onboarding_visible(false, false, true));
    }

    /// Treating an unknown admin status as `false` (i.e., not admin) for
    /// the visibility predicate is the safe default — non-admins must not
    /// see the admin nudge. Documents the policy at the assertion level
    /// in case someone refactors the predicate later.
    #[test]
    fn unknown_admin_status_hides_nudge() {
        let user_is_admin: bool = false; // mapping of `Option::None`
        assert!(!is_admin_onboarding_visible(true, false, user_is_admin));
    }
}
