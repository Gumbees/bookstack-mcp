//! Embedding pipeline: fetch pages from BookStack, chunk, embed, store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fastembed::{
    EmbeddingModel, InitOptions, InitOptionsUserDefined, Pooling, TextEmbedding,
    TokenizerFiles, UserDefinedEmbeddingModel,
};

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::chunking;
use bsmcp_common::db::SemanticDb;
use bsmcp_common::types::{ChunkInsert, PageMeta};

/// Known model configurations.
struct ModelConfig {
    builtin: Option<EmbeddingModel>,
    hf_repo: &'static str,
    dims: usize,
}

/// Resolve a model name to its configuration.
fn resolve_model(name: &str) -> Option<ModelConfig> {
    match name {
        "BAAI/bge-large-en-v1.5" => Some(ModelConfig {
            builtin: Some(EmbeddingModel::BGELargeENV15),
            hf_repo: "BAAI/bge-large-en-v1.5",
            dims: 1024,
        }),
        "BAAI/bge-base-en-v1.5" => Some(ModelConfig {
            builtin: Some(EmbeddingModel::BGEBaseENV15),
            hf_repo: "BAAI/bge-base-en-v1.5",
            dims: 768,
        }),
        "BAAI/bge-small-en-v1.5" => Some(ModelConfig {
            builtin: Some(EmbeddingModel::BGESmallENV15),
            hf_repo: "BAAI/bge-small-en-v1.5",
            dims: 384,
        }),
        "onnx-community/embeddinggemma-300m-ONNX"
        | "google/embeddinggemma-300m"
        | "embeddinggemma-300m" => Some(ModelConfig {
            builtin: None,
            hf_repo: "onnx-community/embeddinggemma-300m-ONNX",
            dims: 768,
        }),
        _ => None,
    }
}

/// Thread-safe wrapper around the fastembed TextEmbedding model.
pub struct EmbedModel {
    model: std::sync::Mutex<TextEmbedding>,
    dims: usize,
}

impl EmbedModel {
    pub fn new(model_name: &str, cache_dir: &str) -> Result<Self, String> {
        let config = resolve_model(model_name)
            .ok_or_else(|| format!("Unknown model: {model_name}. Supported: BAAI/bge-large-en-v1.5, BAAI/bge-base-en-v1.5, BAAI/bge-small-en-v1.5, embeddinggemma-300m"))?;

        let model = if let Some(builtin) = config.builtin {
            // Use fastembed's built-in model registry
            let options = InitOptions::new(builtin)
                .with_cache_dir(cache_dir.into())
                .with_show_download_progress(true);
            TextEmbedding::try_new(options)
                .map_err(|e| format!("Model init failed: {e}"))?
        } else {
            // Custom model: download from HuggingFace and load via UserDefinedEmbeddingModel
            let model_dir = download_hf_model(config.hf_repo, cache_dir)?;
            load_custom_model(&model_dir)?
        };

        eprintln!("Model loaded: {} ({} dims)", config.hf_repo, config.dims);
        Ok(Self {
            model: std::sync::Mutex::new(model),
            dims: config.dims,
        })
    }

    /// Return the embedding dimensions for this model.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Embed a batch of texts. Thread-safe (locks internally).
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        let mut m = self.model.lock().map_err(|e| format!("Model lock poisoned: {e}"))?;
        m.embed(texts, None).map_err(|e| format!("{e}"))
    }
}

/// Download model files from HuggingFace Hub, returning the cached directory.
fn download_hf_model(repo_id: &str, cache_dir: &str) -> Result<PathBuf, String> {
    use hf_hub::api::sync::ApiBuilder;

    eprintln!("Downloading model from HuggingFace: {repo_id}");
    let api = ApiBuilder::new()
        .with_cache_dir(PathBuf::from(cache_dir))
        .build()
        .map_err(|e| format!("HF API init failed: {e}"))?;
    let repo = api.model(repo_id.to_string());

    // Download required files
    let required_files = [
        "model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ];

    let mut model_dir = None;
    for filename in &required_files {
        let path = repo.get(filename)
            .map_err(|e| format!("Failed to download {filename}: {e}"))?;
        if model_dir.is_none() {
            model_dir = path.parent().map(|p| p.to_path_buf());
        }
        eprintln!("  cached: {}", path.display());
    }

    // Also try to download model.onnx_data (external weights, used by EmbeddingGemma)
    match repo.get("model.onnx_data") {
        Ok(path) => eprintln!("  cached: {}", path.display()),
        Err(_) => eprintln!("  no model.onnx_data (not needed for this model)"),
    }

    model_dir.ok_or_else(|| "Failed to determine model directory".to_string())
}

/// Load a custom ONNX model from a local directory.
fn load_custom_model(model_dir: &Path) -> Result<TextEmbedding, String> {
    let read = |name: &str| -> Result<Vec<u8>, String> {
        std::fs::read(model_dir.join(name))
            .map_err(|e| format!("Failed to read {name}: {e}"))
    };

    let onnx_file = read("model.onnx")?;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };

    let mut user_model = UserDefinedEmbeddingModel::new(onnx_file, tokenizer_files)
        .with_pooling(Pooling::Mean);

    // Load external weights file if present (EmbeddingGemma uses this)
    let data_path = model_dir.join("model.onnx_data");
    if data_path.exists() {
        let data = std::fs::read(&data_path)
            .map_err(|e| format!("Failed to read model.onnx_data: {e}"))?;
        user_model = user_model.with_external_initializer("model.onnx_data".to_string(), data);
    }

    let options = InitOptionsUserDefined::default();
    TextEmbedding::try_new_from_user_defined(user_model, options)
        .map_err(|e| format!("Custom model init failed: {e}"))
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

    // Upsert page row BEFORE chunks — the chunks table has a FK to pages(page_id).
    // Use an empty content_hash initially so a crash before insert_chunks completes
    // will cause a re-embed on the next run (hash won't match).
    let preliminary_meta = PageMeta {
        content_hash: String::new(),
        ..meta.clone()
    };
    db.upsert_page(&preliminary_meta).await?;

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

    // Store final page metadata with real content_hash — this is the commit marker.
    db.upsert_page(&meta).await?;

    Ok(())
}
