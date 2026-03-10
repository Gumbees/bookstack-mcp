//! Semantic search module for the MCP server.
//! v0.5.0: Hybrid search (vector + keyword), blanket re-ranking, tighter thresholds.
//! Delegates embedding to the external embedder service (HTTP /embed endpoint).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::RwLock;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::SemanticDb;

const PERMISSION_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

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
    /// Retries once on transient failures (connection errors, timeouts, 5xx).
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>, String> {
        let url = format!("{}/embed", self.embedder_url);
        let mut last_err = String::new();

        for attempt in 0..2 {
            if attempt > 0 {
                eprintln!("embed_query: retry {attempt} after error: {last_err}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            let resp = match self.http_client
                .post(&url)
                .json(&json!({ "texts": [query] }))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("Embedder request failed: {e}");
                    continue;
                }
            };

            if resp.status().is_server_error() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!("Embedder error {status}: {body}");
                continue;
            }

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

            return Ok(vec);
        }

        Err(last_err)
    }

    /// Filter search results by the user's BookStack API permissions.
    /// Checks each page individually via GET /api/pages/{id} — returns 200 for
    /// accessible pages, 403/404 for restricted. This correctly handles custom
    /// entity permissions (unlike filter[id:in] on the list endpoint).
    /// Results are cached per (token_id, page_id) for 5 minutes.
    async fn filter_by_permission(
        &self,
        page_ids: &[i64],
        client: &BookStackClient,
    ) -> Vec<i64> {
        let token_id = client.token_id().to_string();
        let now = Instant::now();

        let mut uncached_ids: Vec<i64> = Vec::new();
        let mut accessible: Vec<i64> = Vec::new();

        {
            let cache = self.permission_cache.read().await;
            for &pid in page_ids {
                let key = (token_id.clone(), pid);
                if let Some(entry) = cache.get(&key) {
                    if now.duration_since(entry.cached_at) < PERMISSION_CACHE_TTL {
                        if entry.accessible {
                            accessible.push(pid);
                        }
                        continue;
                    }
                }
                uncached_ids.push(pid);
            }
        }

        if !uncached_ids.is_empty() {
            // Check each page individually with concurrency limit
            let semaphore = Arc::new(tokio::sync::Semaphore::new(10));
            let mut handles = Vec::new();

            for pid in uncached_ids.clone() {
                let client = client.clone();
                let sem = semaphore.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await;
                    let ok = client.can_access_page(pid).await;
                    (pid, ok)
                }));
            }

            let mut results: Vec<(i64, bool)> = Vec::new();
            for handle in handles {
                if let Ok(result) = handle.await {
                    results.push(result);
                }
            }

            {
                let mut cache = self.permission_cache.write().await;
                for &(pid, ok) in &results {
                    cache.insert((token_id.clone(), pid), CachedAccess {
                        accessible: ok,
                        cached_at: now,
                    });
                    if ok {
                        accessible.push(pid);
                    }
                }
                // Evict stale entries if cache grows large
                if cache.len() > 10_000 {
                    cache.retain(|_, entry| now.duration_since(entry.cached_at) < PERMISSION_CACHE_TTL);
                }
            }
        }

        accessible
    }

    /// Hybrid search: vector + keyword + blanket re-ranking.
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
        hybrid: bool,
        client: &BookStackClient,
    ) -> Result<Value, String> {
        let start = Instant::now();

        // Run vector search and optional keyword search in parallel
        let vector_future = async {
            let query_vec = self.embed_query(query).await?;
            self.db.vector_search(&query_vec, limit * 5, threshold).await
        };

        let keyword_future = async {
            if hybrid {
                match client.search(query, 1, (limit * 2) as i64).await {
                    Ok(resp) => {
                        resp.get("data")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default()
                    }
                    Err(e) => {
                        eprintln!("Hybrid: keyword search failed (non-fatal): {e}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            }
        };

        let (vector_result, keyword_result) = tokio::join!(vector_future, keyword_future);
        let hits = vector_result?;
        let keyword_results: Vec<Value> = keyword_result;

        // Build page scores from vector hits
        let mut page_scores: HashMap<i64, PageScore> = HashMap::new();
        for hit in &hits {
            let entry = page_scores.entry(hit.page_id).or_insert(PageScore {
                vector_score: 0.0,
                keyword_rank: 0.0,
                blanket_boost: 0.0,
                chunks: Vec::new(),
            });
            if hit.score > entry.vector_score {
                entry.vector_score = hit.score;
            }
            entry.chunks.push((hit.chunk_id, hit.score));
        }

        // Merge keyword results — assign a rank-based score (1.0 for first, decaying)
        if hybrid && !keyword_results.is_empty() {
            let total = keyword_results.len() as f32;
            for (i, result) in keyword_results.iter().enumerate() {
                // BookStack search returns pages, chapters, books — only care about pages
                let result_type = result.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if result_type != "page" {
                    continue;
                }
                let page_id = result.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                if page_id == 0 {
                    continue;
                }
                let rank_score = 1.0 - (i as f32 / total); // 1.0 for first, decaying
                let entry = page_scores.entry(page_id).or_insert(PageScore {
                    vector_score: 0.0,
                    keyword_rank: 0.0,
                    blanket_boost: 0.0,
                    chunks: Vec::new(),
                });
                entry.keyword_rank = rank_score;
            }
        }

        // Permission check: filter out pages the user can't access
        let all_page_ids: Vec<i64> = page_scores.keys().copied().collect();
        let accessible_ids = self.filter_by_permission(&all_page_ids, client).await;
        let accessible_set: HashSet<i64> = accessible_ids.iter().copied().collect();
        page_scores.retain(|pid, _| accessible_set.contains(pid));

        // Blanket re-ranking: boost pages whose neighbors also appear in vector results.
        // Use the full set of pages from raw vector hits (not just final candidates),
        // so neighbors that scored below the per-page threshold still contribute.
        let all_hit_page_ids: HashSet<i64> = hits.iter().map(|h| h.page_id).collect();
        let scored_page_ids: HashSet<i64> = page_scores.keys().copied().collect();
        for page_id in scored_page_ids.iter().copied() {
            let blanket = match self.db.get_markov_blanket(page_id).await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("Blanket: error for page {page_id}: {e}");
                    continue;
                }
            };
            let neighbor_ids: Vec<i64> = blanket.linked_from.iter()
                .chain(blanket.links_to.iter())
                .chain(blanket.co_linked.iter())
                .chain(blanket.siblings.iter())
                .map(|p| p.page_id)
                .collect();

            // Count neighbors in final results (strong signal) and raw vector hits (weak signal)
            let mut strong = 0usize;
            let mut weak = 0usize;
            for nid in &neighbor_ids {
                if scored_page_ids.contains(nid) {
                    strong += 1;
                } else if all_hit_page_ids.contains(nid) {
                    weak += 1;
                }
            }

            if strong > 0 || weak > 0 {
                // Strong: neighbor in final results (0.05 each, max 0.15)
                // Weak: neighbor had a vector hit but didn't make final cut (0.02 each, max 0.06)
                let boost = (strong as f32 * 0.05).min(0.15) + (weak as f32 * 0.02).min(0.06);
                if let Some(entry) = page_scores.get_mut(&page_id) {
                    entry.blanket_boost = boost;
                }
            }
        }

        // Compute final blended score and sort
        let mut page_results: Vec<(i64, f32, &PageScore)> = page_scores.iter()
            .map(|(&pid, score)| {
                let blended = if score.keyword_rank > 0.0 && score.vector_score > 0.0 {
                    // Both sources matched — weighted blend
                    score.vector_score * 0.7 + score.keyword_rank * 0.2 + score.blanket_boost
                } else if score.vector_score > 0.0 {
                    // Vector only
                    score.vector_score + score.blanket_boost
                } else {
                    // Keyword only — use a base score that puts it below good vector matches
                    // but above the threshold so it still appears
                    (threshold + 0.05) * 0.8 + score.keyword_rank * 0.2 + score.blanket_boost
                };
                (pid, blended, score)
            })
            .collect();

        page_results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        page_results.truncate(limit);

        // Build result JSON
        let mut results = Vec::new();
        for (page_id, final_score, score) in &page_results {
            let page_meta = self.db.get_page_meta(*page_id).await?;
            let (page_name, book_id) = match &page_meta {
                Some(m) => (m.name.clone(), m.book_id),
                None => ("Unknown".to_string(), 0),
            };

            // Get chunk details if we have vector hits
            let mut chunks_json = Vec::new();
            if !score.chunks.is_empty() {
                let chunk_ids: Vec<i64> = score.chunks.iter().map(|c| c.0).collect();
                let chunk_details = self.db.get_chunk_details(&chunk_ids).await?;
                for detail in &chunk_details {
                    let chunk_score = score.chunks.iter().find(|c| c.0 == detail.chunk_id).map(|c| c.1).unwrap_or(0.0);
                    chunks_json.push(json!({
                        "heading_path": detail.heading_path,
                        "content": detail.content,
                        "score": (chunk_score * 1000.0).round() / 1000.0,
                    }));
                }
            }

            // Gather Markov blanket for context
            let blanket = self.db.get_markov_blanket(*page_id).await?;

            let mut result = json!({
                "page_id": page_id,
                "page_name": page_name,
                "book_id": book_id,
                "score": (*final_score * 1000.0).round() / 1000.0,
                "chunks": chunks_json,
                "blanket": {
                    "linked_from": blanket.linked_from.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    "links_to": blanket.links_to.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    "co_linked": blanket.co_linked.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    "siblings": blanket.siblings.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                },
            });

            // Include scoring breakdown for transparency
            if hybrid {
                result["scoring"] = json!({
                    "vector": (score.vector_score * 1000.0).round() / 1000.0,
                    "keyword": (score.keyword_rank * 1000.0).round() / 1000.0,
                    "blanket_boost": (score.blanket_boost * 1000.0).round() / 1000.0,
                });
            }

            results.push(result);
        }

        let stats = self.db.get_stats().await?;
        let query_time_ms = start.elapsed().as_millis();

        Ok(json!({
            "results": results,
            "stats": {
                "total_indexed": stats.total_pages,
                "total_chunks": stats.total_chunks,
                "query_time_ms": query_time_ms,
                "mode": if hybrid { "hybrid" } else { "vector" },
            }
        }))
    }

    /// Trigger re-embedding by inserting a job into the queue.
    pub async fn trigger_reembed(&self, scope: &str) -> Result<Value, String> {
        let (job_id, is_new) = self.db.create_embed_job(scope).await?;
        let (status, message) = if is_new {
            ("queued", "Embedding job queued. The embedder will pick it up shortly.")
        } else {
            ("already_active", "A job with this scope is already active. Returning existing job.")
        };
        Ok(json!({
            "status": status,
            "job_id": job_id,
            "scope": scope,
            "message": message,
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

    /// List all active (pending/running) jobs plus recent completed/failed jobs.
    pub async fn list_jobs(&self, recent: usize) -> Result<Vec<bsmcp_common::types::EmbedJob>, String> {
        self.db.list_jobs(recent).await
    }

    /// Handle BookStack webhook for content changes.
    ///
    /// Embedding context is `[Shelf > Book > Chapter > Page]`, so any event that
    /// renames, moves, creates, or deletes an entity at any level can change the
    /// context prefix baked into embeddings.
    ///
    /// Strategy:
    /// - Page events → re-embed that specific page
    /// - Chapter/book events → re-embed the affected book (all pages get fresh context)
    /// - Shelf events → full re-embed (can't determine affected books from webhook payload)
    pub async fn handle_webhook(&self, payload: &Value) -> Result<(), String> {
        let event = payload.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let related = payload.get("related_item").unwrap_or(&json!(null));
        let item_id = related.get("id").and_then(|v| v.as_i64());

        eprintln!("Semantic: webhook event={event} item_id={item_id:?}");

        match event {
            // --- Page events ---
            "page_create" | "page_update" | "page_restore" => {
                if let Some(pid) = item_id {
                    let scope = format!("page:{pid}");
                    let (job_id, is_new) = self.db.create_embed_job(&scope).await?;
                    eprintln!("Semantic: {event} — queued page:{pid} embed job {job_id} (new={is_new})");
                }
            }
            "page_move" => {
                // Page moved to different book/chapter — context prefix changed.
                // Re-embed with force since HTML is the same but context differs.
                if let Some(pid) = item_id {
                    let scope = format!("page:{pid}");
                    let (job_id, is_new) = self.db.create_embed_job(&scope).await?;
                    eprintln!("Semantic: page_move — queued page:{pid} embed job {job_id} (new={is_new})");
                }
            }
            "page_delete" => {
                if let Some(pid) = item_id {
                    self.db.delete_page(pid).await?;
                    eprintln!("Semantic: deleted embeddings for page {pid}");
                }
            }

            // --- Chapter events (re-embed the containing book) ---
            "chapter_create" | "chapter_update" | "chapter_delete" => {
                let book_id = related.get("book_id").and_then(|v| v.as_i64());
                if let Some(bid) = book_id {
                    let scope = format!("book:{bid}");
                    let (job_id, is_new) = self.db.create_embed_job(&scope).await?;
                    eprintln!("Semantic: {event} — queued book:{bid} embed job {job_id} (new={is_new})");
                }
            }
            "chapter_move" => {
                // Pages moved between books — re-embed both source and destination.
                // BookStack webhook gives us the chapter's new book_id.
                // We can't easily get the old book_id, so re-embed the new book
                // and queue a full re-embed to catch the orphaned old book.
                let book_id = related.get("book_id").and_then(|v| v.as_i64());
                if let Some(bid) = book_id {
                    let scope = format!("book:{bid}");
                    let (job_id, is_new) = self.db.create_embed_job(&scope).await?;
                    eprintln!("Semantic: chapter_move — queued book:{bid} embed job {job_id} (new={is_new})");
                }
                // Also queue full re-embed to catch the source book
                let (job_id, is_new) = self.db.create_embed_job("all").await?;
                eprintln!("Semantic: chapter_move — queued full re-embed job {job_id} (new={is_new})");
            }

            // --- Book events (re-embed the book) ---
            "book_update" | "book_sort" | "book_create_from_chapter" => {
                // book_update: name changed → context prefix changed
                // book_sort: pages moved between chapters → context prefix changed
                // book_create_from_chapter: pages moved to new book → context changed
                if let Some(bid) = item_id {
                    let scope = format!("book:{bid}");
                    let (job_id, is_new) = self.db.create_embed_job(&scope).await?;
                    eprintln!("Semantic: {event} — queued book:{bid} embed job {job_id} (new={is_new})");
                }
            }
            "book_delete" => {
                // Pages are cascade-deleted by BookStack; page_delete webhooks
                // should fire for each page. Just log for awareness.
                eprintln!("Semantic: book_delete (id={item_id:?}) — page deletions handled by page_delete events");
            }

            // --- Shelf events (full re-embed) ---
            // Shelf changes affect the context prefix for all pages on that shelf.
            // We can't efficiently determine which books belong to a shelf from
            // the webhook payload, so trigger a full re-embed.
            "bookshelf_create_from_book" | "bookshelf_update" | "bookshelf_delete" => {
                let (job_id, is_new) = self.db.create_embed_job("all").await?;
                eprintln!("Semantic: {event} — queued full re-embed job {job_id} (new={is_new})");
            }

            _ => {
                eprintln!("Semantic: ignoring webhook event {event}");
            }
        }

        Ok(())
    }
}

struct PageScore {
    vector_score: f32,
    keyword_rank: f32,
    blanket_boost: f32,
    chunks: Vec<(i64, f32)>,
}
