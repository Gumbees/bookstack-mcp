use std::path::Path;

use async_trait::async_trait;

use crate::settings::{GlobalSettings, UserSettings};
use crate::types::*;

/// Core database operations (auth tokens, backups, user settings).
#[async_trait]
pub trait DbBackend: Send + Sync + 'static {
    /// Atomically insert an access token if under the 10k limit.
    /// Encrypts token_id and token_secret at rest.
    async fn insert_access_token(&self, token: &str, id: &str, secret: &str) -> Result<(), String>;

    /// Retrieve and decrypt an access token's BookStack credentials.
    async fn get_access_token(&self, token: &str) -> Result<Option<(String, String)>, String>;

    /// Delete expired access tokens and refresh tokens.
    async fn cleanup_expired_tokens(&self) -> Result<(), String>;

    /// Store a refresh token mapped to encrypted BookStack credentials.
    async fn insert_refresh_token(&self, token: &str, id: &str, secret: &str) -> Result<(), String>;

    /// Retrieve and decrypt a refresh token's BookStack credentials.
    /// Returns None if the token doesn't exist or has expired.
    async fn get_refresh_token(&self, token: &str) -> Result<Option<(String, String)>, String>;

    /// Delete a refresh token (used during rotation — old token is consumed).
    async fn delete_refresh_token(&self, token: &str) -> Result<(), String>;

    /// Create a database backup. SQLite: VACUUM INTO. Postgres: no-op (use pg_dump).
    async fn backup(&self, path: &Path) -> Result<(), String>;

    // --- User settings (Hive memory flow config) ---

    /// Load user settings keyed by `token_id_hash` (SHA-256 of raw token_id).
    /// Returns Ok(None) when no row exists for this user. Default settings are
    /// applied by the caller (UserSettings::default()) so v1 callers and
    /// pre-existing users behave identically.
    async fn get_user_settings(&self, token_id_hash: &str) -> Result<Option<UserSettings>, String>;

    /// Upsert user settings for `token_id_hash`. Replaces the entire row.
    async fn save_user_settings(&self, token_id_hash: &str, settings: &UserSettings) -> Result<(), String>;

    // --- Remember audit log ---

    /// Insert one audit entry. Failures are logged but do not propagate (audit
    /// logging is best-effort; never blocks the user-facing call).
    async fn insert_audit_entry(&self, entry: &AuditEntryInsert) -> Result<i64, String>;

    /// List audit entries for one user, newest first, paginated.
    async fn list_audit_entries(
        &self,
        token_id_hash: &str,
        limit: i64,
        offset: i64,
        since_unix: Option<i64>,
    ) -> Result<Vec<AuditEntry>, String>;

    // --- Global settings (server-instance-wide) ---

    /// Load the singleton global settings row. Returns defaults if never set.
    async fn get_global_settings(&self) -> Result<GlobalSettings, String>;

    /// Upsert the singleton global settings row. Records the writer's token hash.
    async fn save_global_settings(
        &self,
        settings: &GlobalSettings,
        set_by_token_hash: &str,
    ) -> Result<(), String>;
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

    /// List all pending/running jobs, plus the most recent completed/failed jobs (up to `recent`).
    async fn list_jobs(&self, recent: usize) -> Result<Vec<EmbedJob>, String>;

    // --- Vector search ---

    /// Backend-specific vector search. SQLite: brute-force cosine scan. Postgres: pgvector HNSW.
    ///
    /// `book_ids`: when `Some(&[..])`, restrict candidates to chunks whose
    /// parent page lives in one of those books. When `None` or an empty slice,
    /// search across the entire embedded corpus.
    ///
    /// `user_role_ids`: when `Some(&[..])`, additionally restrict candidates
    /// to pages whose `page_view_acl` row matches one of the user's roles.
    /// Pages with no ACL row are always included (the HTTP fallback path
    /// in `semantic.rs` still verifies them) so search recall stays correct
    /// while the embedded ACL eliminates fan-out for pages we already know
    /// the user can or cannot access.
    async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        threshold: f32,
        book_ids: Option<&[i64]>,
        user_role_ids: Option<&[i64]>,
    ) -> Result<Vec<SearchHit>, String>;

    /// Look up the `book_id` for each requested page in one roundtrip.
    /// Returns the rows that matched, in unspecified order. Pages missing
    /// from the embedding store are simply omitted.
    async fn get_page_book_ids(&self, page_ids: &[i64]) -> Result<Vec<(i64, i64)>, String>;

    /// Batched variant of `get_page_meta`. Returns one entry per requested
    /// page that exists in the embedding store; missing pages are omitted.
    async fn get_page_metas(&self, page_ids: &[i64]) -> Result<Vec<PageMeta>, String>;

    /// Delete all pages, chunks, and relationships. Used for full re-index.
    async fn clear_all_embeddings(&self) -> Result<(), String>;

    /// Alter the embedding vector dimension (e.g. when switching models).
    /// PostgreSQL: alters the pgvector column type and rebuilds the HNSW index.
    /// SQLite: no-op (BLOB columns are dimensionless).
    async fn alter_embedding_dimension(&self, dims: usize) -> Result<(), String>;

    // --- Inferred relationships ---

    /// Compute page centroids from chunk embeddings and store top-N most similar
    /// pages per page as "similar" relationships. Called after a full reindex.
    async fn compute_similar_pages(&self, top_k: usize, threshold: f32) -> Result<usize, String>;

    // --- Metadata key-value store ---

    /// Get a metadata value by key. Used for storing chunk_version, etc.
    async fn get_meta(&self, key: &str) -> Result<Option<String>, String>;

    /// Set a metadata value by key.
    async fn set_meta(&self, key: &str, value: &str) -> Result<(), String>;

    // --- Permission ACL (page-level role visibility) ---

    /// Replace the ACL row for one page. Deletes any prior `page_view_acl`
    /// entries for `page_id` then inserts the new role list. `default_open`
    /// is stored on the `pages` row (`acl_default_open` column) so the
    /// query path can short-circuit role checks for fully-open pages.
    async fn upsert_page_acl(&self, acl: &PageAcl) -> Result<(), String>;

    /// Drop a page from the ACL store. Called on `page_delete` events.
    async fn delete_page_acl(&self, page_id: i64) -> Result<(), String>;

    /// Drop one role from every page's ACL. Called on `role_delete` events.
    async fn delete_role_from_acl(&self, role_id: i64) -> Result<(), String>;

    /// List page IDs that have a stored ACL. Used by the daily reconciliation
    /// job to know which pages to refresh.
    async fn list_acl_page_ids(&self) -> Result<Vec<i64>, String>;

    // --- User role cache (token → BookStack user id → role IDs) ---

    /// Look up cached roles for a token-user. Returns `None` when the
    /// cache entry is missing or older than `max_age_secs`.
    async fn get_cached_user_roles(
        &self,
        token_id_hash: &str,
        max_age_secs: i64,
    ) -> Result<Option<(i64, Vec<i64>)>, String>;

    /// Cache the roles list for a token-user. `bookstack_user_id` is stored
    /// alongside so callers can use it for per-user permission overrides.
    async fn set_cached_user_roles(
        &self,
        token_id_hash: &str,
        bookstack_user_id: i64,
        role_ids: &[i64],
    ) -> Result<(), String>;

    /// Drop every cached entry for the given BookStack user. Called by the
    /// webhook handler on `user_update` (role assignments may have changed)
    /// and `user_delete` (account is gone).
    async fn delete_user_role_cache_by_bs_id(&self, bookstack_user_id: i64) -> Result<(), String>;
}
