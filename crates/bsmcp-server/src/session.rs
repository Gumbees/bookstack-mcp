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
    /// True until the first response carrying `meta.briefing_pending` has
    /// been sent for this session. Flipped to false after the first send.
    /// `record_call` returns the previous value so the caller can decide
    /// whether to attach the briefing this turn.
    pub needs_full_briefing: bool,
    /// The directory snapshot version this session has most recently
    /// received in full. The meta-injector compares against
    /// `DirectoryService::current().version` to decide between attaching the
    /// full snapshot vs the cheap `{version, hash}` pointer. `None` means
    /// the session has never seen a full snapshot.
    pub last_directory_version: Option<u64>,
}

impl SessionEntry {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            first_call_seen_at: now,
            last_briefing_sent_at: now,
            sticky_version: 0,
            needs_full_briefing: true,
            last_directory_version: None,
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
///
/// Also clears `last_directory_version` so the next response re-attaches the
/// full directory snapshot. After compaction the AI has lost the previous
/// snapshot, and the cheap pointer alone wouldn't be useful.
pub async fn mark_compacted(store: &SessionStore, key: &str) {
    let mut sessions = store.write().await;
    let entry = sessions.entry(key.to_string()).or_insert_with(SessionEntry::new);
    entry.needs_full_briefing = true;
    entry.sticky_version = entry.sticky_version.wrapping_add(1);
    entry.last_directory_version = None;
}

/// Mark a session as having received its briefing in-band (the AI called
/// `briefing` directly). Sub-PR 2.2 changed the auto-injection to
/// be one-shot: instead of attaching `meta.briefing` on every response, we
/// attach `meta.briefing_pending` only on the first non-briefing tool call
/// per session. When the AI calls `briefing` itself, that response
/// IS the briefing — flipping `needs_full_briefing` to false here prevents
/// the next non-briefing call from re-emitting it.
///
/// Distinct from `mark_compacted` (which sets `needs_full_briefing = true`
/// after a context drop): both touch the same flag in opposite directions.
pub async fn mark_briefing_delivered(store: &SessionStore, key: &str) {
    let mut sessions = store.write().await;
    let entry = sessions.entry(key.to_string()).or_insert_with(SessionEntry::new);
    entry.needs_full_briefing = false;
    entry.last_briefing_sent_at = Instant::now();
}

/// Compare the session's `last_directory_version` against `current_version`.
/// Returns true when the caller should attach the full snapshot (and bumps
/// the session's tracked version to match), false when a pointer suffices.
///
/// "take" because the call atomically reads-and-updates the session's
/// version: a second call in the same turn would not flip it again.
pub async fn take_directory_version(
    store: &SessionStore,
    key: &str,
    current_version: u64,
) -> bool {
    let mut sessions = store.write().await;
    let entry = sessions.entry(key.to_string()).or_insert_with(SessionEntry::new);
    let needs_full = match entry.last_directory_version {
        None => true,
        Some(v) if v < current_version => true,
        Some(_) => false,
    };
    if needs_full {
        entry.last_directory_version = Some(current_version);
    }
    needs_full
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> &'static str {
        "tok-hash:sess-id"
    }

    #[tokio::test]
    async fn record_call_first_returns_true_then_false() {
        let store = new_store();
        assert!(record_call(&store, key()).await, "first call must be 'first'");
        assert!(!record_call(&store, key()).await, "second call must be sticky-only");
        assert!(!record_call(&store, key()).await, "third call still sticky");
    }

    #[tokio::test]
    async fn mark_compacted_resets_briefing_and_directory() {
        let store = new_store();
        // Burn through the first call so flags are in steady state.
        let _ = record_call(&store, key()).await;
        let _ = take_directory_version(&store, key(), 5).await;
        // Steady state: no full briefing, version is tracked at 5.
        assert!(!record_call(&store, key()).await);
        assert!(
            !take_directory_version(&store, key(), 5).await,
            "same version should still be a pointer"
        );

        mark_compacted(&store, key()).await;
        assert!(
            record_call(&store, key()).await,
            "post-compact: briefing must re-attach"
        );
        assert!(
            take_directory_version(&store, key(), 5).await,
            "post-compact: directory must re-attach as full"
        );
    }

    #[tokio::test]
    async fn take_directory_version_attaches_full_when_version_advances() {
        let store = new_store();
        assert!(
            take_directory_version(&store, key(), 1).await,
            "first call must be full"
        );
        assert!(
            !take_directory_version(&store, key(), 1).await,
            "same version must be pointer"
        );
        assert!(
            take_directory_version(&store, key(), 2).await,
            "bumped version must be full"
        );
        assert!(
            !take_directory_version(&store, key(), 2).await,
            "back to pointer at the new version"
        );
    }

    #[tokio::test]
    async fn take_directory_version_does_not_regress_on_lower_version() {
        // Defensive: if a webhook race somehow passes us a lower version
        // (shouldn't happen — version monotonically increases), we should
        // NOT attach the full snapshot again, since the AI already saw
        // a strictly newer one.
        let store = new_store();
        let _ = take_directory_version(&store, key(), 5).await;
        assert!(
            !take_directory_version(&store, key(), 3).await,
            "stale version must be a pointer, not a full re-attach"
        );
    }

    #[tokio::test]
    async fn mark_briefing_delivered_prevents_redundant_attach() {
        let store = new_store();
        // AI calls briefing on a fresh session.
        mark_briefing_delivered(&store, key()).await;
        // The next non-briefing tool call must NOT see briefing_pending.
        assert!(
            !record_call(&store, key()).await,
            "post in-band briefing: no briefing_pending"
        );
        // Subsequent calls also stay sticky-only.
        assert!(!record_call(&store, key()).await);
    }

    #[tokio::test]
    async fn session_keys_with_different_session_ids_are_isolated() {
        let store = new_store();
        let a = "tok-a:sess-1";
        let b = "tok-a:sess-2";
        assert!(record_call(&store, a).await);
        // a is now sticky, but b is still 'first'.
        assert!(record_call(&store, b).await);
        assert!(!record_call(&store, a).await);
        assert!(!record_call(&store, b).await);
    }
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
