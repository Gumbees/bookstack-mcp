//! Resolver helpers for the journal endpoints (Phase 2.4).
//!
//! These helpers fetch + cache the BookStack identity bits the journal
//! writers need on every call:
//!   - `resolve_first_name` — first whitespace-split token of `users.name`
//!     (24h TTL). Used for chapter / page naming.
//!   - `resolve_email`      — `users.email` (7d TTL). The user's per-user
//!     Journal book is named exactly by their email, so we cache it to
//!     avoid hitting `/api/users/{id}` on every journal write.
//!   - `resolve_user_journal_book` — find-or-create the user's "Journal"
//!     book on the global `user_journals_shelf_id`. Caches the resulting
//!     `book_id` into `UserSettings.user_journal_book_id`. The workhorse
//!     for 2.4: when the journal endpoints land they only need to call
//!     this + section-append.
//!
//! Cache freshness uses unix-second deltas — see `is_cache_fresh` for the
//! pure function tested in this module's `#[cfg(test)]` block. Persisting
//! the refreshed values is the resolver's responsibility (callers pass
//! `&mut UserSettings` and the resolver re-saves on refresh).

// 2.3 ships these helpers without callers — the journal endpoints in 2.4
// will wire them in. Suppress the dead-code lint until then; the unit
// tests in this module exercise the pure logic in the meantime.
#![allow(dead_code)]

use std::sync::Arc;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::DbBackend;
use bsmcp_common::settings::{GlobalSettings, UserSettings};

/// Refresh the cached first name after this many seconds (24h).
pub const FIRST_NAME_TTL_SECS: i64 = 24 * 60 * 60;

/// Refresh the cached email after this many seconds (7d).
pub const EMAIL_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Description applied to the per-user Journal book on auto-create.
const JOURNAL_BOOK_DESCRIPTION: &str = "Personal journal — agent + user entries";

/// Typed errors surfaced by the resolvers. Callers in 2.4 will translate to
/// the remember-envelope `ErrorCode` of their choosing.
#[derive(Debug, Clone)]
pub enum ResolverError {
    /// `UserSettings.bookstack_user_id` is None — we have nothing to resolve
    /// against. Caller should run identity discovery first (e.g. whoami) or
    /// surface a setup error.
    MissingBookstackUserId,
    /// `GlobalSettings.user_journals_shelf_id` is None — admin hasn't
    /// configured the User Journals shelf so per-user books can't live
    /// anywhere.
    MissingShelfConfig,
    /// BookStack API error (network failure, non-2xx response, malformed
    /// payload). Carries the raw error string for logging.
    BookstackError(String),
    /// DB get/save failure.
    DbError(String),
}

impl std::fmt::Display for ResolverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBookstackUserId => write!(
                f,
                "bookstack_user_id not set on UserSettings — run identity discovery first"
            ),
            Self::MissingShelfConfig => write!(
                f,
                "user_journals_shelf_id not configured — admin must set it via /settings"
            ),
            Self::BookstackError(e) => write!(f, "BookStack API: {e}"),
            Self::DbError(e) => write!(f, "DB: {e}"),
        }
    }
}

impl std::error::Error for ResolverError {}

impl From<String> for ResolverError {
    fn from(s: String) -> Self {
        Self::BookstackError(s)
    }
}

