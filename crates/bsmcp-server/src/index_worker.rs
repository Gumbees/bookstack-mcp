//! v1.0.0 reconciliation worker.
//!
//! Background tokio task that keeps `bookstack_*` index tables and
//! `page_cache` in sync with the live BookStack instance. Phase 4b ships
//! the worker scaffolding + the initial full walk + a single-page
//! reconcile helper (the building block that webhook + delta walk in
//! Phase 4c will reuse).
//!
//! Triggers (this phase):
//!   - On startup: enqueue an `all` job if `index_meta.last_full_walk_at`
//!     is not yet set.
//!
//! Triggers added in Phase 4c:
//!   - Webhook: enqueue `page:{id}` jobs on BookStack page events.
//!   - Periodic delta walk: every BSMCP_INDEX_DELTA_INTERVAL_SECONDS,
//!     reconcile pages whose `updated_at > last_delta_walk_at`.
//!
//! Storage backend: writes through the IndexDb trait. SQLite has the real
//! impl; Postgres returns a clear error from each method (issue #36) — so
//! the worker is a no-op on Postgres deployments until that lands. A
//! BSMCP_INDEX_WORKER env flag gates the spawn entirely so operators can
//! opt out.
//!
//! Auth: uses the BSMCP_INDEX_TOKEN_* (falling back to BSMCP_EMBED_TOKEN_*)
//! BookStack API token, which must have read access to every shelf the
//! worker walks. Per-user content access for live MCP requests still uses
//! the user's own token; the worker is structural reconciliation only.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::{DbBackend, IndexDb};
use bsmcp_common::index::*;
use bsmcp_common::settings::GlobalSettings;

/// How often the poll loop checks for a pending job when the queue is empty.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Cap on the number of pages reconciled before we yield the runtime
/// briefly during a full walk. Prevents one giant walk from monopolising
/// the worker task.
const YIELD_EVERY: usize = 25;

pub struct IndexWorker {
    bs_client: BookStackClient,
    db: Arc<dyn DbBackend>,
    index_db: Arc<dyn IndexDb>,
}

impl IndexWorker {
    pub fn new(
        bs_client: BookStackClient,
        db: Arc<dyn DbBackend>,
        index_db: Arc<dyn IndexDb>,
    ) -> Self {
        Self { bs_client, db, index_db }
    }

