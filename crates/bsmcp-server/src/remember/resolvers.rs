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

use chrono::{Datelike, NaiveDate};

use bsmcp_common::bookstack::{BookStackClient, ContentType};
use bsmcp_common::db::DbBackend;
use bsmcp_common::settings::{GlobalSettings, UserSettings};

/// Refresh the cached first name after this many seconds (24h).
pub const FIRST_NAME_TTL_SECS: i64 = 24 * 60 * 60;

/// Refresh the cached email after this many seconds (7d).
pub const EMAIL_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Description applied to the per-user Journal book on auto-create.
const JOURNAL_BOOK_DESCRIPTION: &str = "Personal journal — agent + user entries";

/// Chapter name for the user's first-person identity narrative. Single
/// chapter per Journal book; one page of the same name lives inside.
pub const USER_IDENTITY_CHAPTER_NAME: &str = "User Identity";

/// Description stamped on the User Identity chapter at create time.
const USER_IDENTITY_CHAPTER_DESCRIPTION: &str =
    "User's first-person identity narrative — written by the AI on the user's behalf";

/// Description stamped on a per-agent AI Identity chapter at create time.
const AI_IDENTITY_CHAPTER_DESCRIPTION: &str =
    "Per-agent identity narrative — evolved by the named AI agent for this user";

/// Build the chapter name for a normalized agent name.
pub fn ai_identity_chapter_name(normalized_agent_name: &str) -> String {
    format!("AI Identity: {normalized_agent_name}")
}

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

    // Scrub permissions: this book is per-user-private. The user is the
    // creator (BookStack sets owner_id automatically on POST /books). We
    // strip role-based grants and disable inheritance via fallback so no
    // other registered user can view it. BookStack admins still see
    // everything via the system-level `*-view-all` / `content-export`
    // role permissions — that admin override is independent of
    // content_permissions and intentionally untouched here.
    //
    // API: PUT /api/content-permissions/book/{id} with the scrub payload
    // built by `private_book_permission_scrub_payload`. Single round-trip;
    // BookStack accepts owner_id + role_permissions + fallback_permissions
    // on the same call (admin role required for owner_id changes; user
    // tokens can update permissions on books they own without it as long
    // as we don't change owner_id, which we omit here since the create
    // already stamped it).
    if let Err(e) = scrub_book_permissions(client, new_book_id).await {
        // Non-fatal: book exists and is usable; permissions just didn't
        // lock. Surface a loud eprintln so admins notice but don't fail
        // the user's first journal write. The book is still readable
        // (default ACL) — just not user-private. A subsequent retry path
        // (admin reconciliation, or a manual PUT) can re-scrub later.
        let user_id = settings.bookstack_user_id.unwrap_or(-1);
        eprintln!(
            "Resolvers: failed to scrub permissions on new journal book {new_book_id} for user {user_id}: {e}"
        );
    }

    settings.user_journal_book_id = Some(new_book_id);
    db.save_user_settings(token_id_hash, settings)
        .await
        .map_err(ResolverError::DbError)?;
    Ok(new_book_id)
}

/// Scrub permissions on a freshly-created per-user Journal book so only the
/// owner (and BookStack admins via the system-level admin override) can see
/// it. Sends one PUT to `/api/content-permissions/book/{id}` with:
/// - `role_permissions: []` — strip every role grant
/// - `fallback_permissions: { inheriting: false, view: false, create: false,
///                            update: false, delete: false }` — block
///                            non-owner non-admin access
///
/// `owner_id` is intentionally NOT included: it was set on `POST /books`
/// to the creating user (us) and including it on the PUT requires admin
/// privileges. Leaving it omitted keeps this call working with the user's
/// own token.
async fn scrub_book_permissions(
    client: &BookStackClient,
    book_id: i64,
) -> Result<(), String> {
    let payload = private_book_permission_scrub_payload();
    client
        .update_content_permissions(ContentType::Book, book_id, &payload)
        .await?;
    Ok(())
}

/// Pure helper exposed for tests: produces the JSON payload sent to
/// `PUT /api/content-permissions/book/{id}` to lock a book down to its
/// owner. Kept separate so the test can assert the wire shape without
/// any mock-HTTP scaffolding.
pub fn private_book_permission_scrub_payload() -> serde_json::Value {
    serde_json::json!({
        "role_permissions": [],
        "fallback_permissions": {
            "inheriting": false,
            "view": false,
            "create": false,
            "update": false,
            "delete": false,
        },
    })
}

