//! Server-wide settings for the BookStack MCP server.
//!
//! v0.10.0: dropped per-user `UserSettings` and the briefing-only
//! `GlobalSettings` fields. The server is now a BookStack CRUD facade plus
//! semantic search; there is no per-user state to persist. The surviving
//! `GlobalSettings` covers the fields the index worker still needs
//! (`hive_shelf_id`, `user_journals_shelf_id`) plus a single behavior
//! toggle the `/settings` UI exposes.

use serde::{Deserialize, Serialize};

/// Hash a token_id to a stable identifier suitable for use as a database key.
/// SHA-256 hex digest. The raw token_id never appears in storage.
pub fn hash_token_id(token_id: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(token_id.as_bytes());
    format!("{hash:x}")
}

/// Server-instance settings shared by all users on the same BookStack.
///
/// Single-row table. Admin-only writes (the settings UI silently drops
/// non-admin updates).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GlobalSettings {
    /// Identity-shelf id consumed by the index worker's full walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive_shelf_id: Option<i64>,

    /// User-journals-shelf id consumed by the index worker's full walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_journals_shelf_id: Option<i64>,

    /// Hash of the first token_id that set these values (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by_token_hash: Option<String>,

    /// Unix epoch seconds of last update. 0 = never set.
    #[serde(default)]
    pub updated_at: i64,
}
