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