/// Find-or-create the `{YYYY-MM}-{name}` monthly journal chapter inside
/// the user's Journal book. Single chapter per (month, name) tuple —
/// match is case-insensitive against the rendered chapter name.
///
/// Caller is responsible for resolving `book_id` via
/// `resolve_user_journal_book` so this stays a pure chapter-scoped op.
pub async fn resolve_journal_chapter(
    book_id: i64,
    year: i32,
    month: u32,
    name: &str,
    client: &BookStackClient,
) -> Result<i64, ResolverError> {
    let chapter_name = journal_chapter_name(year, month, name);
    find_or_create_chapter(client, book_id, &chapter_name, JOURNAL_CHAPTER_DESCRIPTION)
        .await
}

/// Find-or-create the `{YYYY-MM-DD}-{name}` daily journal page inside
/// the given chapter. Returns `(page_id, was_created)`. On create the
/// body is empty — the journal `write` action appends the first
/// time-stamped section after this returns.
pub async fn resolve_journal_page(
    chapter_id: i64,
    date: NaiveDate,
    name: &str,
    client: &BookStackClient,
) -> Result<(i64, bool), ResolverError> {
    let page_name = journal_page_name(date, name);
    if let Some(existing) = client
        .find_page_in_chapter(chapter_id, &page_name)
        .await
        .map_err(ResolverError::BookstackError)?
    {
        let id = existing
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolverError::BookstackError("page missing id".to_string()))?;
        return Ok((id, false));
    }
    let page = client
        .create_page(&serde_json::json!({
            "chapter_id": chapter_id,
            "name": page_name,
            "markdown": "",
        }))
        .await
        .map_err(ResolverError::BookstackError)?;
    let id = page
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_page: missing id in response".to_string()))?;
    Ok((id, true))
}

/// Description stamped on monthly journal chapters at create time.
const JOURNAL_CHAPTER_DESCRIPTION: &str =
    "Monthly journal entries — one daily page per session-day, append-only";

/// Description stamped on the user's `Reminders` chapter at create time.
const REMINDERS_CHAPTER_DESCRIPTION: &str =
    "Rolling task list — monthly pages with open/done sections. Written by the `reminders` MCP tool.";

/// Description stamped on the user's `Events` chapter at create time.
const EVENTS_CHAPTER_DESCRIPTION: &str =
    "Future-scheduled calendar items — monthly pages, written by the `events` MCP tool.";

/// Find-or-create the singleton `Reminders` chapter inside the user's
/// per-user Journal book. Single chapter; monthly pages live inside it.
pub async fn resolve_reminders_chapter(
    book_id: i64,
    client: &BookStackClient,
) -> Result<i64, ResolverError> {
    find_or_create_chapter(client, book_id, "Reminders", REMINDERS_CHAPTER_DESCRIPTION).await
}

/// Find-or-create the `{YYYY-MM}-Reminders` monthly page inside the
/// reminders chapter. Returns `(page_id, was_created)`. On create the
/// body is the empty-page seed (Open + Done section headers); the
/// reminders handlers read+modify+write the markdown wholesale.
pub async fn resolve_reminders_monthly_page(
    chapter_id: i64,
    year: i32,
    month: u32,
    client: &BookStackClient,
) -> Result<(i64, bool), ResolverError> {
    let page_name = reminders_monthly_page_name(year, month);
    if let Some(existing) = client
        .find_page_in_chapter(chapter_id, &page_name)
        .await
        .map_err(ResolverError::BookstackError)?
    {
        let id = existing
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolverError::BookstackError("page missing id".to_string()))?;
        return Ok((id, false));
    }
    let page = client
        .create_page(&serde_json::json!({
            "chapter_id": chapter_id,
            "name": page_name,
            "markdown": reminders_seed(),
        }))
        .await
        .map_err(ResolverError::BookstackError)?;
    let id = page
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_page: missing id in response".to_string()))?;
    Ok((id, true))
}

/// Render `{YYYY-MM}-Reminders`. Pure helper.
pub fn reminders_monthly_page_name(year: i32, month: u32) -> String {
    format!("{year:04}-{month:02}-Reminders")
}

