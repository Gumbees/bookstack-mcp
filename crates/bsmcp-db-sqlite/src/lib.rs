use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, AeadCore};
use async_trait::async_trait;
use base64::Engine;
use rusqlite::{Connection, params};
use sha2::Digest;
use zeroize::Zeroizing;

use bsmcp_common::config::ACCESS_TOKEN_TTL;
use bsmcp_common::db::{DbBackend, SemanticDb};
use bsmcp_common::types::*;
use bsmcp_common::vector;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

pub struct SqliteDb {
    conn: Arc<Mutex<Connection>>,
    encryption_key: Zeroizing<[u8; 32]>,
}

impl SqliteDb {
    pub fn open(path: &Path, encryption_key: &str) -> Self {
        let conn = Connection::open(path).expect("Failed to open SQLite database");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS access_tokens (
                 token TEXT PRIMARY KEY,
                 token_id TEXT NOT NULL,
                 token_secret TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_tokens_created ON access_tokens(created_at);
             DROP TABLE IF EXISTS registrations;",
        )
        .expect("Failed to initialize database schema");

        let hash = sha2::Sha256::digest(encryption_key.as_bytes());
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&hash);

        Self {
            conn: Arc::new(Mutex::new(conn)),
            encryption_key: key,
        }
    }

    /// SHA-256 hash a bearer token before storing as primary key.
    /// This prevents token theft from database read access.
    fn hash_token(token: &str) -> String {
        let hash = sha2::Sha256::digest(token.as_bytes());
        format!("{hash:x}")
    }

    fn encrypt(&self, plaintext: &str) -> String {
        let cipher = Aes256Gcm::new((&*self.encryption_key).into());
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("AES-GCM encryption failed");
        let mut combined = nonce.to_vec();
        combined.extend_from_slice(&ciphertext);
        BASE64.encode(&combined)
    }

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }

    fn cutoff_secs(ttl: Duration) -> i64 {
        Self::now_secs() - ttl.as_secs() as i64
    }

    fn cleanup_old_backups(backup_dir: &Path) {
        let mut backups: Vec<_> = std::fs::read_dir(backup_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("bookstack-mcp-backup-")
                    && e.file_name().to_string_lossy().ends_with(".db")
            })
            .collect();

        backups.sort_by_key(|e| e.file_name());

        if backups.len() > 3 {
            for entry in &backups[..backups.len() - 3] {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    eprintln!("Failed to remove old backup {}: {e}", entry.path().display());
                } else {
                    eprintln!("Removed old backup: {}", entry.file_name().to_string_lossy());
                }
            }
        }
    }
}

