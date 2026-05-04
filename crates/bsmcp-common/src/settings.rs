//! User-scoped + global settings for the BookStack MCP server.
//!
//! v0.8.0: dropped the personal-memory pointers (AI identity / journals /
//! collages / user identity) — those move to memberberry.ai. Added typed
//! setup slots (guide_page_id, org_identity_page_id, policies/sops/
//! best_practices scopes) on the global side.
//!
//! Per-user settings keyed by SHA-256 hash of the BookStack token_id; raw
//! token never appears in storage. Global settings are a single-row table
//! shared by all users on the same BookStack instance.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default value for `UserSettings.account_label`. A new field on a
/// row-without-the-key deserializes to this; constructing via
/// `UserSettings::default()` also lands here.
pub const DEFAULT_ACCOUNT_LABEL: &str = "default";

fn default_account_label() -> String { DEFAULT_ACCOUNT_LABEL.to_string() }
fn default_use_org_identity() -> bool { true }

/// Per-user settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSettings {
    /// Free-form label for this BookStack instance (e.g. "DTC", "Bee's Roadhouse").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Free-form role hint (e.g. "work", "personal").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    /// Stable identifier for the human user (typically email). Recorded in
    /// audit/log entries; used as the `user=` token in semantic-search query
    /// enrichment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// BookStack user row ID for the calling token's owner. When set, semantic
    /// search applies role-level ACL filtering and the MCP `tools/list` call
    /// filters tools by the user's BookStack role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bookstack_user_id: Option<i64>,

    /// Per-account-personality label. Combined with `bookstack_user_id` this
    /// is the **stable identity key** that survives BookStack API token
    /// rotations — the `token_bindings` table maps `token_id_hash` to
    /// `(bookstack_user_id, account_label)`, and `user_settings` rows live
    /// against that pair instead of the raw token hash.
    ///
    /// Single-account users never see this — defaults to `"default"`. Users
    /// running the same BookStack as two different personalities (e.g. one
    /// MCP wired to the DTC Anthropic account, one to the personal account)
    /// distinguish them with labels like `"dtc"` and `"personal"`. The
    /// `/setup/user` wizard offers existing labels to pick from on first
    /// authentication of a new token, so token rotation preserves settings.
    #[serde(default = "default_account_label")]
    pub account_label: String,

    /// Domains owned by the user. Surfaced in the briefing's
    /// `system_prompt_additions` so the AI can distinguish "ours" (URLs/emails
    /// on these domains) from external content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,

    /// Free-form fallback for the typed setup slots (`global_settings.guide_page_id`
    /// etc.). Page IDs listed here get loaded verbatim into every briefing's
    /// `system_prompt_additions` array. Use for short, durable per-user context
    /// (writing style, preferences) that doesn't fit one of the typed slots.
    #[serde(default)]
    pub system_prompt_page_ids: Vec<i64>,

    /// Opt-in: search the entire knowledge base on semantic queries. Expensive
    /// for large instances; off by default.
    #[serde(default)]
    pub semantic_against_full_kb: bool,

    /// User's IANA timezone name. Auto-populated by the briefing when the
    /// client passes `client_timezone`; refreshed every `TIMEZONE_REFRESH_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,

    /// Unix epoch seconds the timezone was last set/refreshed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone_fetched_at: Option<i64>,

    /// Unix epoch seconds until which the briefing's "configure your settings"
    /// nudge is snoozed. Written by `config dismiss_setup_nudge` and
    /// the legacy `dismiss_setup_nudge` MCP tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_nudge_dismissed_until: Option<i64>,

    /// Free-form per-user K/V store. Written by `config write` for
    /// arbitrary keys that don't fit one of the typed slots above (e.g.
    /// per-Hive shelf/book/page IDs locked in by the setup workflow). Distinct
    /// from `extras` — these are deliberate writes, not v0.7.x roundtrip
    /// leftovers.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub config_extras: std::collections::HashMap<String, String>,

    // --- Journal-resolver caches (Phase 2.3) ---
    //
    // Populated lazily by `crate::remember::resolvers` so the journal
    // endpoints landing in 2.4 don't pay BookStack round-trips on every
    // write. All three are TTL-refreshed; resolvers re-fetch when the
    // *_fetched_at watermark is older than the per-field threshold.

    /// Cached BookStack book ID for this user's per-user "Journal" book on
    /// the global `user_journals_shelf_id`. Populated by
    /// `resolve_user_journal_book` after find-or-create. Cleared by the
    /// resolver when the cached ID stops resolving (book deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journal_book_id: Option<i64>,

    /// Cached BookStack user email. The user's Journal book is named
    /// exactly by this email, so the resolver caches it to avoid hitting
    /// `/api/users/{id}` on every journal write. Refreshed every 7 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_user_email: Option<String>,

    /// Unix epoch seconds the cached email was last fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_user_email_fetched_at: Option<i64>,

    /// Cached first-name token (whitespace-split [0]) of the BookStack
    /// `users.name` field. Used by 2.4 chapter/page naming. Refreshed
    /// every 24 hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_first_name: Option<String>,

    /// Unix epoch seconds the cached first name was last fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_first_name_fetched_at: Option<i64>,

    // --- Journaling toggle (Phase 2.4c) ---
    //
    // Surfaced through the briefing as a per-session reminder. The journal
    // tool itself is always available; this flag only controls whether the
    // briefing nudges the AI to use it. Off by default — a user opts in via
    // `user write` once they've decided journaling fits their workflow.

    /// When true, the briefing payload appends a "remember to journal …"
    /// reminder AND the `journal` and `identity` (target=agent) write tools
    /// will accept writes on this instance. When false, the reminder is
    /// silent and write attempts return `Forbidden` with a "journaling not
    /// enabled on this instance" message.
    ///
    /// This is the per-instance "is this MCP a journaling target?" toggle.
    /// Multi-instance setups (e.g. one personal MCP, one DTC MCP wired to
    /// the same Claude session) flip it on for the primary and leave it off
    /// for bootstrap-only sources. Default `false` so an unconfigured
    /// instance never accidentally accepts journal/identity writes.
    #[serde(default)]
    pub journaling_enabled: bool,

    /// Inject `globals.org_identity_page_id` (when admin-configured) into
    /// this user's `system_prompt_additions`. Default `true` — the
    /// admin-set org identity applies by default. Users who don't want the
    /// org's canonical identity bound to their session (e.g. a DTC-employed
    /// contractor whose primary identity lives on their personal MCP) flip
    /// this off and the org_identity entry is omitted from their briefing.
    #[serde(default = "default_use_org_identity")]
    pub use_org_identity: bool,

    /// User's preferred AI identity name. When set, the briefing's
    /// journaling reminder addresses the agent by this name even if the
    /// caller didn't pass an `agent_name` on the briefing call. Normalized
    /// to whatever the user set — not auto-lowercased here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_ai_identity: Option<String>,

    // --- Per-tool enable/disable (Phase 2.4d) ---
    //
    // Map of tool_name -> enabled flag. Absent key = fall back to
    // `GlobalSettings.tool_defaults` (which itself defaults ON for absent
    // keys). The user-level override always wins over the global default.
    // Empty map encoded as omitted JSON so v0.7.x rows decode unchanged.
    /// Per-tool enable/disable overrides. Keyed by MCP tool name (the
    /// `name` field of `tool_definitions`). `true` forces on, `false`
    /// forces off, absent = use the global default. Read by
    /// `bsmcp_common::settings::is_tool_enabled`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tool_overrides: std::collections::HashMap<String, bool>,

    // --- Admin status cache (Phase 2.4f) ---
    //
    // Cached "is this user a BookStack admin" bit, used by the
    // `meta.admin_onboarding_pending` injection on every MCP response and
    // by the `/setup/admin` POST guard. Refreshed every 24 hours. `None`
    // means "never fetched / unknown" — callers err on the side of NOT
    // injecting the admin nudge, so non-admins don't get nagged.

    /// Cached result of `is_bookstack_admin` — true if the user's BookStack
    /// role list contains a role with `system_name == "admin"`. Refreshed
    /// every 24 hours; populated lazily on the first MCP response after the
    /// cache expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_is_admin: Option<bool>,

    /// Unix epoch seconds the `cached_is_admin` value was last fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_is_admin_fetched_at: Option<i64>,

    // --- Onboarding (Phase 2.4e) ---
    //
    // True once the user has completed the `/setup/user` onboarding wizard.
    // Drives `meta.onboarding_pending` injection on every MCP response: the
    // wizard link rides along on every tool call until this flips. Distinct
    // from the briefing's "settings fields complete" heuristic — that one
    // checks whether typed settings slots have values; this one is set
    // exactly once when the user submits the wizard form.
    /// True once the user has submitted the `/setup/user` onboarding form.
    /// Until then, every MCP tool response carries `meta.onboarding_pending`
    /// pointing at the wizard URL. Set by `setup_ui::handle_setup_post`;
    /// never cleared except by manually editing the row.
    #[serde(default)]
    pub setup_complete: bool,

    /// v0.7.x leftover keys captured on deserialize. Round-trips through
    /// saves until the briefing builder explicitly clears them — that way an
    /// unrelated save path (oauth auto-populate, settings UI, dismiss tool)
    /// can't silently nuke the user's legacy data before the migration
    /// warning has fired. The briefing's migration handler clears this field
    /// after surfacing the warning, on the same call.
    #[serde(default, flatten)]
    pub extras: std::collections::HashMap<String, Value>,
}

