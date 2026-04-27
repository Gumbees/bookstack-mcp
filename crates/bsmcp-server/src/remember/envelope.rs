//! Shared envelope/meta/warning types for the /remember responses.

use serde_json::{json, Value};

use bsmcp_common::settings::{GlobalSettings, UserSettings};

/// Discriminated error codes — clients can switch on these.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // SemanticUnavailable reserved for explicit "semantic required" calls
pub enum ErrorCode {
    SettingsNotConfigured,
    InvalidArgument,
    UnknownAction,
    BookStackError,
    NotFound,
    SemanticUnavailable,
    InternalError,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SettingsNotConfigured => "settings_not_configured",
            Self::InvalidArgument => "invalid_argument",
            Self::UnknownAction => "unknown_action",
            Self::BookStackError => "bookstack_error",
            Self::NotFound => "not_found",
            Self::SemanticUnavailable => "semantic_unavailable",
            Self::InternalError => "internal_error",
        }
    }
}

/// Soft warning attached to a successful response. Doesn't fail the call.
#[derive(Clone, Debug)]
pub struct RememberWarning {
    pub code: String,
    pub message: String,
}

impl RememberWarning {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

pub fn build_meta(
    trace_id: &str,
    elapsed_ms: u64,
    settings: &UserSettings,
    globals: &GlobalSettings,
    warnings: Vec<RememberWarning>,
) -> Value {
    let mut meta = json!({
        "trace_id": trace_id,
        "elapsed_ms": elapsed_ms,
        "config": {
            "label": settings.label,
            "role": settings.role,
            "ai_identity_name": settings.ai_identity_name,
            "ai_identity_ouid": settings.ai_identity_ouid,
            "user_id": settings.user_id,
        },
        "warnings": warnings.iter().map(|w| json!({
            "code": w.code,
            "message": w.message,
        })).collect::<Vec<_>>(),
    });

    // Compact setup-status pointer — surfaced on every response (not just
    // briefing) so the AI notices misconfiguration even when calling
    // unrelated tools. Only inflates the response when something's pending.
    if let Some(status) = setup_incomplete_summary(settings, globals) {
        meta["setup_incomplete"] = json!(true);
        meta["setup_summary"] = status;
    }

    meta
}

/// Build a one-line setup status when anything's still missing. Returns
/// None when the user + globals are fully configured (so meta stays slim).
fn setup_incomplete_summary(s: &UserSettings, g: &GlobalSettings) -> Option<Value> {
    let user_missing = pending_user_summary(s);
    let global_missing = pending_global_summary(g);
    if user_missing.is_empty() && global_missing.is_empty() {
        return None;
    }
    Some(json!({
        "user_pending": user_missing,
        "global_pending": global_missing,
        "next_step": "Call `remember_briefing action=read` for the full setup_nudge with per-field workflow guidance and pending counts. Each settings_not_configured error from a remember_* tool also includes an `error.fix` block with the exact MCP call to make.",
    }))
}

fn pending_user_summary(s: &UserSettings) -> Vec<&'static str> {
    let mut out = Vec::new();
    if s.user_id.is_none() { out.push("user_id"); }
    if s.ai_identity_page_id.is_none() { out.push("ai_identity_page_id"); }
    if s.ai_hive_journal_book_id.is_none() { out.push("ai_hive_journal_book_id"); }
    if s.user_journal_book_id.is_none() { out.push("user_journal_book_id"); }
    if s.user_identity_page_id.is_none() { out.push("user_identity_page_id"); }
    if s.domains.is_empty() { out.push("domains"); }
    if s.bookstack_user_id.is_none() { out.push("bookstack_user_id"); }
    out
}

fn pending_global_summary(g: &GlobalSettings) -> Vec<&'static str> {
    let mut out = Vec::new();
    if g.hive_shelf_id.is_none() { out.push("hive_shelf_id"); }
    if g.user_journals_shelf_id.is_none() { out.push("user_journals_shelf_id"); }
    if g.org_identity_page_id.is_none() { out.push("org_identity_page_id"); }
    if g.org_domains.is_empty() { out.push("org_domains"); }
    out
}

