//! User-scoped settings for the Hive memory flow.
//!
//! Settings are keyed by a SHA-256 hash of the user's BookStack token_id, so
//! the raw token never appears in the settings table. All fields are optional;
//! missing fields disable the dependent section of the `remember` response
//! rather than failing the request.

use serde::{Deserialize, Serialize};

/// Per-user configuration for the Hive memory flow.
///
/// All fields are nullable. The `remember` handler checks each field and
/// silently skips the corresponding section if the field is missing or empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserSettings {
    // --- Labelling ---

    /// Free-form label for this BookStack instance (e.g., "DTC", "Bee's Roadhouse").
    /// Surfaced in `remember` response config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Free-form role hint (e.g., "work", "personal"). Surfaced in `remember` response config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    // --- AI identity ---

    /// Stable identifier for the AI agent (ULID, UUID, whatever the user picks).
    /// Echoed back in the `remember` response identity block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_ouid: Option<String>,

    /// BookStack book ID for the AI's Identity book. Container for the manifest page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_book_id: Option<i64>,

    /// BookStack page ID of the AI agent's identity manifest (lives inside the Identity book).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_page_id: Option<i64>,

    /// Optional friendly name for the AI agent (defaults to the manifest page name if unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_name: Option<String>,

    /// BookStack shelf ID containing the AI's Hive (informational; surfaced in config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hive_shelf_id: Option<i64>,

    /// BookStack book ID for the AI's Topics/Collage book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_collage_book_id: Option<i64>,

    /// BookStack book ID for cross-agent shared collage (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_shared_collage_book_id: Option<i64>,

    /// **Deprecated, kept for migration:** BookStack book ID for the legacy
    /// per-identity Journal book. v1.0.0 collapsed this into a chapter inside
    /// `ai_identity_book_id` — see `ai_identity_journal_chapter_id`. Stays in
    /// the schema as a tombstone so `remember_migrate` and the briefing's
    /// `setup_nudge` legacy detector can recognize an un-migrated identity.
    /// Cleared automatically once `remember_migrate action=apply` finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hive_journal_book_id: Option<i64>,

    /// BookStack chapter ID of the `Agents` chapter inside the AI's Identity
    /// book. Where agent definition pages (`Agent: <name>`) live. Required
    /// for write paths that scaffold sub-agent definitions; reads degrade
    /// gracefully when null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_agents_chapter_id: Option<i64>,

    /// BookStack chapter ID of the `Subagent Conversations` chapter inside
    /// the AI's Identity book. Where future agent-to-agent transcripts will
    /// land. Scaffolded empty at identity creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_subagent_conversations_chapter_id: Option<i64>,

    /// BookStack chapter ID of the current-year `Journal` chapter inside the
    /// AI's Identity book. Daily entries (`YYYY-MM-DD`) write here. The
    /// year-rollover sweep (run on every `journal action=write`) moves any
    /// stale-year pages into a `Journal Archive - {YEAR}` chapter
    /// (find-or-created lazily, scoped strictly within
    /// `ai_identity_book_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_journal_chapter_id: Option<i64>,

    // --- User identity ---

    /// Stable identifier for the human user (e.g., email).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// BookStack user row ID for the calling token's owner. Set automatically
    /// at /authorize when an admin token can resolve it via `/api/users`, or
    /// manually via /settings. When set, semantic search resolves the user's
    /// role IDs once per session (cached in `user_role_cache`) and applies a
    /// role-level ACL filter to vector candidates — eliminating the per-page
    /// HTTP fan-out for pages we already know the user can or cannot view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bookstack_user_id: Option<i64>,

    /// BookStack page ID of the user's identity page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_identity_page_id: Option<i64>,

    /// BookStack book ID of the user's per-user identity book (where the
    /// identity page + journal-agent definition page live). Auto-provisioned
    /// on the user-journals shelf when the user first calls `remember_user`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_identity_book_id: Option<i64>,

    /// BookStack page ID of the auto-provisioned `{user_id}-journal-agent`
    /// agent definition page (lives in the user's identity book).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journal_agent_page_id: Option<i64>,

    /// BookStack book ID of the user's personal journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journal_book_id: Option<i64>,

    /// Domains owned by the user (e.g. `["example.com"]`). Surfaced in the
    /// briefing's `system_prompt_additions` so the AI can distinguish "ours"
    /// (URLs/emails on these domains) from external content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,

    // --- Semantic search toggles (default true except full_kb) ---

    #[serde(default = "default_true")]
    pub semantic_against_journal: bool,

    #[serde(default = "default_true")]
    pub semantic_against_collage: bool,

    #[serde(default = "default_true")]
    pub semantic_against_shared_collage: bool,

    #[serde(default = "default_true")]
    pub semantic_against_user_journal: bool,

    /// Opt-in: search the entire knowledge base. Expensive; off by default.
    #[serde(default)]
    pub semantic_against_full_kb: bool,

    // --- Behavior toggles ---

    /// If true, the response indicates the AI should run a follow-up
    /// reconstitution agent after consuming the structured pull.
    #[serde(default)]
    pub use_follow_up_remember_agent: bool,

    /// BookStack page IDs to load verbatim into the briefing's
    /// `system_prompt_additions` array. Intended for short, durable context
    /// the AI should always carry — writing style guides, communication
    /// preferences, formatting rules, ethical constraints, etc.
    ///
    /// Recommended: keep each page short (< 500 words). The full markdown body
    /// of every listed page is included in every briefing response. Long pages
    /// will inflate every response and cost tokens.
    #[serde(default)]
    pub system_prompt_page_ids: Vec<i64>,

    /// Number of recent journal entries to include (default 3).
    #[serde(default = "default_recent_count")]
    pub recent_journal_count: usize,

    /// Number of active collage entries to include (default 10).
    #[serde(default = "default_collage_count")]
    pub active_collage_count: usize,

    /// User's IANA timezone name (e.g., "America/New_York"). Surfaced in the
    /// briefing's `time` block so the AI can format timestamps in the user's
    /// local time. If unset, the briefing reports UTC.
    ///
    /// Auto-populated by the briefing when the client passes
    /// `client_timezone`; manually editable on /settings; refreshed every
    /// `TIMEZONE_REFRESH_SECS` so DST transitions and travel are picked up
    /// without forcing the user to re-save settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,

    /// Unix epoch seconds the timezone was last set/refreshed by the client.
    /// The briefing surfaces `timezone_refresh_due: true` when the cache is
    /// older than 4h so the client knows to re-detect and pass `client_timezone`
    /// on the next call. Manually-set values (via /settings) get a fresh
    /// fetched_at too — no special casing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone_fetched_at: Option<i64>,

    /// Unix epoch seconds until which the briefing's "configure your settings"
    /// nudge is snoozed. When `now < this`, the nudge is suppressed. Set via
    /// `remember_config action=dismiss_setup_nudge days=N`. Auto-becomes
    /// irrelevant once any user setting is configured (the nudge predicate
    /// already returns false in that case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_nudge_dismissed_until: Option<i64>,
}

