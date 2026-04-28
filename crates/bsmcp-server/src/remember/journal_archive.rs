//! Year-rollover and archive-chapter resolution for the per-identity Journal.
//!
//! Phase 6 of the v1.0.0 architecture: pages live flat inside the Journal
//! chapter for the current year. On each `journal action=write`, the
//! year-rollover sweep moves any page whose `created_at` falls in a stale
//! year into a `Journal Archive - {YEAR}` chapter (find-or-created lazily,
//! scoped strictly within the same Identity book so we never confuse one
//! identity's archive with another's).
//!
//! Year semantics use the user's IANA timezone (from `UserSettings::timezone`)
//! so a `2026-01-01` journal page archives at the user's local-day boundary,
//! not at UTC midnight. Rationale: the page name is `YYYY-MM-DD` and the
//! archive boundary should align with the user's calendar day.

use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use serde_json::json;

use bsmcp_common::settings::UserSettings;

use super::provision;
use super::Context;

/// The user's IANA timezone, falling back to UTC when unset/invalid.
fn resolve_tz(settings: &UserSettings) -> Tz {
    settings
        .timezone
        .as_deref()
        .and_then(|name| name.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

/// Current year in the user's local timezone.
pub fn current_year_local(settings: &UserSettings) -> i32 {
    let tz = resolve_tz(settings);
    Utc::now().with_timezone(&tz).year()
}

/// Parse a BookStack ISO-8601 timestamp (`created_at` / `updated_at`) and
/// return the year as observed in the user's local timezone. Returns `None`
/// if the timestamp can't be parsed.
pub fn year_from_iso_timestamp(iso: &str, settings: &UserSettings) -> Option<i32> {
    let dt: DateTime<Utc> = iso.parse().ok()?;
    let tz = resolve_tz(settings);
    Some(dt.with_timezone(&tz).year())
}

/// Canonical archive-chapter name. Hard-coded format per RFC decision #1
/// — changing it later is a real refactor (the migration tool matches by
/// exact name).
pub fn archive_chapter_name(year: i32) -> String {
    format!("Journal Archive - {year:04}")
}

/// Find-or-create the `Journal Archive - {YEAR}` chapter inside the
/// identity book. Idempotent — re-running with the same year reuses the
/// existing chapter.
pub async fn find_or_create_archive_chapter(
    identity_book_id: i64,
    year: i32,
    ctx: &Context,
) -> Result<i64, String> {
    let chapter_name = archive_chapter_name(year);
    let description = format!(
        "Journal entries from {year}. Auto-created by year-rollover sweep on the first {} write.",
        current_year_local(&ctx.settings)
    );
    let outcome = provision::find_or_create_chapter(
        &ctx.client,
        ctx.index_db.as_ref(),
        identity_book_id,
        &chapter_name,
        &description,
    )
    .await;
    outcome
        .id()
        .ok_or_else(|| format!("archive chapter provisioning failed for year {year}"))
}

/// Walk every page in the journal chapter, group by created_at year (in the
/// user's local timezone), and move any stale-year pages into the
/// corresponding archive chapter. Idempotent — re-running on an
/// already-swept chapter is a no-op (current-year pages stay; stale pages
/// have already moved).
///
/// `journal_chapter_id` is the current-year `Journal` chapter — only its
/// direct children are considered. `identity_book_id` scopes archive-chapter
/// lookup so we never confuse one identity's archive with another's.
///
/// Returns the count of pages moved. Best-effort: a single failed
/// move_page is logged and skipped; the sweep keeps going so a partial
/// failure doesn't block the user's write.
pub async fn year_rollover_sweep(
    journal_chapter_id: i64,
    identity_book_id: i64,
    ctx: &Context,
) -> Result<usize, String> {
    let current_year = current_year_local(&ctx.settings);

    // Pull every page in the journal chapter. usize::MAX matches the existing
    // behaviour for `list_book_pages_by_updated` everywhere else in the
    // module — a journal chapter with hundreds of stale pages is the
    // worst-case once-per-year cost.
    let pages = ctx
        .client
        .list_chapter_pages_by_updated(journal_chapter_id, usize::MAX)
        .await?;

    let mut moved: usize = 0;
    for page in &pages {
        let page_id = match page.get("id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => continue,
        };
        let created_at = page.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let year = match year_from_iso_timestamp(created_at, &ctx.settings) {
            Some(y) => y,
            None => continue, // unparseable timestamp — leave the page alone
        };
        if year >= current_year {
            continue;
        }

        let archive_chapter_id =
            match find_or_create_archive_chapter(identity_book_id, year, ctx).await {
                Ok(id) => id,
                Err(e) => {
                    eprintln!(
                        "year_rollover_sweep: failed to provision archive chapter for {year} (page {page_id}): {e}"
                    );
                    continue;
                }
            };

        // BookStack moves a page by PUT /api/pages/{id} with chapter_id set.
        let payload = json!({ "chapter_id": archive_chapter_id });
        if let Err(e) = ctx.client.update_page(page_id, &payload).await {
            eprintln!(
                "year_rollover_sweep: move page {page_id} -> chapter {archive_chapter_id} failed (non-fatal): {e}"
            );
            continue;
        }
        moved += 1;
    }
    Ok(moved)
}

/// Resolve the parent for a journal read-by-key. Parses the year from the
/// key (`YYYY-MM-DD`); if the year is the current local year, returns the
/// current-year journal chapter; otherwise looks up the archive chapter.
/// Returns `None` if no archive chapter exists for that year (caller surfaces
/// a NotFound).
pub async fn resolve_read_chapter_for_key(
    key: &str,
    settings: &UserSettings,
    identity_book_id: i64,
    journal_chapter_id: i64,
    ctx: &Context,
) -> Result<Option<i64>, String> {
    let key_year = match parse_year_from_key(key) {
        Some(y) => y,
        None => {
            // Malformed key — fall back to the current journal chapter so
            // the caller's name lookup runs there. NotFound bubbles up
            // naturally if the page isn't there.
            return Ok(Some(journal_chapter_id));
        }
    };
    let current_year = current_year_local(settings);
    if key_year >= current_year {
        return Ok(Some(journal_chapter_id));
    }

    // Past year — look up the archive chapter by name. We don't auto-create
    // here (read shouldn't manufacture structure); if the archive doesn't
    // exist yet, the caller surfaces NotFound for the page.
    let chapter_name = archive_chapter_name(key_year);

    // Try the index first, fall back to live BookStack walk.
    if let Ok(chapters) = ctx.index_db.list_indexed_chapters_by_book(identity_book_id).await {
        if let Some(c) = chapters.iter().find(|c| c.name == chapter_name) {
            return Ok(Some(c.chapter_id));
        }
    }
    let row = ctx.client.find_chapter_in_book(identity_book_id, &chapter_name).await?;
    Ok(row.and_then(|r| r.get("id").and_then(|v| v.as_i64())))
}

/// Extract the year from a journal key. Accepts `YYYY-MM-DD` or anything
/// that starts with a 4-digit year prefix. Returns `None` when the prefix
/// doesn't parse.
fn parse_year_from_key(key: &str) -> Option<i32> {
    let prefix: String = key.chars().take(4).collect();
    if prefix.len() < 4 {
        return None;
    }
    prefix.parse::<i32>().ok()
}

/// Best-effort helper: when the parent journal-chapter id is known, peek
/// at the resolved chapter for any year. Used by `handle_search` to walk
/// every archive chapter when `--include-archives=true`. Falls back to an
/// empty list when neither index nor BookStack returns chapters.
pub async fn list_archive_chapter_ids(identity_book_id: i64, ctx: &Context) -> Vec<i64> {
    let prefix = "Journal Archive - ";
    if let Ok(chapters) = ctx.index_db.list_indexed_chapters_by_book(identity_book_id).await {
        let mut ids: Vec<i64> = chapters
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .map(|c| c.chapter_id)
            .collect();
        if !ids.is_empty() {
            // Sort newest first so search results return recent archives
            // before old ones. Stable, year-based ordering.
            ids.sort_by(|a, b| b.cmp(a));
            return ids;
        }
    }

    if let Ok(book) = ctx.client.get_book(identity_book_id).await {
        if let Some(contents) = book.get("contents").and_then(|v| v.as_array()) {
            return contents
                .iter()
                .filter(|item| {
                    item.get("type").and_then(|t| t.as_str()) == Some("chapter")
                        && item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|n| n.starts_with(prefix))
                            .unwrap_or(false)
                })
                .filter_map(|item| item.get("id").and_then(|v| v.as_i64()))
                .collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_tz(tz: &str) -> UserSettings {
        UserSettings {
            timezone: Some(tz.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn archive_name_is_canonical() {
        assert_eq!(archive_chapter_name(2024), "Journal Archive - 2024");
        assert_eq!(archive_chapter_name(1999), "Journal Archive - 1999");
    }

    #[test]
    fn parse_year_handles_iso_date_keys() {
        assert_eq!(parse_year_from_key("2026-04-28"), Some(2026));
        assert_eq!(parse_year_from_key("1999-12-31"), Some(1999));
        assert_eq!(parse_year_from_key("nope"), None);
        assert_eq!(parse_year_from_key("12-1-2"), None);
    }

    #[test]
    fn year_from_iso_uses_user_tz() {
        // 2026-01-01T03:00:00Z is still Dec 31, 2025 in NYC.
        let s = settings_with_tz("America/New_York");
        let y = year_from_iso_timestamp("2026-01-01T03:00:00Z", &s).unwrap();
        assert_eq!(y, 2025, "NYC tz should pull this back to 2025");

        // Same instant in Tokyo is already Jan 1, 2026.
        let s = settings_with_tz("Asia/Tokyo");
        let y = year_from_iso_timestamp("2026-01-01T03:00:00Z", &s).unwrap();
        assert_eq!(y, 2026);
    }

    #[test]
    fn year_from_iso_falls_back_to_utc() {
        // Empty/invalid tz → UTC.
        let s = UserSettings::default();
        let y = year_from_iso_timestamp("2025-06-15T12:00:00Z", &s).unwrap();
        assert_eq!(y, 2025);
    }

    #[test]
    fn year_from_iso_returns_none_for_bad_input() {
        let s = UserSettings::default();
        assert!(year_from_iso_timestamp("not-a-date", &s).is_none());
        assert!(year_from_iso_timestamp("", &s).is_none());
    }
}
