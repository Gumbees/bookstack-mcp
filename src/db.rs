use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::oauth::ACCESS_TOKEN_TTL;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Self {
        let conn = Connection::open(path).expect("Failed to open SQLite database");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS access_tokens (
                 token TEXT PRIMARY KEY,
                 token_id TEXT NOT NULL,
                 token_secret TEXT NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS registrations (
                 client_id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL
             );",
        )
        .expect("Failed to initialize database schema");
        Self {
            conn: Mutex::new(conn),
        }
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

    pub fn insert_access_token(&self, token: &str, token_id: &str, token_secret: &str) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO access_tokens (token, token_id, token_secret, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![token, token_id, token_secret, Self::now_secs()],
        )
        .ok();
    }

    pub fn get_access_token(&self, token: &str) -> Option<(String, String)> {
        let conn = self.conn.lock().unwrap();
        let cutoff = Self::cutoff_secs(ACCESS_TOKEN_TTL);
        conn.query_row(
            "SELECT token_id, token_secret FROM access_tokens WHERE token = ?1 AND created_at > ?2",
            params![token, cutoff],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    }

    pub fn cleanup_expired_tokens(&self) {
        let conn = self.conn.lock().unwrap();
        let cutoff = Self::cutoff_secs(ACCESS_TOKEN_TTL);
        conn.execute("DELETE FROM access_tokens WHERE created_at <= ?1", params![cutoff]).ok();
    }

    pub fn count_tokens(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM access_tokens", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }
}