impl Default for UserSettings {
    /// Manual `Default` impl rather than `#[derive]` because `account_label`
    /// must default to `"default"` (not `""`) and `use_org_identity` to
    /// `true` (not `false`). Every other field matches its serde-default.
    fn default() -> Self {
        Self {
            label: None,
            role: None,
            user_id: None,
            bookstack_user_id: None,
            account_label: default_account_label(),
            domains: Vec::new(),
            system_prompt_page_ids: Vec::new(),
            semantic_against_full_kb: false,
            timezone: None,
            timezone_fetched_at: None,
            settings_nudge_dismissed_until: None,
            config_extras: std::collections::HashMap::new(),
            user_journal_book_id: None,
            cached_user_email: None,
            cached_user_email_fetched_at: None,
            cached_first_name: None,
            cached_first_name_fetched_at: None,
            journaling_enabled: false,
            use_org_identity: default_use_org_identity(),
            chosen_ai_identity: None,
            tool_overrides: std::collections::HashMap::new(),
            cached_is_admin: None,
            cached_is_admin_fetched_at: None,
            setup_complete: false,
            extras: std::collections::HashMap::new(),
        }
    }
}

impl UserSettings {
    /// Returns true if the settings have at least one configured field.
    pub fn is_configured(&self) -> bool {
        self.user_id.is_some()
            || self.bookstack_user_id.is_some()
            || !self.domains.is_empty()
            || !self.system_prompt_page_ids.is_empty()
            || self.timezone.is_some()
    }
}

