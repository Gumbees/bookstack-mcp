//! Embedding pipeline: fetch pages from BookStack, chunk, embed, store.

use std::sync::Arc;

use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::chunking;
use bsmcp_common::db::SemanticDb;
use bsmcp_common::types::{ChunkInsert, PageMeta};

/// Thread-safe wrapper around the fastembed TextEmbedding model.
pub struct EmbedModel {
    model: std::sync::Mutex<TextEmbedding>,
}

impl EmbedModel {
    pub fn new(model_path: &str) -> Result<Self, String> {
        let options = InitOptions::new(EmbeddingModel::BGELargeENV15)
            .with_cache_dir(model_path.into())
            .with_show_download_progress(true);
        let model = TextEmbedding::try_new(options)
            .map_err(|e| format!("Model init failed: {e}"))?;
        eprintln!("Model loaded: BAAI/bge-large-en-v1.5 (1024 dims)");
        Ok(Self {
            model: std::sync::Mutex::new(model),
        })
    }

    /// Embed a batch of texts. Thread-safe (locks internally).
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        let mut m = self.model.lock().map_err(|e| format!("Model lock poisoned: {e}"))?;
        m.embed(texts, None).map_err(|e| format!("{e}"))
    }
}

/// Run the embedding pipeline for a job.
pub async fn run_pipeline(
    db: &Arc<dyn SemanticDb>,
    model: &Arc<EmbedModel>,
    client: &BookStackClient,
    job_id: i64,
    scope: &str,
    delay_ms: u64,
    _batch_size: usize,
) -> Result<(), String> {
    eprintln!("Pipeline: starting (scope={scope}, job_id={job_id})");

    // Collect page IDs to embed
    let mut offset = 0i64;
    let mut all_page_ids: Vec<i64> = Vec::new();

    loop {
        let list = client.list_pages(100, offset).await?;
        let data = list.get("data").and_then(|v| v.as_array());
        let Some(pages) = data else { break };
        if pages.is_empty() {
            break;
        }

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
    db.update_job_progress(job_id, 0, total_pages).await?;
    eprintln!("Pipeline: found {total_pages} pages to embed");

    for (i, page_id) in all_page_ids.iter().enumerate() {
        if let Err(e) = embed_single_page(db, model, client, *page_id).await {
            eprintln!("Pipeline: error embedding page {page_id}: {e}");
        }

        db.update_job_progress(job_id, (i + 1) as i64, total_pages).await?;

        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
    }

    eprintln!("Pipeline: completed ({total_pages} pages)");
    Ok(())
}

/// Embed a single page: fetch, check hash, chunk, embed, store relationships.
async fn embed_single_page(
    db: &Arc<dyn SemanticDb>,
    model: &Arc<EmbedModel>,
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
    if let Ok(Some(existing_hash)) = db.get_page_content_hash(page_id).await {
        if existing_hash == content_hash {
            return Ok(());
        }
    }

    let meta = PageMeta {
        page_id,
        book_id,
        chapter_id,
        name: name.to_string(),
        slug: slug.to_string(),
        content_hash: content_hash.clone(),
    };

    // Chunk the HTML
    let chunks = chunking::chunk_html(html);
    if chunks.is_empty() {
        db.upsert_page(&meta).await?;
        return Ok(());
    }

    // Embed all chunks
    let texts: Vec<String> = chunks.iter().map(|c| c.content.clone()).collect();
    let model = model.clone();
    let embeddings = tokio::task::spawn_blocking(move || {
        model.embed(texts)
    })
    .await
    .map_err(|e| format!("Embed task failed: {e}"))??;

    // Store page metadata
    db.upsert_page(&meta).await?;

    // Store chunks with embeddings
    let chunk_inserts: Vec<ChunkInsert> = chunks
        .iter()
        .zip(embeddings.iter())
        .map(|(chunk, embedding)| ChunkInsert {
            chunk_index: chunk.index,
            heading_path: chunk.heading_path.clone(),
            content: chunk.content.clone(),
            content_hash: chunk.content_hash.clone(),
            embedding: embedding.clone(),
        })
        .collect();

    db.insert_chunks(page_id, &chunk_inserts).await?;

    // Extract and store link relationships
    let links = chunking::extract_links(html);
    let mut targets: Vec<(i64, String)> = Vec::new();
    for link in &links {
        if link.contains("/page/") {
            if let Some(slug_part) = link.rsplit("/page/").next() {
                if let Ok(Some(target_id)) = db.resolve_page_slug(slug_part).await {
                    targets.push((target_id, "link".to_string()));
                }
            }
        } else if let Some(link_id_str) = link.strip_prefix("/link/") {
            if let Ok(link_id) = link_id_str.parse::<i64>() {
                targets.push((link_id, "link".to_string()));
            }
        }
    }
    db.replace_relationships(page_id, &targets).await?;

    Ok(())
}
