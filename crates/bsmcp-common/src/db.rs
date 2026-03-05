use std::path::Path;

use async_trait::async_trait;

use crate::types::*;

/// Core database operations (auth tokens, backups).
#[async_trait]
pub trait DbBackend: Send + Sync + 'static {
    /// Atomically insert an access token if under the 10k limit.
    /// Encrypts token_id and token_secret at rest.
    async fn insert_access_token(&self, token: &str, id: &str, secret: &str) -> Result<(), String>;

    /// Retrieve and decrypt an access token's BookStack credentials.
    async fn get_access_token(&self, token: &str) -> Result<Option<(String, String)>, String>;

    /// Delete expired access tokens.
    async fn cleanup_expired_tokens(&self) -> Result<(), String>;

    /// Create a database backup. SQLite: VACUUM INTO. Postgres: no-op (use pg_dump).
    async fn backup(&self, path: &Path) -> Result<(), String>;
}

/// Semantic search database operations.
#[async_trait]
pub trait SemanticDb: Send + Sync + 'static {
    /// Create semantic search tables if they don't exist.
    async fn init_semantic_tables(&self) -> Result<(), String>;

    // --- Pages ---

    async fn upsert_page(&self, meta: &PageMeta) -> Result<(), String>;
    async fn delete_page(&self, page_id: i64) -> Result<(), String>;
    async fn get_page_content_hash(&self, page_id: i64) -> Result<Option<String>, String>;
    async fn get_page_meta(&self, page_id: i64) -> Result<Option<PageMeta>, String>;
    async fn resolve_page_slug(&self, slug: &str) -> Result<Option<i64>, String>;

    // --- Chunks + embeddings ---

    async fn insert_chunks(&self, page_id: i64, chunks: &[ChunkInsert]) -> Result<(), String>;
    async fn get_chunk_details(&self, chunk_ids: &[i64]) -> Result<Vec<ChunkDetail>, String>;

    // --- Relationships ---

    async fn replace_relationships(&self, source: i64, targets: &[(i64, String)]) -> Result<(), String>;
    async fn get_markov_blanket(&self, page_id: i64) -> Result<MarkovBlanket, String>;

    // --- Job queue ---

    /// Create a pending embed job. Returns `(job_id, is_new)`.
    /// If a pending job with the same scope exists, returns it (`is_new=false`).
    /// If a running job with the same scope exists, returns it (`is_new=false`).
    /// Only creates a new job if no active job with the same scope exists.
    async fn create_embed_job(&self, scope: &str) -> Result<(i64, bool), String>;

    /// Atomically claim the next pending job (set status to 'running'). Returns None if queue is empty.
    /// Stamps the job with `worker_id` to identify which embedder owns it.
    async fn claim_next_job(&self, worker_id: &str) -> Result<Option<EmbedJob>, String>;

    /// Reset jobs stuck in 'running' for longer than the given duration back to 'pending'.
    async fn expire_stale_jobs(&self, stale_secs: i64) -> Result<usize, String>;

    /// Recover jobs owned by this worker that are stuck in 'running' (e.g. after a crash).
    /// Resets them to 'pending' so they can be reclaimed.
    async fn recover_worker_jobs(&self, worker_id: &str) -> Result<usize, String>;

    async fn update_job_progress(&self, job_id: i64, done: i64, total: i64) -> Result<(), String>;
    async fn complete_job(&self, job_id: i64, error: Option<&str>) -> Result<(), String>;
    async fn get_latest_job(&self) -> Result<Option<EmbedJob>, String>;
    async fn get_stats(&self) -> Result<EmbedStats, String>;

    // --- Vector search ---

    /// Backend-specific vector search. SQLite: brute-force cosine scan. Postgres: pgvector HNSW.
    async fn vector_search(&self, query_embedding: &[f32], limit: usize, threshold: f32) -> Result<Vec<SearchHit>, String>;

    /// Delete all pages, chunks, and relationships. Used for full re-index.
    async fn clear_all_embeddings(&self) -> Result<(), String>;

    /// Alter the embedding vector dimension (e.g. when switching models).
    /// PostgreSQL: alters the pgvector column type and rebuilds the HNSW index.
    /// SQLite: no-op (BLOB columns are dimensionless).
    async fn alter_embedding_dimension(&self, dims: usize) -> Result<(), String>;

    // --- Metadata key-value store ---

    /// Get a metadata value by key. Used for storing chunk_version, etc.
    async fn get_meta(&self, key: &str) -> Result<Option<String>, String>;

    /// Set a metadata value by key.
    async fn set_meta(&self, key: &str, value: &str) -> Result<(), String>;
}