/// Per-field instructions for a settings_not_configured error. The AI gets
/// this attached to the error envelope so it knows exactly which call to
/// make next. Falls back to a generic pointer when the field isn't in the
/// table (kept narrow so callers don't drift away from the helper).
pub fn fix_for_field(field: &str) -> Value {
    match field {
        "ai_identity_page_id" => json!({
            "summary": "AI identity manifest page is required. Without it the briefing falls back to the org default (if set) and whoami fails.",
            "auto_provision_supported": false,
            "how": [
                "Quickest: visit /settings, pick from the dropdown.",
                "Scaffold a fresh agent: `remember_identity action=create name=<your-agent-name>` creates the Identity book + manifest page in one call, returning the new ID.",
                "Adopt an existing manifest: `remember_directory action=read kind=identities` lists what's on the global Hive shelf, then `remember_config action=write settings={\"ai_identity_page_id\": <id>}`.",
            ],
        }),
        "user_identity_page_id" => json!({
            "summary": "User identity page is required for remember_user write and the identity-refresh nudge.",
            "auto_provision_supported": true,
            "how": [
                "Set `user_id` first via `remember_config action=write settings={\"user_id\": \"you@example.com\"}` (any stable identifier).",
                "Then call `remember_user action=read` — auto-provisions the per-user Identity book + Identity page on the user-journals shelf, stamping the page ID into settings.",
            ],
        }),
        "user_journal_book_id" => json!({
            "summary": "User journal book is required for remember_user_journal read/write/search.",
            "auto_provision_supported": true,
            "how": [
                "Set `user_id` first if not already set.",
                "Call `remember_user action=read` — auto-provisions the journal book on the user-journals shelf alongside the identity page.",
                "Or pick an existing book on /settings (dropdown) and save.",
            ],
        }),
        "ai_hive_journal_book_id" => json!({
            "summary": "AI journal book is required for remember_journal read/write/search/delete.",
            "auto_provision_supported": false,
            "how": [
                "Visit /settings — \"AI Journal & Topics\" card has a dropdown plus a \"create if missing\" checkbox.",
                "Or via MCP: `remember_config action=write settings={\"ai_hive_journal_book_id\": <id>}` if you already have a journal book to adopt.",
            ],
        }),
        "ai_collage_book_id" => json!({
            "summary": "AI topics/collage book is required for remember_collage read/write/search/delete.",
            "auto_provision_supported": false,
            "how": [
                "Visit /settings — \"AI Journal & Topics\" card.",
                "Or `remember_config action=write settings={\"ai_collage_book_id\": <id>}`.",
            ],
        }),
        "ai_shared_collage_book_id" => json!({
            "summary": "Cross-agent shared collage book is required for remember_shared_collage. Optional — only configure if multiple agents share topics.",
            "auto_provision_supported": false,
            "how": [
                "Visit /settings — \"AI Journal & Topics\" card.",
                "Or `remember_config action=write settings={\"ai_shared_collage_book_id\": <id>}`.",
            ],
        }),
        "hive_shelf_id" => json!({
            "summary": "Global Hive shelf is required for remember_directory kind=identities and remember_identity action=create. Admin-only, first-write-wins.",
            "auto_provision_supported": false,
            "admin_only": true,
            "how": [
                "Have an admin visit /settings — \"Global shelves\" card has a dropdown + \"Create Hive shelf\" checkbox.",
                "Or admin-only MCP: `remember_config action=write global_settings={\"hive_shelf_id\": <id>}`.",
            ],
        }),
        "user_journals_shelf_id" => json!({
            "summary": "Global User Journals shelf is required for user auto-provisioning + remember_directory kind=user_journals. Admin-only, first-write-wins.",
            "auto_provision_supported": false,
            "admin_only": true,
            "how": [
                "Have an admin visit /settings — \"Global shelves\" card has a dropdown + \"Create User Journals shelf\" checkbox.",
                "Or admin-only MCP: `remember_config action=write global_settings={\"user_journals_shelf_id\": <id>}`.",
            ],
        }),
        "user_id" => json!({
            "summary": "User ID is the stable identifier (typically email) used to name per-user resources and stamp journal frontmatter.",
            "auto_provision_supported": false,
            "how": [
                "`remember_config action=write settings={\"user_id\": \"you@example.com\"}` — any stable identifier you control.",
            ],
        }),
        "global_settings" => json!({
            "summary": "Global settings can only be written by BookStack admins.",
            "auto_provision_supported": false,
            "admin_only": true,
            "how": [
                "Ask a BookStack admin to visit /settings (admins see additional cards) or to call `remember_config action=write global_settings={...}`.",
            ],
        }),
        _ => json!({
            "summary": format!("`{field}` is not configured."),
            "auto_provision_supported": false,
            "how": [
                "Visit /settings to fill it in via the form.",
                format!("Or via MCP: `remember_config action=write settings={{\"{field}\": <value>}}`."),
                "Run `remember_briefing action=read` for the full setup_nudge with the suggested workflow.",
            ],
        }),
    }
}
