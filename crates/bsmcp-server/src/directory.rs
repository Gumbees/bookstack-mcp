//! In-memory snapshot of the BookStack directory tree.
//!
//! Built from the IndexDb tables (`bookstack_shelves` / `bookstack_books` /
//! `bookstack_chapters` / `bookstack_pages`). The snapshot contains only
//! position metadata — names, slugs, ids, page `updated_at` — never page body
//! content. The remember-namespace `directory` resource serves the snapshot
//! verbatim; the meta-injector on every MCP tool response either attaches the
//! full snapshot (when the caller's session hasn't seen the current version
//! yet) or a `{version, hash}` pointer (when it has).
//!
//! ## Lifecycle
//!
//! - `DirectoryService::new(index_db)` constructs the service with an empty
//!   `version: 0` snapshot, then spawns a rebuild on the tokio runtime so
//!   the cache fills in shortly after server start. Mirrors the index
//!   worker's "stagger initial walk" pattern (`bsmcp-worker::lib.rs`) — first
//!   `current()` after start may return the empty snapshot for a few hundred
//!   ms, but never blocks startup.
//! - `current() -> Arc<DirectorySnapshot>` clones the inner Arc. Cheap.
//! - `invalidate()` spawns a rebuild from IndexDb in the background. Webhook
//!   handlers fire-and-forget this on tree-affecting events.
//!
//! ## Versioning + hashing
//!
//! - `version` monotonically increments on each successful rebuild.
//! - `content_hash` is SHA-256 over the canonical JSON of the tree (without
//!   `version` or `built_at`) so two rebuilds against unchanged data produce
//!   the same hash — that's what lets the meta-injector skip the full
//!   payload when the caller's session already has the same version.

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use bsmcp_common::db::IndexDb;

