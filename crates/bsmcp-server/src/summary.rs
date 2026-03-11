//! Instance summary generator.
//! Uses an LLM to analyze the knowledge base structure and content samples,
//! producing a short contextual summary that gets included in MCP instructions.

use std::sync::Arc;

use tokio::sync::RwLock;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::db::SemanticDb;

use crate::llm::LlmClient;

const META_KEY: &str = "instance_summary";
const META_KEY_TS: &str = "instance_summary_ts";

/// Cached instance summary, shared across the server.
pub type SummaryCache = Arc<RwLock<Option<String>>>;

/// Check if the cached summary is stale (older than max_age_secs).
async fn is_cache_stale(db: &Option<Arc<dyn SemanticDb>>, max_age_secs: u64) -> bool {
    if max_age_secs == 0 {
        return false; // No interval = never stale once cached
    }
    if let Some(ref db) = db {
        if let Ok(Some(ts_str)) = db.get_meta(META_KEY_TS).await {
            if let Ok(ts) = ts_str.parse::<u64>() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                return now.saturating_sub(ts) > max_age_secs;
            }
        }
    }
    true // No timestamp = treat as stale
}

/// Generate an instance summary.
/// Checks the DB cache first; if missing, stale, or forced, calls the LLM.
pub async fn generate_summary(
    llm: LlmClient,
    client: BookStackClient,
    db: Option<Arc<dyn SemanticDb>>,
    cache: SummaryCache,
    force: bool,
    max_age_secs: u64,
) {
    // Check DB cache first (survives restarts)
    if !force {
        if let Some(ref db) = db {
            if let Ok(Some(cached)) = db.get_meta(META_KEY).await {
                if !cached.is_empty() {
                    let stale = is_cache_stale(&Some(db.clone()), max_age_secs).await;
                    if !stale {
                        eprintln!("Summary: loaded from cache ({} chars)", cached.len());
                        *cache.write().await = Some(cached);
                        return;
                    }
                    // Cache is stale but usable — load it, then regenerate
                    eprintln!("Summary: cache is stale, loading existing and regenerating...");
                    *cache.write().await = Some(cached);
                }
            }
        }
    }

    eprintln!("Summary: generating instance summary...");

    // 1. Gather the structure tree
    let structure = match gather_structure(&client).await {
        Some(s) => s,
        None => {
            eprintln!("Summary: failed to gather structure, skipping");
            return;
        }
    };

    // 2. Sample page titles from each book to understand content themes
    let samples = gather_page_samples(&client).await;

    // 3. Build the LLM prompt
    let system = "You are analyzing a BookStack knowledge base to produce a concise summary \
        for AI assistants that will connect to it. Your summary should help an AI immediately \
        understand: what this knowledge base is about, who maintains it, what kind of \
        organization/family/team uses it, and what topics it covers. \
        Write 1-2 short paragraphs (under 200 words total). Be specific and factual based \
        on the evidence. Do not speculate beyond what the structure and content suggest. \
        Do not use markdown formatting — write plain text.";

    let user_msg = format!(
        "Here is the complete shelf/book/chapter structure of this BookStack instance:\n\n\
         {structure}\n\n\
         Here are sample page titles from across the knowledge base:\n\n\
         {samples}\n\n\
         Based on this structure and these page titles, write a concise summary of what this \
         knowledge base is about and who uses it."
    );

    match llm.complete(system, &user_msg).await {
        Ok(summary) => {
            eprintln!("Summary: generated ({} chars)", summary.len());
            // Store in DB cache with timestamp
            if let Some(ref db) = db {
                if let Err(e) = db.set_meta(META_KEY, &summary).await {
                    eprintln!("Summary: failed to cache in DB: {e}");
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if let Err(e) = db.set_meta(META_KEY_TS, &now.to_string()).await {
                    eprintln!("Summary: failed to cache timestamp: {e}");
                }
            }
            *cache.write().await = Some(summary);
        }
        Err(e) => {
            eprintln!("Summary: LLM call failed: {e}");
        }
    }
}

/// Spawn summary generation in the background.
/// If `interval_secs > 0`, regenerates periodically. Otherwise, generates once.
pub fn spawn_summary_loop(
    llm: LlmClient,
    client: BookStackClient,
    db: Option<Arc<dyn SemanticDb>>,
    cache: SummaryCache,
    interval_secs: u64,
) {
    tokio::spawn(async move {
        // Initial generation (non-forced, respects cache + staleness)
        generate_summary(llm.clone(), client.clone(), db.clone(), cache.clone(), false, interval_secs).await;
        // Periodic regeneration (only if interval is set)
        if interval_secs > 0 {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                eprintln!("Summary: periodic regeneration triggered");
                generate_summary(llm.clone(), client.clone(), db.clone(), cache.clone(), true, interval_secs).await;
            }
        }
    });
}