/// Empty-page seed for a fresh `{YYYY-MM}-Reminders` page. Establishes
/// the section headers the `reminders` tool's create/complete handlers
/// rely on for placement.
pub fn reminders_seed() -> String {
    "## 🟢 Open\n\n## ✅ Done\n".to_string()
}

/// Find-or-create the singleton `Events` chapter inside the user's
/// per-user Journal book.
pub async fn resolve_events_chapter(
    book_id: i64,
    client: &BookStackClient,
) -> Result<i64, ResolverError> {
    find_or_create_chapter(client, book_id, "Events", EVENTS_CHAPTER_DESCRIPTION).await
}

/// Find-or-create the `{YYYY-MM}-Events` monthly page inside the events
/// chapter. Returns `(page_id, was_created)`.
pub async fn resolve_events_monthly_page(
    chapter_id: i64,
    year: i32,
    month: u32,
    client: &BookStackClient,
) -> Result<(i64, bool), ResolverError> {
    let page_name = events_monthly_page_name(year, month);
    if let Some(existing) = client
        .find_page_in_chapter(chapter_id, &page_name)
        .await
        .map_err(ResolverError::BookstackError)?
    {
        let id = existing
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolverError::BookstackError("page missing id".to_string()))?;
        return Ok((id, false));
    }
    let page = client
        .create_page(&serde_json::json!({
            "chapter_id": chapter_id,
            "name": page_name,
            "markdown": events_seed(),
        }))
        .await
        .map_err(ResolverError::BookstackError)?;
    let id = page
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_page: missing id in response".to_string()))?;
    Ok((id, true))
}

/// Render `{YYYY-MM}-Events`. Pure helper.
pub fn events_monthly_page_name(year: i32, month: u32) -> String {
    format!("{year:04}-{month:02}-Events")
}

/// Empty-page seed for a fresh `{YYYY-MM}-Events` page. One section,
/// since events don't have a distinct lifecycle state — they're just
/// scheduled or cancelled (cancelled = removed).
pub fn events_seed() -> String {
    "## 📅 Scheduled\n".to_string()
}

/// Description stamped on per-agent `Sessions: {agent_name}` chapters
/// at create time.
const SESSIONS_CHAPTER_DESCRIPTION: &str =
    "AI session captures (forager / Claude Code / external clients) — page-per-session, append-only blocks";

/// Find-or-create the `Sessions: {agent_name}` chapter inside the
/// user's per-user Journal book. Mirrors the `AI Identity:` chapter
/// pattern.
pub async fn resolve_sessions_chapter(
    book_id: i64,
    agent_name: &str,
    client: &BookStackClient,
) -> Result<i64, ResolverError> {
    let chapter_name = sessions_chapter_name(agent_name);
    find_or_create_chapter(client, book_id, &chapter_name, SESSIONS_CHAPTER_DESCRIPTION).await
}

/// Render the chapter name `Sessions: {agent_name}`. Pure helper so
/// tests can assert the formatting.
pub fn sessions_chapter_name(agent_name: &str) -> String {
    format!("Sessions: {agent_name}")
}

/// Render a session page name. Used on first append for the session.
/// Format: `{YYYY-MM-DD}-{title-slug}` when a title is supplied,
/// `{YYYY-MM-DD}-{session_id_short}` otherwise.
pub fn session_page_name(date: chrono::NaiveDate, title: Option<&str>, session_id: &str) -> String {
    let date_part = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    );
    let suffix = match title {
        Some(t) if !t.trim().is_empty() => slugify_natural_key(t),
        _ => session_id_short(session_id),
    };
    format!("{date_part}-{suffix}")
}

/// Take the first 8 ASCII alphanumeric characters of a session_id for
/// use in a page name when no title is supplied. Stable + readable.
fn session_id_short(session_id: &str) -> String {
    let s: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if s.is_empty() {
        "session".to_string()
    } else {
        s
    }
}

/// Slugify free-form text into a stable natural-key. Used by reminders
/// + events when the caller doesn't supply one explicitly. Lowercase,
/// non-alphanumeric → dash, collapse runs, trim, cap to 40 chars.
pub fn slugify_natural_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed: &str = out.trim_matches('-');
    let mut s: String = trimmed.chars().take(40).collect();
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        // Fallback so the bullet always carries a parseable key.
        "untitled".to_string()
    } else {
        s
    }
}