/// Pure helper: is a cached value still fresh?
///
/// Returns true when `fetched_at` is `Some` AND `now - fetched_at <= ttl`.
/// `None` is always stale so first-call code paths fetch.
pub fn is_cache_fresh(fetched_at: Option<i64>, now: i64, ttl: i64) -> bool {
    match fetched_at {
        Some(t) => now.saturating_sub(t) <= ttl,
        None => false,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the user's first-name token. Returns the cached value if fresh
/// (< 24h), else fetches `/api/users/{bookstack_user_id}`, splits `name`
/// on whitespace, takes [0], updates settings + persists, and returns it.
///
/// Errors with `MissingBookstackUserId` when settings has no bookstack_user_id.
pub async fn resolve_first_name(
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
) -> Result<String, ResolverError> {
    if let Some(cached) = settings.cached_first_name.clone() {
        if is_cache_fresh(settings.cached_first_name_fetched_at, now_unix(), FIRST_NAME_TTL_SECS) {
            return Ok(cached);
        }
    }

    let user_id = settings
        .bookstack_user_id
        .ok_or(ResolverError::MissingBookstackUserId)?;
    let user = client.get_user(user_id).await?;
    let full_name = user
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let first = full_name
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    if first.is_empty() {
        return Err(ResolverError::BookstackError(format!(
            "BookStack user {user_id} has empty name field — cannot derive first name"
        )));
    }

    settings.cached_first_name = Some(first.clone());
    settings.cached_first_name_fetched_at = Some(now_unix());
    db.save_user_settings(token_id_hash, settings)
        .await
        .map_err(ResolverError::DbError)?;
    Ok(first)
}

/// Resolve the user's email. Returns the cached value if fresh (< 7d), else
/// fetches `/api/users/{bookstack_user_id}`, updates settings + persists,
/// and returns it.
///
/// Errors with `MissingBookstackUserId` when settings has no bookstack_user_id,
/// or `BookstackError` if the user record carries no email (rare — typically
/// only seeded service accounts).
pub async fn resolve_email(
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
) -> Result<String, ResolverError> {
    if let Some(cached) = settings.cached_user_email.clone() {
        if is_cache_fresh(settings.cached_user_email_fetched_at, now_unix(), EMAIL_TTL_SECS) {
            return Ok(cached);
        }
    }

    let user_id = settings
        .bookstack_user_id
        .ok_or(ResolverError::MissingBookstackUserId)?;
    let user = client.get_user(user_id).await?;
    let email = user
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ResolverError::BookstackError(format!("BookStack user {user_id} has no email"))
        })?;

    settings.cached_user_email = Some(email.clone());
    settings.cached_user_email_fetched_at = Some(now_unix());
    db.save_user_settings(token_id_hash, settings)
        .await
        .map_err(ResolverError::DbError)?;
    Ok(email)
}

