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

/// Per-user settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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

    /// v0.7.x leftover keys captured on deserialize. Round-trips through
    /// saves until the briefing builder explicitly clears them — that way an
    /// unrelated save path (oauth auto-populate, settings UI, dismiss tool)
    /// can't silently nuke the user's legacy data before the migration
    /// warning has fired. The briefing's migration handler clears this field
    /// after surfacing the warning, on the same call.
    #[serde(default, flatten)]
    pub extras: std::collections::HashMap<String, Value>,
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
            set_by_token_hash: None,
            updated_at: 0,
        }
    }
}

fn default_true() -> bool {
    true
}
