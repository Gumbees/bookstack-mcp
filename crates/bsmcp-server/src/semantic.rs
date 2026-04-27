//! Semantic search module for the MCP server.
//! v0.5.0: Hybrid search (vector + keyword), blanket re-ranking, tighter thresholds.
//! Delegates embedding to the external embedder service (HTTP /embed endpoint).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::SemanticDb;
use bsmcp_common::types::MarkovBlanket;

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

    /// Spawn the daily ACL reconciliation cron. Wakes every
    /// `BSMCP_ACL_RECONCILE_HOURS` (default 24) and queues an `acl_reconcile`
    /// embed job — the embedder pipeline picks it up and refreshes
    /// `page_view_acl` for every stored page. This is the safety net for
    /// permission changes that webhook events miss (e.g., webhook drops, role
    /// detail edits that don't fire `role_update` for some reason).
    pub fn spawn_acl_reconcile(self: Arc<Self>) {
        let interval_hours: u64 = std::env::var("BSMCP_ACL_RECONCILE_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24);
        if interval_hours == 0 {
            eprintln!("Semantic: ACL reconciliation disabled (BSMCP_ACL_RECONCILE_HOURS=0)");
            return;
        }
        let interval = Duration::from_secs(interval_hours * 3600);
        eprintln!("Semantic: ACL reconcile cron active — every {interval_hours}h");
        tokio::spawn(async move {
            // Stagger initial run so server startup isn't immediately followed
            // by a heavy reconcile. 5 minutes is enough for the embedder to
            // come up and pull pending jobs first.
            tokio::time::sleep(Duration::from_secs(5 * 60)).await;
            loop {
                match self.db.create_embed_job("acl_reconcile").await {
                    Ok((job_id, is_new)) => eprintln!(
                        "Semantic: ACL reconcile cron — queued job {job_id} (new={is_new})"
                    ),
                    Err(e) => eprintln!("Semantic: ACL reconcile cron — queue failed: {e}"),
                }
                tokio::time::sleep(interval).await;
            }
        });
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
            // Check each page individually with concurrency limit. Bumped from
            // 10 → 25 because the cold-cache permission filter is the dominant
            // cost in semantic search; BookStack handles the burst comfortably.
            let semaphore = Arc::new(tokio::sync::Semaphore::new(25));
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

    /// Resolve the calling user's BookStack roles. Cached per token (15 min
    /// TTL) so the role list is fetched once per session.
    ///
    /// Returns `None` (skip ACL filtering, fall back to HTTP per-page check)
    /// when we can't determine the user's roles — e.g. settings haven't stamped
    /// `bookstack_user_id` yet, or `/api/users/{id}` returned an error.
    pub async fn resolve_user_roles(
        &self,
        token_id_hash: &str,
        bookstack_user_id: Option<i64>,
        client: &BookStackClient,
    ) -> Option<Vec<i64>> {
        // 15-minute cache window — short enough that a role grant or revoke
        // applied during a working session takes effect on the next search,
        // long enough to amortize the user fetch across the cache TTL.
        const ROLE_CACHE_TTL_SECS: i64 = 15 * 60;

        if let Ok(Some((_uid, roles))) = self
            .db
            .get_cached_user_roles(token_id_hash, ROLE_CACHE_TTL_SECS)
            .await
        {
            return Some(roles);
        }

        let user_id = bookstack_user_id?;
        let user = match client.get_user(user_id).await {
            Ok(u) => u,
            Err(e) => {
                eprintln!("ACL: failed to fetch /api/users/{user_id} for role resolution: {e}");
                return None;
            }
        };
        let roles: Vec<i64> = user
            .get("roles")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|r| r.get("id").and_then(|i| i.as_i64())).collect())
            .unwrap_or_default();
        if roles.is_empty() {
            // Empty roles list means BookStack returned the user but they're
            // role-less — skip ACL filtering rather than blocking everything.
            eprintln!("ACL: user {user_id} has no roles; skipping ACL filter for this session");
            return None;
        }
        if let Err(e) = self.db.set_cached_user_roles(token_id_hash, user_id, &roles).await {
            eprintln!("ACL: failed to cache user roles (non-fatal): {e}");
        }
        Some(roles)
    }

    /// Hybrid search: vector + keyword + blanket re-ranking.
    ///
    /// `book_filter`: when `Some(&[..])`, restricts the vector pass to chunks
    /// whose page lives in one of the supplied books. The keyword pass and
    /// permission/blanket steps are unaffected; the vector candidate pool is
    /// just smaller from the outset, which proportionally shrinks the
    /// permission filter and per-result fan-out. `None` keeps the old
    /// whole-corpus behavior.
    ///
    /// `user_role_ids`: when `Some(&[..])`, applies a role-level ACL filter
    /// to candidates via `page_view_acl`. Pages whose ACL hasn't been
    /// computed are still included (the HTTP fallback below verifies them).
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
        hybrid: bool,
        verbose: bool,
        client: &BookStackClient,
        book_filter: Option<&[i64]>,
        user_role_ids: Option<&[i64]>,
    ) -> Result<Value, String> {
        let start = Instant::now();

        // Run vector search and optional keyword search in parallel.
        // Candidate over-fetch dropped from limit*5 → limit*2 — empirically
        // sufficient headroom after permission filtering, and halves both the
        // permission HTTP fan-out and the blanket DB fan-out.
        let book_filter_owned: Option<Vec<i64>> = book_filter
            .filter(|s| !s.is_empty())
            .map(|s| s.to_vec());
        let role_filter_owned: Option<Vec<i64>> = user_role_ids
            .filter(|s| !s.is_empty())
            .map(|s| s.to_vec());
        let vector_future = async {
            let query_vec = self.embed_query(query).await?;
            self.db
                .vector_search(
                    &query_vec,
                    limit * 2,
                    threshold,
                    book_filter_owned.as_deref(),
                    role_filter_owned.as_deref(),
                )
                .await
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
        let mut keyword_results: Vec<Value> = keyword_result;

        // If a book filter was applied to the vector pass, apply the same
        // filter to keyword results so we don't re-introduce out-of-scope
        // pages via the hybrid merge path.
        if let Some(allowed) = book_filter_owned.as_deref() {
            let allowed_set: HashSet<i64> = allowed.iter().copied().collect();
            // Keyword results don't carry book_id; fetch the book_id for each
            // candidate page in one batched DB call and drop anything outside
            // the allowed set.
            let candidate_ids: Vec<i64> = keyword_results.iter()
                .filter(|r| r.get("type").and_then(|v| v.as_str()) == Some("page"))
                .filter_map(|r| r.get("id").and_then(|v| v.as_i64()))
                .collect();
            if !candidate_ids.is_empty() {
                let book_lookup: HashMap<i64, i64> = self.db
                    .get_page_book_ids(&candidate_ids)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                keyword_results.retain(|r| {
                    let pid = r.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                    match book_lookup.get(&pid) {
                        Some(bid) => allowed_set.contains(bid),
                        // Page not in embedding store — drop it (out-of-scope by definition).
                        None => false,
                    }
                });
            }
        }

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
        //
        // Each `get_markov_blanket` is 4 small indexed queries; previously this
        // ran serially over ~40 scored pages, costing ~1s on Postgres latency
        // alone. Parallelize at concurrency 20 — same compute, ~10x wall-clock.
        // Cache the fetched blankets so verbose mode below can reuse them.
        let all_hit_page_ids: HashSet<i64> = hits.iter().map(|h| h.page_id).collect();
        let scored_page_ids: Vec<i64> = page_scores.keys().copied().collect();
        let scored_set: HashSet<i64> = scored_page_ids.iter().copied().collect();

        let blanket_fetches: Vec<(i64, MarkovBlanket)> = stream::iter(scored_page_ids.iter().copied())
            .map(|pid| async move {
                match self.db.get_markov_blanket(pid).await {
                    Ok(b) => Some((pid, b)),
                    Err(e) => {
                        eprintln!("Blanket: error for page {pid}: {e}");
                        None
                    }
                }
            })
            .buffer_unordered(20)
            .filter_map(|x| async move { x })
            .collect()
            .await;

        let blanket_cache: HashMap<i64, MarkovBlanket> = blanket_fetches.into_iter().collect();

        for (&page_id, blanket) in blanket_cache.iter() {
            let mut strong = 0usize;
            let mut weak = 0usize;
            for related in blanket.linked_from.iter()
                .chain(blanket.links_to.iter())
                .chain(blanket.co_linked.iter())
                .chain(blanket.siblings.iter())
            {
                let nid = related.page_id;
                if scored_set.contains(&nid) {
                    strong += 1;
                } else if all_hit_page_ids.contains(&nid) {
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

        // In hybrid mode, filter out keyword-only results (vector_score == 0.0).
        // A keyword match with zero semantic relevance is noise.
        if hybrid {
            page_scores.retain(|_, score| score.vector_score > 0.0 || score.keyword_rank == 0.0);
        }

        // Compute final blended score and sort
        let mut page_results: Vec<(i64, f32, &PageScore)> = page_scores.iter()
            .map(|(&pid, score)| {
                let blended = if score.keyword_rank > 0.0 && score.vector_score > 0.0 {
                    // Both sources matched — weighted blend
                    score.vector_score * 0.7 + score.keyword_rank * 0.2 + score.blanket_boost
                } else {
                    // Vector only (keyword-only results were filtered above)
                    score.vector_score + score.blanket_boost
                };
                (pid, blended, score)
            })
            .collect();

        page_results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        page_results.truncate(limit);

        // Guarantee results even if threshold filtering left us empty — fall back to
        // top-k from raw vector hits (ignoring threshold) so the caller always gets something.
        if page_results.is_empty() && !hits.is_empty() {
            page_scores.clear();
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
            page_scores.retain(|pid, _| accessible_set.contains(pid));

            page_results = page_scores.iter()
                .map(|(&pid, score)| (pid, score.vector_score, score))
                .collect();
            page_results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            page_results.truncate(limit);
        }

        // Batch the per-result lookups. Previously this loop did one
        // get_page_meta and one get_chunk_details per result (~80 sequential
        // DB roundtrips for limit=40). Collect all IDs once, fetch in two
        // queries, then assemble.
        let final_page_ids: Vec<i64> = page_results.iter().map(|(pid, _, _)| *pid).collect();
        let all_chunk_ids: Vec<i64> = page_results.iter()
            .flat_map(|(_, _, score)| score.chunks.iter().map(|c| c.0))
            .collect();

        let (metas, chunk_details) = tokio::try_join!(
            self.db.get_page_metas(&final_page_ids),
            self.db.get_chunk_details(&all_chunk_ids),
        )?;

        let meta_by_page: HashMap<i64, &bsmcp_common::types::PageMeta> =
            metas.iter().map(|m| (m.page_id, m)).collect();

        // Group chunk details by their page_id so each result picks up only its chunks.
        let mut chunks_by_page: HashMap<i64, Vec<&bsmcp_common::types::ChunkDetail>> = HashMap::new();
        for detail in &chunk_details {
            chunks_by_page.entry(detail.page_id).or_default().push(detail);
        }

        // For verbose mode, fetch any blankets we haven't already cached during
        // re-ranking. Most final results will hit the cache for free.
        let mut blanket_cache = blanket_cache;
        if verbose {
            let missing: Vec<i64> = final_page_ids.iter()
                .copied()
                .filter(|pid| !blanket_cache.contains_key(pid))
                .collect();
            if !missing.is_empty() {
                let extras: Vec<(i64, MarkovBlanket)> = stream::iter(missing.into_iter())
                    .map(|pid| async move {
                        self.db.get_markov_blanket(pid).await.ok().map(|b| (pid, b))
                    })
                    .buffer_unordered(20)
                    .filter_map(|x| async move { x })
                    .collect()
                    .await;
                for (pid, b) in extras {
                    blanket_cache.insert(pid, b);
                }
            }
        }

        // Build result JSON
        let mut results = Vec::new();
        for (page_id, final_score, score) in &page_results {
            let (page_name, book_id, updated_at) = match meta_by_page.get(page_id) {
                Some(m) => (m.name.clone(), m.book_id, m.updated_at.clone()),
                None => ("Unknown".to_string(), 0, None),
            };

            // Get chunk details if we have vector hits — pulled from the batched fetch.
            let mut chunks_json = Vec::new();
            if !score.chunks.is_empty() {
                if let Some(details) = chunks_by_page.get(page_id) {
                    for detail in details {
                        let chunk_score = score.chunks.iter().find(|c| c.0 == detail.chunk_id).map(|c| c.1).unwrap_or(0.0);
                        chunks_json.push(json!({
                            "heading_path": detail.heading_path,
                            "content": detail.content,
                            "score": (chunk_score * 1000.0).round() / 1000.0,
                        }));
                    }
                }
            }

            let mut result = json!({
                "page_id": page_id,
                "page_name": page_name,
                "book_id": book_id,
                "score": (*final_score * 1000.0).round() / 1000.0,
                "chunks": chunks_json,
                "scoring": {
                    "vector": (score.vector_score * 1000.0).round() / 1000.0,
                    "keyword": (score.keyword_rank * 1000.0).round() / 1000.0,
                    "blanket_boost": (score.blanket_boost * 1000.0).round() / 1000.0,
                },
            });

            if let Some(ref ts) = updated_at {
                result["updated_at"] = json!(ts);
            }

            // Only include full blanket data in verbose mode — reuse the
            // re-ranking cache so we don't re-fetch what we already pulled.
            if verbose {
                if let Some(blanket) = blanket_cache.get(page_id) {
                    result["blanket"] = json!({
                        "linked_from": blanket.linked_from.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                        "links_to": blanket.links_to.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                        "co_linked": blanket.co_linked.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                        "siblings": blanket.siblings.iter().map(|p| json!({"page_id": p.page_id, "name": p.name})).collect::<Vec<_>>(),
                    });
                }
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
                    // delete_page CASCADE-removes chunks + relationships;
                    // page_view_acl rows are explicitly cleared so the per-role
                    // index doesn't accumulate dead entries.
                    self.db.delete_page(pid).await?;
                    let _ = self.db.delete_page_acl(pid).await;
                    eprintln!("Semantic: deleted embeddings + ACL for page {pid}");
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
            // the webhook payload, so trigger a full re-embed. The re-embed
            // pipeline restamps page_view_acl as a side-effect, so shelf-level
            // permission changes propagate naturally.
            "bookshelf_create_from_book" | "bookshelf_update" | "bookshelf_delete" => {
                let (job_id, is_new) = self.db.create_embed_job("all").await?;
                eprintln!("Semantic: {event} — queued full re-embed job {job_id} (new={is_new})");
            }

            // --- Role events (ACL-only reconciliation) ---
            // Role permission changes don't affect embeddings — they only
            // change which roles can view existing content. Queue an
            // `acl_reconcile` job (handled by the embedder pipeline) so the
            // ACL store is refreshed without paying the cost of re-embedding.
            "role_create" | "role_update" => {
                let (job_id, is_new) = self.db.create_embed_job("acl_reconcile").await?;
                eprintln!("Semantic: {event} — queued ACL reconcile job {job_id} (new={is_new})");
            }
            "role_delete" => {
                if let Some(rid) = item_id {
                    let _ = self.db.delete_role_from_acl(rid).await;
                    eprintln!("Semantic: role_delete — purged role {rid} from page_view_acl");
                }
                let (job_id, is_new) = self.db.create_embed_job("acl_reconcile").await?;
                eprintln!("Semantic: role_delete — queued ACL reconcile job {job_id} (new={is_new})");
            }

            // --- Permission change on a specific entity ---
            // Fired by BookStack's PermissionsUpdater whenever role/fallback
            // permissions are edited on a page/chapter/book/shelf. Queue a
            // full ACL reconcile because the change can cascade to descendants
            // (book perm change affects every page in it). Cheaper than
            // computing the cascade ourselves and the cron-style reconcile
            // path is already battle-tested.
            "permissions_update" => {
                let (job_id, is_new) = self.db.create_embed_job("acl_reconcile").await?;
                eprintln!("Semantic: permissions_update (item={item_id:?}) — queued ACL reconcile job {job_id} (new={is_new})");
            }

            // --- User events ---
            // Role assignments live on the user, so updates can change which
            // roles a token's owner holds. Drop their cached entry so the
            // next semantic_search re-fetches `/api/users/{id}` and picks up
            // the new role list.
            "user_update" | "user_delete" => {
                if let Some(uid) = item_id {
                    let _ = self.db.delete_user_role_cache_by_bs_id(uid).await;
                    eprintln!("Semantic: {event} — invalidated user_role_cache for bookstack_user_id={uid}");
                }
            }
            "user_create" => {
                // No-op — new users have no cache entry yet, no token mapping
                // exists until they authorize through the OAuth flow.
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