/// Find-or-create the user's per-user Journal book on the global
/// `user_journals_shelf_id`. Returns the book ID and caches it into
/// `settings.user_journal_book_id`.
///
/// 1. If `settings.user_journal_book_id` is `Some`, verify the book still
///    exists via `GET /api/books/{id}`. If 200, return it. If 404 (book
///    deleted), fall through to create.
/// 2. Resolve the user's email via `resolve_email`.
/// 3. Search for an existing book on `user_journals_shelf_id` named exactly
///    by the email (case-insensitive). If found, cache + return.
/// 4. Otherwise: `POST /api/books` with `{name: <email>, description: ...}`
///    then attach to the shelf via the GET-modify-PUT pattern on the
///    shelf's `books` array. Cache + return.
///
/// Errors with `MissingShelfConfig` when admin hasn't set
/// `user_journals_shelf_id`.
pub async fn resolve_user_journal_book(
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    globals: &GlobalSettings,
) -> Result<i64, ResolverError> {
    // 1. Cached book — verify it still exists.
    if let Some(book_id) = settings.user_journal_book_id {
        match client.get_book(book_id).await {
            Ok(_) => return Ok(book_id),
            Err(e) if e.contains("404") => {
                // Book was deleted out from under us — clear cache, recreate.
                settings.user_journal_book_id = None;
            }
            Err(e) => return Err(ResolverError::BookstackError(e)),
        }
    }

    let shelf_id = globals
        .user_journals_shelf_id
        .ok_or(ResolverError::MissingShelfConfig)?;

    // 2. Resolve email — also persists the cache update.
    let email = resolve_email(token_id_hash, settings, client, db.clone()).await?;

    // 3. Search the shelf's books for a name match.
    let shelf = client
        .get_shelf(shelf_id)
        .await
        .map_err(ResolverError::BookstackError)?;
    let shelf_books: Vec<(i64, String)> = shelf
        .get("books")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    let id = b.get("id").and_then(|v| v.as_i64())?;
                    let name = b
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some((id, name))
                })
                .collect()
        })
        .unwrap_or_default();

    if let Some((existing_id, _)) = shelf_books
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(&email))
    {
        settings.user_journal_book_id = Some(*existing_id);
        db.save_user_settings(token_id_hash, settings)
            .await
            .map_err(ResolverError::DbError)?;
        return Ok(*existing_id);
    }

    // 4. Create the book and attach to the shelf via GET-modify-PUT.
    //    NOTE: same TOCTOU caveat as `move_book_to_shelf` — concurrent
    //    creates on the same shelf can drop assignments. Acceptable while
    //    journal writes are per-user and serialized through one resolver
    //    call per request.
    let new_book = client
        .create_book(&email, JOURNAL_BOOK_DESCRIPTION)
        .await
        .map_err(ResolverError::BookstackError)?;
    let new_book_id = new_book
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_book: missing id in response".to_string()))?;

    let mut book_ids: Vec<i64> = shelf_books.iter().map(|(id, _)| *id).collect();
    if !book_ids.contains(&new_book_id) {
        book_ids.push(new_book_id);
    }
    client
        .update_shelf(shelf_id, &serde_json::json!({ "books": book_ids }))
        .await
        .map_err(ResolverError::BookstackError)?;

    settings.user_journal_book_id = Some(new_book_id);
    db.save_user_settings(token_id_hash, settings)
        .await
        .map_err(ResolverError::DbError)?;
    Ok(new_book_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_fresh_when_within_ttl() {
        // Fetched 1 hour ago, TTL 24h — fresh.
        assert!(is_cache_fresh(Some(1_000), 1_000 + 3_600, FIRST_NAME_TTL_SECS));
    }

    #[test]
    fn cache_stale_when_past_ttl() {
        // Fetched 25 hours ago, TTL 24h — stale.
        let now = 1_000_000;
        let fetched = now - (25 * 60 * 60);
        assert!(!is_cache_fresh(Some(fetched), now, FIRST_NAME_TTL_SECS));
    }

    #[test]
    fn cache_fresh_at_exact_ttl_boundary() {
        // Exactly at the boundary should be considered fresh (<=, not <).
        let now = 1_000_000;
        let fetched = now - FIRST_NAME_TTL_SECS;
        assert!(is_cache_fresh(Some(fetched), now, FIRST_NAME_TTL_SECS));
    }

    #[test]
    fn cache_stale_one_second_past_boundary() {
        let now = 1_000_000;
        let fetched = now - FIRST_NAME_TTL_SECS - 1;
        assert!(!is_cache_fresh(Some(fetched), now, FIRST_NAME_TTL_SECS));
    }

    #[test]
    fn cache_stale_when_fetched_at_is_none() {
        // First call ever — no watermark, must fetch.
        assert!(!is_cache_fresh(None, 1_000, FIRST_NAME_TTL_SECS));
        assert!(!is_cache_fresh(None, 0, EMAIL_TTL_SECS));
    }

    #[test]
    fn cache_handles_clock_skew_gracefully() {
        // fetched_at in the future (e.g. system clock jumped backward).
        // saturating_sub avoids panic; we treat it as fresh.
        let now = 1_000_000;
        let fetched = now + 60; // 60s in the "future"
        assert!(is_cache_fresh(Some(fetched), now, FIRST_NAME_TTL_SECS));
    }

    #[test]
    fn email_ttl_is_seven_days() {
        // Sanity check — guards against accidental edits to the constant.
        assert_eq!(EMAIL_TTL_SECS, 7 * 86_400);
    }

    #[test]
    fn first_name_ttl_is_one_day() {
        assert_eq!(FIRST_NAME_TTL_SECS, 86_400);
    }

    #[test]
    fn resolver_error_display_shapes() {
        // Smoke-test Display so the error strings stay human-readable; if
        // someone refactors the variants the messages should still be
        // informative without leaking internal types.
        let cases = [
            (
                ResolverError::MissingBookstackUserId,
                "bookstack_user_id",
            ),
            (
                ResolverError::MissingShelfConfig,
                "user_journals_shelf_id",
            ),
            (
                ResolverError::BookstackError("boom".to_string()),
                "BookStack API: boom",
            ),
            (
                ResolverError::DbError("boom".to_string()),
                "DB: boom",
            ),
        ];
        for (err, expected_substr) in cases {
            let s = format!("{err}");
            assert!(
                s.contains(expected_substr),
                "expected `{}` in `{}`",
                expected_substr,
                s
            );
        }
    }

    #[test]
    fn resolver_error_from_string_is_bookstack_variant() {
        let err: ResolverError = "boom".to_string().into();
        match err {
            ResolverError::BookstackError(s) => assert_eq!(s, "boom"),
            other => panic!("expected BookstackError, got {other:?}"),
        }
    }
}
