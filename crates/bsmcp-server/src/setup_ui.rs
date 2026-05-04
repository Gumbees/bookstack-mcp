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
use serde_json::{json, Value};

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::{stable_id_for, DbBackend, TokenBinding};
use bsmcp_common::settings::{
    hash_token_id, GlobalSettings, UserSettings, DEFAULT_ACCOUNT_LABEL,
};

use crate::mcp;
use crate::remember;
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

    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
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

    // Existing personalities for this BookStack user → drive the
    // account_label datalist on the form. Empty when the user is
    // single-account or the binding lookup fails (non-fatal).
    let existing_labels = labels_for_token(state.db.as_ref(), &token_id_hash).await;

    let bs_client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let migration = MigrationView {
        sources: load_migration_sources(&token_id, &bs_client, state.db.clone(), state.directory.clone()).await,
        form: MigrationFormFields::default(),
        plan: None,
    };

    Html(render_setup_page(&settings, &migration, &existing_labels)).into_response()
}

/// Resolve the list of `account_label`s already bound for the
/// BookStack user behind `token_id_hash`. Returns empty Vec on any
/// error or missing binding — the wizard treats it as "no other
/// personalities exist." Pulled out into a helper so the migrate
/// preview path uses the same logic.
async fn labels_for_token(db: &dyn DbBackend, token_id_hash: &str) -> Vec<String> {
    match db.get_token_binding(token_id_hash).await {
        Ok(Some(binding)) => db
            .list_account_labels_for_user(binding.bookstack_user_id)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[derive(Deserialize, Default)]
pub struct SetupForm {
    #[serde(default)]
    pub chosen_ai_identity: Option<String>,
    #[serde(default)]
    pub journaling_enabled: Option<String>,
    /// Per-account-personality label. Empty input becomes
    /// `DEFAULT_ACCOUNT_LABEL` so single-account users get the
    /// expected behavior even when leaving the field blank.
    #[serde(default)]
    pub account_label: Option<String>,
    /// Inject `globals.org_identity_page_id` (when admin-configured)
    /// into this user's `system_prompt_additions`. Default true; the
    /// HTML form sends nothing when unchecked, so an absent field
    /// flips the bool to `false` (`checkbox()` returns false on None).
    #[serde(default)]
    pub use_org_identity: Option<String>,
}

pub async fn handle_setup_user_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawForm(body): RawForm,
) -> Response {
    if !mcp::onboarding_enabled() {
        return not_found_response();
    }

    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return redirect_to_authorize(),
    };

    let body_str = std::str::from_utf8(&body).unwrap_or("");
    let raw_pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(body_str).unwrap_or_default();
    let form: SetupForm = serde_urlencoded::from_str(body_str).unwrap_or_default();

    let token_id_hash = hash_token_id(&token_id);

    // Look up the current binding so we can detect a label change.
    // No binding = the SSE/oauth path didn't run for this token yet,
    // which shouldn't happen here (the settings session was issued by
    // /authorize and that always calls ensure_token_binding) but is
    // still a coherent failure mode worth surfacing.
    let binding = match state.db.get_token_binding(&token_id_hash).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return error_response(
                "no token binding for this session — re-authenticate via /authorize".to_string(),
            );
        }
        Err(e) => return error_response(format!("get_token_binding failed: {e}")),
    };

    // Stage a settings draft. If the label is unchanged, this is just
    // the existing row with form fields re-applied. If the label is
    // changing AND a row already exists at the new stable_id, that's a
    // re-attach: pull the existing settings and overlay the form
    // fields, so the user keeps everything they had under that label
    // and only the wizard's editable fields get refreshed.
    let new_label = normalize_account_label(form.account_label.as_deref());
    let label_changed = new_label != binding.account_label;

    let mut settings = if label_changed {
        let new_stable_id = stable_id_for(binding.bookstack_user_id, &new_label);
        match state.db.get_user_settings_by_stable_id(&new_stable_id).await {
            Ok(Some(existing)) => existing, // re-attach onto previous personality
            Ok(None) => UserSettings::default(),
            Err(e) => {
                return error_response(format!(
                    "get_user_settings_by_stable_id failed: {e}"
                ));
            }
        }
    } else {
        state
            .db
            .get_user_settings_by_stable_id(&binding.stable_id())
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    };

    apply_setup_form(&mut settings, &form, &raw_pairs);

    let target_stable_id =
        stable_id_for(binding.bookstack_user_id, &settings.account_label);

    if label_changed {
        // Repoint the binding *before* saving, so any subsequent reads
        // (e.g. the inline migration step below, which calls
        // get_user_settings via the binding chase) hit the new
        // stable_id. created_at is preserved so the binding's age
        // continues to mean "when did this token first authenticate."
        let updated = TokenBinding {
            token_id_hash: binding.token_id_hash.clone(),
            bookstack_user_id: binding.bookstack_user_id,
            account_label: settings.account_label.clone(),
            created_at: binding.created_at,
        };
        if let Err(e) = state.db.set_token_binding(&updated).await {
            return error_response(format!("Failed to update token binding: {e}"));
        }
    }

    if let Err(e) = state
        .db
        .save_user_settings_by_stable_id(&target_stable_id, &settings)
        .await
    {
        return error_response(format!("Failed to save user settings: {e}"));
    }

    // Optional inline migration. The migration form fields ride along on
    // the same submit; if the user picked a source book and at least one
    // page (or accepted the all-dated default), run `execute` now so the
    // result lands on the success page in the same round-trip. Sync is
    // fine while typical legacy journals are <100 pages — see brief.
    let migration_form = parse_migration_form(&raw_pairs);
    let migration_result = if migration_form.execute_requested && migration_form.book_id.is_some() {
        let bs_client = BookStackClient::new(
            &state.bookstack_url,
            &token_id,
            &token_secret,
            state.http_client.clone(),
        );
        Some(
            run_migration_execute(
                &migration_form,
                &token_id,
                &bs_client,
                state.db.clone(),
                state.directory.clone(),
            )
            .await,
        )
    } else {
        None
    };

    Html(render_success_page(migration_result.as_ref())).into_response()
}