/// Render the chapter name `{YYYY-MM}-{name}`. Pure helper so tests can
/// assert the formatting without hitting BookStack.
pub fn journal_chapter_name(year: i32, month: u32, name: &str) -> String {
    format!("{year:04}-{month:02}-{name}")
}

/// Render the page name `{YYYY-MM-DD}-{name}`. Pure helper.
pub fn journal_page_name(date: NaiveDate, name: &str) -> String {
    format!(
        "{:04}-{:02}-{:02}-{}",
        date.year(),
        date.month(),
        date.day(),
        name
    )
}

/// Normalize a raw agent_name into the canonical form used in chapter
/// titles: trimmed, lowercased, internal whitespace runs collapsed to a
/// single dash. Returns `None` when the result is empty or contains
/// characters outside `[a-z0-9_-]` (the AI gets a clear error rather
/// than discovering BookStack rejects the chapter create later).
pub fn normalize_agent_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Lowercase + replace whitespace runs with a single dash. Multi-pass:
    // split_whitespace handles tabs/newlines too and collapses runs.
    let lowered = trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if lowered.is_empty() {
        return None;
    }
    // Defensive char filter — BookStack itself is permissive but the
    // chapter title gets composed into "AI Identity: {name}", so we keep
    // it to ASCII alphanumerics + dash + underscore. Anything weirder
    // probably points at copy-paste noise rather than a deliberate name.
    if !lowered
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(lowered)
}

/// Find-or-create the "User Identity" chapter inside the user's per-user
/// Journal book. Single chapter per book — name match is case-insensitive
/// against `USER_IDENTITY_CHAPTER_NAME`. Returns the chapter ID.
pub async fn resolve_user_identity_chapter(
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    globals: &GlobalSettings,
) -> Result<i64, ResolverError> {
    let book_id =
        resolve_user_journal_book(token_id_hash, settings, client, db.clone(), globals).await?;
    find_or_create_chapter(
        client,
        book_id,
        USER_IDENTITY_CHAPTER_NAME,
        USER_IDENTITY_CHAPTER_DESCRIPTION,
    )
    .await
}

/// Find-or-create the "User Identity" page inside the User Identity
/// chapter. On bootstrap (page missing), writes the seed markdown
/// rendered against `resolve_first_name` + `resolve_email`. Returns
/// `(page_id, was_bootstrapped)`.
pub async fn resolve_user_identity_page(
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    globals: &GlobalSettings,
) -> Result<(i64, bool), ResolverError> {
    let chapter_id =
        resolve_user_identity_chapter(token_id_hash, settings, client, db.clone(), globals)
            .await?;

    if let Some(existing) = client
        .find_page_in_chapter(chapter_id, USER_IDENTITY_CHAPTER_NAME)
        .await
        .map_err(ResolverError::BookstackError)?
    {
        let id = existing
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolverError::BookstackError("page missing id".to_string()))?;
        return Ok((id, false));
    }

    let first_name =
        resolve_first_name(token_id_hash, settings, client, db.clone()).await?;
    let email = resolve_email(token_id_hash, settings, client, db.clone()).await?;
    let body = user_identity_bootstrap(&first_name, &email);
    let page = client
        .create_page(&serde_json::json!({
            "chapter_id": chapter_id,
            "name": USER_IDENTITY_CHAPTER_NAME,
            "markdown": body,
        }))
        .await
        .map_err(ResolverError::BookstackError)?;
    let id = page
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_page: missing id in response".to_string()))?;
    Ok((id, true))
}

/// Find-or-create the "AI Identity: {agent_name}" chapter inside the
/// user's per-user Journal book. `agent_name` must already be normalized
/// via `normalize_agent_name`. Returns the chapter ID.
pub async fn resolve_ai_identity_chapter(
    agent_name: &str,
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    globals: &GlobalSettings,
) -> Result<i64, ResolverError> {
    let book_id =
        resolve_user_journal_book(token_id_hash, settings, client, db.clone(), globals).await?;
    let chapter_name = ai_identity_chapter_name(agent_name);
    find_or_create_chapter(
        client,
        book_id,
        &chapter_name,
        AI_IDENTITY_CHAPTER_DESCRIPTION,
    )
    .await
}