/// Build the shelf > book > chapter structure tree (same format as MCP instructions).
async fn gather_structure(client: &BookStackClient) -> Option<String> {
    let shelves = client.list_shelves(500, 0).await.ok()?;
    let shelf_list = shelves["data"].as_array()?;

    let shelf_futures: Vec<_> = shelf_list
        .iter()
        .filter_map(|s| s["id"].as_i64())
        .map(|id| client.get_shelf(id))
        .collect();
    let shelf_details = futures::future::join_all(shelf_futures).await;

    let chapters = client
        .list_chapters(500, 0)
        .await
        .ok()
        .and_then(|v| v["data"].as_array().cloned())
        .unwrap_or_default();

    let mut chapters_by_book: std::collections::HashMap<i64, Vec<(i64, String)>> =
        std::collections::HashMap::new();
    for ch in &chapters {
        if let (Some(book_id), Some(id), Some(name)) = (
            ch["book_id"].as_i64(),
            ch["id"].as_i64(),
            ch["name"].as_str(),
        ) {
            chapters_by_book
                .entry(book_id)
                .or_default()
                .push((id, name.to_string()));
        }
    }

    let mut output = String::new();
    for shelf in shelf_details.iter().flatten() {
        let name = shelf["name"].as_str().unwrap_or("?");
        output.push_str(&format!("Shelf: {name}\n"));

        if let Some(books) = shelf["books"].as_array() {
            for book in books {
                let bname = book["name"].as_str().unwrap_or("?");
                let bid = book["id"].as_i64().unwrap_or(0);
                output.push_str(&format!("  Book: {bname}\n"));

                if let Some(chs) = chapters_by_book.get(&bid) {
                    for (_cid, cname) in chs {
                        output.push_str(&format!("    Chapter: {cname}\n"));
                    }
                }
            }
        }
        output.push('\n');
    }

    if output.is_empty() {
        None
    } else {
        Some(output)
    }
}

/// Sample page titles from across the knowledge base.
/// Gets up to 10 pages per book to understand content themes.
async fn gather_page_samples(client: &BookStackClient) -> String {
    let pages = match client.list_pages(500, 0).await {
        Ok(p) => p,
        Err(_) => return String::from("(unable to fetch pages)"),
    };

    let page_list = match pages["data"].as_array() {
        Some(arr) => arr,
        None => return String::from("(no pages found)"),
    };

    // Group by book, take first 10 per book
    let mut by_book: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for page in page_list {
        let book_name = page
            .get("book_id")
            .and_then(|_| page.get("book_slug"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let page_name = page["name"].as_str().unwrap_or("?");

        let entries = by_book.entry(book_name.replace('-', " ")).or_default();
        if entries.len() < 10 {
            entries.push(page_name.to_string());
        }
    }

    let mut output = String::new();
    for (book, pages) in &by_book {
        output.push_str(&format!("Book \"{book}\": {}\n", pages.join(", ")));
    }

    if output.is_empty() {
        "(no page samples available)".to_string()
    } else {
        output
    }
}

/// Invalidate the cached summary (called after reembed, etc.)
#[allow(dead_code)]
pub async fn invalidate_summary(
    llm: Option<&LlmClient>,
    client: &BookStackClient,
    db: Option<&Arc<dyn SemanticDb>>,
    cache: &SummaryCache,
) {
    if let Some(llm) = llm {
        let llm = llm.clone();
        let client = client.clone();
        let db = db.cloned();
        let cache = cache.clone();
        tokio::spawn(async move {
            generate_summary(llm, client, db, cache, true, 0).await;
        });
    }
}