#[async_trait]
impl DbBackend for SqliteDb {
    async fn insert_access_token(&self, token: &str, id: &str, secret: &str) -> Result<(), String> {
        let conn = self.conn.clone();
        let token_hash = Self::hash_token(token);
        let enc_id = self.encrypt(id);
        let enc_secret = self.encrypt(secret);

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM access_tokens", [], |row| row.get(0))
                .unwrap_or(0);
            if count >= 10000 {
                let cutoff = SqliteDb::cutoff_secs(ACCESS_TOKEN_TTL);
                conn.execute("DELETE FROM access_tokens WHERE created_at <= ?1", params![cutoff]).ok();
            }
            conn.execute(
                "INSERT OR REPLACE INTO access_tokens (token, token_id, token_secret, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![token_hash, enc_id, enc_secret, SqliteDb::now_secs()],
            ).ok();
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_access_token(&self, token: &str) -> Result<Option<(String, String)>, String> {
        let conn = self.conn.clone();
        let token_hash = Self::hash_token(token);
        let token_raw = token.to_string();
        let encryption_key = *self.encryption_key;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let cutoff = SqliteDb::cutoff_secs(ACCESS_TOKEN_TTL);

            // Try hashed token first, then fall back to raw token (pre-hash migration)
            let result: Option<(String, String)> = conn.query_row(
                "SELECT token_id, token_secret FROM access_tokens WHERE token = ?1 AND created_at > ?2",
                params![token_hash, cutoff],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).ok().or_else(|| {
                conn.query_row(
                    "SELECT token_id, token_secret FROM access_tokens WHERE token = ?1 AND created_at > ?2",
                    params![token_raw, cutoff],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).ok()
            });

            let Some((stored_id, stored_secret)) = result else {
                return Ok(None);
            };

            // Try decrypting
            let cipher = Aes256Gcm::new((&encryption_key).into());
            let try_decrypt = |stored: &str| -> Option<String> {
                let combined = BASE64.decode(stored).ok()?;
                if combined.len() < 12 { return None; }
                let (nonce_bytes, ciphertext) = combined.split_at(12);
                let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
                let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
                String::from_utf8(plaintext).ok()
            };

            if let (Some(tid), Some(tsec)) = (try_decrypt(&stored_id), try_decrypt(&stored_secret)) {
                return Ok(Some((tid, tsec)));
            }

            // Decryption failed — treat as plaintext (pre-encryption data)
            // Re-encrypt in place for transparent migration
            let re_encrypt = |plaintext: &str| -> String {
                let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
                let ciphertext = cipher.encrypt(&nonce, plaintext.as_bytes()).expect("AES-GCM encryption failed");
                let mut combined = nonce.to_vec();
                combined.extend_from_slice(&ciphertext);
                BASE64.encode(&combined)
            };
            let enc_id = re_encrypt(&stored_id);
            let enc_secret = re_encrypt(&stored_secret);
            conn.execute(
                "UPDATE access_tokens SET token_id = ?1, token_secret = ?2 WHERE token = ?3",
                params![enc_id, enc_secret, token_raw],
            ).ok();

            Ok(Some((stored_id, stored_secret)))
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn cleanup_expired_tokens(&self) -> Result<(), String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let cutoff = SqliteDb::cutoff_secs(ACCESS_TOKEN_TTL);
            conn.execute("DELETE FROM access_tokens WHERE created_at <= ?1", params![cutoff]).ok();
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn backup(&self, backup_dir: &Path) -> Result<(), String> {
        let conn = self.conn.clone();
        let backup_dir = backup_dir.to_path_buf();

        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&backup_dir)
                .map_err(|e| format!("Failed to create backup directory: {e}"))?;

            let timestamp = {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let days = secs / 86400;
                let time_of_day = secs % 86400;
                let (year, month, day) = unix_days_to_ymd(days as i64);
                let hours = time_of_day / 3600;
                let minutes = (time_of_day % 3600) / 60;
                let seconds = time_of_day % 60;
                format!("{year:04}{month:02}{day:02}T{hours:02}{minutes:02}{seconds:02}")
            };

            let backup_file = backup_dir.join(format!("bookstack-mcp-backup-{timestamp}.db"));
            let backup_path_str = backup_file.to_string_lossy();

            let conn = conn.lock().unwrap();
            conn.execute_batch(&format!("VACUUM INTO '{}'", backup_path_str.replace('\'', "''")))
                .map_err(|e| format!("VACUUM INTO failed: {e}"))?;

            drop(conn);
            eprintln!("Backup created: {}", backup_file.display());

            SqliteDb::cleanup_old_backups(&backup_dir);
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }
}

#[async_trait]
impl SemanticDb for SqliteDb {
    async fn init_semantic_tables(&self) -> Result<(), String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pages (
                    page_id INTEGER PRIMARY KEY,
                    book_id INTEGER NOT NULL,
                    chapter_id INTEGER,
                    name TEXT NOT NULL,
                    slug TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    embedded_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS chunks (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    page_id INTEGER NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
                    chunk_index INTEGER NOT NULL,
                    heading_path TEXT NOT NULL,
                    content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    embedding BLOB NOT NULL,
                    UNIQUE(page_id, chunk_index)
                );
                CREATE TABLE IF NOT EXISTS relationships (
                    source_page_id INTEGER NOT NULL,
                    target_page_id INTEGER NOT NULL,
                    link_type TEXT NOT NULL DEFAULT 'link',
                    PRIMARY KEY (source_page_id, target_page_id, link_type)
                );
                CREATE TABLE IF NOT EXISTS embed_jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    scope TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    total_pages INTEGER DEFAULT 0,
                    done_pages INTEGER DEFAULT 0,
                    started_at INTEGER,
                    finished_at INTEGER,
                    error TEXT,
                    worker_id TEXT
                );",
            )
            .map_err(|e| format!("Failed to initialize semantic tables: {e}"))?;