/// Hash a token_id to a stable identifier suitable for use as a database key.
/// SHA-256 hex digest. The raw token_id never appears in the settings table.
pub fn hash_token_id(token_id: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(token_id.as_bytes());
    format!("{hash:x}")
}

/// Reference to a BookStack KB region — used by the typed setup slots.
/// Serializes as `{"type": "shelf|book|page", "id": <i64>}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "lowercase")]
pub enum KbScope {
    Shelf(i64),
    Book(i64),
    Page(i64),
}

impl KbScope {
    pub fn id(&self) -> i64 {
        match self {
            Self::Shelf(id) | Self::Book(id) | Self::Page(id) => *id,
        }
    }
}

/// Server-instance settings shared by all users on the same BookStack.
///
/// Single-row table. Most fields are admin-only writes (the settings UI
/// silently drops them on non-admin saves).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobalSettings {
    // --- Typed setup slots ---

    /// Page describing how to use this BookStack with this MCP server. When
    /// configured, the briefing auto-includes its full markdown in
    /// `system_prompt_additions` so the AI sees it on every session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guide_page_id: Option<i64>,

    /// Single page describing the organization (mission, structure,
    /// conventions). Pulled into every briefing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_identity_page_id: Option<i64>,

    /// Scope (shelf / book / page) for the org's policy content. Used by
    /// semantic search to bias toward policy hits when relevant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policies_scope: Option<KbScope>,

    /// Scope for the org's standard operating procedures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sops_scope: Option<KbScope>,

    /// Scope for the org's best-practices documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_practices_scope: Option<KbScope>,

    // --- Always-on context lists ---

    /// Org-mandated instruction pages. Included verbatim in every briefing.
    /// Use for content every agent must follow regardless of who they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_required_instructions_page_ids: Vec<i64>,

    /// Org AI-usage policy pages. Tagged separately from instructions so the
    /// AI can distinguish "policy" from "how-to."
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_ai_usage_policy_page_ids: Vec<i64>,

    /// Domains the org owns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_domains: Vec<String>,

    // --- Org-wide booleans ---

    /// Briefing/responses include human-friendly headings and summaries when
    /// true. Off = terse machine-readable form.
    #[serde(default = "default_true")]
    pub friendly_structure: bool,

    /// If true, briefing includes full bodies of system_prompt_additions
    /// pages inline. If false, only IDs + names + summaries.
    #[serde(default)]
    pub full_content_in_briefing: bool,

    /// When true, MCP tool calls return a `setup_required` error envelope
    /// instead of soft warnings until setup is complete. Default soft.
    #[serde(default)]
    pub strict_setup: bool,

    // --- Index worker (kept for the reconciliation worker; semantic until
    //     the typed scopes fully replace these) ---

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive_shelf_id: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journals_shelf_id: Option<i64>,

    // --- Per-tool defaults (Phase 2.4d) ---

    /// Admin-set per-tool default enabled flag. Keyed by MCP tool name
    /// (the `name` field of `tool_definitions`). `true` forces on for all
    /// users (subject to per-user override), `false` forces off, absent =
    /// default ON. Read by `bsmcp_common::settings::is_tool_enabled`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub tool_defaults: std::collections::HashMap<String, bool>,

    // --- Admin onboarding (Phase 2.4f) ---
    //
    // True once any BookStack admin has submitted the `/setup/admin` wizard.
    // Drives `meta.admin_onboarding_pending` injection: while false AND the
    // calling user is a BookStack admin, every MCP tool response carries an
    // admin onboarding nudge. "Run once" semantics — the first admin to
    // complete it flips this for everyone.
    /// True once any admin has submitted the `/setup/admin` wizard.
    /// Distinct from `UserSettings.setup_complete` (per-user) — this is a
    /// single global bit. Set by `setup_ui::handle_setup_admin_post`;
    /// `BSMCP_FORCE_ADMIN_SETUP=1` env override ignores this for ops
    /// recovery (restored backup, re-onboarding).
    #[serde(default)]
    pub admin_setup_complete: bool,

    // --- Bookkeeping ---

    /// Hash of the first token_id that set these values (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by_token_hash: Option<String>,

    /// Unix epoch seconds of last update. 0 = never set.
    #[serde(default)]
    pub updated_at: i64,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            guide_page_id: None,
            org_identity_page_id: None,
            policies_scope: None,
            sops_scope: None,
            best_practices_scope: None,
            org_required_instructions_page_ids: Vec::new(),
            org_ai_usage_policy_page_ids: Vec::new(),
            org_domains: Vec::new(),
            friendly_structure: true,
            full_content_in_briefing: false,
            strict_setup: false,
            hive_shelf_id: None,
            user_journals_shelf_id: None,
            tool_defaults: std::collections::HashMap::new(),
            admin_setup_complete: false,
            set_by_token_hash: None,
            updated_at: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Decide whether a tool is enabled for a given (user, global) settings pair.