    /// Spawn the worker as a background tokio task. Returns the JoinHandle
    /// so the caller can hold a reference (or `forget()` it for fire-and-
    /// forget semantics, which matches the existing `spawn_acl_reconcile`
    /// pattern in semantic.rs).
    ///
    /// `delta_interval_secs` controls the periodic delta-walk cadence
    /// (0 disables it). Webhook-triggered jobs still arrive in real time;
    /// the periodic walk is a safety net for missed webhooks.
    pub fn spawn(self, delta_interval_secs: u64) -> tokio::task::JoinHandle<()> {
        let worker = Arc::new(self);
        let worker_for_delta = worker.clone();

        // Periodic delta walk timer — independent task that just enqueues
        // a `delta` job at intervals. The poll loop picks it up and runs
        // the actual walk. Gated on a non-zero interval so operators can
        // disable polling entirely (webhook-only mode).
        if delta_interval_secs > 0 {
            tokio::spawn(async move {
                let interval = Duration::from_secs(delta_interval_secs);
                eprintln!(
                    "IndexWorker: delta walk cron active — every {delta_interval_secs}s"
                );
                // Stagger the first delta so it doesn't race the initial
                // full walk.
                tokio::time::sleep(Duration::from_secs(60)).await;
                loop {
                    match worker_for_delta
                        .index_db
                        .create_index_job("delta", "both", "cron")
                        .await
                    {
                        Ok((id, is_new)) => {
                            if is_new {
                                eprintln!("IndexWorker: delta cron — queued job {id}");
                            }
                        }
                        Err(e) => eprintln!("IndexWorker: delta cron enqueue failed: {e}"),
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        } else {
            eprintln!("IndexWorker: delta walk cron disabled (interval=0)");
        }

        // Main poll loop.
        tokio::spawn(async move {
            // Stagger initial check by a few seconds so server startup
            // isn't immediately followed by a heavy walk.
            tokio::time::sleep(Duration::from_secs(10)).await;

            if let Err(e) = worker.maybe_enqueue_initial_walk().await {
                eprintln!("IndexWorker: maybe_enqueue_initial_walk failed (non-fatal): {e}");
            }
            worker.poll_loop().await;
        })
    }

    /// On first start, queue a full walk if it's never been done.
    async fn maybe_enqueue_initial_walk(&self) -> Result<(), String> {
        if self.index_db.get_index_meta("last_full_walk_at").await?.is_some() {
            return Ok(());
        }
        let (id, is_new) = self
            .index_db
            .create_index_job("all", "both", "startup")
            .await?;
        if is_new {
            eprintln!("IndexWorker: enqueued initial full walk (job {id})");
        }
        Ok(())
    }

    async fn poll_loop(&self) {
        loop {
            match self.index_db.claim_next_index_job().await {
                Ok(Some(job)) => self.handle_job(job).await,
                Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
                Err(e) => {
                    eprintln!("IndexWorker: claim_next_index_job error: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    async fn handle_job(&self, job: IndexJob) {
        eprintln!(
            "IndexWorker: processing job {} scope={} kind={} triggered_by={}",
            job.id, job.scope, job.kind, job.triggered_by
        );
        let result = self.process_job(&job).await;
        let outcome = match &result {
            Ok(()) => self.index_db.complete_index_job(job.id, None).await,
            Err(e) => {
                eprintln!("IndexWorker: job {} failed: {e}", job.id);
                self.index_db.complete_index_job(job.id, Some(e)).await
            }
        };
        if let Err(e) = outcome {
            eprintln!("IndexWorker: job {} completion update failed: {e}", job.id);
        }
    }

    async fn process_job(&self, job: &IndexJob) -> Result<(), String> {
        match job.scope.as_str() {
            "all" => self.walk_all().await,
            "delta" => self.walk_delta().await,
            scope if scope.starts_with("page:") => {
                let id: i64 = scope
                    .strip_prefix("page:")
                    .ok_or("invalid page scope")?
                    .parse()
                    .map_err(|e| format!("invalid page scope id: {e}"))?;
                self.reconcile_page(id).await
            }
            other => Err(format!("unknown scope: {other}")),
        }
    }

    /// Initial full walk — every shelf in globals → books → chapters →
    /// pages. Also handles pages loose at the book root (no chapter).
    /// Sets `index_meta.last_full_walk_at` on success.
    async fn walk_all(&self) -> Result<(), String> {
        let globals = self.db.get_global_settings().await.unwrap_or_default();
        let shelves: Vec<i64> = [globals.hive_shelf_id, globals.user_journals_shelf_id]
            .into_iter()
            .flatten()
            .collect();
        if shelves.is_empty() {
            eprintln!("IndexWorker: no shelves configured in globals — full walk does nothing");
            self.stamp_full_walk_done().await?;
            return Ok(());
        }
        let mut total_pages = 0usize;
        for shelf_id in shelves {
            match self.walk_shelf(shelf_id, &globals).await {
                Ok(n) => total_pages += n,
                Err(e) => eprintln!("IndexWorker: walk_shelf({shelf_id}) failed (non-fatal): {e}"),
            }
        }
        self.stamp_full_walk_done().await?;
        eprintln!("IndexWorker: full walk complete — {total_pages} pages reconciled");
        Ok(())
    }

    async fn stamp_full_walk_done(&self) -> Result<(), String> {
        let now = current_unix();
        self.index_db
            .set_index_meta("last_full_walk_at", &now.to_string())
            .await
    }

    /// Periodic delta walk — list pages whose `updated_at` advanced past
    /// `last_delta_walk_at` (or `last_full_walk_at` on first run) and
    /// reconcile each. Advances `last_delta_walk_at` to the newest
    /// `updated_at` seen so a subsequent run resumes from the correct
    /// boundary even if some pages failed to reconcile.
    async fn walk_delta(&self) -> Result<(), String> {
        let last_walk = match self.index_db.get_index_meta("last_delta_walk_at").await? {
            Some(v) => v,
            None => match self.index_db.get_index_meta("last_full_walk_at").await? {
                Some(v) => unix_to_iso(&v),
                None => {
                    eprintln!(
                        "IndexWorker: walk_delta — no last_full_walk_at; full walk first"
                    );
                    return Ok(());
                }
            },
        };

        let pages = self
            .bs_client
            .list_pages_updated_since(&last_walk, 250)
            .await?;
        eprintln!(
            "IndexWorker: walk_delta since {last_walk} — {} candidate pages",
            pages.len()
        );

        let mut newest_seen = last_walk.clone();
        let mut reconciled = 0usize;
        for page in pages {
            let Some(page_id) = page.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            let updated_at = page
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from);

            if let Err(e) = self.reconcile_page(page_id).await {
                eprintln!(
                    "IndexWorker: walk_delta reconcile_page({page_id}) failed (non-fatal): {e}"
                );
                continue;
            }
            reconciled += 1;
            if let Some(ts) = updated_at {
                if ts > newest_seen {
                    newest_seen = ts;
                }
            }
        }

        // Stamp the boundary even on a no-op pass so periodic runs don't
        // keep re-listing the same window. If newest_seen advanced, future
        // walks pick up from there; if no pages came back, we use `now`
        // so we don't redundantly query the same window.
        let advance_to = if newest_seen != last_walk {
            newest_seen
        } else {
            iso_now()
        };
        self.index_db
            .set_index_meta("last_delta_walk_at", &advance_to)
            .await?;
        eprintln!(
            "IndexWorker: walk_delta complete — {reconciled} reconciled, advanced to {advance_to}"
        );
        Ok(())
    }

    async fn walk_shelf(&self, shelf_id: i64, globals: &GlobalSettings) -> Result<usize, String> {
        let shelf = self.bs_client.get_shelf(shelf_id).await?;
        let name = string_field(&shelf, "name");
        let slug = string_field(&shelf, "slug");
        let shelf_kind = classify_shelf(
            shelf_id,
            globals.hive_shelf_id,
            globals.user_journals_shelf_id,
        );
        self.index_db
            .upsert_indexed_shelf(&IndexedShelf {
                shelf_id,
                name,
                slug,
                shelf_kind,
                indexed_at: current_unix(),
                deleted: false,
            })
            .await?;
        let books: Vec<i64> = shelf
            .get("books")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("id").and_then(|v| v.as_i64()))
                    .collect()
            })
            .unwrap_or_default();
        let mut count = 0usize;
        for book_id in books {
            match self.walk_book(book_id, Some(shelf_id), shelf_kind).await {
                Ok(n) => count += n,
                Err(e) => eprintln!(
                    "IndexWorker: walk_book({book_id}) failed (non-fatal): {e}"
                ),
            }
        }
        Ok(count)
    }

    async fn walk_book(
        &self,
        book_id: i64,
        shelf_id: Option<i64>,
        shelf_kind: ShelfKind,
    ) -> Result<usize, String> {
        let book = self.bs_client.get_book(book_id).await?;
        let name = string_field(&book, "name");
        let slug = string_field(&book, "slug");
        let book_kind = classify_book(&name, shelf_kind);
        // For Identity/UserIdentity books, dig out the ouid from the
        // manifest page's frontmatter so it can be propagated to every
        // descendant we index.
        let identity_ouid = if matches!(book_kind, BookKind::Identity | BookKind::UserIdentity) {
            self.find_book_identity_ouid(&book).await
        } else {
            None
        };
        self.index_db
            .upsert_indexed_book(&IndexedBook {
                book_id,
                name,
                slug,
                shelf_id,
                identity_ouid: identity_ouid.clone(),
                book_kind,
                indexed_at: current_unix(),
                deleted: false,
            })
            .await?;

        // The book contents array mixes chapters and loose-at-root pages.
        // Dispatch each accordingly.
        let contents = book
            .get("contents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut count = 0usize;
        let mut idx = 0usize;
        for item in contents {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("chapter") => {
                    if let Some(chapter_id) = item.get("id").and_then(|v| v.as_i64()) {
                        match self
                            .walk_chapter(chapter_id, book_id, book_kind, identity_ouid.as_deref())
                            .await
                        {
                            Ok(n) => count += n,
                            Err(e) => eprintln!(
                                "IndexWorker: walk_chapter({chapter_id}) failed (non-fatal): {e}"
                            ),
                        }
                    }
                }
                Some("page") => {
                    if let Some(page_id) = item.get("id").and_then(|v| v.as_i64()) {
                        if let Err(e) = self
                            .reconcile_page_with_parents(
                                page_id,
                                book_id,
                                None,
                                book_kind,
                                None,
                                None,
                                identity_ouid.as_deref(),
                            )
                            .await
                        {
                            eprintln!(
                                "IndexWorker: reconcile_page({page_id}) failed (non-fatal): {e}"
                            );
                        }
                        count += 1;
                    }
                }
                _ => {}
            }
            idx += 1;
            if idx % YIELD_EVERY == 0 {
                tokio::task::yield_now().await;
            }
        }
        Ok(count)
    }

    async fn walk_chapter(
        &self,
        chapter_id: i64,
        book_id: i64,
        parent_book_kind: BookKind,
        identity_ouid: Option<&str>,
    ) -> Result<usize, String> {
        let chapter = self.bs_client.get_chapter(chapter_id).await?;
        let name = string_field(&chapter, "name");
        let slug = string_field(&chapter, "slug");
        let (chapter_kind, archive_year) = classify_chapter(&name, parent_book_kind);
        self.index_db
            .upsert_indexed_chapter(&IndexedChapter {
                chapter_id,
                book_id,
                name,
                slug,
                identity_ouid: identity_ouid.map(String::from),
                chapter_kind,
                archive_year,
                indexed_at: current_unix(),
                deleted: false,
            })
            .await?;
        let pages = chapter
            .get("pages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut count = 0usize;
        let mut idx = 0usize;
        for page in pages {
            if let Some(page_id) = page.get("id").and_then(|v| v.as_i64()) {
                if let Err(e) = self
                    .reconcile_page_with_parents(
                        page_id,
                        book_id,
                        Some(chapter_id),
                        parent_book_kind,
                        Some(chapter_kind),
                        archive_year,
                        identity_ouid,
                    )
                    .await
                {
                    eprintln!(
                        "IndexWorker: reconcile_page({page_id}) failed (non-fatal): {e}"
                    );
                }
                count += 1;
            }
            idx += 1;
            if idx % YIELD_EVERY == 0 {
                tokio::task::yield_now().await;
            }
        }
        Ok(count)
    }

    /// Single-page reconcile keyed off page id. Looks up parent classification
    /// from the index (so the parent walk has to have happened first), then
    /// upserts the page row + cache row in one transaction. Used by:
    ///   - Phase 4c webhook handler (page event → enqueue page:{id} job)
    ///   - Phase 4c periodic delta walk (each delta page goes through here)
    async fn reconcile_page(&self, page_id: i64) -> Result<(), String> {
        let page = self.bs_client.get_page(page_id).await?;
        let book_id = page
            .get("book_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| format!("page {page_id} has no book_id"))?;
        let chapter_id = page
            .get("chapter_id")
            .and_then(|v| v.as_i64())
            .filter(|&id| id != 0);

        let parent_book = self.index_db.get_indexed_book(book_id).await?.ok_or_else(|| {
            format!(
                "parent book {book_id} not in index — initial full walk hasn't covered it yet"
            )
        })?;
        let parent_chapter = if let Some(cid) = chapter_id {
            self.index_db.get_indexed_chapter(cid).await?
        } else {
            None
        };

        let parent_chapter_kind = parent_chapter.as_ref().map(|c| c.chapter_kind);
        let parent_chapter_archive_year = parent_chapter.as_ref().and_then(|c| c.archive_year);

        // The classify_page call inside reconcile_page_with_parents already
        // pulls the page name from `page`; but to keep that helper backend-
        // agnostic we re-fetch here. Given page is already in scope, pass it
        // directly via the lower-level helper.
        self.reconcile_page_inner(
            &page,
            book_id,
            chapter_id,
            parent_book.book_kind,
            parent_chapter_kind,
            parent_chapter_archive_year,
            parent_book.identity_ouid.as_deref(),
        )
        .await
    }

    /// Same as `reconcile_page` but takes parent classification + ouid from
    /// the caller (the full walk already has them in hand without the extra
    /// index lookup).
    #[allow(clippy::too_many_arguments)]
    async fn reconcile_page_with_parents(
        &self,
        page_id: i64,
        book_id: i64,
        chapter_id: Option<i64>,
        parent_book_kind: BookKind,
        parent_chapter_kind: Option<ChapterKind>,
        parent_chapter_archive_year: Option<i32>,
        identity_ouid: Option<&str>,
    ) -> Result<(), String> {
        let page = self.bs_client.get_page(page_id).await?;
        self.reconcile_page_inner(
            &page,
            book_id,
            chapter_id,
            parent_book_kind,
            parent_chapter_kind,
            parent_chapter_archive_year,
            identity_ouid,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn reconcile_page_inner(
        &self,
        page: &Value,
        book_id: i64,
        chapter_id: Option<i64>,
        parent_book_kind: BookKind,
        parent_chapter_kind: Option<ChapterKind>,
        parent_chapter_archive_year: Option<i32>,
        identity_ouid: Option<&str>,
    ) -> Result<(), String> {
        let page_id = page
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or("page has no id")?;
        let name = string_field(page, "name");
        let (page_kind, page_key, archive_year_from_page) = classify_page(
            &name,
            parent_book_kind,
            parent_chapter_kind,
            parent_chapter_archive_year,
        );
        // archive_year priority: classifier's signal first (pages in archive
        // chapters); fall back to chapter's archive year if the classifier
        // didn't surface one (defensive — usually identical).
        let archive_year = archive_year_from_page.or(parent_chapter_archive_year);
        let now = current_unix();
        let raw_md = page.get("markdown").and_then(|v| v.as_str()).map(String::from);
        let html = page.get("html").and_then(|v| v.as_str()).map(String::from);
        let page_updated_at = page
            .get("updated_at")
            .and_then(|v| v.as_str())
            .map(String::from);
        let stripped_md = raw_md.as_deref().map(strip_frontmatter);

        let indexed = IndexedPage {
            page_id,
            book_id,
            chapter_id,
            name,
            slug: string_field(page, "slug"),
            url: page.get("url").and_then(|v| v.as_str()).map(String::from),
            page_created_at: page
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            page_updated_at: page_updated_at.clone(),
            identity_ouid: identity_ouid.map(String::from),
            page_kind,
            page_key,
            archive_year,
            indexed_at: now,
            deleted: false,
        };
        let cache = PageCache {
            page_id,
            markdown: stripped_md,
            raw_markdown: raw_md,
            html,
            cached_at: now,
            page_updated_at,
        };
        self.index_db.upsert_indexed_page(&indexed, Some(&cache)).await
    }

    /// For an Identity / UserIdentity book, find the manifest page (loose
    /// at book root, named "Identity") and pull its `ai_identity_ouid`
    /// frontmatter field. Best-effort — returns None if the book has no
    /// manifest yet, the manifest has no frontmatter, or the field is
    /// absent. Identity books without an ouid still index correctly; the
    /// dedup UNIQUE index just doesn't fire for them.
    async fn find_book_identity_ouid(&self, book: &Value) -> Option<String> {
        let contents = book.get("contents").and_then(|v| v.as_array())?;
        let manifest = contents.iter().find(|item| {
            item.get("type").and_then(|v| v.as_str()) == Some("page")
                && item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.trim())
                    == Some("Identity")
        })?;
        let manifest_id = manifest.get("id").and_then(|v| v.as_i64())?;
        let page = self.bs_client.get_page(manifest_id).await.ok()?;
        let md = page.get("markdown").and_then(|v| v.as_str())?;
        extract_ouid_from_frontmatter(md)
    }
}

// --- helpers ---

fn current_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render the current UTC moment as ISO 8601 (e.g. `2026-04-27T03:14:15Z`).
/// Used by the delta walk's `last_delta_walk_at` checkpoint when no pages
/// came back in a polling pass.
fn iso_now() -> String {
    let secs = current_unix();
    let (y, mo, d, h, mi, s) = unix_to_components(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Convert a stored unix-seconds value (e.g., from `last_full_walk_at`) to
/// an ISO 8601 string suitable for the BookStack `filter[updated_at:gt]`
/// param. Falls back to "1970-01-01T00:00:00Z" on parse failure.
fn unix_to_iso(stored: &str) -> String {
    let secs: i64 = stored.parse().unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_components(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn unix_to_components(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400) as u32;
    let h = time / 3600;
    let mi = (time % 3600) / 60;
    let s = time % 60;
    let (y, mo, d) = days_to_ymd(days);
    (y, mo, d, h, mi, s)
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn string_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// Strip a leading YAML frontmatter block, if any. Inlined (rather than
/// reusing `crate::remember::frontmatter::strip`) to avoid a coupling
/// where the worker depends on the remember module.
fn strip_frontmatter(md: &str) -> String {
    let trimmed = md.trim_start();
    let Some(after_open) = trimmed.strip_prefix("---") else {
        return md.to_string();
    };
    let after_open = after_open
        .strip_prefix("\r\n")
        .or_else(|| after_open.strip_prefix('\n'))
        .unwrap_or(after_open);
    let mut pos = 0usize;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return after_open[pos + line.len()..].trim_start().to_string();
        }
        pos += line.len();
    }
    md.to_string()
}

fn extract_ouid_from_frontmatter(md: &str) -> Option<String> {
    let trimmed = md.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let mut iter = trimmed.lines();
    iter.next(); // opening ---
    for line in iter {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(rest) = line
            .strip_prefix("ai_identity_ouid:")
            .or_else(|| line.strip_prefix("ouid:"))
        {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ouid_present() {
        let md = "---\nai_identity_ouid: 019dc66e4dc27d329e4a4abd1bec0c80\nname: Pia\n---\n\n# Pia\n";
        assert_eq!(
            extract_ouid_from_frontmatter(md).as_deref(),
            Some("019dc66e4dc27d329e4a4abd1bec0c80")
        );
    }

    #[test]
    fn extract_ouid_missing() {
        let md = "---\nname: Pia\n---\n\n# Pia\n";
        assert!(extract_ouid_from_frontmatter(md).is_none());
    }

    #[test]
    fn extract_ouid_no_frontmatter() {
        assert!(extract_ouid_from_frontmatter("# Pia\n\nmanifest body").is_none());
    }

    #[test]
    fn extract_ouid_quoted() {
        let md = "---\nouid: \"abc-123\"\n---\nbody";
        assert_eq!(extract_ouid_from_frontmatter(md).as_deref(), Some("abc-123"));
    }

    #[test]
    fn strip_frontmatter_removes_block() {
        let md = "---\nfoo: bar\n---\n\nbody text\n";
        assert_eq!(strip_frontmatter(md), "body text\n");
    }

    #[test]
    fn strip_frontmatter_no_block() {
        assert_eq!(strip_frontmatter("just body"), "just body");
    }
}
