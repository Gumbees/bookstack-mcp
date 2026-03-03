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