///
/// Resolution order:
/// 1. `user_settings.tool_overrides[name]` — user-level override always wins.
/// 2. `global_settings.tool_defaults[name]` — admin-set default.
/// 3. `true` — tools default ON when neither side has an opinion.
///
/// Pure (no I/O); safe to call from any thread. Used by both the MCP
/// `tools/list` filter and the `execute_tool` defense-in-depth guard, plus
/// the briefing meta-injection's `briefing` self-check.
pub fn is_tool_enabled(
    tool_name: &str,
    user_settings: &UserSettings,
    global_settings: &GlobalSettings,
) -> bool {
    if let Some(&v) = user_settings.tool_overrides.get(tool_name) {
        return v;
    }
    if let Some(&v) = global_settings.tool_defaults.get(tool_name) {
        return v;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_with(overrides: &[(&str, bool)]) -> UserSettings {
        UserSettings {
            tool_overrides: overrides.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
            ..UserSettings::default()
        }
    }

    fn global_with(defaults: &[(&str, bool)]) -> GlobalSettings {
        GlobalSettings {
            tool_defaults: defaults.iter().map(|(k, v)| ((*k).to_string(), *v)).collect(),
            ..GlobalSettings::default()
        }
    }

    #[test]
    fn is_tool_enabled_defaults_on_when_neither_set() {
        let u = UserSettings::default();
        let g = GlobalSettings::default();
        assert!(is_tool_enabled("anything", &u, &g));
    }

    #[test]
    fn is_tool_enabled_global_off_propagates() {
        let u = UserSettings::default();
        let g = global_with(&[("journal", false)]);
        assert!(!is_tool_enabled("journal", &u, &g));
        // Other tools unaffected.
        assert!(is_tool_enabled("identity", &u, &g));
    }

    #[test]
    fn is_tool_enabled_user_override_on_beats_global_off() {
        let u = user_with(&[("journal", true)]);
        let g = global_with(&[("journal", false)]);
        assert!(is_tool_enabled("journal", &u, &g));
    }

    #[test]
    fn is_tool_enabled_user_override_off_beats_global_on() {
        let u = user_with(&[("journal", false)]);
        let g = global_with(&[("journal", true)]);
        assert!(!is_tool_enabled("journal", &u, &g));
    }

    #[test]
    fn is_tool_enabled_user_override_off_beats_default_on() {
        let u = user_with(&[("briefing", false)]);
        let g = GlobalSettings::default();
        assert!(!is_tool_enabled("briefing", &u, &g));
    }

    #[test]
    fn user_settings_round_trips_tool_overrides() {
        let s = user_with(&[("journal", false), ("identity", true)]);

        let json = serde_json::to_string(&s).expect("serialize");
        let back: UserSettings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.tool_overrides.get("journal"), Some(&false));
        assert_eq!(back.tool_overrides.get("identity"), Some(&true));
    }

    #[test]
    fn user_settings_decodes_legacy_row_without_tool_overrides() {
        // v0.7.x rows have no tool_overrides key. Must decode cleanly.
        let json = r#"{"label":"DTC","role":"work"}"#;
        let s: UserSettings = serde_json::from_str(json).expect("legacy decode");
        assert!(s.tool_overrides.is_empty());
        assert_eq!(s.label.as_deref(), Some("DTC"));
    }

    #[test]
    fn global_settings_round_trips_tool_defaults() {
        let g = global_with(&[("journal", false), ("identity", true)]);

        let json = serde_json::to_string(&g).expect("serialize");
        let back: GlobalSettings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.tool_defaults.get("journal"), Some(&false));
        assert_eq!(back.tool_defaults.get("identity"), Some(&true));
    }

    #[test]
    fn global_settings_decodes_without_tool_defaults_field() {
        // Pre-2.4d global row encoded without the column / field.
        let json = r#"{"updated_at": 0}"#;
        let g: GlobalSettings = serde_json::from_str(json).expect("legacy decode");
        assert!(g.tool_defaults.is_empty());
    }

    #[test]
    fn empty_tool_defaults_serializes_compact() {
        // Empty map should be omitted from JSON output (skip_serializing_if).
        let g = GlobalSettings::default();
        let json = serde_json::to_string(&g).expect("serialize");
        assert!(
            !json.contains("tool_defaults"),
            "expected empty tool_defaults to be omitted, got: {json}"
        );
    }

    #[test]
    fn is_tool_enabled_handles_explicit_user_true_with_no_global() {
        let u = user_with(&[("identity", true)]);
        let g = GlobalSettings::default();
        assert!(is_tool_enabled("identity", &u, &g));
    }

    // --- Admin onboarding (Phase 2.4f) ---

    #[test]
    fn global_settings_decodes_without_admin_setup_complete() {
        // Pre-2.4f rows have no admin_setup_complete key. Must decode
        // cleanly with the field defaulting to false.
        let json = r#"{"updated_at": 0}"#;
        let g: GlobalSettings = serde_json::from_str(json).expect("legacy decode");
        assert!(!g.admin_setup_complete);
    }

    #[test]
    fn global_settings_round_trips_admin_setup_complete() {
        let mut g = GlobalSettings::default();
        g.admin_setup_complete = true;
        let json = serde_json::to_string(&g).expect("serialize");
        let back: GlobalSettings = serde_json::from_str(&json).expect("deserialize");
        assert!(back.admin_setup_complete);
    }

    #[test]
    fn user_settings_decodes_without_cached_is_admin() {
        // Pre-2.4f rows have no cached_is_admin key. Must decode cleanly
        // with both fields defaulting to None.
        let json = r#"{"label":"DTC"}"#;
        let s: UserSettings = serde_json::from_str(json).expect("legacy decode");
        assert!(s.cached_is_admin.is_none());
        assert!(s.cached_is_admin_fetched_at.is_none());
    }

    #[test]
    fn user_settings_round_trips_cached_is_admin() {
        let mut s = UserSettings::default();
        s.cached_is_admin = Some(true);
        s.cached_is_admin_fetched_at = Some(1_700_000_000);
        let json = serde_json::to_string(&s).expect("serialize");
        let back: UserSettings = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.cached_is_admin, Some(true));
        assert_eq!(back.cached_is_admin_fetched_at, Some(1_700_000_000));
    }
}
