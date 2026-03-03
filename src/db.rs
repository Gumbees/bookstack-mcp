use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, AeadCore};
use base64::Engine;
use rusqlite::{Connection, params};
use sha2::Digest;

use crate::oauth::ACCESS_TOKEN_TTL;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

pub struct Db {
    conn: Mutex<Connection>,
    encryption_key: [u8; 32],
}

impl Db {
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
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);

        Self {
            conn: Mutex::new(conn),
            encryption_key: key,
        }
    }

    fn encrypt(&self, plaintext: &str) -> String {
        let cipher = Aes256Gcm::new((&self.encryption_key).into());
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("AES-GCM encryption failed");
        // Prepend 12-byte nonce to ciphertext, then base64 encode
        let mut combined = nonce.to_vec();
        combined.extend_from_slice(&ciphertext);
        BASE64.encode(&combined)
    }

    fn decrypt(&self, stored: &str) -> Option<String> {
        let combined = BASE64.decode(stored).ok()?;
        if combined.len() < 12 {
            return None;
        }
        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new((&self.encryption_key).into());
        let plaintext = cipher.decrypt(nonce, ciphertext).ok()?;
        String::from_utf8(plaintext).ok()
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

    // --- Access Tokens ---

    /// Atomically check token count and insert if under limit.
    /// Cleans up expired tokens if count is high, then inserts.
    /// Encrypts token_id and token_secret at rest if encryption key is set.
    pub fn insert_access_token_if_under_limit(&self, token: &str, token_id: &str, token_secret: &str) {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_tokens", [], |row| row.get(0))
            .unwrap_or(0);
        if count >= 10000 {
            let cutoff = Self::cutoff_secs(ACCESS_TOKEN_TTL);
            conn.execute("DELETE FROM access_tokens WHERE created_at <= ?1", params![cutoff]).ok();
        }
        let enc_id = self.encrypt(token_id);
        let enc_secret = self.encrypt(token_secret);
        conn.execute(
            "INSERT OR REPLACE INTO access_tokens (token, token_id, token_secret, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![token, enc_id, enc_secret, Self::now_secs()],
        )
        .ok();
    }

    /// Retrieve and decrypt an access token's BookStack credentials.
    /// If decryption fails (e.g. token was stored before encryption was enabled),
    /// falls back to reading as plaintext and re-encrypts in place for transparent migration.
    pub fn get_access_token(&self, token: &str) -> Option<(String, String)> {
        let conn = self.conn.lock().unwrap();
        let cutoff = Self::cutoff_secs(ACCESS_TOKEN_TTL);
        let (stored_id, stored_secret): (String, String) = conn.query_row(
            "SELECT token_id, token_secret FROM access_tokens WHERE token = ?1 AND created_at > ?2",
            params![token, cutoff],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;

        // Try decrypting first
        if let (Some(tid), Some(tsec)) = (self.decrypt(&stored_id), self.decrypt(&stored_secret)) {
            return Some((tid, tsec));
        }

        // Decryption failed — treat as plaintext (pre-encryption data)
        // Re-encrypt in place for transparent migration
        let enc_id = self.encrypt(&stored_id);
        let enc_secret = self.encrypt(&stored_secret);
        conn.execute(
            "UPDATE access_tokens SET token_id = ?1, token_secret = ?2 WHERE token = ?3",
            params![enc_id, enc_secret, token],
        )
        .ok();

        Some((stored_id, stored_secret))
    }

    pub fn cleanup_expired_tokens(&self) {
        let conn = self.conn.lock().unwrap();
        let cutoff = Self::cutoff_secs(ACCESS_TOKEN_TTL);
        conn.execute("DELETE FROM access_tokens WHERE created_at <= ?1", params![cutoff]).ok();
    }

    // --- Backups ---

    /// Create a consistent backup of the database using VACUUM INTO.
    /// Keeps the last 3 backups and deletes older ones.
    pub fn backup(&self, backup_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(backup_dir)
            .map_err(|e| format!("Failed to create backup directory: {e}"))?;

        let timestamp = {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Format as compact ISO 8601: YYYYMMDDTHHMMSS
            let secs = now;
            let days = secs / 86400;
            let time_of_day = secs % 86400;
            // Simple date calculation from unix timestamp
            let (year, month, day) = unix_days_to_ymd(days as i64);
            let hours = time_of_day / 3600;
            let minutes = (time_of_day % 3600) / 60;
            let seconds = time_of_day % 60;
            format!("{year:04}{month:02}{day:02}T{hours:02}{minutes:02}{seconds:02}")
        };

        let backup_file = backup_dir.join(format!("bookstack-mcp-backup-{timestamp}.db"));
        let backup_path_str = backup_file.to_string_lossy();

        let conn = self.conn.lock().unwrap();
        conn.execute_batch(&format!("VACUUM INTO '{}'", backup_path_str.replace('\'', "''")))
            .map_err(|e| format!("VACUUM INTO failed: {e}"))?;

        drop(conn);
        eprintln!("Backup created: {}", backup_file.display());

        // Keep last 3 backups, delete older ones
        self.cleanup_old_backups(backup_dir);

        Ok(())
    }

    // --- Semantic Search Tables ---

    /// Initialize semantic search tables. Only called when semantic search is enabled.
    pub fn init_semantic_tables(&self) {
        let conn = self.conn.lock().unwrap();
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
                error TEXT
            );",
        )
        .expect("Failed to initialize semantic search tables");
        eprintln!("Semantic: tables initialized");
    }

    // --- Page metadata ---

    pub fn upsert_page(&self, page_id: i64, book_id: i64, chapter_id: Option<i64>, name: &str, slug: &str, content_hash: &str) {
        let conn = self.conn.lock().unwrap();
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
            params![page_id, book_id, chapter_id, name, slug, content_hash, Self::now_secs()],
        ).ok();
    }

    pub fn delete_page_and_chunks(&self, page_id: i64) {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().expect("Failed to begin transaction");
        if let Err(e) = tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![page_id]) {
            eprintln!("DB: delete chunks for page {page_id}: {e}");
        }
        if let Err(e) = tx.execute("DELETE FROM relationships WHERE source_page_id = ?1 OR target_page_id = ?1", params![page_id]) {
            eprintln!("DB: delete relationships for page {page_id}: {e}");
        }
        if let Err(e) = tx.execute("DELETE FROM pages WHERE page_id = ?1", params![page_id]) {
            eprintln!("DB: delete page {page_id}: {e}");
        }
        if let Err(e) = tx.commit() {
            eprintln!("DB: commit delete_page_and_chunks for page {page_id}: {e}");
        }
    }

    pub fn get_page_content_hash(&self, page_id: i64) -> Option<String> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT content_hash FROM pages WHERE page_id = ?1",
            params![page_id],
            |row| row.get(0),
        ).ok()
    }

    // --- Chunks ---

    pub fn insert_chunks(&self, page_id: i64, chunks: &[(usize, &str, &str, &str, &[u8])]) {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().expect("Failed to begin transaction");
        if let Err(e) = tx.execute("DELETE FROM chunks WHERE page_id = ?1", params![page_id]) {
            eprintln!("DB: delete old chunks for page {page_id}: {e}");
        }
        for &(index, heading_path, content, content_hash, embedding) in chunks {
            if let Err(e) = tx.execute(
                "INSERT INTO chunks (page_id, chunk_index, heading_path, content, content_hash, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![page_id, index as i64, heading_path, content, content_hash, embedding],
            ) {
                eprintln!("DB: insert chunk {index} for page {page_id}: {e}");
            }
        }
        if let Err(e) = tx.commit() {
            eprintln!("DB: commit insert_chunks for page {page_id}: {e}");
        }
    }

    /// Load all embeddings for brute-force search: (chunk_id, page_id, embedding_blob)
    pub fn load_all_embeddings(&self) -> Vec<(i64, i64, Vec<u8>)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, page_id, embedding FROM chunks")
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    /// Get chunk details by IDs for search result formatting.
    pub fn get_chunk_details(&self, chunk_ids: &[i64]) -> Vec<(i64, i64, String, String, String)> {
        if chunk_ids.is_empty() {
            return Vec::new();
        }
        let conn = self.conn.lock().unwrap();
        let placeholders: Vec<String> = (0..chunk_ids.len()).map(|i| format!("?{}", i + 1)).collect();
        let sql = format!(
            "SELECT c.id, c.page_id, c.heading_path, c.content, p.name
             FROM chunks c JOIN pages p ON c.page_id = p.page_id
             WHERE c.id IN ({})",
            placeholders.join(",")
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let params: Vec<&dyn rusqlite::types::ToSql> = chunk_ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        stmt.query_map(params.as_slice(), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    // --- Relationships ---

    pub fn replace_relationships(&self, source_page_id: i64, targets: &[(i64, &str)]) {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().expect("Failed to begin transaction");
        if let Err(e) = tx.execute(
            "DELETE FROM relationships WHERE source_page_id = ?1",
            params![source_page_id],
        ) {
            eprintln!("DB: delete relationships for page {source_page_id}: {e}");
        }
        for &(target_id, link_type) in targets {
            if let Err(e) = tx.execute(
                "INSERT OR IGNORE INTO relationships (source_page_id, target_page_id, link_type)
                 VALUES (?1, ?2, ?3)",
                params![source_page_id, target_id, link_type],
            ) {
                eprintln!("DB: insert relationship {source_page_id}->{target_id}: {e}");
            }
        }
        if let Err(e) = tx.commit() {
            eprintln!("DB: commit replace_relationships for page {source_page_id}: {e}");
        }
    }

    /// Get Markov blanket for a page: (linked_from, links_to, co_linked, siblings)
    #[allow(clippy::type_complexity)]
    pub fn get_markov_blanket(&self, page_id: i64) -> (
        Vec<(i64, String)>,  // linked_from (parents)
        Vec<(i64, String)>,  // links_to (children)
        Vec<(i64, String)>,  // co_linked
        Vec<(i64, String)>,  // siblings
    ) {
        let conn = self.conn.lock().unwrap();

        // Parents: pages linking TO this page
        let linked_from = conn
            .prepare(
                "SELECT r.source_page_id, p.name FROM relationships r
                 JOIN pages p ON r.source_page_id = p.page_id
                 WHERE r.target_page_id = ?1 LIMIT 20"
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![page_id], |row| Ok((row.get(0)?, row.get(1)?)))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        // Children: pages this links TO
        let links_to = conn
            .prepare(
                "SELECT r.target_page_id, p.name FROM relationships r
                 JOIN pages p ON r.target_page_id = p.page_id
                 WHERE r.source_page_id = ?1 LIMIT 20"
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![page_id], |row| Ok((row.get(0)?, row.get(1)?)))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        // Co-linked: pages sharing a common link target
        let co_linked = conn
            .prepare(
                "SELECT DISTINCT r2.source_page_id, p.name FROM relationships r1
                 JOIN relationships r2 ON r1.target_page_id = r2.target_page_id
                 JOIN pages p ON r2.source_page_id = p.page_id
                 WHERE r1.source_page_id = ?1 AND r2.source_page_id != ?1
                 LIMIT 10"
            )
            .and_then(|mut stmt| {
                stmt.query_map(params![page_id], |row| Ok((row.get(0)?, row.get(1)?)))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        // Siblings: same chapter or same book
        let siblings = self.get_hierarchical_siblings_inner(&conn, page_id);

        (linked_from, links_to, co_linked, siblings)
    }

    fn get_hierarchical_siblings_inner(&self, conn: &Connection, page_id: i64) -> Vec<(i64, String)> {
        // Try chapter siblings first
        let chapter_id: Option<i64> = conn
            .query_row("SELECT chapter_id FROM pages WHERE page_id = ?1", params![page_id], |row| row.get(0))
            .ok()
            .flatten();

        if let Some(cid) = chapter_id {
            let result: Vec<(i64, String)> = conn
                .prepare("SELECT page_id, name FROM pages WHERE chapter_id = ?1 AND page_id != ?2 LIMIT 20")
                .and_then(|mut stmt| {
                    stmt.query_map(params![cid, page_id], |row| Ok((row.get(0)?, row.get(1)?)))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default();
            if !result.is_empty() {
                return result;
            }
        }

        // Fall back to book siblings
        let book_id: Option<i64> = conn
            .query_row("SELECT book_id FROM pages WHERE page_id = ?1", params![page_id], |row| row.get(0))
            .ok();

        if let Some(bid) = book_id {
            conn.prepare("SELECT page_id, name FROM pages WHERE book_id = ?1 AND page_id != ?2 LIMIT 20")
                .and_then(|mut stmt| {
                    stmt.query_map(params![bid, page_id], |row| Ok((row.get(0)?, row.get(1)?)))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    // --- Embed Jobs ---

    pub fn create_embed_job(&self, scope: &str) -> i64 {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO embed_jobs (scope, status, started_at) VALUES (?1, 'running', ?2)",
            params![scope, Self::now_secs()],
        ).expect("Failed to create embed job");
        conn.last_insert_rowid()
    }

    pub fn update_embed_job_progress(&self, job_id: i64, done_pages: i64, total_pages: i64) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE embed_jobs SET done_pages = ?1, total_pages = ?2 WHERE id = ?3",
            params![done_pages, total_pages, job_id],
        ).ok();
    }

    pub fn complete_embed_job(&self, job_id: i64, error: Option<&str>) {
        let conn = self.conn.lock().unwrap();
        let status = if error.is_some() { "error" } else { "completed" };
        conn.execute(
            "UPDATE embed_jobs SET status = ?1, finished_at = ?2, error = ?3 WHERE id = ?4",
            params![status, Self::now_secs(), error, job_id],
        ).ok();
    }

    #[allow(clippy::type_complexity)]
    pub fn get_latest_embed_job(&self) -> Option<(i64, String, String, i64, i64, Option<i64>, Option<i64>, Option<String>)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, scope, status, total_pages, done_pages, started_at, finished_at, error
             FROM embed_jobs ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((
                row.get(0)?, row.get(1)?, row.get(2)?,
                row.get(3)?, row.get(4)?, row.get(5)?,
                row.get(6)?, row.get(7)?,
            )),
        ).ok()
    }

    pub fn get_embedding_stats(&self) -> (i64, i64) {
        let conn = self.conn.lock().unwrap();
        let total_pages: i64 = conn
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap_or(0);
        let total_chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
            .unwrap_or(0);
        (total_pages, total_chunks)
    }

    /// Resolve a BookStack page slug to a page_id from the local pages table.
    pub fn resolve_page_slug(&self, slug: &str) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT page_id FROM pages WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        ).ok()
    }

    /// Get page metadata by ID from the local pages table.
    pub fn get_page_meta(&self, page_id: i64) -> Option<(i64, Option<i64>, String, String)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT book_id, chapter_id, name, slug FROM pages WHERE page_id = ?1",
            params![page_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).ok()
    }

    fn cleanup_old_backups(&self, backup_dir: &Path) {
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

        // Sort by name (timestamp-based, so alphabetical = chronological)
        backups.sort_by_key(|e| e.file_name());

        // Keep last 3
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
