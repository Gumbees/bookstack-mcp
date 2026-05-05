//! Per-session briefing state.
//!
//! v0.8.0: tracks `(token_hash, session_id) -> {first_call_seen_at,
//! last_briefing_sent_at, sticky_version}` so the briefing-meta-injection on
//! every MCP tool response can decide between full content (first call) and
//! sticky-only (subsequent calls).
//!
//! `session_id` comes from the `Mcp-Session-Id` header (Streamable HTTP) or
//! the `?sessionId=` query param (SSE 2024-11-05). When the client doesn't
//! send one — e.g. a non-spec-compliant caller — we fall back to a stable
//! `{token_hash}:no-session` key. That gives them one full briefing on first
//! contact and sticky-only thereafter. To reset, the AI calls `briefing` or
//! `session_event action=compacted`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

/// Sessions older than this are evicted by the cleanup task.
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum sessions tracked in-memory. When the cap is hit, the cleanup loop
/// evicts the oldest. Single-instance store; if we ever go multi-instance,
/// move to Postgres.
const MAX_SESSIONS: usize = 10_000;

#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub first_call_seen_at: Instant,
    #[allow(dead_code)]
    pub last_briefing_sent_at: Instant,
    /// Bumped when the AI calls `session_event action=compacted` to force
    /// the next response to inject the full briefing again. Exposed for
    /// future telemetry / debugging — the meta-injection path keys off
    /// `needs_full_briefing` (which `mark_compacted` flips alongside the
    /// version bump).
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
/// to a stable `no-session` slot per token. The no-session slot collapses
/// every absent-id call from one token into a single entry — first call gets
/// full briefing, sticky-only thereafter until the AI explicitly resets via
/// `briefing` or `session_event`.
pub fn session_key(token_hash: &str, session_id: Option<&str>) -> String {
    match session_id {
        Some(s) if !s.is_empty() => format!("{token_hash}:{s}"),
        _ => format!("{token_hash}:no-session"),
    }
}

/// Mark a session call. Returns true if this is the first call for the
/// session (caller should inject full briefing) or false (sticky-only).
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

/// Reset a session so the next call gets a fresh full briefing. Called by
/// the `session_event` MCP tool on `action=compacted`, and by the `briefing`
/// tool itself (calling `briefing` resets the session so the next response
/// carries full content again — useful after the AI gets compacted by its
/// harness).
pub async fn mark_compacted(store: &SessionStore, key: &str) {
    let mut sessions = store.write().await;
    let entry = sessions.entry(key.to_string()).or_insert_with(SessionEntry::new);
    entry.needs_full_briefing = true;
    entry.sticky_version = entry.sticky_version.wrapping_add(1);
}

/// Spawn the periodic cleanup task. Once per minute evicts entries older
/// than `SESSION_TTL` and trims to `MAX_SESSIONS` if the cap was crossed
/// without any intervening write (defense-in-depth — `record_call` already
/// enforces the cap on insert).
pub fn spawn_cleanup(store: SessionStore) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let cutoff = Instant::now() - SESSION_TTL;
            let mut sessions = store.write().await;
            sessions.retain(|_, e| e.first_call_seen_at > cutoff);
            if sessions.len() > MAX_SESSIONS {
                // Evict oldest first until we're back under cap.
                let overflow = sessions.len() - MAX_SESSIONS;
                let mut by_age: Vec<(String, Instant)> = sessions
                    .iter()
                    .map(|(k, e)| (k.clone(), e.first_call_seen_at))
                    .collect();
                by_age.sort_by_key(|(_, t)| *t);
                for (k, _) in by_age.into_iter().take(overflow) {
                    sessions.remove(&k);
                }
            }
        }
    });
}
