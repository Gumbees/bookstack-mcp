//! Semantic search module for the MCP server.
//! Delegates embedding to the external embedder service (HTTP /embed endpoint).
//! Handles search, permission filtering with caching, job management, and webhooks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::RwLock;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::SemanticDb;

const PERMISSION_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Per-token permission cache entry.
struct CachedAccess {
    accessible: bool,
    cached_at: Instant,
}

pub struct SemanticState {
    db: Arc<dyn SemanticDb>,
    embedder_url: String,
    webhook_secret: String,
    http_client: reqwest::Client,
    /// Permission cache: (token_id, page_id) -> CachedAccess
    permission_cache: RwLock<HashMap<(String, i64), CachedAccess>>,
}

impl SemanticState {
    pub fn new(
        db: Arc<dyn SemanticDb>,
        embedder_url: String,
        webhook_secret: String,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to build embedder HTTP client");
        Self {
            db,
            embedder_url: embedder_url.trim_end_matches('/').to_string(),
            webhook_secret,
            http_client,
            permission_cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn webhook_secret(&self) -> &str {
        &self.webhook_secret
    }

    /// Embed a query by calling the external embedder service.
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, String> {
        let url = format!("{}/embed", self.embedder_url);
        let resp = self.http_client
            .post(&url)
            .json(&json!({ "texts": [query] }))
            .send()
            .await
            .map_err(|e| format!("Embedder request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Embedder error {status}: {body}"));
        }

        let body: Value = resp.json().await
            .map_err(|e| format!("Embedder response parse error: {e}"))?;

        let embedding = body.get("embeddings")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_array())
            .ok_or("Invalid embedder response format")?;

        let vec: Vec<f32> = embedding.iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        if vec.is_empty() {
            return Err("Empty embedding returned".to_string());
        }

        Ok(vec)
    }

    /// Filter search results by the user's BookStack API permissions.
    /// Uses a per-(token_id, page_id) cache with 5-minute TTL.
    async fn filter_by_permission(
        &self,
        page_ids: &[i64],
        client: &BookStackClient,
    ) -> Vec<i64> {
        let token_id = client.token_id().to_string();
        let now = Instant::now();

        // Check cache for each page
        let mut uncached_ids: Vec<i64> = Vec::new();
        let mut cached_accessible: Vec<i64> = Vec::new();

        {
            let cache = self.permission_cache.read().await;
            for &pid in page_ids {
                let key = (token_id.clone(), pid);
                if let Some(entry) = cache.get(&key) {
                    if now.duration_since(entry.cached_at) < PERMISSION_CACHE_TTL {
                        if entry.accessible {
                            cached_accessible.push(pid);
                        }
                        continue;
                    }
                }
                uncached_ids.push(pid);
            }
        }

        // Batch check uncached page IDs against BookStack API
        if !uncached_ids.is_empty() {
            let accessible = client.check_pages_access(&uncached_ids).await;
            let accessible_set: std::collections::HashSet<i64> = accessible.iter().copied().collect();

            // Update cache
            {
                let mut cache = self.permission_cache.write().await;
                for &pid in &uncached_ids {
                    let key = (token_id.clone(), pid);
                    cache.insert(key, CachedAccess {
                        accessible: accessible_set.contains(&pid),
                        cached_at: now,
                    });
                }
                // Prune expired entries periodically (when cache grows large)
                if cache.len() > 10_000 {
                    cache.retain(|_, entry| now.duration_since(entry.cached_at) < PERMISSION_CACHE_TTL);
                }
            }

            cached_accessible.extend(accessible);
        }

        cached_accessible
    }