            // Migration: add worker_id column if missing (existing databases)
            conn.execute_batch(
                "ALTER TABLE embed_jobs ADD COLUMN worker_id TEXT;"
            ).ok(); // ignore error if column already exists

            eprintln!("Semantic: tables initialized");
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn upsert_page(&self, meta: &PageMeta) -> Result<(), String> {
        let conn = self.conn.clone();
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO pages (page_id, book_id, chapter_id, name, slug, content_hash, embedded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(page_id) DO UPDATE SET
                    book_id = excluded.book_id,
                    chapter_id = excluded.chapter_id,
                    name = excluded.name,
                    slug = excluded.slug,
                    content_hash = excluded.content_hash,
                    embedded_at = excluded.embedded_at",
                params![meta.page_id, meta.book_id, meta.chapter_id, meta.name, meta.slug, meta.content_hash, SqliteDb::now_secs()],
            ).map_err(|e| format!("upsert_page failed: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn delete_page(&self, page_id: i64) -> Result<(), String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let tx = conn.unchecked_transaction().map_err(|e| format!("Transaction failed: {e}"))?;
            tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![page_id]).ok();
            tx.execute("DELETE FROM relationships WHERE source_page_id = ?1 OR target_page_id = ?1", params![page_id]).ok();
            tx.execute("DELETE FROM pages WHERE page_id = ?1", params![page_id]).ok();
            tx.commit().map_err(|e| format!("Commit failed: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_page_content_hash(&self, page_id: i64) -> Result<Option<String>, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Ok(conn.query_row(
                "SELECT content_hash FROM pages WHERE page_id = ?1",
                params![page_id],
                |row| row.get(0),
            ).ok())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_page_meta(&self, page_id: i64) -> Result<Option<PageMeta>, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Ok(conn.query_row(
                "SELECT page_id, book_id, chapter_id, name, slug, content_hash FROM pages WHERE page_id = ?1",
                params![page_id],
                |row| Ok(PageMeta {
                    page_id: row.get(0)?,
                    book_id: row.get(1)?,
                    chapter_id: row.get(2)?,
                    name: row.get(3)?,
                    slug: row.get(4)?,
                    content_hash: row.get(5)?,
                }),
            ).ok())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn resolve_page_slug(&self, slug: &str) -> Result<Option<i64>, String> {
        let conn = self.conn.clone();
        let slug = slug.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Ok(conn.query_row(
                "SELECT page_id FROM pages WHERE slug = ?1",
                params![slug],
                |row| row.get(0),
            ).ok())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn insert_chunks(&self, page_id: i64, chunks: &[ChunkInsert]) -> Result<(), String> {
        let conn = self.conn.clone();
        let chunks: Vec<ChunkInsert> = chunks.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let tx = conn.unchecked_transaction().map_err(|e| format!("Transaction failed: {e}"))?;
            tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![page_id]).ok();
            for chunk in &chunks {
                let blob = vector::embedding_to_blob(&chunk.embedding);
                if let Err(e) = tx.execute(
                    "INSERT INTO chunks (page_id, chunk_index, heading_path, content, content_hash, embedding)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![page_id, chunk.chunk_index as i64, chunk.heading_path, chunk.content, chunk.content_hash, blob],
                ) {
                    eprintln!("DB: insert chunk {} for page {page_id}: {e}", chunk.chunk_index);
                }
            }
            tx.commit().map_err(|e| format!("Commit failed: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_chunk_details(&self, chunk_ids: &[i64]) -> Result<Vec<ChunkDetail>, String> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let chunk_ids = chunk_ids.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let placeholders: Vec<String> = (0..chunk_ids.len()).map(|i| format!("?{}", i + 1)).collect();
            let sql = format!(
                "SELECT c.id, c.page_id, c.heading_path, c.content, p.name
                 FROM chunks c JOIN pages p ON c.page_id = p.page_id
                 WHERE c.id IN ({})",
                placeholders.join(",")
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare failed: {e}"))?;
            let params: Vec<&dyn rusqlite::types::ToSql> = chunk_ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok(ChunkDetail {
                    chunk_id: row.get(0)?,
                    page_id: row.get(1)?,
                    heading_path: row.get(2)?,
                    content: row.get(3)?,
                    page_name: row.get(4)?,
                })
            }).map_err(|e| format!("Query failed: {e}"))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn replace_relationships(&self, source: i64, targets: &[(i64, String)]) -> Result<(), String> {
        let conn = self.conn.clone();
        let targets: Vec<(i64, String)> = targets.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let tx = conn.unchecked_transaction().map_err(|e| format!("Transaction failed: {e}"))?;
            tx.execute("DELETE FROM relationships WHERE source_page_id = ?1", params![source]).ok();
            for (target_id, link_type) in &targets {
                tx.execute(
                    "INSERT OR IGNORE INTO relationships (source_page_id, target_page_id, link_type)
                     VALUES (?1, ?2, ?3)",
                    params![source, target_id, link_type],
                ).ok();
            }
            tx.commit().map_err(|e| format!("Commit failed: {e}"))?;
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_markov_blanket(&self, page_id: i64) -> Result<MarkovBlanket, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let query_related = |sql: &str, page_id: i64| -> Vec<RelatedPage> {
                conn.prepare(sql)
                    .and_then(|mut stmt| {
                        stmt.query_map(params![page_id], |row| Ok(RelatedPage {
                            page_id: row.get(0)?,
                            name: row.get(1)?,
                        }))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    })
                    .unwrap_or_default()
            };

            let linked_from = query_related(
                "SELECT r.source_page_id, p.name FROM relationships r
                 JOIN pages p ON r.source_page_id = p.page_id
                 WHERE r.target_page_id = ?1 LIMIT 20",
                page_id,
            );

            let links_to = query_related(
                "SELECT r.target_page_id, p.name FROM relationships r
                 JOIN pages p ON r.target_page_id = p.page_id
                 WHERE r.source_page_id = ?1 LIMIT 20",
                page_id,
            );

            let co_linked = query_related(
                "SELECT DISTINCT r2.source_page_id, p.name FROM relationships r1
                 JOIN relationships r2 ON r1.target_page_id = r2.target_page_id
                 JOIN pages p ON r2.source_page_id = p.page_id
                 WHERE r1.source_page_id = ?1 AND r2.source_page_id != ?1
                 LIMIT 10",
                page_id,
            );

            // Siblings: same chapter or same book
            let siblings = {
                let chapter_id: Option<i64> = conn
                    .query_row("SELECT chapter_id FROM pages WHERE page_id = ?1", params![page_id], |row| row.get(0))
                    .ok()
                    .flatten();

                if let Some(cid) = chapter_id {
                    let result: Vec<RelatedPage> = conn
                        .prepare("SELECT page_id, name FROM pages WHERE chapter_id = ?1 AND page_id != ?2 LIMIT 20")
                        .and_then(|mut stmt| {
                            stmt.query_map(params![cid, page_id], |row| Ok(RelatedPage {
                                page_id: row.get(0)?,
                                name: row.get(1)?,
                            }))
                            .map(|rows| rows.filter_map(|r| r.ok()).collect())
                        })
                        .unwrap_or_default();
                    if !result.is_empty() {
                        result
                    } else {
                        // Fall back to book siblings
                        let book_id: Option<i64> = conn
                            .query_row("SELECT book_id FROM pages WHERE page_id = ?1", params![page_id], |row| row.get(0))
                            .ok();
                        if let Some(bid) = book_id {
                            conn.prepare("SELECT page_id, name FROM pages WHERE book_id = ?1 AND page_id != ?2 LIMIT 20")
                                .and_then(|mut stmt| {
                                    stmt.query_map(params![bid, page_id], |row| Ok(RelatedPage {
                                        page_id: row.get(0)?,
                                        name: row.get(1)?,
                                    }))
                                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                                })
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        }
                    }
                } else {
                    let book_id: Option<i64> = conn
                        .query_row("SELECT book_id FROM pages WHERE page_id = ?1", params![page_id], |row| row.get(0))
                        .ok();
                    if let Some(bid) = book_id {
                        conn.prepare("SELECT page_id, name FROM pages WHERE book_id = ?1 AND page_id != ?2 LIMIT 20")
                            .and_then(|mut stmt| {
                                stmt.query_map(params![bid, page_id], |row| Ok(RelatedPage {
                                    page_id: row.get(0)?,
                                    name: row.get(1)?,
                                }))
                                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                            })
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                }
            };

            Ok(MarkovBlanket { linked_from, links_to, co_linked, siblings })
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn create_embed_job(&self, scope: &str) -> Result<(i64, bool), String> {
        let conn = self.conn.clone();
        let scope = scope.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            // Check for existing active job with same scope — return it instead of creating duplicate
            let existing: Option<i64> = conn.query_row(
                "SELECT id FROM embed_jobs WHERE scope = ?1 AND status IN ('pending', 'running') ORDER BY id DESC LIMIT 1",
                params![scope],
                |row| row.get(0),
            ).ok();
            if let Some(id) = existing {
                return Ok((id, false));
            }

            conn.execute(
                "INSERT INTO embed_jobs (scope, status, started_at) VALUES (?1, 'pending', ?2)",
                params![scope, SqliteDb::now_secs()],
            ).map_err(|e| format!("Failed to create embed job: {e}"))?;
            Ok((conn.last_insert_rowid(), true))
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn expire_stale_jobs(&self, stale_secs: i64) -> Result<usize, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let cutoff = SqliteDb::now_secs() - stale_secs;
            let now = SqliteDb::now_secs();

            // Supersede stale jobs that have a newer job with the same scope
            let superseded = conn.execute(
                "UPDATE embed_jobs SET status = 'error', finished_at = ?1, error = 'superseded'
                 WHERE status = 'running' AND started_at < ?2
                   AND EXISTS (
                       SELECT 1 FROM embed_jobs e2
                       WHERE e2.scope = embed_jobs.scope AND e2.id > embed_jobs.id
                         AND e2.status IN ('pending', 'running')
                   )",
                params![now, cutoff],
            ).map_err(|e| format!("expire_stale_jobs (supersede) failed: {e}"))?;

            // Reset remaining stale jobs (no newer sibling) back to pending
            let reset = conn.execute(
                "UPDATE embed_jobs SET status = 'pending', started_at = NULL
                 WHERE status = 'running' AND started_at < ?1",
                params![cutoff],
            ).map_err(|e| format!("expire_stale_jobs (reset) failed: {e}"))?;

            Ok(superseded + reset)
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn claim_next_job(&self, worker_id: &str) -> Result<Option<EmbedJob>, String> {
        let conn = self.conn.clone();
        let worker_id = worker_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            // SQLite single-writer means no contention — simple update + query
            let id: Option<i64> = conn.query_row(
                "SELECT id FROM embed_jobs WHERE status = 'pending' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            ).ok();

            let Some(id) = id else { return Ok(None); };

            conn.execute(
                "UPDATE embed_jobs SET status = 'running', started_at = ?1, worker_id = ?2 WHERE id = ?3",
                params![SqliteDb::now_secs(), worker_id, id],
            ).ok();

            let job = conn.query_row(
                "SELECT id, scope, status, total_pages, done_pages, started_at, finished_at, error, worker_id
                 FROM embed_jobs WHERE id = ?1",
                params![id],
                |row| Ok(EmbedJob {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    status: row.get(2)?,
                    total_pages: row.get(3)?,
                    done_pages: row.get(4)?,
                    started_at: row.get(5)?,
                    finished_at: row.get(6)?,
                    error: row.get(7)?,
                    worker_id: row.get(8)?,
                }),
            ).ok();

            Ok(job)
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn recover_worker_jobs(&self, worker_id: &str) -> Result<usize, String> {
        let conn = self.conn.clone();
        let worker_id = worker_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let now = SqliteDb::now_secs();

            // Mark duplicate-scope orphans as superseded (keep only the newest per scope)
            let superseded = conn.execute(
                "UPDATE embed_jobs SET status = 'error', finished_at = ?1, error = 'superseded'
                 WHERE status = 'running' AND (worker_id = ?2 OR worker_id IS NULL)
                   AND EXISTS (
                       SELECT 1 FROM embed_jobs e2
                       WHERE e2.scope = embed_jobs.scope AND e2.id > embed_jobs.id
                         AND e2.status = 'running' AND (e2.worker_id = ?2 OR e2.worker_id IS NULL)
                   )",
                params![now, worker_id],
            ).map_err(|e| format!("recover_worker_jobs (supersede) failed: {e}"))?;

            // Reset remaining jobs owned by this worker or orphaned from pre-0.3.1
            let reset = conn.execute(
                "UPDATE embed_jobs SET status = 'pending', started_at = NULL, worker_id = NULL
                 WHERE status = 'running' AND (worker_id = ?1 OR worker_id IS NULL)",
                params![worker_id],
            ).map_err(|e| format!("recover_worker_jobs (reset) failed: {e}"))?;

            Ok(superseded + reset)
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn update_job_progress(&self, job_id: i64, done: i64, total: i64) -> Result<(), String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "UPDATE embed_jobs SET done_pages = ?1, total_pages = ?2 WHERE id = ?3",
                params![done, total, job_id],
            ).ok();
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn complete_job(&self, job_id: i64, error: Option<&str>) -> Result<(), String> {
        let conn = self.conn.clone();
        let error = error.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let status = if error.is_some() { "error" } else { "completed" };
            conn.execute(
                "UPDATE embed_jobs SET status = ?1, finished_at = ?2, error = ?3 WHERE id = ?4",
                params![status, SqliteDb::now_secs(), error, job_id],
            ).ok();
            Ok(())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_latest_job(&self) -> Result<Option<EmbedJob>, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            Ok(conn.query_row(
                "SELECT id, scope, status, total_pages, done_pages, started_at, finished_at, error, worker_id
                 FROM embed_jobs ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok(EmbedJob {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    status: row.get(2)?,
                    total_pages: row.get(3)?,
                    done_pages: row.get(4)?,
                    started_at: row.get(5)?,
                    finished_at: row.get(6)?,
                    error: row.get(7)?,
                    worker_id: row.get(8)?,
                }),
            ).ok())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn get_stats(&self) -> Result<EmbedStats, String> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let total_pages: i64 = conn
                .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
                .unwrap_or(0);
            let total_chunks: i64 = conn
                .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
                .unwrap_or(0);
            let latest_job = conn.query_row(
                "SELECT id, scope, status, total_pages, done_pages, started_at, finished_at, error, worker_id
                 FROM embed_jobs ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok(EmbedJob {
                    id: row.get(0)?,
                    scope: row.get(1)?,
                    status: row.get(2)?,
                    total_pages: row.get(3)?,
                    done_pages: row.get(4)?,
                    started_at: row.get(5)?,
                    finished_at: row.get(6)?,
                    error: row.get(7)?,
                    worker_id: row.get(8)?,
                }),
            ).ok();
            Ok(EmbedStats { total_pages, total_chunks, latest_job })
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }

    async fn vector_search(&self, query_embedding: &[f32], limit: usize, threshold: f32) -> Result<Vec<SearchHit>, String> {
        let conn = self.conn.clone();
        let query_embedding = query_embedding.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn
                .prepare("SELECT id, page_id, embedding FROM chunks")
                .map_err(|e| format!("Prepare failed: {e}"))?;
            let all_chunks: Vec<(i64, i64, Vec<u8>)> = stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| format!("Query failed: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

            let hits = vector::search_embeddings(&query_embedding, &all_chunks, limit, threshold);
            Ok(hits.into_iter().map(|(chunk_id, page_id, score)| SearchHit { chunk_id, page_id, score }).collect())
        })
        .await
        .map_err(|e| format!("Task failed: {e}"))?
    }
}

/// Convert unix days (since epoch) to (year, month, day).
fn unix_days_to_ymd(days: i64) -> (i64, u32, u32) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
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
