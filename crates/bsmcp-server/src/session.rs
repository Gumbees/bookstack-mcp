//! Per-session briefing state.
//!
//! v0.8.0: tracks `(token_hash, session_id) -> {first_call_seen_at,
//! last_briefing_sent_at, sticky_version}` so the briefing-meta-injection on
//! every MCP tool response can decide between full content (first call) and
//! sticky-only (subsequent calls).
//!
//! Streamable HTTP is stateless, so the session_id comes from the client.
//! When absent, fall back to a per-hour bucket keyed by token_hash so the
//! user gets full briefing roughly once per hour instead of once per
//! conversation. The compaction tool (`session_event`) lets the AI signal
//! "I just compacted" so the next request is treated as a first call.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

/// Sessions older than this are evicted by the cleanup task.
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum sessions tracked in-memory. When the cap is hit, the cleanup loop
/// evicts the oldest. Single-instance store; if we ever go multi-instance,
/// move to Postgres.
#[allow(dead_code)]
const MAX_SESSIONS: usize = 10_000;

#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub first_call_seen_at: Instant,
    #[allow(dead_code)]
    pub last_briefing_sent_at: Instant,
    /// Bumped when the AI calls `session_event action=compacted` to force
    /// the next response to inject the full briefing again.
    #[allow(dead_code)]
    pub sticky_version: u64,
    /// True until the first response carrying meta.briefing has been sent
    /// for this session. Flipped to false after the first send.
    pub needs_full_briefing: bool,
}

impl SessionEntry {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            first_call_seen_at: now,
            last_briefing_sent_at: now,
            sticky_version: 0,
            needs_full_briefing: true,
        }
    }
}

/// `(token_hash, session_id)` keyed map of session state.
pub type SessionStore = Arc<RwLock<HashMap<String, SessionEntry>>>;

pub fn new_store() -> SessionStore {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Compute the session key. Prefer the client-supplied session_id; fall back
/// to a per-hour bucket per token so absent-id callers still get a coarse
/// "first call this hour" notion.
#[allow(dead_code)]
pub fn session_key(token_hash: &str, session_id: Option<&str>) -> String {
    match session_id {
        Some(s) if !s.is_empty() => format!("{token_hash}:{s}"),
        _ => {
            let hour = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() / 3600)
                .unwrap_or(0);
            format!("{token_hash}:bucket-{hour}")
        }
    }
}

/// Mark a session call. Returns true if this is the first call for the
/// session (caller should inject full briefing) or false (sticky-only).
#[allow(dead_code)]
pub async fn record_call(store: &SessionStore, key: &str) -> bool {
    let mut sessions = store.write().await;
    if sessions.len() >= MAX_SESSIONS {
        let cutoff = Instant::now() - SESSION_TTL;
        sessions.retain(|_, e| e.first_call_seen_at > cutoff);
    }
    let entry = sessions.entry(key.to_string()).or_insert_with(SessionEntry::new);
    let was_first = entry.needs_full_briefing;
    entry.needs_full_briefing = false;
    entry.last_briefing_sent_at = Instant::now();
    was_first
}

/// Reset a session so the next call gets a fresh full briefing.
/// Called by the `session_event` MCP tool on `action=compacted` or `reset`.
#[allow(dead_code)]
pub async fn reset_session(store: &SessionStore, key: &str) {
    let mut sessions = store.write().await;
    let entry = sessions.entry(key.to_string()).or_insert_with(SessionEntry::new);
    entry.needs_full_briefing = true;
    entry.sticky_version = entry.sticky_version.wrapping_add(1);
}

/// Spawn the periodic cleanup task. Once per minute evicts entries older
/// than `SESSION_TTL`.
pub fn spawn_cleanup(store: SessionStore) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let cutoff = Instant::now() - SESSION_TTL;
            let mut sessions = store.write().await;
            sessions.retain(|_, e| e.first_call_seen_at > cutoff);
        }
    });
}
