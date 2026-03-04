use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, AeadCore};
use async_trait::async_trait;
use base64::Engine;
use pgvector::Vector;
use sha2::Digest;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use bsmcp_common::config::ACCESS_TOKEN_TTL;
use bsmcp_common::db::{DbBackend, SemanticDb};
use bsmcp_common::types::*;

const BASE64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

pub struct PostgresDb {
    pool: PgPool,
    encryption_key: [u8; 32],
}

impl PostgresDb {
    pub async fn new(database_url: &str, encryption_key: &str) -> Result<Self, String> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .map_err(|e| format!("Failed to connect to PostgreSQL: {e}"))?;

        // Create pgvector extension and access_tokens table
        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&pool)
            .await
            .map_err(|e| format!("Failed to create vector extension: {e}"))?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS access_tokens (
                token TEXT PRIMARY KEY,
                token_id TEXT NOT NULL,
                token_secret TEXT NOT NULL,
                created_at BIGINT NOT NULL
            )"
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("Failed to create access_tokens table: {e}"))?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_tokens_created ON access_tokens(created_at)")
            .execute(&pool)
            .await
            .ok();

        let hash = sha2::Sha256::digest(encryption_key.as_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);

        Ok(Self { pool, encryption_key: key })
    }

    fn encrypt(&self, plaintext: &str) -> String {
        let cipher = Aes256Gcm::new((&self.encryption_key).into());
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("AES-GCM encryption failed");
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
}

#[async_trait]
impl DbBackend for PostgresDb {
    async fn insert_access_token(&self, token: &str, id: &str, secret: &str) -> Result<(), String> {
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM access_tokens")
            .fetch_one(&self.pool)
            .await
            .unwrap_or((0,));
        if count.0 >= 10000 {
            let cutoff = Self::now_secs() - ACCESS_TOKEN_TTL.as_secs() as i64;
            sqlx::query("DELETE FROM access_tokens WHERE created_at <= $1")
                .bind(cutoff)
                .execute(&self.pool)
                .await
                .ok();
        }
        let enc_id = self.encrypt(id);
        let enc_secret = self.encrypt(secret);
        sqlx::query(
            "INSERT INTO access_tokens (token, token_id, token_secret, created_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (token) DO UPDATE SET token_id = $2, token_secret = $3, created_at = $4"
        )
        .bind(token)
        .bind(&enc_id)
        .bind(&enc_secret)
        .bind(Self::now_secs())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("insert_access_token failed: {e}"))?;
        Ok(())
    }

    async fn get_access_token(&self, token: &str) -> Result<Option<(String, String)>, String> {
        let cutoff = Self::now_secs() - ACCESS_TOKEN_TTL.as_secs() as i64;
        let row = sqlx::query(
            "SELECT token_id, token_secret FROM access_tokens WHERE token = $1 AND created_at > $2"
        )
        .bind(token)
        .bind(cutoff)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get_access_token failed: {e}"))?;

        let Some(row) = row else { return Ok(None); };

        let stored_id: String = row.get("token_id");
        let stored_secret: String = row.get("token_secret");

        if let (Some(tid), Some(tsec)) = (self.decrypt(&stored_id), self.decrypt(&stored_secret)) {
            return Ok(Some((tid, tsec)));
        }

        // Plaintext fallback — re-encrypt in place
        let enc_id = self.encrypt(&stored_id);
        let enc_secret = self.encrypt(&stored_secret);
        sqlx::query("UPDATE access_tokens SET token_id = $1, token_secret = $2 WHERE token = $3")
            .bind(&enc_id)
            .bind(&enc_secret)
            .bind(token)
            .execute(&self.pool)
            .await
            .ok();

        Ok(Some((stored_id, stored_secret)))
    }

    async fn cleanup_expired_tokens(&self) -> Result<(), String> {
        let cutoff = Self::now_secs() - ACCESS_TOKEN_TTL.as_secs() as i64;
        sqlx::query("DELETE FROM access_tokens WHERE created_at <= $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .ok();
        Ok(())
    }

    async fn backup(&self, _path: &Path) -> Result<(), String> {
        eprintln!("Backup: PostgreSQL backups should use pg_dump externally");
        Ok(())
    }
}

