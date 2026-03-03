//! Semantic search orchestration module.
//! Manages fastembed model, embedding pipeline, search with Markov blanket context,
//! and webhook handling for incremental re-embedding.

use std::sync::Arc;
use std::time::Instant;

use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::bookstack::BookStackClient;
use crate::chunking;
use crate::db::Db;
use crate::vector;

pub struct SemanticState {
    model: Arc<std::sync::Mutex<TextEmbedding>>,
    db: Arc<Db>,
    client: BookStackClient,
    active_job: Arc<Mutex<Option<i64>>>,
    webhook_secret: String,
}

impl SemanticState {
    /// Initialize the semantic search state. Downloads model if not cached.
    pub async fn new(
        db: Arc<Db>,
        model_path: &str,
        client: BookStackClient,
        webhook_secret: String,
    ) -> Result<Self, String> {
        db.init_semantic_tables();

        let cache_dir = model_path.to_string();
        let model = tokio::task::spawn_blocking(move || {
            let options = InitOptions::new(EmbeddingModel::BGELargeENV15)
                .with_cache_dir(cache_dir.into())
                .with_show_download_progress(true);
            TextEmbedding::try_new(options)
        })
        .await
        .map_err(|e| format!("Model init task failed: {e}"))?
        .map_err(|e| format!("Model init failed: {e}"))?;

        eprintln!("Semantic: model loaded (BAAI/bge-large-en-v1.5, 1024 dims)");

        Ok(Self {
            model: Arc::new(std::sync::Mutex::new(model)),
            db,
            client,
            active_job: Arc::new(Mutex::new(None)),
            webhook_secret,
        })
    }

    pub fn webhook_secret(&self) -> &str {
        &self.webhook_secret
    }

    /// Semantic search: embed query, brute-force scan, gather Markov blanket.
    pub async fn search(&self, query: &str, limit: usize, threshold: f32) -> Result<Value, String> {
        let start = Instant::now();

        // Embed query
        let model = self.model.clone();
        let query_str = query.to_string();
        let query_embedding = tokio::task::spawn_blocking(move || {
            let mut m = model.lock().map_err(|e| format!("Model lock poisoned: {e}"))?;
            m.embed(vec![query_str], None).map_err(|e| format!("{e}"))
        })
        .await
        .map_err(|e| format!("Embed task failed: {e}"))??;

        let query_vec = &query_embedding[0];

        // Load all embeddings and search
        let all_chunks = self.db.load_all_embeddings();
        let hits = vector::search_embeddings(query_vec, &all_chunks, limit * 3, threshold);

        // Group by page_id, keeping best chunk score per page
        let mut page_scores: std::collections::HashMap<i64, Vec<(i64, f32)>> = std::collections::HashMap::new();
        for (chunk_id, page_id, score) in &hits {
            page_scores.entry(*page_id).or_default().push((*chunk_id, *score));
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
            let page_meta = self.db.get_page_meta(*page_id);
            let (book_id, _chapter_id, page_name, _slug) = page_meta.unwrap_or((0, None, "Unknown".to_string(), String::new()));

            let best_score = chunk_hits.iter().map(|c| c.1).fold(0.0f32, f32::max);

            // Get chunk details
            let chunk_ids: Vec<i64> = chunk_hits.iter().map(|c| c.0).collect();
            let chunk_details = self.db.get_chunk_details(&chunk_ids);

            let mut chunks_json = Vec::new();
            for (chunk_id, _pid, heading_path, content, _pname) in &chunk_details {
                let score = chunk_hits.iter().find(|c| c.0 == *chunk_id).map(|c| c.1).unwrap_or(0.0);
                chunks_json.push(json!({
                    "heading_path": heading_path,
                    "content": content,
                    "score": (score * 1000.0).round() / 1000.0,
                }));
            }

            // Gather Markov blanket
            let (linked_from, links_to, co_linked, siblings) = self.db.get_markov_blanket(*page_id);

            results.push(json!({
                "page_id": page_id,
                "page_name": page_name,
                "book_id": book_id,
                "score": (best_score * 1000.0).round() / 1000.0,
                "chunks": chunks_json,
                "blanket": {
                    "linked_from": linked_from.iter().map(|(id, name)| json!({"page_id": id, "name": name})).collect::<Vec<_>>(),
                    "links_to": links_to.iter().map(|(id, name)| json!({"page_id": id, "name": name})).collect::<Vec<_>>(),
                    "co_linked": co_linked.iter().map(|(id, name)| json!({"page_id": id, "name": name})).collect::<Vec<_>>(),
                    "siblings": siblings.iter().map(|(id, name)| json!({"page_id": id, "name": name})).collect::<Vec<_>>(),
                },
            }));
        }

        let (total_pages, total_chunks) = self.db.get_embedding_stats();
        let query_time_ms = start.elapsed().as_millis();

        Ok(json!({
            "results": results,
            "stats": {
                "total_indexed": total_pages,
                "total_chunks": total_chunks,
                "query_time_ms": query_time_ms,
            }
        }))
    }

