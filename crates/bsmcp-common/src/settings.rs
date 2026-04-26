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

    /// BookStack book ID for the AI's Identity book. Container for the manifest page
    /// plus the Connections, Opportunities, and Subagents chapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_book_id: Option<i64>,

    /// BookStack page ID of the AI agent's identity manifest (lives inside the Identity book).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_page_id: Option<i64>,

    /// Optional friendly name for the AI agent (defaults to the manifest page name if unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_identity_name: Option<String>,

    /// BookStack chapter ID containing subagent definition pages (inside Identity book).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_subagents_chapter_id: Option<i64>,

    /// BookStack chapter ID containing connection pages (people/agents met). Inside Identity book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_connections_chapter_id: Option<i64>,

    /// BookStack chapter ID containing opportunity pages. Inside Identity book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_opportunities_chapter_id: Option<i64>,

    /// BookStack shelf ID containing the AI's Hive (informational; surfaced in config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hive_shelf_id: Option<i64>,

    /// BookStack book ID for the AI's Topics/Collage book.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_collage_book_id: Option<i64>,

    /// BookStack book ID for cross-agent shared collage (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_shared_collage_book_id: Option<i64>,

    /// BookStack book ID for the AI's journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_hive_journal_book_id: Option<i64>,

    /// BookStack chapter ID for the activity feed — sits inside the Journal book,
    /// listed *before* the YYYY-MM date chapters. Conversations, social events,
    /// and other append-only activity entries land here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_activity_chapter_id: Option<i64>,

    // --- User identity ---

    /// Stable identifier for the human user (e.g., email).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    /// BookStack page ID of the user's identity page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_identity_page_id: Option<i64>,

    /// BookStack book ID of the user's personal journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journal_book_id: Option<i64>,

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