#[async_trait]
impl SemanticDb for PostgresDb {
    async fn init_semantic_tables(&self) -> Result<(), String> {
        let statements = [
            "CREATE TABLE IF NOT EXISTS pages (
                page_id BIGINT PRIMARY KEY,
                book_id BIGINT NOT NULL,
                chapter_id BIGINT,
                name TEXT NOT NULL,
                slug TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedded_at BIGINT NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS chunks (
                id BIGSERIAL PRIMARY KEY,
                page_id BIGINT NOT NULL REFERENCES pages(page_id) ON DELETE CASCADE,
                chunk_index INT NOT NULL,
                heading_path TEXT NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding vector(1024) NOT NULL,
                UNIQUE(page_id, chunk_index)
            )",
            "CREATE TABLE IF NOT EXISTS relationships (
                source_page_id BIGINT NOT NULL,
                target_page_id BIGINT NOT NULL,
                link_type TEXT NOT NULL DEFAULT 'link',
                PRIMARY KEY (source_page_id, target_page_id, link_type)
            )",
            "CREATE TABLE IF NOT EXISTS embed_jobs (
                id BIGSERIAL PRIMARY KEY,
                scope TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                total_pages BIGINT DEFAULT 0,
                done_pages BIGINT DEFAULT 0,
                started_at BIGINT,
                finished_at BIGINT,
                error TEXT
            )",
        ];
        for sql in statements {
            sqlx::query(sql)
                .execute(&self.pool)
                .await
                .map_err(|e| format!("Failed to create table: {e}"))?;
        }

        // Create indexes (ignore errors if they exist)
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_chunks_embedding ON chunks USING hnsw (embedding vector_cosine_ops)")
            .execute(&self.pool).await.ok();
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_embed_jobs_pending ON embed_jobs(status) WHERE status = 'pending'")
            .execute(&self.pool).await.ok();

        eprintln!("Semantic: PostgreSQL tables initialized");
        Ok(())
    }

    async fn upsert_page(&self, meta: &PageMeta) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO pages (page_id, book_id, chapter_id, name, slug, content_hash, embedded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (page_id) DO UPDATE SET
                book_id = EXCLUDED.book_id,
                chapter_id = EXCLUDED.chapter_id,
                name = EXCLUDED.name,
                slug = EXCLUDED.slug,
                content_hash = EXCLUDED.content_hash,
                embedded_at = EXCLUDED.embedded_at"
        )
        .bind(meta.page_id)
        .bind(meta.book_id)
        .bind(meta.chapter_id)
        .bind(&meta.name)
        .bind(&meta.slug)
        .bind(&meta.content_hash)
        .bind(Self::now_secs())
        .execute(&self.pool)
        .await
        .map_err(|e| format!("upsert_page failed: {e}"))?;
        Ok(())
    }

    async fn delete_page(&self, page_id: i64) -> Result<(), String> {
        // CASCADE handles chunks; manually delete relationships
        sqlx::query("DELETE FROM relationships WHERE source_page_id = $1 OR target_page_id = $1")
            .bind(page_id)
            .execute(&self.pool).await.ok();
        sqlx::query("DELETE FROM pages WHERE page_id = $1")
            .bind(page_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("delete_page failed: {e}"))?;
        Ok(())
    }

    async fn get_page_content_hash(&self, page_id: i64) -> Result<Option<String>, String> {
        let row = sqlx::query("SELECT content_hash FROM pages WHERE page_id = $1")
            .bind(page_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("get_page_content_hash failed: {e}"))?;
        Ok(row.map(|r| r.get("content_hash")))
    }

    async fn get_page_meta(&self, page_id: i64) -> Result<Option<PageMeta>, String> {
        let row = sqlx::query(
            "SELECT page_id, book_id, chapter_id, name, slug, content_hash FROM pages WHERE page_id = $1"
        )
        .bind(page_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get_page_meta failed: {e}"))?;

        Ok(row.map(|r| PageMeta {
            page_id: r.get("page_id"),
            book_id: r.get("book_id"),
            chapter_id: r.get("chapter_id"),
            name: r.get("name"),
            slug: r.get("slug"),
            content_hash: r.get("content_hash"),
        }))
    }

    async fn resolve_page_slug(&self, slug: &str) -> Result<Option<i64>, String> {
        let row = sqlx::query("SELECT page_id FROM pages WHERE slug = $1")
            .bind(slug)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| format!("resolve_page_slug failed: {e}"))?;
        Ok(row.map(|r| r.get("page_id")))
    }

    async fn insert_chunks(&self, page_id: i64, chunks: &[ChunkInsert]) -> Result<(), String> {
        // Delete old chunks first
        sqlx::query("DELETE FROM chunks WHERE page_id = $1")
            .bind(page_id)
            .execute(&self.pool).await.ok();

        for chunk in chunks {
            let vec = Vector::from(chunk.embedding.clone());
            sqlx::query(
                "INSERT INTO chunks (page_id, chunk_index, heading_path, content, content_hash, embedding)
                 VALUES ($1, $2, $3, $4, $5, $6)"
            )
            .bind(page_id)
            .bind(chunk.chunk_index as i32)
            .bind(&chunk.heading_path)
            .bind(&chunk.content)
            .bind(&chunk.content_hash)
            .bind(vec)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("insert_chunks failed for chunk {}: {e}", chunk.chunk_index))?;
        }
        Ok(())
    }

    async fn get_chunk_details(&self, chunk_ids: &[i64]) -> Result<Vec<ChunkDetail>, String> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT c.id, c.page_id, c.heading_path, c.content, p.name
             FROM chunks c JOIN pages p ON c.page_id = p.page_id
             WHERE c.id = ANY($1)"
        )
        .bind(chunk_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("get_chunk_details failed: {e}"))?;

        Ok(rows.iter().map(|r| ChunkDetail {
            chunk_id: r.get("id"),
            page_id: r.get("page_id"),
            heading_path: r.get("heading_path"),
            content: r.get("content"),
            page_name: r.get("name"),
        }).collect())
    }

    async fn replace_relationships(&self, source: i64, targets: &[(i64, String)]) -> Result<(), String> {
        sqlx::query("DELETE FROM relationships WHERE source_page_id = $1")
            .bind(source)
            .execute(&self.pool).await.ok();

        for (target_id, link_type) in targets {
            sqlx::query(
                "INSERT INTO relationships (source_page_id, target_page_id, link_type)
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"
            )
            .bind(source)
            .bind(target_id)
            .bind(link_type)
            .execute(&self.pool).await.ok();
        }
        Ok(())
    }

    async fn get_markov_blanket(&self, page_id: i64) -> Result<MarkovBlanket, String> {
        let query_related = |pool: &PgPool, sql: &str, page_id: i64| {
            let pool = pool.clone();
            let sql = sql.to_string();
            async move {
                sqlx::query(&sql)
                    .bind(page_id)
                    .fetch_all(&pool)
                    .await
                    .map(|rows| rows.iter().map(|r| RelatedPage {
                        page_id: r.get(0),
                        name: r.get(1),
                    }).collect::<Vec<_>>())
                    .unwrap_or_default()
            }
        };

        let linked_from = query_related(&self.pool,
            "SELECT r.source_page_id, p.name FROM relationships r
             JOIN pages p ON r.source_page_id = p.page_id
             WHERE r.target_page_id = $1 LIMIT 20",
            page_id,
        ).await;

        let links_to = query_related(&self.pool,
            "SELECT r.target_page_id, p.name FROM relationships r
             JOIN pages p ON r.target_page_id = p.page_id
             WHERE r.source_page_id = $1 LIMIT 20",
            page_id,
        ).await;

        let co_linked = query_related(&self.pool,
            "SELECT DISTINCT r2.source_page_id, p.name FROM relationships r1
             JOIN relationships r2 ON r1.target_page_id = r2.target_page_id
             JOIN pages p ON r2.source_page_id = p.page_id
             WHERE r1.source_page_id = $1 AND r2.source_page_id != $1
             LIMIT 10",
            page_id,
        ).await;

        // Siblings: same chapter or same book
        let siblings = {
            let meta = sqlx::query("SELECT chapter_id, book_id FROM pages WHERE page_id = $1")
                .bind(page_id)
                .fetch_optional(&self.pool)
                .await
                .ok()
                .flatten();

            if let Some(meta) = meta {
                let chapter_id: Option<i64> = meta.get("chapter_id");
                let book_id: i64 = meta.get("book_id");

                if let Some(cid) = chapter_id {
                    let result: Vec<RelatedPage> = sqlx::query(
                        "SELECT page_id, name FROM pages WHERE chapter_id = $1 AND page_id != $2 LIMIT 20"
                    )
                    .bind(cid)
                    .bind(page_id)
                    .fetch_all(&self.pool)
                    .await
                    .map(|rows| rows.iter().map(|r| RelatedPage { page_id: r.get(0), name: r.get(1) }).collect())
                    .unwrap_or_default();

                    if !result.is_empty() {
                        result
                    } else {
                        sqlx::query("SELECT page_id, name FROM pages WHERE book_id = $1 AND page_id != $2 LIMIT 20")
                            .bind(book_id).bind(page_id)
                            .fetch_all(&self.pool).await
                            .map(|rows| rows.iter().map(|r| RelatedPage { page_id: r.get(0), name: r.get(1) }).collect())
                            .unwrap_or_default()
                    }
                } else {
                    sqlx::query("SELECT page_id, name FROM pages WHERE book_id = $1 AND page_id != $2 LIMIT 20")
                        .bind(book_id).bind(page_id)
                        .fetch_all(&self.pool).await
                        .map(|rows| rows.iter().map(|r| RelatedPage { page_id: r.get(0), name: r.get(1) }).collect())
                        .unwrap_or_default()
                }
            } else {
                Vec::new()
            }
        };

        Ok(MarkovBlanket { linked_from, links_to, co_linked, siblings })
    }

    async fn create_embed_job(&self, scope: &str) -> Result<i64, String> {
        let row = sqlx::query(
            "INSERT INTO embed_jobs (scope, status, started_at) VALUES ($1, 'pending', $2) RETURNING id"
        )
        .bind(scope)
        .bind(Self::now_secs())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("create_embed_job failed: {e}"))?;

        Ok(row.get("id"))
    }

    async fn claim_next_job(&self) -> Result<Option<EmbedJob>, String> {
        // FOR UPDATE SKIP LOCKED enables concurrent embedder workers
        let row = sqlx::query(
            "UPDATE embed_jobs SET status = 'running', started_at = $1
             WHERE id = (
                SELECT id FROM embed_jobs WHERE status = 'pending'
                ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED
             )
             RETURNING id, scope, status, total_pages, done_pages, started_at, finished_at, error"
        )
        .bind(Self::now_secs())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("claim_next_job failed: {e}"))?;

        Ok(row.map(|r| EmbedJob {
            id: r.get("id"),
            scope: r.get("scope"),
            status: r.get("status"),
            total_pages: r.get("total_pages"),
            done_pages: r.get("done_pages"),
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
            error: r.get("error"),
        }))
    }

    async fn expire_stale_jobs(&self, stale_secs: i64) -> Result<usize, String> {
        let cutoff = Self::now_secs() - stale_secs;
        let result = sqlx::query(
            "UPDATE embed_jobs SET status = 'pending', started_at = NULL
             WHERE status = 'running' AND started_at < $1"
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("expire_stale_jobs failed: {e}"))?;
        Ok(result.rows_affected() as usize)
    }

    async fn update_job_progress(&self, job_id: i64, done: i64, total: i64) -> Result<(), String> {
        sqlx::query("UPDATE embed_jobs SET done_pages = $1, total_pages = $2 WHERE id = $3")
            .bind(done)
            .bind(total)
            .bind(job_id)
            .execute(&self.pool).await.ok();
        Ok(())
    }

    async fn complete_job(&self, job_id: i64, error: Option<&str>) -> Result<(), String> {
        let status = if error.is_some() { "error" } else { "completed" };
        sqlx::query("UPDATE embed_jobs SET status = $1, finished_at = $2, error = $3 WHERE id = $4")
            .bind(status)
            .bind(Self::now_secs())
            .bind(error)
            .bind(job_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("complete_job failed: {e}"))?;
        Ok(())
    }

    async fn get_latest_job(&self) -> Result<Option<EmbedJob>, String> {
        let row = sqlx::query(
            "SELECT id, scope, status, total_pages, done_pages, started_at, finished_at, error
             FROM embed_jobs ORDER BY id DESC LIMIT 1"
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("get_latest_job failed: {e}"))?;

        Ok(row.map(|r| EmbedJob {
            id: r.get("id"),
            scope: r.get("scope"),
            status: r.get("status"),
            total_pages: r.get("total_pages"),
            done_pages: r.get("done_pages"),
            started_at: r.get("started_at"),
            finished_at: r.get("finished_at"),
            error: r.get("error"),
        }))
    }

    async fn get_stats(&self) -> Result<EmbedStats, String> {
        let pages: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pages")
            .fetch_one(&self.pool).await.unwrap_or((0,));
        let chunks: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM chunks")
            .fetch_one(&self.pool).await.unwrap_or((0,));

        let latest_job = self.get_latest_job().await?;

        Ok(EmbedStats {
            total_pages: pages.0,
            total_chunks: chunks.0,
            latest_job,
        })
    }

    async fn vector_search(&self, query_embedding: &[f32], limit: usize, threshold: f32) -> Result<Vec<SearchHit>, String> {
        let vec = Vector::from(query_embedding.to_vec());
        let rows = sqlx::query(
            "SELECT id, page_id, 1 - (embedding <=> $1::vector) AS score
             FROM chunks
             WHERE 1 - (embedding <=> $1::vector) > $2
             ORDER BY embedding <=> $1::vector
             LIMIT $3"
        )
        .bind(&vec)
        .bind(threshold)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("vector_search failed: {e}"))?;

        Ok(rows.iter().map(|r| SearchHit {
            chunk_id: r.get("id"),
            page_id: r.get("page_id"),
            score: r.get("score"),
        }).collect())
    }
}