fn default_true() -> bool {
    true
}

fn default_recent_count() -> usize {
    3
}

fn default_collage_count() -> usize {
    10
}

impl UserSettings {
    /// Returns true if the settings have at least one configured field.
    /// A fully-empty settings record means the user has never visited /settings.
    pub fn is_configured(&self) -> bool {
        self.ai_identity_page_id.is_some()
            || self.ai_collage_book_id.is_some()
            || self.ai_hive_journal_book_id.is_some()
            || self.user_identity_page_id.is_some()
            || self.user_journal_book_id.is_some()
    }
}

/// Hash a token_id to a stable identifier suitable for use as a database key.
/// SHA-256 hex digest. The raw token_id never appears in the settings table.
pub fn hash_token_id(token_id: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(token_id.as_bytes());
    format!("{hash:x}")
}

/// Server-instance-level settings shared by all users on the same BookStack.
///
/// Stored as a single-row table. `set_by_token_hash` records the first user who
/// configured the field; the UI uses this to render pre-set values as read-only
/// for subsequent users (first-write-wins; DB allows overwrites).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GlobalSettings {
    /// Shared shelf containing every AI agent's Identity book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive_shelf_id: Option<i64>,

    /// Shared shelf containing every human user's journal book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journals_shelf_id: Option<i64>,

    /// Org-wide default AI identity manifest page ID. When a user has not set
    /// their own `ai_identity_page_id`, the briefing / whoami response falls
    /// back to this. Lets an admin stand up a "house" agent that any new user
    /// gets automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ai_identity_page_id: Option<i64>,

    /// Default AI identity display name (paired with the page above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ai_identity_name: Option<String>,

    /// Default AI identity OUID (paired with the page above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ai_identity_ouid: Option<String>,

    /// Org-mandated instruction pages — included verbatim in every briefing
    /// response. Use for content that EVERY agent on this BookStack must
    /// follow regardless of who they are (compliance directives, escalation
    /// rules, etc.). Admin-only.
    ///
    /// Page IDs only (not chapters) — keeps the response size predictable and
    /// auditable. If the policy set evolves frequently, an admin updates this
    /// list rather than letting a chapter's contents drift into every briefing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_required_instructions_page_ids: Vec<i64>,

    /// Org AI-usage policy pages — included verbatim in every briefing.
    /// Same shape as org_required_instructions but tagged separately so the
    /// AI can distinguish "policy" (what we're allowed/required to do) from
    /// "instructions" (how to act). Admin-only. Page IDs only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_ai_usage_policy_page_ids: Vec<i64>,

    /// Single page describing the organization itself (mission, structure,
    /// people, conventions). Pulled verbatim into every briefing's
    /// `system_prompt_additions` under an `## Organization` section. Pairs
    /// with `org_domains` to give every agent on the instance a shared baseline.
    /// Admin-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_identity_page_id: Option<i64>,

    /// Domains owned by the org (e.g. `["example.com", "example.net"]`).
    /// Surfaced in every briefing's `system_prompt_additions`. Admin-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub org_domains: Vec<String>,

    /// Hash of the first token_id that set these values (informational; does
    /// not gate writes — UI handles the lock-after-set semantics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by_token_hash: Option<String>,

    /// Unix epoch seconds of last update. 0 = never set.
    #[serde(default)]
    pub updated_at: i64,
}

impl GlobalSettings {
    /// Resolve the AI identity for a user — user's own settings take precedence,
    /// org defaults fill in any nulls. Returns the page_id, name, ouid triple.
    pub fn resolve_identity(&self, user: &UserSettings) -> ResolvedIdentity {
        ResolvedIdentity {
            page_id: user.ai_identity_page_id.or(self.default_ai_identity_page_id),
            name: user.ai_identity_name.clone().or_else(|| self.default_ai_identity_name.clone()),
            ouid: user.ai_identity_ouid.clone().or_else(|| self.default_ai_identity_ouid.clone()),
            using_default: user.ai_identity_page_id.is_none()
                && self.default_ai_identity_page_id.is_some(),
        }
    }
}

/// Output of [`GlobalSettings::resolve_identity`] — the AI identity to use for
/// a request after applying the user → org-default fallback chain.
#[derive(Clone, Debug)]
pub struct ResolvedIdentity {
    pub page_id: Option<i64>,
    pub name: Option<String>,
    pub ouid: Option<String>,
    /// True when the resolved page_id came from the org default (not the user).
    /// Surfaced in the briefing response so the AI knows it's running on the
    /// house identity rather than its own configured one.
    pub using_default: bool,
}