/// POST `/setup/user/migrate/preview` — re-render the wizard with the
/// migration plan attached. The form posts back the same SetupForm fields
/// (so we don't lose the user's journaling/tool-overrides edits) plus the
/// migration source/entry-type fields. We DO NOT save settings on this
/// route — preview is a dry-run pass; only the main POST persists.
pub async fn handle_setup_user_migrate_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawForm(body): RawForm,
) -> Response {
    if !mcp::onboarding_enabled() {
        return not_found_response();
    }

    let (token_id, token_secret) = match resolve_session_creds(&headers, &state.settings_sessions).await {
        Some(creds) => creds,
        None => return redirect_to_authorize(),
    };

    let body_str = std::str::from_utf8(&body).unwrap_or("");
    let raw_pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(body_str).unwrap_or_default();
    let form: SetupForm = serde_urlencoded::from_str(body_str).unwrap_or_default();

    let token_id_hash = hash_token_id(&token_id);
    // Use the in-memory settings shape so the rest of the form re-renders
    // with the user's draft values. We DON'T save — preview is read-only.
    let mut settings = state
        .db
        .get_user_settings(&token_id_hash)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    settings.chosen_ai_identity = nonempty(&form.chosen_ai_identity);
    settings.journaling_enabled = checkbox(&form.journaling_enabled);
    settings.tool_overrides = parse_tool_overrides(&raw_pairs);

    let migration_form = parse_migration_form(&raw_pairs);
    let bs_client = BookStackClient::new(
        &state.bookstack_url,
        &token_id,
        &token_secret,
        state.http_client.clone(),
    );

    let plan = run_migration_plan(
        &migration_form,
        &token_id,
        &bs_client,
        state.db.clone(),
        state.directory.clone(),
    )
    .await;

    let migration = MigrationView {
        sources: load_migration_sources(&token_id, &bs_client, state.db.clone(), state.directory.clone()).await,
        form: migration_form,
        plan: Some(plan),
    };

    let existing_labels = labels_for_token(state.db.as_ref(), &token_id_hash).await;

    Html(render_setup_page(&settings, &migration, &existing_labels)).into_response()
}

/// Apply the parsed wizard form to a `UserSettings` instance. Pure (no
/// I/O) so the test suite can exercise the field-flip semantics directly.
/// Always stamps `setup_complete = true` — a successful POST means the
/// user submitted the wizard, even if they left every field blank.
///
/// Note: migration form fields are NOT applied here. Migration is runtime
/// state (which source book + which pages were imported) — the result
/// shows on the success page but isn't persisted to UserSettings. The
/// post-import target book id is already cached on UserSettings via
/// `resolve_user_journal_book` during the import itself.
pub fn apply_setup_form(
    settings: &mut UserSettings,
    form: &SetupForm,
    raw_pairs: &[(String, String)],
) {
    settings.chosen_ai_identity = nonempty(&form.chosen_ai_identity);
    settings.journaling_enabled = checkbox(&form.journaling_enabled);
    settings.tool_overrides = parse_tool_overrides(raw_pairs);
    settings.account_label = normalize_account_label(form.account_label.as_deref());
    // `use_org_identity` defaults to true on a fresh form; checkbox()
    // returns false when the box is absent in the POST body. To keep
    // the default-true semantics, we OR in the form value: present
    // (whether checked or not) means the form was submitted, so
    // whatever the box says is authoritative. Since HTML forms only
    // send unchecked checkboxes when explicitly named, absence here
    // means "submitted with the box unchecked" → false.
    settings.use_org_identity = checkbox(&form.use_org_identity);
    settings.setup_complete = true;
}

/// Normalize a user-typed `account_label` value to the canonical form
/// stored on `UserSettings.account_label` and used in `stable_id_for`.
///
/// Rules: trim whitespace, fall back to `DEFAULT_ACCOUNT_LABEL` when
/// empty, strip ASCII colons (the `stable_id` separator). We don't
/// otherwise restrict the character set — users want to label
/// personalities however they think; e.g. `"dtc"`, `"personal-laptop"`,
/// `"Pia (work)"` are all acceptable.
pub fn normalize_account_label(raw: Option<&str>) -> String {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() {
        return DEFAULT_ACCOUNT_LABEL.to_string();
    }
    trimmed.replace(':', "")
}

// =====================================================================
// Migration wizard helpers (Phase 2.5)
// =====================================================================

/// Form fields harvested from the wizard's migration section. Captures
/// both step 1 (source picker) and step 2 (per-page selection +
/// optional manual date for undated pages) so a single parser handles
/// both submit shapes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationFormFields {
    /// Numeric BookStack book ID picked from the source dropdown.
    pub book_id: Option<i64>,
    /// `"user"` or `"agent"`.
    pub entry_type: Option<String>,
    /// Required when `entry_type = "agent"`. Free-form before
    /// normalization — passed verbatim to the migrate tool, which calls
    /// `normalize_agent_name` and surfaces the same error the user would
    /// get from the MCP tool.
    pub agent_name: Option<String>,
    /// Source page IDs the user explicitly checked in step 2. When
    /// `execute_requested` is true and this is empty AND no date overrides
    /// were supplied, the migration is a no-op (matches the `execute`
    /// tool's empty-`pages` behavior).
    pub selected_pages: Vec<i64>,
    /// Per-page manual date overrides for undated pages — keyed by source
    /// page id, value is `YYYY-MM-DD`. Only entries with a non-empty value
    /// are kept; bare checkboxes without a date are dropped.
    pub date_overrides: HashMap<i64, String>,
    /// True when the user clicked "Import" (the main wizard submit), as
    /// opposed to "Preview" (preview-only round-trip).
    pub execute_requested: bool,
}