    /// Trigger re-embedding. Returns immediately with job info.
    /// Coalesces: won't run two simultaneously.
    pub async fn trigger_reembed(&self, scope: &str) -> Result<Value, String> {
        let mut active = self.active_job.lock().await;
        if let Some(job_id) = *active {
            return Ok(json!({
                "status": "already_running",
                "job_id": job_id,
                "message": "An embedding job is already in progress"
            }));
        }

        let job_id = self.db.create_embed_job(scope);
        *active = Some(job_id);
        drop(active);

        // Spawn background embedding pipeline
        let db = self.db.clone();
        let model = self.model.clone();
        let client = self.client.clone();
        let active_job = self.active_job.clone();
        let scope_owned = scope.to_string();

        tokio::spawn(async move {
            let result = embed_pipeline(&db, &model, &client, job_id, &scope_owned).await;
            if let Err(e) = &result {
                eprintln!("Semantic: embed pipeline error: {e}");
                db.complete_embed_job(job_id, Some(e));
            } else {
                db.complete_embed_job(job_id, None);
            }
            let mut active = active_job.lock().await;
            *active = None;
        });

        Ok(json!({
            "status": "started",
            "job_id": job_id,
            "scope": scope,
        }))
    }

    /// Get embedding status.
    pub fn embedding_status(&self) -> Value {
        let (total_pages, total_chunks) = self.db.get_embedding_stats();
        let latest_job = self.db.get_latest_embed_job();

        let job_info = match latest_job {
            Some((id, scope, status, total, done, started, finished, error)) => json!({
                "id": id,
                "scope": scope,
                "status": status,
                "total_pages": total,
                "done_pages": done,
                "started_at": started,
                "finished_at": finished,
                "error": error,
            }),
            None => json!(null),
        };

        json!({
            "total_indexed_pages": total_pages,
            "total_chunks": total_chunks,
            "latest_job": job_info,
        })
    }