    /// Semantic search: embed query via embedder, vector search, filter by permissions, gather Markov blanket.
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
        client: &BookStackClient,
    ) -> Result<Value, String> {
        let start = Instant::now();

        // Embed query via external embedder
        let query_vec = self.embed_query(query).await?;

        // Vector search (backend handles brute-force vs pgvector)
        let hits = self.db.vector_search(&query_vec, limit * 5, threshold).await?;

        // Collect unique page IDs for permission check
        let all_page_ids: Vec<i64> = {
            let mut ids: Vec<i64> = hits.iter().map(|h| h.page_id).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };

        // Filter by user permissions
        let accessible_ids = self.filter_by_permission(&all_page_ids, client).await;
        let accessible_set: std::collections::HashSet<i64> = accessible_ids.iter().copied().collect();

        // Filter hits to only accessible pages
        let filtered_hits: Vec<_> = hits.iter()
            .filter(|h| accessible_set.contains(&h.page_id))
            .collect();

        // Group by page_id, keeping best chunk score per page
        let mut page_scores: HashMap<i64, Vec<(i64, f32)>> = HashMap::new();
        for hit in &filtered_hits {
            page_scores.entry(hit.page_id).or_default().push((hit.chunk_id, hit.score));
        }

        // Sort pages by best chunk score
        let mut page_results: Vec<(i64, Vec<(i64, f32)>)> = page_scores.into_iter().collect();
        page_results.sort_by(|a, b| {
            let best_a = a.1.iter().map(|c| c.1).fold(0.0f32, f32::max);
            let best_b = b.1.iter().map(|c| c.1).fold(0.0f32, f32::max);
            best_b.partial_cmp(&best_a).unwrap_or(std::cmp::Ordering::Equal)
        });
        page_results.truncate(limit);

        // Build result JSON
        let mut results = Vec::new();
        for (page_id, chunk_hits) in &page_results {
            let page_meta = self.db.get_page_meta(*page_id).await?;
            let (page_name, book_id) = match &page_meta {
                Some(m) => (m.name.clone(), m.book_id),
                None => ("Unknown".to_string(), 0),
            };

            let best_score = chunk_hits.iter().map(|c| c.1).fold(0.0f32, f32::max);

            // Get chunk details
            let chunk_ids: Vec<i64> = chunk_hits.iter().map(|c| c.0).collect();
            let chunk_details = self.db.get_chunk_details(&chunk_ids).await?;

            let mut chunks_json = Vec::new();
            for detail in &chunk_details {
                let score = chunk_hits.iter().find(|c| c.0 == detail.chunk_id).map(|c| c.1).unwrap_or(0.0);
                chunks_json.push(json!({
                    "heading_path": detail.heading_path,
                    "content": detail.content,
                    "score": (score * 1000.0).round() / 1000.0,
                }));
            }

            // Gather Markov blanket
            let blanket = self.db.get_markov_blanket(*page_id).await?;

            results.push(json!({
                "page_id": page_id,
                "page_name": page_name,
                "book_id": book_id,
                "score": (best_score * 1000.0).round() / 1000.0,
                "chunks": chunks_json,
                "blanket": {
                    "linked_from": blanket.linked_from.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    "links_to": blanket.links_to.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    "co_linked": blanket.co_linked.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    "siblings": blanket.siblings.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                },
            }));
        }

        let stats = self.db.get_stats().await?;
        let query_time_ms = start.elapsed().as_millis();

        Ok(json!({
            "results": results,
            "stats": {
                "total_indexed": stats.total_pages,
                "total_chunks": stats.total_chunks,
                "query_time_ms": query_time_ms,
            }
        }))
    }

    /// Trigger re-embedding by inserting a job into the queue.
    /// The external embedder picks it up.
    pub async fn trigger_reembed(&self, scope: &str) -> Result<Value, String> {
        let job_id = self.db.create_embed_job(scope).await?;
        Ok(json!({
            "status": "queued",
            "job_id": job_id,
            "scope": scope,
            "message": "Embedding job queued. The embedder will pick it up shortly."
        }))
    }

    /// Get embedding status.
    pub async fn embedding_status(&self) -> Result<Value, String> {
        let stats = self.db.get_stats().await?;
        let job_info = match stats.latest_job {
            Some(ref job) => json!({
                "id": job.id,
                "scope": job.scope,
                "status": job.status,
                "total_pages": job.total_pages,
                "done_pages": job.done_pages,
                "started_at": job.started_at,
                "finished_at": job.finished_at,
                "error": job.error,
            }),
            None => json!(null),
        };
        Ok(json!({
            "total_indexed_pages": stats.total_pages,
            "total_chunks": stats.total_chunks,
            "latest_job": job_info,
        }))
    }

    /// Handle BookStack webhook for page changes.
    /// Creates a page-scoped embed job for the embedder to pick up.
    pub async fn handle_webhook(&self, payload: &Value) -> Result<(), String> {
        let event = payload.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let related = payload.get("related_item").unwrap_or(&json!(null));
        let page_id = related.get("id").and_then(|v| v.as_i64());

        eprintln!("Semantic: webhook event={event} page_id={page_id:?}");

        match event {
            "page_create" | "page_update" => {
                if let Some(pid) = page_id {
                    let scope = format!("page:{pid}");
                    self.db.create_embed_job(&scope).await?;
                    eprintln!("Semantic: queued embed job for page {pid}");
                }
            }
            "page_delete" => {
                if let Some(pid) = page_id {
                    self.db.delete_page(pid).await?;
                    eprintln!("Semantic: deleted embeddings for page {pid}");
                }
            }
            _ => {
                eprintln!("Semantic: ignoring webhook event {event}");
            }
        }

        Ok(())
    }
}