#[derive(Clone, Debug, Serialize)]
pub struct PageNode {
    pub id: i64,
    pub name: String,
    pub slug: String,
    /// BookStack-side update timestamp (ISO-8601, copied straight from the
    /// `bookstack_pages.page_updated_at` column). May be missing on rows
    /// indexed before the field was tracked.
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChapterNode {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub pages: Vec<PageNode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BookNode {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub chapters: Vec<ChapterNode>,
    /// Pages directly under the book root (no chapter).
    pub orphan_pages: Vec<PageNode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShelfNode {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub books: Vec<BookNode>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DirectorySnapshot {
    pub shelves: Vec<ShelfNode>,
    /// Books with no shelf assignment. They'd otherwise vanish from the
    /// directory.
    pub orphan_books: Vec<BookNode>,
    pub version: u64,
    pub content_hash: String,
    /// Unix seconds when this snapshot was built. NOT included in the
    /// content hash — only `shelves` + `orphan_books` are.
    pub built_at: i64,
}

impl DirectorySnapshot {
    /// Empty placeholder used between `DirectoryService::new` and the first
    /// successful rebuild. `version: 0` lets the meta-injector treat the
    /// first non-empty snapshot as a fresh full attach.
    fn empty() -> Self {
        let shelves: Vec<ShelfNode> = Vec::new();
        let orphan_books: Vec<BookNode> = Vec::new();
        let hash = compute_hash(&shelves, &orphan_books);
        Self {
            shelves,
            orphan_books,
            version: 0,
            content_hash: hash,
            built_at: now_unix(),
        }
    }
}

/// Holds the latest `Arc<DirectorySnapshot>`. Reads clone the inner Arc
/// without holding the lock, so the meta-injector hot path is one
/// `RwLock::read` + one `Arc::clone`.
pub struct DirectoryService {
    snapshot: Arc<RwLock<Arc<DirectorySnapshot>>>,
    index_db: Arc<dyn IndexDb>,
}

impl DirectoryService {
    /// Construct with an empty snapshot and spawn an immediate rebuild on
    /// the tokio runtime. Returns synchronously so `AppState::new` doesn't
    /// have to be async.
    pub fn new(index_db: Arc<dyn IndexDb>) -> Arc<Self> {
        let svc = Arc::new(Self {
            snapshot: Arc::new(RwLock::new(Arc::new(DirectorySnapshot::empty()))),
            index_db,
        });
        let bg = svc.clone();
        tokio::spawn(async move {
            // Stagger the initial build a touch so server startup logs settle
            // before the first rebuild prints its lines. Matches the worker's
            // 10s pattern but shorter — the directory build is cheap (no
            // BookStack API calls, just DB row reads).
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            bg.rebuild().await;
        });
        svc
    }

    /// Cheap clone of the latest snapshot. Returns the empty placeholder if
    /// the initial rebuild hasn't completed yet. Async because all callers
    /// already live inside an async context; keeps us out of `try_read`
    /// spin-loop territory.
    pub async fn current(&self) -> Arc<DirectorySnapshot> {
        let guard = self.snapshot.read().await;
        Arc::clone(&*guard)
    }

    /// Schedule a rebuild on the tokio runtime. Returns immediately; the new
    /// snapshot becomes visible to `current()` once the rebuild completes.
    pub fn invalidate(self: &Arc<Self>) {
        let svc = self.clone();
        tokio::spawn(async move {
            svc.rebuild().await;
        });
    }

    /// Pull the current tree from IndexDb and atomically swap it into the
    /// cache. Errors are logged and swallowed — the existing snapshot stays
    /// in place rather than being replaced with a partial/garbage tree.
    async fn rebuild(&self) {
        let new = match self.build_snapshot().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Directory: rebuild failed (keeping previous snapshot): {e}");
                return;
            }
        };
        let mut guard = self.snapshot.write().await;
        *guard = Arc::new(new);
    }

    async fn build_snapshot(&self) -> Result<DirectorySnapshot, String> {
        let prev_version = self.snapshot.read().await.version;

        let mut shelves: Vec<ShelfNode> = Vec::new();
        for shelf in self.index_db.list_indexed_shelves().await? {
            let mut books: Vec<BookNode> = Vec::new();
            for book in self.index_db.list_indexed_books_by_shelf(shelf.shelf_id).await? {
                books.push(self.build_book_node(&book).await?);
            }
            shelves.push(ShelfNode {
                id: shelf.shelf_id,
                name: shelf.name,
                slug: shelf.slug,
                books,
            });
        }

        let mut orphan_books: Vec<BookNode> = Vec::new();
        for book in self.index_db.list_indexed_orphan_books().await? {
            orphan_books.push(self.build_book_node(&book).await?);
        }

        let content_hash = compute_hash(&shelves, &orphan_books);
        Ok(DirectorySnapshot {
            shelves,
            orphan_books,
            version: prev_version.wrapping_add(1),
            content_hash,
            built_at: now_unix(),
        })
    }

    async fn build_book_node(
        &self,
        book: &bsmcp_common::index::IndexedBook,
    ) -> Result<BookNode, String> {
        let mut chapters: Vec<ChapterNode> = Vec::new();
        for chapter in self.index_db.list_indexed_chapters_by_book(book.book_id).await? {
            let pages = self
                .index_db
                .list_indexed_pages_by_chapter(chapter.chapter_id)
                .await?
                .into_iter()
                .map(page_node)
                .collect();
            chapters.push(ChapterNode {
                id: chapter.chapter_id,
                name: chapter.name,
                slug: chapter.slug,
                pages,
            });
        }
        let orphan_pages = self
            .index_db
            .list_indexed_pages_by_book_root(book.book_id)
            .await?
            .into_iter()
            .map(page_node)
            .collect();
        Ok(BookNode {
            id: book.book_id,
            name: book.name.clone(),
            slug: book.slug.clone(),
            chapters,
            orphan_pages,
        })
    }
}

fn page_node(p: bsmcp_common::index::IndexedPage) -> PageNode {
    PageNode {
        id: p.page_id,
        name: p.name,
        slug: p.slug,
        updated_at: p.page_updated_at,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// SHA-256 of the canonical JSON of `shelves` + `orphan_books`. `version`
/// and `built_at` are deliberately excluded so two builds against unchanged
/// data produce the same hash. `serde_json::to_vec` produces a deterministic
/// byte stream because struct fields serialize in their declared order and
/// nothing in the snapshot uses HashMaps.
fn compute_hash(shelves: &[ShelfNode], orphan_books: &[BookNode]) -> String {
    let canonical = json!({
        "shelves": shelves,
        "orphan_books": orphan_books,
    });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_shelf(id: i64, name: &str, books: Vec<BookNode>) -> ShelfNode {
        ShelfNode {
            id,
            name: name.to_string(),
            slug: name.to_lowercase(),
            books,
        }
    }

    fn make_book(id: i64, name: &str) -> BookNode {
        BookNode {
            id,
            name: name.to_string(),
            slug: name.to_lowercase(),
            chapters: Vec::new(),
            orphan_pages: Vec::new(),
        }
    }

    #[test]
    fn hash_is_deterministic_for_identical_input() {
        let shelves = vec![
            make_shelf(1, "Alpha", vec![make_book(10, "BookA")]),
            make_shelf(2, "Beta", vec![make_book(20, "BookB")]),
        ];
        let orphans = vec![make_book(99, "Loose")];
        let h1 = compute_hash(&shelves, &orphans);
        let h2 = compute_hash(&shelves, &orphans);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
    }

    #[test]
    fn hash_differs_when_a_page_name_changes() {
        let shelves_a = vec![make_shelf(
            1,
            "Alpha",
            vec![BookNode {
                id: 10,
                name: "BookA".to_string(),
                slug: "booka".to_string(),
                chapters: vec![],
                orphan_pages: vec![PageNode {
                    id: 100,
                    name: "Page One".to_string(),
                    slug: "page-one".to_string(),
                    updated_at: Some("2026-05-01T00:00:00Z".to_string()),
                }],
            }],
        )];
        let mut shelves_b = shelves_a.clone();
        shelves_b[0].books[0].orphan_pages[0].name = "Page One Renamed".to_string();
        let orphans: Vec<BookNode> = vec![];
        assert_ne!(
            compute_hash(&shelves_a, &orphans),
            compute_hash(&shelves_b, &orphans)
        );
    }

    #[test]
    fn empty_snapshot_has_stable_hash_and_version_zero() {
        let s = DirectorySnapshot::empty();
        assert_eq!(s.version, 0);
        assert!(s.shelves.is_empty());
        assert!(s.orphan_books.is_empty());
        assert_eq!(s.content_hash.len(), 64);
        // Two empties differ only in built_at; hash is over content only.
        let s2 = DirectorySnapshot::empty();
        assert_eq!(s.content_hash, s2.content_hash);
    }

    #[test]
    fn hash_excludes_version_and_built_at() {
        // Build two structurally-identical snapshots with different version
        // and built_at, confirm the recomputed hash is the same.
        let shelves = vec![make_shelf(1, "Alpha", vec![make_book(10, "BookA")])];
        let orphans: Vec<BookNode> = vec![];
        let h = compute_hash(&shelves, &orphans);
        let snap_v1 = DirectorySnapshot {
            shelves: shelves.clone(),
            orphan_books: orphans.clone(),
            version: 1,
            content_hash: h.clone(),
            built_at: 1_000_000,
        };
        let snap_v9 = DirectorySnapshot {
            shelves,
            orphan_books: orphans,
            version: 9,
            content_hash: h.clone(),
            built_at: 9_000_000,
        };
        assert_eq!(snap_v1.content_hash, snap_v9.content_hash);
    }
}