/// Find-or-create the AI Identity page inside its chapter. On bootstrap
/// (page missing), writes the seed markdown rendered with the agent's
/// name. Returns `(page_id, was_bootstrapped)`.
pub async fn resolve_ai_identity_page(
    agent_name: &str,
    token_id_hash: &str,
    settings: &mut UserSettings,
    client: &BookStackClient,
    db: Arc<dyn DbBackend>,
    globals: &GlobalSettings,
) -> Result<(i64, bool), ResolverError> {
    let chapter_id = resolve_ai_identity_chapter(
        agent_name,
        token_id_hash,
        settings,
        client,
        db.clone(),
        globals,
    )
    .await?;
    let page_name = ai_identity_chapter_name(agent_name);

    if let Some(existing) = client
        .find_page_in_chapter(chapter_id, &page_name)
        .await
        .map_err(ResolverError::BookstackError)?
    {
        let id = existing
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolverError::BookstackError("page missing id".to_string()))?;
        return Ok((id, false));
    }

    let body = ai_identity_bootstrap(agent_name);
    let page = client
        .create_page(&serde_json::json!({
            "chapter_id": chapter_id,
            "name": page_name,
            "markdown": body,
        }))
        .await
        .map_err(ResolverError::BookstackError)?;
    let id = page
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_page: missing id in response".to_string()))?;
    Ok((id, true))
}

/// Find-or-create a chapter by name in `book_id`. One `find_chapter_in_book`
/// call (which itself is one `get_book`); on miss, one `create_chapter`.
async fn find_or_create_chapter(
    client: &BookStackClient,
    book_id: i64,
    chapter_name: &str,
    description: &str,
) -> Result<i64, ResolverError> {
    if let Some(existing) = client
        .find_chapter_in_book(book_id, chapter_name)
        .await
        .map_err(ResolverError::BookstackError)?
    {
        return existing
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolverError::BookstackError("chapter missing id".to_string()));
    }
    let chapter = client
        .create_chapter(book_id, chapter_name, description)
        .await
        .map_err(ResolverError::BookstackError)?;
    chapter
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolverError::BookstackError("create_chapter: missing id in response".to_string()))
}

/// Bootstrap markdown for the user-identity page. The AI overwrites this
/// wholesale on its first `identity write target=user` call.
pub fn user_identity_bootstrap(first_name: &str, email: &str) -> String {
    format!(
        "name: {first_name}\nemail: {email}\n\n(Replace this content with your own narrative.)\n"
    )
}