    /// Handle BookStack webhook for page changes.
    /// Returns 202 immediately, spawns async re-embed.
    pub async fn handle_webhook(&self, payload: &Value) -> Result<(), String> {
        let event = payload.get("event").and_then(|v| v.as_str()).unwrap_or("");
        let related = payload.get("related_item").unwrap_or(&json!(null));
        let page_id = related.get("id").and_then(|v| v.as_i64());

        eprintln!("Semantic: webhook event={event} page_id={page_id:?}");

        match event {
            "page_create" | "page_update" => {
                if let Some(pid) = page_id {
                    let db = self.db.clone();
                    let model = self.model.clone();
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = embed_single_page(&db, &model, &client, pid).await {
                            eprintln!("Semantic: webhook re-embed error for page {pid}: {e}");
                        }
                    });
                }
            }
            "page_delete" => {
                if let Some(pid) = page_id {
                    self.db.delete_page_and_chunks(pid);
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

/// Background embedding pipeline: paginate all pages, chunk, embed, store.
async fn embed_pipeline(
    db: &Arc<Db>,
    model: &Arc<std::sync::Mutex<TextEmbedding>>,
    client: &BookStackClient,
    job_id: i64,
    scope: &str,
) -> Result<(), String> {
    eprintln!("Semantic: starting embed pipeline (scope={scope}, job_id={job_id})");

    // Count total pages first
    let mut offset = 0i64;
    let mut all_page_ids: Vec<i64> = Vec::new();

    loop {
        let list = client.list_pages(100, offset).await?;
        let data = list.get("data").and_then(|v| v.as_array());
        let Some(pages) = data else { break };
        if pages.is_empty() {
            break;
        }

        // Filter by scope if specified
        for page in pages {
            let pid = page.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            if pid == 0 {
                continue;
            }

            // Scope filtering
            if scope != "all" {
                if let Some(book_prefix) = scope.strip_prefix("book:") {
                    if let Ok(target_book) = book_prefix.parse::<i64>() {
                        let bid = page.get("book_id").and_then(|v| v.as_i64()).unwrap_or(0);
                        if bid != target_book {
                            continue;
                        }
                    }
                } else if let Some(page_prefix) = scope.strip_prefix("page:") {
                    if let Ok(target_page) = page_prefix.parse::<i64>() {
                        if pid != target_page {
                            continue;
                        }
                    }
                }
            }

            all_page_ids.push(pid);
        }

        let total_in_response = list.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        offset += 100;
        if offset >= total_in_response {
            break;
        }
    }

    let total_pages = all_page_ids.len() as i64;
    db.update_embed_job_progress(job_id, 0, total_pages);
    eprintln!("Semantic: found {total_pages} pages to embed");

    for (i, page_id) in all_page_ids.iter().enumerate() {
        if let Err(e) = embed_single_page(db, model, client, *page_id).await {
            eprintln!("Semantic: error embedding page {page_id}: {e}");
        }

        db.update_embed_job_progress(job_id, (i + 1) as i64, total_pages);

        // 50ms delay to avoid hammering BookStack API
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    eprintln!("Semantic: embed pipeline completed ({total_pages} pages)");
    Ok(())
}

/// Embed a single page: fetch, check hash, chunk, embed, store relationships.
async fn embed_single_page(
    db: &Arc<Db>,
    model: &Arc<std::sync::Mutex<TextEmbedding>>,
    client: &BookStackClient,
    page_id: i64,
) -> Result<(), String> {
    let page = client.get_page(page_id).await?;

    let html = page.get("html").and_then(|v| v.as_str()).unwrap_or("");
    let name = page.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let slug = page.get("slug").and_then(|v| v.as_str()).unwrap_or("");
    let book_id = page.get("book_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let chapter_id = page.get("chapter_id").and_then(|v| v.as_i64());

    // Compute content hash to skip unchanged pages
    let content_hash = {
        use sha2::{Sha256, Digest};
        let hash = Sha256::digest(html.as_bytes());
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };

    // Check if already embedded with same content
    if let Some(existing_hash) = db.get_page_content_hash(page_id) {
        if existing_hash == content_hash {
            return Ok(());
        }
    }

    // Chunk the HTML
    let chunks = chunking::chunk_html(html);
    if chunks.is_empty() {
        // Page has no meaningful content, store metadata but no chunks
        db.upsert_page(page_id, book_id, chapter_id, name, slug, &content_hash);
        return Ok(());
    }

    // Embed all chunks (batched)
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let model = model.clone();
    let embeddings = tokio::task::spawn_blocking(move || {
        let mut m = model.lock().map_err(|e| format!("Model lock poisoned: {e}"))?;
        m.embed(texts, None).map_err(|e| format!("{e}"))
    })
    .await
    .map_err(|e| format!("Embed task failed: {e}"))??;

    // Store page metadata
    db.upsert_page(page_id, book_id, chapter_id, name, slug, &content_hash);

    // Store chunks with embeddings
    let blobs: Vec<Vec<u8>> = embeddings.iter().map(|emb| vector::embedding_to_blob(emb)).collect();
    let chunk_refs: Vec<(usize, &str, &str, &str, &[u8])> = chunks
        .iter()
        .zip(blobs.iter())
        .map(|(chunk, blob)| {
            (chunk.index, chunk.heading_path.as_str(), chunk.content.as_str(), chunk.content_hash.as_str(), blob.as_slice())
        })
        .collect();

    db.insert_chunks(page_id, &chunk_refs);

    // Extract and store link relationships
    let links = chunking::extract_links(html);
    let mut targets: Vec<(i64, &str)> = Vec::new();
    for link in &links {
        // Try to resolve slug to page_id
        if let Some(slug_part) = link.rsplit("/page/").next() {
            if let Some(target_id) = db.resolve_page_slug(slug_part) {
                targets.push((target_id, "link"));
            }
        } else if let Some(link_id_str) = link.strip_prefix("/link/") {
            if let Ok(link_id) = link_id_str.parse::<i64>() {
                // /link/ IDs are BookStack's internal redirect IDs, which map to page_ids
                targets.push((link_id, "link"));
            }
        }
    }
    db.replace_relationships(page_id, &targets);

    Ok(())
}
