//! Browser-based user-onboarding wizard (Phase 2.4e).
//!
//! First-time users land on `GET /setup/user` from the
//! `meta.onboarding_pending` link injected on every MCP response. The
//! wizard captures four things:
//!
//! 1. **AI agent identity** (`UserSettings.chosen_ai_identity`) — optional,
//!    used by the briefing's journaling reminder when no per-call agent
//!    name is supplied.
//! 2. **Journaling toggle** (`UserSettings.journaling_enabled`) — when on,
//!    the briefing reminds the AI to journal.
//! 3. **Per-tool overrides** (`UserSettings.tool_overrides`) — same map
//!    the admin settings page exposes for `tool_defaults`, but for the
//!    calling user. Tri-state per tool: `default` (absent from map),
//!    `on` (forced true), `off` (forced false).
//! 4. **Migration stub** — placeholder for the journal-import flow that
//!    lights up in sub-PR 2.5.
//!
//! On submit, the handler stamps `UserSettings.setup_complete = true` so
//! the `meta.onboarding_pending` injection in `mcp::build_response_meta`
//! stops appearing. There is no "un-complete" path from the UI; an admin
//! can flip the flag manually by editing the `user_settings` row.
//!
//! Auth is the same browser-cookie pattern as `settings_ui.rs`. The
//! `/authorize?return_to=/setup/user` short-circuit (in `oauth.rs`)
//! validates the user's BookStack token, issues the
//! `bsmcp_settings_session` cookie, and redirects here. The cookie's
//! `Path` was widened to `/` (from `/settings`) so a single session
//! covers both `/settings` and `/setup/user`.

use std::collections::HashMap;

use axum::extract::{RawForm, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use bsmcp_common::settings::{hash_token_id, UserSettings};

use crate::mcp;
use crate::settings_ui::resolve_session_creds;
use crate::sse::AppState;

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
    axum::response::Redirect::to(
        "/authorize?response_type=code&client_id=settings-ui&redirect_uri=/setup/user&code_challenge=&code_challenge_method=&return_to=/setup/user",
    )
    .into_response()
}

fn not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<p>Onboarding is disabled on this server (BSMCP_ONBOARDING_ENABLED=false).</p>"),
    )
        .into_response()
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
}