/// Bootstrap markdown for an AI-identity page. The AI overwrites this
/// wholesale on its first `identity write target=agent` call.
pub fn ai_identity_bootstrap(agent_name: &str) -> String {
    format!(
        "name: {agent_name}\n\n(Replace this content with your own narrative.)\n"
    )
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

    // --- normalize_agent_name ---

    #[test]
    fn normalize_agent_name_lowercases() {
        assert_eq!(normalize_agent_name("Claude").as_deref(), Some("claude"));
        assert_eq!(normalize_agent_name("GPT-4").as_deref(), Some("gpt-4"));
    }

    #[test]
    fn normalize_agent_name_trims_outer_whitespace() {
        assert_eq!(
            normalize_agent_name("  claude  ").as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn normalize_agent_name_replaces_inner_whitespace_with_dash() {
        assert_eq!(
            normalize_agent_name("Claude Opus").as_deref(),
            Some("claude-opus"),
        );
        // Multiple whitespace runs (including mixed tab/newline) collapse
        // to a single dash apiece.
        assert_eq!(
            normalize_agent_name("  Claude   Opus  Lite  ").as_deref(),
            Some("claude-opus-lite"),
        );
        assert_eq!(
            normalize_agent_name("Claude\tOpus\nLite").as_deref(),
            Some("claude-opus-lite"),
        );
    }

    #[test]
    fn normalize_agent_name_allows_dash_and_underscore() {
        assert_eq!(
            normalize_agent_name("my_agent-7").as_deref(),
            Some("my_agent-7"),
        );
    }

    #[test]
    fn normalize_agent_name_rejects_empty_after_trim() {
        assert_eq!(normalize_agent_name(""), None);
        assert_eq!(normalize_agent_name("   "), None);
        assert_eq!(normalize_agent_name("\t\n"), None);
    }

    #[test]
    fn normalize_agent_name_rejects_special_characters() {
        // Punctuation, slashes, dots, colons — anything outside
        // [a-z0-9_-] after normalization is rejected.
        assert_eq!(normalize_agent_name("claude.opus"), None);
        assert_eq!(normalize_agent_name("claude/opus"), None);
        assert_eq!(normalize_agent_name("claude:opus"), None);
        assert_eq!(normalize_agent_name("claude!"), None);
        assert_eq!(normalize_agent_name("AI Identity: claude"), None);
    }

    #[test]
    fn normalize_agent_name_rejects_non_ascii() {
        // Non-ASCII alphabetics aren't alphanumeric per is_ascii_alphanumeric.
        assert_eq!(normalize_agent_name("café"), None);
        assert_eq!(normalize_agent_name("クロード"), None);
    }

    #[test]
    fn ai_identity_chapter_name_renders_predictably() {
        assert_eq!(
            ai_identity_chapter_name("claude"),
            "AI Identity: claude"
        );
        assert_eq!(
            ai_identity_chapter_name("gpt-4o"),
            "AI Identity: gpt-4o"
        );
    }

    // --- bootstrap content shape ---

    #[test]
    fn user_identity_bootstrap_includes_name_and_email() {
        let body = user_identity_bootstrap("Nate", "nate@example.com");
        assert!(body.contains("name: Nate"), "missing name line: {body}");
        assert!(
            body.contains("email: nate@example.com"),
            "missing email line: {body}"
        );
        assert!(body.contains("Replace this content"));
        // Sanity: don't accidentally embed the page's own H1 — the
        // BookStack renderer adds the page name as an H1 already.
        assert!(
            !body.contains("# User Identity"),
            "bootstrap must not duplicate the page title as an H1: {body}"
        );
    }

    #[test]
    fn ai_identity_bootstrap_includes_agent_name() {
        let body = ai_identity_bootstrap("claude");
        assert!(body.contains("name: claude"));
        assert!(body.contains("Replace this content"));
        assert!(
            !body.contains("# AI Identity"),
            "bootstrap must not duplicate the page title as an H1: {body}"
        );
    }

    // --- journal name rendering ---

    #[test]
    fn journal_chapter_name_pads_year_and_month() {
        assert_eq!(journal_chapter_name(2026, 5, "nate"), "2026-05-nate");
        assert_eq!(journal_chapter_name(2026, 12, "claude"), "2026-12-claude");
        // January = 01, not 1.
        assert_eq!(journal_chapter_name(2026, 1, "nate"), "2026-01-nate");
        // Single-digit year stays four-wide.
        assert_eq!(journal_chapter_name(99, 7, "nate"), "0099-07-nate");
    }

    #[test]
    fn journal_page_name_pads_full_date() {
        let date = NaiveDate::from_ymd_opt(2026, 5, 3).unwrap();
        assert_eq!(journal_page_name(date, "nate"), "2026-05-03-nate");

        let date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert_eq!(journal_page_name(date, "claude"), "2026-12-31-claude");
    }

    // --- permission scrub payload shape ---

    #[test]
    fn private_book_permission_scrub_payload_has_expected_shape() {
        let payload = private_book_permission_scrub_payload();
        // role_permissions: empty array — strip every role grant.
        let roles = payload.get("role_permissions").expect("role_permissions present");
        assert!(roles.is_array(), "role_permissions must be an array");
        assert_eq!(roles.as_array().unwrap().len(), 0, "role_permissions must be empty");

        // fallback_permissions: deny everything, no inheritance.
        let fb = payload
            .get("fallback_permissions")
            .expect("fallback_permissions present");
        assert_eq!(fb.get("inheriting").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(fb.get("view").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(fb.get("create").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(fb.get("update").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(fb.get("delete").and_then(|v| v.as_bool()), Some(false));

        // owner_id is intentionally NOT included — see scrub fn comment.
        assert!(
            payload.get("owner_id").is_none(),
            "owner_id must be omitted so the user's own token can send this PUT"
        );
    }
}
