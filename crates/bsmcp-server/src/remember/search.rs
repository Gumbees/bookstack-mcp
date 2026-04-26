//! `/remember/v1/search/read` — cross-resource semantic + keyword search.
//!
//! Body:
//!   - `query` (required): the search string
//!   - `scopes` (optional, default = all configured): array of resource names
//!     to include (e.g., `["journal", "collage", "user_journal"]`)
//!   - `limit` (optional, default 10, max 50): per-scope result cap

use std::collections::HashMap;

use serde_json::{json, Value};

use super::envelope::{ErrorCode, RememberWarning};
use super::{Context, Outcome};

pub async fn read(ctx: &Context) -> Outcome {
    let query = match ctx.body_str("query") {
        Some(q) => q,
        None => {
            return Outcome::error(
                ErrorCode::InvalidArgument,
                "query field is required",
                Some("query"),
            );
        }
    };
    let limit = ctx.body_count("limit", 10, 50);

    let requested_scopes: Vec<String> = ctx
        .body
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_else(default_scopes);

    // Collect configured book/chapter IDs per requested scope.
    let scope_targets = collect_scope_targets(&requested_scopes, ctx);

    // One big semantic search, then partition results by scope.
    let mut warnings = Vec::new();
    let raw_hits: Vec<Value> = if let Some(sem) = &ctx.semantic {
        match sem.search(&query, limit * (scope_targets.len().max(1)) * 2, 0.40, true, false, &ctx.client).await {
            Ok(v) => v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default(),
            Err(e) => {
                warnings.push(RememberWarning::new(
                    "semantic_unavailable",
                    format!("Semantic search failed: {e}"),
                ));
                Vec::new()
            }
        }
    } else {
        warnings.push(RememberWarning::new(
            "semantic_disabled",
            "BSMCP_SEMANTIC_SEARCH=false — keyword results only",
        ));
        Vec::new()
    };

    let mut by_scope: HashMap<String, Vec<Value>> = HashMap::new();
    for (scope_name, book_id) in &scope_targets {
        let filtered: Vec<Value> = raw_hits
            .iter()
            .filter(|h| h.get("book_id").and_then(|v| v.as_i64()) == Some(*book_id))
            .take(limit)
            .cloned()
            .collect();
        by_scope.insert(scope_name.clone(), filtered);
    }

    // Run keyword search per-scope to augment when semantic is empty.
    for (scope_name, book_id) in &scope_targets {
        let filter = format!("{{in_book:{book_id}}}");
        let q = format!("{query} {{type:page}} {filter}");
        let kw = match ctx.client.search(&q, 1, limit as i64).await {
            Ok(v) => v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let entry = by_scope.entry(scope_name.clone()).or_default();
        let existing_ids: std::collections::HashSet<i64> = entry
            .iter()
            .filter_map(|h| h.get("page_id").and_then(|v| v.as_i64()))
            .collect();
        for hit in kw {
            if hit.get("type").and_then(|t| t.as_str()) != Some("page") {
                continue;
            }
            if let Some(id) = hit.get("id").and_then(|v| v.as_i64()) {
                if !existing_ids.contains(&id) && entry.len() < limit {
                    entry.push(json!({
                        "page_id": id,
                        "page_name": hit.get("name").cloned().unwrap_or(Value::Null),
                        "url": hit.get("url").cloned().unwrap_or(Value::Null),
                        "match_kind": "keyword",
                    }));
                }
            }
        }
    }

    let mut outcome = Outcome::ok(json!({
        "query": query,
        "scopes": requested_scopes,
        "results_by_scope": by_scope,
    }));
    for w in warnings {
        outcome = outcome.with_warning(w);
    }
    outcome
}

fn collect_scope_targets(scopes: &[String], ctx: &Context) -> Vec<(String, i64)> {
    let s = &ctx.settings;
    let mut out = Vec::new();
    for scope in scopes {
        let book_id = match scope.as_str() {
            "journal" => s.ai_hive_journal_book_id,
            "collage" => s.ai_collage_book_id,
            "shared_collage" => s.ai_shared_collage_book_id,
            "user_journal" => s.user_journal_book_id,
            _ => None,
        };
        if let Some(id) = book_id {
            out.push((scope.clone(), id));
        }
    }
    out
}

fn default_scopes() -> Vec<String> {
    vec![
        "journal".into(),
        "collage".into(),
        "shared_collage".into(),
        "user_journal".into(),
    ]
}