/// Snapshot of everything the migration section needs to render. Gathers
/// the listed sources, the form values to repopulate, and (optionally)
/// the planned page set after a preview round-trip or the result after
/// an execute round-trip. Kept small — both the user wizard and its
/// tests build it the same way.
pub struct MigrationView {
    /// Books on the User Journals shelf the calling user can see, plus
    /// `owned`/`page_count` annotations from `migrate list_sources`. An
    /// `Err` here disables the migration section in the UI (e.g. shelf
    /// not configured yet) — the rest of the wizard still works.
    pub sources: Result<Value, String>,
    /// Form values from the most-recent submit, used to re-populate the
    /// dropdown / radio / checkboxes after a preview round-trip.
    pub form: MigrationFormFields,
    /// Result of `migrate plan` — present only on the preview render.
    /// Execute results don't go here; they ride straight onto the
    /// success page (different render path).
    pub plan: Option<Result<Value, String>>,
}

/// Parse the migration section out of the raw form pairs. Pure (no I/O)
/// so both wizard handlers + the test suite can use it. Two states it
/// covers:
///
/// - **Step 1 (source picked, no preview yet):** caller submitted
///   `migration_book_id`, `migration_entry_type`, optionally
///   `migration_agent_name`, plus `migration_action=preview`.
/// - **Step 2 (after preview, ready to import):** the same fields plus
///   `migration_page_<source_page_id>=on` for each checked page and
///   optional `migration_date_<source_page_id>=YYYY-MM-DD` for undated
///   pages where the user typed a date. `migration_action=execute`.
///
/// The `execute_requested` bit lets the caller distinguish a preview
/// round-trip (don't import yet) from a final submit (do import).
pub fn parse_migration_form(pairs: &[(String, String)]) -> MigrationFormFields {
    let mut out = MigrationFormFields::default();
    for (k, v) in pairs {
        match k.as_str() {
            "migration_book_id" => {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    out.book_id = trimmed.parse().ok();
                }
            }
            "migration_entry_type" => {
                let t = v.trim();
                if !t.is_empty() {
                    out.entry_type = Some(t.to_string());
                }
            }
            "migration_agent_name" => {
                let t = v.trim();
                if !t.is_empty() {
                    out.agent_name = Some(t.to_string());
                }
            }
            "migration_action" => {
                if v == "execute" {
                    out.execute_requested = true;
                }
            }
            _ => {
                if let Some(rest) = k.strip_prefix("migration_page_") {
                    if matches!(v.as_str(), "on" | "true" | "1") {
                        if let Ok(id) = rest.parse::<i64>() {
                            out.selected_pages.push(id);
                        }
                    }
                } else if let Some(rest) = k.strip_prefix("migration_date_") {
                    let trimmed = v.trim();
                    if !trimmed.is_empty() {
                        if let Ok(id) = rest.parse::<i64>() {
                            out.date_overrides.insert(id, trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Build the body the migrate dispatcher expects from a parsed
/// MigrationFormFields + chosen action. Pure helper — no I/O. Exists so
/// the wizard's "construct the dispatch body" step is testable without
/// spinning up an HTTP request.
pub fn build_migration_dispatch_body(form: &MigrationFormFields, include_pages: bool) -> Value {
    let mut body = serde_json::Map::new();
    if let Some(id) = form.book_id {
        body.insert("book_id".to_string(), json!(id));
    }
    if let Some(et) = &form.entry_type {
        body.insert("entry_type".to_string(), json!(et));
    }
    if let Some(an) = &form.agent_name {
        body.insert("agent_name".to_string(), json!(an));
    }
    if include_pages {
        // Selected page ids ride through as an integer array. Empty
        // array is intentional — see `migrate::execute` no-op path.
        body.insert("pages".to_string(), json!(form.selected_pages));
        if !form.date_overrides.is_empty() {
            // Convert i64 keys to strings so the JSON object is valid.
            let mut map = serde_json::Map::new();
            for (id, date) in &form.date_overrides {
                map.insert(id.to_string(), json!(date));
            }
            body.insert("page_date_overrides".to_string(), Value::Object(map));
        }
    }
    Value::Object(body)
}

/// Call `migrate list_sources` for the wizard. Returns the dispatcher's
/// `data` payload on success or a stringified error on failure (so the
/// UI can show a clean message instead of crashing).
async fn load_migration_sources(
    token_id: &str,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    directory: Arc<crate::directory::DirectoryService>,
) -> Result<Value, String> {
    let envelope = remember::dispatch(
        "migrate",
        "list_sources",
        json!({}),
        token_id,
        client,
        db,
        None,
        Some(directory),
    )
    .await;
    extract_envelope_data(&envelope)
}

/// Call `migrate plan` for the wizard preview round-trip. Same envelope
/// unpacking as `load_migration_sources`.
async fn run_migration_plan(
    form: &MigrationFormFields,
    token_id: &str,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    directory: Arc<crate::directory::DirectoryService>,
) -> Result<Value, String> {
    let body = build_migration_dispatch_body(form, false);
    let envelope = remember::dispatch(
        "migrate",
        "plan",
        body,
        token_id,
        client,
        db,
        None,
        Some(directory),
    )
    .await;
    extract_envelope_data(&envelope)
}

/// Call `migrate execute` from the main submit handler. Same envelope
/// unpacking as the other two.
async fn run_migration_execute(
    form: &MigrationFormFields,
    token_id: &str,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    directory: Arc<crate::directory::DirectoryService>,
) -> Result<Value, String> {
    let body = build_migration_dispatch_body(form, true);
    let envelope = remember::dispatch(
        "migrate",
        "execute",
        body,
        token_id,
        client,
        db,
        None,
        Some(directory),
    )
    .await;
    extract_envelope_data(&envelope)
}

/// Pull `data` out of the standard `{ok, data, meta, error}` envelope.
/// On failure, surface the error message (or `"unknown error"` if the
/// envelope shape is wrong). Pure helper.
fn extract_envelope_data(envelope: &Value) -> Result<Value, String> {
    let ok = envelope.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(envelope
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .to_string())
    }
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

fn render_setup_page(
    s: &UserSettings,
    migration: &MigrationView,
    existing_labels: &[String],
) -> String {
    let chosen = html_escape(s.chosen_ai_identity.as_deref().unwrap_or(""));
    let journaling_checked = if s.journaling_enabled { "checked" } else { "" };
    let use_org_identity_checked = if s.use_org_identity { "checked" } else { "" };
    let tool_rows = render_user_tool_overrides_section(s);
    let already_done_banner = if s.setup_complete {
        r#"<div class="banner">You've already completed setup. Re-submitting will update your preferences.</div>"#
    } else {
        ""
    };
    let migration_section = render_migration_section(migration);

    // Account label section: always-shown text input, with a `<datalist>`
    // pre-populated from any existing labels for this BookStack user.
    // Single-account users see one option ("default") and the field is
    // mostly invisible. Multi-account users see all their personalities
    // and can switch by typing/selecting one.
    let account_label_value = html_escape(&s.account_label);
    let account_label_datalist = if existing_labels.is_empty() {
        String::new()
    } else {
        let opts: String = existing_labels
            .iter()
            .map(|l| format!("<option value=\"{}\">", html_escape(l)))
            .collect::<Vec<_>>()
            .join("");
        format!("<datalist id=\"existing-account-labels\">{opts}</datalist>")
    };
    let datalist_attr = if existing_labels.is_empty() {
        ""
    } else {
        " list=\"existing-account-labels\""
    };
    let account_label_help = if existing_labels.len() > 1 {
        format!(
            "Distinguishes which set of settings applies when the same BookStack user runs this MCP from multiple Anthropic accounts. \
             Existing personalities for this BookStack user: <code>{}</code>. Type one to re-attach this token's settings to it, or type a new one to create a fresh personality. Leave as <code>default</code> if you only have one.",
            existing_labels.iter()
                .map(|l| html_escape(l))
                .collect::<Vec<_>>()
                .join("</code>, <code>")
        )
    } else {
        "Distinguishes which set of settings applies when the same BookStack user runs this MCP from multiple Anthropic accounts. \
         Leave as <code>default</code> if you only have one — you can always change this later."
            .to_string()
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
.banner.error {{ background: #fbeae9; border-color: #c0392b; color: #6a1f1a; }}
label {{ display: block; margin: 0.5em 0; }}
label.inline {{ display: inline-block; margin-right: 1em; }}
input[type=text], input[type=date] {{ padding: 0.5em; box-sizing: border-box; font-family: inherit; }}
input[type=text] {{ width: 100%; }}
input[type=date] {{ width: 12em; }}
select {{ padding: 0.5em; font-family: inherit; max-width: 100%; }}
button {{ margin-top: 1em; padding: 0.6em 1.2em; font-size: 1em; cursor: pointer; }}
.tool-overrides {{ display: grid; grid-template-columns: 1fr; gap: 0.4em; margin: 0.5em 0; }}
.tool-overrides .tool-row {{ display: grid; grid-template-columns: 220px repeat(3, auto); gap: 0.5em; align-items: center; padding: 0.2em 0.4em; border-bottom: 1px solid #eee; font-size: 0.9em; }}
.tool-overrides .tool-row code {{ font-size: 0.95em; }}
.tool-overrides label {{ display: inline-flex; align-items: center; gap: 0.3em; margin: 0; font-weight: normal; }}
.migration-section {{ background: #f6f8fa; border: 1px solid #d0d7de; padding: 1em; border-radius: 4px; }}
.migration-section.disabled {{ opacity: 0.6; }}
.migration-table {{ width: 100%; border-collapse: collapse; margin-top: 0.6em; font-size: 0.9em; }}
.migration-table th, .migration-table td {{ border: 1px solid #d0d7de; padding: 0.3em 0.5em; text-align: left; vertical-align: middle; }}
.migration-table th {{ background: #ececec; font-weight: 600; }}
.migration-table td.target {{ font-family: ui-monospace, monospace; font-size: 0.85em; color: #444; }}
.migration-actions {{ margin-top: 0.6em; display: flex; gap: 0.6em; flex-wrap: wrap; }}
</style>
</head>
<body>
<h1>BookStack MCP — User Setup</h1>
<p class="note">First-time setup for your MCP user. Once you submit, the onboarding link stops appearing on your tool responses.</p>
{already_done_banner}
<form method="post" action="/setup/user">

  <h2>1. AI agent identity</h2>
  <label>Default AI agent name <input type="text" name="chosen_ai_identity" value="{chosen}" placeholder="e.g. pia"></label>
  <p class="help">Optional. Your default AI agent name. Used for journal chapter naming and the briefing reminder. Leave blank to use the AI's default self.</p>

  <label>Account label <input type="text" name="account_label" value="{account_label_value}" placeholder="default"{datalist_attr}></label>
  {account_label_datalist}
  <p class="help">{account_label_help}</p>

  <label><input type="checkbox" name="use_org_identity" {use_org_identity_checked}> Use this instance's org identity</label>
  <p class="help">When on, the briefing pulls in this instance's <code>org_identity_page_id</code> (if the admin set one). Turn off if your primary identity lives on a different MCP and this one's org identity shouldn't bind your session.</p>

  <h2>2. Journaling</h2>
  <label><input type="checkbox" name="journaling_enabled" {journaling_checked}> Enable journaling on this instance</label>
  <p class="help">When on, the briefing reminds you to journal AND the <code>journal write</code> / <code>identity write</code> tools accept writes here. Multi-MCP setups: turn on for the primary, off for bootstrap-only sources.</p>

  <h2>3. Tool overrides</h2>
  <p class="help">Per-tool overrides for your account. <em>Use admin default</em> follows whatever the admin sets globally; <em>on</em> and <em>off</em> force the tool regardless of the global setting.</p>
  <div class="tool-overrides">
    {tool_rows}
  </div>

  <h2>4. Migration</h2>
  {migration_section}

  <div class="migration-actions">
    <button type="submit" name="migration_action" value="complete">Complete setup</button>
  </div>
</form>
</body>
</html>"#,
        already_done_banner = already_done_banner,
        chosen = chosen,
        account_label_value = account_label_value,
        account_label_datalist = account_label_datalist,
        datalist_attr = datalist_attr,
        account_label_help = account_label_help,
        journaling_checked = journaling_checked,
        use_org_identity_checked = use_org_identity_checked,
        tool_rows = tool_rows,
        migration_section = migration_section,
    )
}

/// Render the migration block. Three modes:
/// - sources unavailable (e.g. shelf not configured) → show explanatory
///   note instead of the dropdown
/// - sources available, no plan → step-1 form (dropdown + radio + Preview)
/// - plan present → step-2 form (table of pages + Import button)
fn render_migration_section(view: &MigrationView) -> String {
    let sources = match &view.sources {
        Ok(v) => v,
        Err(e) => {
            return format!(
                r#"<div class="migration-section disabled"><p class="help">Migration unavailable: {}. Ask your admin to configure the User Journals shelf in <code>/setup/admin</code>, then revisit this page.</p></div>"#,
                html_escape(e),
            );
        }
    };
    let source_list = sources
        .get("sources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if source_list.is_empty() {
        return r#"<div class="migration-section disabled"><p class="help">No source books found on the User Journals shelf. Skip this step.</p></div>"#.to_string();
    }

    if let Some(Ok(plan)) = &view.plan {
        return render_migration_plan(view, plan, &source_list);
    }

    let plan_error_banner = if let Some(Err(e)) = &view.plan {
        format!(
            r#"<div class="banner error">Preview failed: {}</div>"#,
            html_escape(e),
        )
    } else {
        String::new()
    };

    let mut options = String::new();
    for src in &source_list {
        let id = src.get("book_id").and_then(|v| v.as_i64()).unwrap_or_default();
        let name = src.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let owned = src.get("owned").and_then(|v| v.as_bool()).unwrap_or(false);
        let count = src.get("page_count").and_then(|v| v.as_u64()).unwrap_or(0);
        let selected = if Some(id) == view.form.book_id { " selected" } else { "" };
        let owned_marker = if owned { " (owned)" } else { "" };
        options.push_str(&format!(
            "<option value=\"{id}\"{selected}>{name} — {count} pages{owned_marker}</option>",
            id = id,
            selected = selected,
            name = html_escape(name),
            count = count,
            owned_marker = owned_marker,
        ));
    }

    let entry_user_checked = if view.form.entry_type.as_deref() == Some("user") || view.form.entry_type.is_none() {
        " checked"
    } else {
        ""
    };
    let entry_agent_checked = if view.form.entry_type.as_deref() == Some("agent") {
        " checked"
    } else {
        ""
    };
    let agent_name = html_escape(view.form.agent_name.as_deref().unwrap_or(""));

    format!(
        r#"<div class="migration-section">
{plan_error_banner}
<p class="help">Optional. Import an existing journal book into the v1.0.0 layout. Pick a source book, choose whether to import as your user journal or as an agent's journal, then preview the page list before committing.</p>
<label>Source book
  <select name="migration_book_id">
    <option value="">-- pick a source --</option>
    {options}
  </select>
</label>
<fieldset style="border: none; padding: 0; margin: 0.4em 0;">
  <legend class="help" style="padding: 0;">Import as</legend>
  <label class="inline"><input type="radio" name="migration_entry_type" value="user"{entry_user_checked}> user (your first name)</label>
  <label class="inline"><input type="radio" name="migration_entry_type" value="agent"{entry_agent_checked}> agent</label>
</fieldset>
<label>Agent name <input type="text" name="migration_agent_name" value="{agent_name}" placeholder="e.g. pia"></label>
<p class="help">Required when "agent" is picked. Lowercase ASCII letters, digits, dashes, underscores; whitespace becomes a dash.</p>
<div class="migration-actions">
  <button type="submit" formaction="/setup/user/migrate/preview" name="migration_action" value="preview">Preview import</button>
</div>
</div>"#,
        plan_error_banner = plan_error_banner,
        options = options,
        entry_user_checked = entry_user_checked,
        entry_agent_checked = entry_agent_checked,
        agent_name = agent_name,
    )
}

/// Render the step-2 page table after a successful preview.
fn render_migration_plan(
    view: &MigrationView,
    plan: &Value,
    source_list: &[Value],
) -> String {
    let book_id = view.form.book_id.unwrap_or_default();
    let entry_type = view.form.entry_type.as_deref().unwrap_or("user");
    let agent_name = view.form.agent_name.as_deref().unwrap_or("");

    // Find the source book name from the list for the heading.
    let source_name = source_list
        .iter()
        .find(|s| s.get("book_id").and_then(|v| v.as_i64()) == Some(book_id))
        .and_then(|s| s.get("name").and_then(|v| v.as_str()))
        .unwrap_or("(unknown)");

    let dated = plan
        .get("pages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let undated = plan
        .get("undated_pages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut rows = String::new();
    for p in &dated {
        let id = p.get("source_page_id").and_then(|v| v.as_i64()).unwrap_or_default();
        let name = p.get("source_name").and_then(|v| v.as_str()).unwrap_or("");
        let date = p.get("detected_date").and_then(|v| v.as_str()).unwrap_or("");
        let target_chapter = p.get("target_chapter").and_then(|v| v.as_str()).unwrap_or("");
        let target_page = p.get("target_page").and_then(|v| v.as_str()).unwrap_or("");
        rows.push_str(&format!(
            "<tr><td><input type=\"checkbox\" name=\"migration_page_{id}\" value=\"on\" checked></td>\
             <td>{name}</td><td>{date}</td>\
             <td class=\"target\">{tc} / {tp}</td></tr>",
            id = id,
            name = html_escape(name),
            date = html_escape(date),
            tc = html_escape(target_chapter),
            tp = html_escape(target_page),
        ));
    }
    for p in &undated {
        let id = p.get("source_page_id").and_then(|v| v.as_i64()).unwrap_or_default();
        let name = p.get("source_name").and_then(|v| v.as_str()).unwrap_or("");
        rows.push_str(&format!(
            "<tr><td><input type=\"checkbox\" name=\"migration_page_{id}\" value=\"on\"></td>\
             <td>{name}</td>\
             <td><input type=\"date\" name=\"migration_date_{id}\" placeholder=\"YYYY-MM-DD\"></td>\
             <td class=\"target\"><em>(undated — pick a date to import)</em></td></tr>",
            id = id,
            name = html_escape(name),
        ));
    }

    let entry_hidden = format!(
        r#"<input type="hidden" name="migration_book_id" value="{id}">
<input type="hidden" name="migration_entry_type" value="{et}">
<input type="hidden" name="migration_agent_name" value="{an}">"#,
        id = book_id,
        et = html_escape(entry_type),
        an = html_escape(agent_name),
    );

    let total = dated.len() + undated.len();

    format!(
        r#"<div class="migration-section">
<p class="note"><strong>Source:</strong> {source_name} ({total} pages)</p>
{entry_hidden}
<table class="migration-table">
  <thead><tr><th>Import</th><th>Source page</th><th>Date</th><th>Target chapter / page</th></tr></thead>
  <tbody>
    {rows}
  </tbody>
</table>
<p class="help">Dated pages are checked by default. Undated pages need a date you pick manually before they'll import. Toggle individual rows as needed.</p>
<div class="migration-actions">
  <button type="submit" name="migration_action" value="execute">Import selected pages and complete setup</button>
  <button type="submit" formaction="/setup/user/migrate/preview" name="migration_action" value="preview">Re-preview</button>
</div>
</div>"#,
        source_name = html_escape(source_name),
        total = total,
        entry_hidden = entry_hidden,
        rows = rows,
    )
}

fn render_success_page(migration: Option<&Result<Value, String>>) -> String {
    let migration_block = match migration {
        None => String::new(),
        Some(Ok(v)) => {
            let imported = v.get("imported").and_then(|x| x.as_u64()).unwrap_or(0);
            let skipped = v.get("skipped").and_then(|x| x.as_u64()).unwrap_or(0);
            let errors = v.get("errors").and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let mut error_list = String::new();
            if !errors.is_empty() {
                error_list.push_str("<ul style=\"text-align: left; max-width: 480px; margin: 0.5em auto;\">");
                for e in &errors {
                    let name = e.get("source_name").and_then(|x| x.as_str()).unwrap_or("(unknown)");
                    let reason = e.get("reason").and_then(|x| x.as_str()).unwrap_or("(unknown)");
                    error_list.push_str(&format!(
                        "<li><code>{}</code>: {}</li>",
                        html_escape(name),
                        html_escape(reason),
                    ));
                }
                error_list.push_str("</ul>");
            }
            format!(
                r#"<h2 style="color: #1a7f37; margin-top: 1.5em;">Migration result</h2>
<p class="note">Imported <strong>{imported}</strong> pages. Skipped <strong>{skipped}</strong>.</p>
{error_list}"#,
                imported = imported,
                skipped = skipped,
                error_list = error_list,
            )
        }
        Some(Err(e)) => format!(
            r#"<h2 style="color: #c0392b; margin-top: 1.5em;">Migration failed</h2>
<p class="note">{}</p>"#,
            html_escape(e),
        ),
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Setup complete</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 600px; margin: 4em auto; padding: 0 1em; text-align: center; color: #222; }}
h1 {{ color: #1a7f37; }}
h2 {{ font-size: 1.2em; }}
.note {{ color: #666; font-size: 0.95em; line-height: 1.5; }}
code {{ background: #f3f3f3; padding: 0 0.3em; border-radius: 3px; font-size: 0.9em; }}
a {{ color: #0969da; }}
</style>
</head>
<body>
<h1>&#10003; Setup complete</h1>
<p class="note">Your user setup has been saved. The onboarding link will stop appearing on your MCP tool responses.</p>
{migration_block}
<p class="note">You can close this window. To revise your preferences later, visit <a href="/setup/user">/setup/user</a> again or use the admin <a href="/settings">/settings</a> page.</p>
</body>
</html>"#,
        migration_block = migration_block,
    )
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
            account_label: None,
            use_org_identity: Some("on".to_string()),
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

    // =====================================================================
    // Migration wizard (Phase 2.5)
    // =====================================================================

    #[test]
    fn parse_migration_form_step1_picks_book_and_entry_type() {
        // Step 1: user picked a source + entry_type, clicked Preview.
        // No per-page selections yet.
        let pairs = vec![
            pair("chosen_ai_identity", "pia"),
            pair("migration_book_id", "42"),
            pair("migration_entry_type", "user"),
            pair("migration_action", "preview"),
        ];
        let f = parse_migration_form(&pairs);
        assert_eq!(f.book_id, Some(42));
        assert_eq!(f.entry_type.as_deref(), Some("user"));
        assert!(f.agent_name.is_none());
        assert!(f.selected_pages.is_empty());
        assert!(f.date_overrides.is_empty());
        assert!(!f.execute_requested, "preview action must not flip execute_requested");
    }

    #[test]
    fn parse_migration_form_step1_with_agent_carries_agent_name() {
        let pairs = vec![
            pair("migration_book_id", "42"),
            pair("migration_entry_type", "agent"),
            pair("migration_agent_name", "Pia"),
            pair("migration_action", "preview"),
        ];
        let f = parse_migration_form(&pairs);
        assert_eq!(f.entry_type.as_deref(), Some("agent"));
        assert_eq!(f.agent_name.as_deref(), Some("Pia"));
        // We deliberately don't normalize at parse-time — the dispatcher
        // does that and surfaces the same error the MCP tool would.
    }

    #[test]
    fn parse_migration_form_step2_collects_page_selections_and_overrides() {
        // Step 2: user clicked Import, with checkboxes + a manual date
        // for one undated page.
        let pairs = vec![
            pair("migration_book_id", "42"),
            pair("migration_entry_type", "user"),
            pair("migration_action", "execute"),
            pair("migration_page_100", "on"),
            pair("migration_page_101", "on"),
            // Page 102's checkbox unchecked → the field isn't submitted
            // at all (browser convention). Date input still rides along
            // — we keep the override only because it might land on
            // another future submit.
            pair("migration_date_103", "2026-04-12"),
            pair("migration_page_103", "on"),
            // Empty date input → dropped, not stored as empty string.
            pair("migration_date_104", ""),
        ];
        let f = parse_migration_form(&pairs);
        assert!(f.execute_requested);
        assert_eq!(f.selected_pages, vec![100, 101, 103]);
        assert_eq!(f.date_overrides.get(&103).map(String::as_str), Some("2026-04-12"));
        assert!(!f.date_overrides.contains_key(&104), "empty date string must not be stored");
    }

    #[test]
    fn parse_migration_form_handles_blank_book_id() {
        // User left the dropdown on its placeholder option ("-- pick a
        // source --"). Empty value stays None.
        let pairs = vec![
            pair("migration_book_id", ""),
            pair("migration_entry_type", "user"),
        ];
        let f = parse_migration_form(&pairs);
        assert!(f.book_id.is_none());
    }

    #[test]
    fn parse_migration_form_ignores_unrelated_pairs() {
        // SetupForm + tool override pairs share the same urlencoded body;
        // the migration parser must not pick them up.
        let pairs = vec![
            pair("chosen_ai_identity", "pia"),
            pair("journaling_enabled", "on"),
            pair("tool_user_briefing", "on"),
        ];
        let f = parse_migration_form(&pairs);
        assert_eq!(f, MigrationFormFields::default());
    }

    #[test]
    fn build_migration_dispatch_body_step1_excludes_pages_array() {
        let f = MigrationFormFields {
            book_id: Some(42),
            entry_type: Some("user".to_string()),
            agent_name: None,
            selected_pages: vec![],
            date_overrides: HashMap::new(),
            execute_requested: false,
        };
        let body = build_migration_dispatch_body(&f, false);
        assert_eq!(body.get("book_id").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(body.get("entry_type").and_then(|v| v.as_str()), Some("user"));
        assert!(body.get("pages").is_none(), "step 1 must not include pages array");
        assert!(body.get("page_date_overrides").is_none());
    }

    #[test]
    fn build_migration_dispatch_body_step2_stringifies_override_keys() {
        let mut overrides = HashMap::new();
        overrides.insert(103_i64, "2026-04-12".to_string());
        let f = MigrationFormFields {
            book_id: Some(42),
            entry_type: Some("agent".to_string()),
            agent_name: Some("Pia".to_string()),
            selected_pages: vec![100, 101, 103],
            date_overrides: overrides,
            execute_requested: true,
        };
        let body = build_migration_dispatch_body(&f, true);
        assert_eq!(body.get("agent_name").and_then(|v| v.as_str()), Some("Pia"));
        let pages = body.get("pages").and_then(|v| v.as_array()).cloned().unwrap();
        let ids: Vec<i64> = pages.iter().filter_map(|v| v.as_i64()).collect();
        assert_eq!(ids, vec![100, 101, 103]);
        // Override map: keys must be strings (JSON object keys are
        // always strings); values are dates as strings.
        let pdo = body.get("page_date_overrides").and_then(|v| v.as_object()).unwrap();
        assert_eq!(pdo.get("103").and_then(|v| v.as_str()), Some("2026-04-12"));
    }

    #[test]
    fn extract_envelope_data_handles_ok_and_err() {
        let ok = json!({ "ok": true, "data": { "x": 1 } });
        assert_eq!(extract_envelope_data(&ok).unwrap(), json!({ "x": 1 }));

        let err = json!({ "ok": false, "error": { "message": "boom", "code": "internal_error" } });
        assert_eq!(extract_envelope_data(&err).unwrap_err(), "boom");

        let malformed = json!({ "weird": "shape" });
        // Missing `ok` is treated as failure with `unknown error` placeholder.
        assert_eq!(extract_envelope_data(&malformed).unwrap_err(), "unknown error");
    }

    #[test]
    fn apply_setup_form_does_not_touch_migration_settings() {
        // Migration is runtime state, not persisted to UserSettings.
        // Apply_setup_form must remain a pure function over the four
        // documented fields, not pick up anything migration-shaped.
        let mut s = UserSettings::default();
        let form = SetupForm::default();
        let pairs = vec![
            pair("migration_book_id", "42"),
            pair("migration_entry_type", "user"),
            pair("migration_action", "execute"),
            pair("migration_page_100", "on"),
        ];
        apply_setup_form(&mut s, &form, &pairs);
        assert!(s.setup_complete);
        // The four documented fields hold their default values; nothing
        // migration-shaped landed on the settings struct.
        assert!(s.chosen_ai_identity.is_none());
        assert!(!s.journaling_enabled);
        assert!(s.tool_overrides.is_empty());
    }

    /// Round-trip the wizard: form pairs that include a migration submit
    /// must (a) parse cleanly via `parse_migration_form`, (b) project to
    /// a dispatch body we can hand to `migrate execute`, and (c) NOT
    /// pollute the settings save.
    #[test]
    fn form_submit_with_migration_round_trips_choices() {
        let pairs = vec![
            pair("chosen_ai_identity", "pia"),
            pair("journaling_enabled", "on"),
            pair("tool_user_briefing", "on"),
            pair("migration_book_id", "42"),
            pair("migration_entry_type", "agent"),
            pair("migration_agent_name", "pia"),
            pair("migration_action", "execute"),
            pair("migration_page_100", "on"),
            pair("migration_page_101", "on"),
            pair("migration_date_103", "2026-04-12"),
            pair("migration_page_103", "on"),
        ];

        // 1. parse_migration_form pulls out only migration fields.
        let mform = parse_migration_form(&pairs);
        assert_eq!(mform.book_id, Some(42));
        assert_eq!(mform.entry_type.as_deref(), Some("agent"));
        assert_eq!(mform.agent_name.as_deref(), Some("pia"));
        assert!(mform.execute_requested);
        assert_eq!(mform.selected_pages, vec![100, 101, 103]);
        assert_eq!(mform.date_overrides.get(&103).map(String::as_str), Some("2026-04-12"));

        // 2. The dispatch body for execute matches what `migrate execute`
        //    expects (per the migrate.rs unit tests on the parsing side).
        let body = build_migration_dispatch_body(&mform, true);
        assert_eq!(body.get("book_id").and_then(|v| v.as_i64()), Some(42));
        assert_eq!(body.get("entry_type").and_then(|v| v.as_str()), Some("agent"));
        let ids: Vec<i64> = body
            .get("pages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_i64())
            .collect();
        assert_eq!(ids, vec![100, 101, 103]);

        // 3. apply_setup_form populates UserSettings from the rest of the
        //    form. Migration field don't pollute the settings struct.
        let mut settings = UserSettings::default();
        let setup_form = SetupForm {
            chosen_ai_identity: Some("pia".to_string()),
            journaling_enabled: Some("on".to_string()),
            account_label: None,
            use_org_identity: Some("on".to_string()),
        };
        apply_setup_form(&mut settings, &setup_form, &pairs);
        assert_eq!(settings.chosen_ai_identity.as_deref(), Some("pia"));
        assert!(settings.journaling_enabled);
        assert_eq!(settings.tool_overrides.get("briefing"), Some(&true));
        assert!(settings.setup_complete);
    }

    #[test]
    fn render_setup_page_disables_migration_when_sources_unavailable() {
        let s = UserSettings::default();
        let view = MigrationView {
            sources: Err("user_journals_shelf_id not configured".to_string()),
            form: MigrationFormFields::default(),
            plan: None,
        };
        let html = render_setup_page(&s, &view, &[]);
        assert!(
            html.contains("Migration unavailable"),
            "should explain why migration is off when sources errored"
        );
        assert!(html.contains("user_journals_shelf_id not configured"));
    }

    #[test]
    fn render_setup_page_step1_lists_each_source_in_dropdown() {
        let s = UserSettings::default();
        let view = MigrationView {
            sources: Ok(json!({
                "sources": [
                    { "book_id": 100, "name": "Pia's Old Journal", "slug": "pia", "page_count": 47, "owned": true },
                    { "book_id": 101, "name": "Other User's Journal", "slug": "other", "page_count": 3, "owned": false },
                ],
            })),
            form: MigrationFormFields::default(),
            plan: None,
        };
        let html = render_setup_page(&s, &view, &[]);
        assert!(html.contains("value=\"100\""), "should include book 100 option");
        assert!(html.contains("Pia&#x27;s Old Journal"), "should html-escape names");
        assert!(html.contains("47 pages"));
        assert!(html.contains("(owned)"), "owned books should be marked");
        assert!(html.contains("Preview import"));
    }

    #[test]
    fn render_setup_page_step2_renders_table_after_preview() {
        let s = UserSettings::default();
        let view = MigrationView {
            sources: Ok(json!({
                "sources": [
                    { "book_id": 100, "name": "Pia's Old Journal", "slug": "pia", "page_count": 2, "owned": true },
                ],
            })),
            form: MigrationFormFields {
                book_id: Some(100),
                entry_type: Some("user".to_string()),
                ..MigrationFormFields::default()
            },
            plan: Some(Ok(json!({
                "source": { "book_id": 100, "name": "Pia's Old Journal" },
                "target": { "book_id": null, "chapter_naming": "{YYYY-MM}-pia" },
                "pages": [
                    {
                        "source_page_id": 1,
                        "source_name": "2025-11-08-conversation",
                        "detected_date": "2025-11-08",
                        "target_chapter": "2025-11-pia",
                        "target_page": "2025-11-08-pia",
                        "import": true,
                    }
                ],
                "undated_pages": [
                    {
                        "source_page_id": 2,
                        "source_name": "untitled",
                        "detected_date": null,
                        "target_chapter": null,
                        "target_page": null,
                        "import": false,
                    }
                ],
                "estimated_block_count": 1,
            }))),
        };
        let html = render_setup_page(&s, &view, &[]);
        // Dated row: checkbox is pre-checked.
        assert!(
            html.contains("name=\"migration_page_1\" value=\"on\" checked"),
            "dated page checkbox should be pre-checked"
        );
        // Target chapter / page name surfaces in the row.
        assert!(html.contains("2025-11-pia"));
        assert!(html.contains("2025-11-08-pia"));
        // Undated row: NOT pre-checked, plus a date input.
        assert!(html.contains("name=\"migration_page_2\" value=\"on\">"));
        assert!(html.contains("name=\"migration_date_2\""));
        // Import button is the primary action on step 2.
        assert!(html.contains("Import selected pages and complete setup"));
    }

    #[test]
    fn render_setup_page_step1_surfaces_preview_error() {
        // Plan failed (e.g. missing entry_type) — the wizard re-renders
        // step 1 with an error banner so the user can fix the form.
        let s = UserSettings::default();
        let view = MigrationView {
            sources: Ok(json!({
                "sources": [
                    { "book_id": 100, "name": "Pia", "slug": "pia", "page_count": 1, "owned": true },
                ],
            })),
            form: MigrationFormFields {
                book_id: Some(100),
                entry_type: None,
                ..MigrationFormFields::default()
            },
            plan: Some(Err("Missing required argument: entry_type".to_string())),
        };
        let html = render_setup_page(&s, &view, &[]);
        assert!(html.contains("Preview failed"));
        assert!(html.contains("entry_type"));
    }

    #[test]
    fn render_success_page_shows_migration_result_when_present() {
        let html = render_success_page(Some(&Ok(json!({
            "imported": 5,
            "skipped": 1,
            "errors": [
                { "source_page_id": 99, "source_name": "broken", "reason": "undated" }
            ],
        }))));
        assert!(html.contains("Imported"));
        assert!(html.contains("<strong>5</strong>"));
        assert!(html.contains("<strong>1</strong>"));
        // Error list surfaces source name and reason.
        assert!(html.contains("broken"));
        assert!(html.contains("undated"));
    }

    #[test]
    fn render_success_page_omits_migration_block_when_none() {
        let html = render_success_page(None);
        assert!(!html.contains("Migration result"));
        assert!(!html.contains("Migration failed"));
        // Core success message still there.
        assert!(html.contains("Setup complete"));
    }
}
