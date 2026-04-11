mod llm;
mod mcp;
mod migrate;
mod oauth;
mod semantic;
mod sse;
mod staging;
mod summary;

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderName, Method, StatusCode};
use axum::response::{Html, IntoResponse, Json};
use axum::{Router, routing::get};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};

use bsmcp_common::config::DbBackendType;
use bsmcp_common::db::{DbBackend, SemanticDb};

#[tokio::main]
async fn main() {
    // Check for migration subcommand
    let args: Vec<String> = env::args().collect();
    if args.len() >= 2 && args[1] == "migrate" {
        run_migration(&args[2..]).await;
        return;
    }

    eprintln!("BookStack MCP Server v{}", env!("CARGO_PKG_VERSION"));

    let bookstack_url = env::var("BSMCP_BOOKSTACK_URL")
        .expect("BSMCP_BOOKSTACK_URL is required");

    let host = env::var("BSMCP_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = env::var("BSMCP_PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse()
        .expect("BSMCP_PORT must be a valid port number");

    let encryption_key = env::var("BSMCP_ENCRYPTION_KEY")
        .expect("BSMCP_ENCRYPTION_KEY is required (32+ character key for AES-256-GCM token encryption)");
    if encryption_key.len() < 32 {
        panic!("BSMCP_ENCRYPTION_KEY must be at least 32 characters");
    }
    eprintln!("Encryption: enabled (AES-256-GCM)");

    // Select database backend
    let backend_type = DbBackendType::from_env();
    let (db, semantic_db): (Arc<dyn DbBackend>, Option<Arc<dyn SemanticDb>>) = match backend_type {
        DbBackendType::Sqlite => {
            let db_path = env::var("BSMCP_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/data/bookstack-mcp.db"));
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            eprintln!("Database: SQLite ({})", db_path.display());
            let db = Arc::new(bsmcp_db_sqlite::SqliteDb::open(&db_path, &encryption_key));
            (db.clone(), Some(db as Arc<dyn SemanticDb>))
        }
        DbBackendType::Postgres => {
            let database_url = env::var("BSMCP_DATABASE_URL")
                .expect("BSMCP_DATABASE_URL is required when BSMCP_DB_BACKEND=postgres");
            eprintln!("Database: PostgreSQL");

            // Auto-migrate from SQLite if a database file exists
            let sqlite_path = env::var("BSMCP_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/data/bookstack-mcp.db"));
            if sqlite_path.exists() {
                eprintln!("Auto-migration: found SQLite database at {}", sqlite_path.display());
                match migrate::run(&sqlite_path, &database_url).await {
                    Ok(()) => {
                        // Rename to prevent re-migration on next startup
                        let migrated_path = sqlite_path.with_extension("db.migrated");
                        if let Err(e) = std::fs::rename(&sqlite_path, &migrated_path) {
                            eprintln!("Auto-migration: warning — could not rename SQLite file: {e}");
                            eprintln!("Auto-migration: manually remove {} to prevent re-migration", sqlite_path.display());
                        } else {
                            eprintln!("Auto-migration: renamed {} → {}", sqlite_path.display(), migrated_path.display());
                        }
                    }
                    Err(e) => {
                        eprintln!("Auto-migration: failed — {e}");
                        eprintln!("Auto-migration: continuing with empty PostgreSQL database");
                    }
                }
            }

            let db = Arc::new(
                bsmcp_db_postgres::PostgresDb::new(&database_url, &encryption_key)
                    .await
                    .expect("Failed to connect to PostgreSQL"),
            );
            (db.clone(), Some(db as Arc<dyn SemanticDb>))
        }
    };

    // Build known_urls from BSMCP_PUBLIC_DOMAIN and BSMCP_INTERNAL_DOMAIN
    let known_urls = {
        let mut urls: Vec<String> = Vec::new();
        let public_domain = env::var("BSMCP_PUBLIC_DOMAIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().trim_end_matches('/').to_string());
        if let Some(domain) = &public_domain {
            urls.push(format!("https://{domain}"));
        } else {
            eprintln!("Warning: BSMCP_PUBLIC_DOMAIN is not set — AI assistants won't be able to present clickable BookStack links to users");
        }
        if let Ok(domain) = env::var("BSMCP_INTERNAL_DOMAIN") {
            let domain = domain.trim().trim_end_matches('/');
            if !domain.is_empty() {
                urls.push(format!("http://{domain}"));
            }
        }
        if !urls.is_empty() {
            eprintln!("Known URLs: {}", urls.join(", "));
        }
        urls
    };

    // Backup configuration
    let backup_interval_hours: Option<u64> = env::var("BSMCP_BACKUP_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&h| h > 0);

    let backup_path = env::var("BSMCP_BACKUP_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/backups"));

    // Semantic search (optional)
    let semantic_enabled = env::var("BSMCP_SEMANTIC_SEARCH")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);

    let semantic = if semantic_enabled {
        let webhook_secret = env::var("BSMCP_WEBHOOK_SECRET")
            .expect("BSMCP_WEBHOOK_SECRET is required when semantic search is enabled");
        if webhook_secret.len() < 16 {
            panic!("BSMCP_WEBHOOK_SECRET must be at least 16 characters");
        }
        let embedder_url = env::var("BSMCP_EMBEDDER_URL")
            .unwrap_or_else(|_| "http://bsmcp-embedder:8081".into());

        match &semantic_db {
            Some(sdb) => {
                if let Err(e) = sdb.init_semantic_tables().await {
                    eprintln!("Semantic: failed to initialize tables — {e}");
                    eprintln!("Semantic: continuing without semantic search");
                    None
                } else {
                    eprintln!("Semantic: enabled (embedder_url={embedder_url})");
                    Some(Arc::new(semantic::SemanticState::new(
                        sdb.clone(),
                        embedder_url,
                        webhook_secret,
                    )))
                }
            }
            None => {
                eprintln!("Semantic: no semantic database available");
                None
            }
        }
    } else {
        eprintln!("Semantic: disabled");
        None
    };

    // Instance summary (optional — requires LLM API key + BookStack service token)
    let summary_cache: summary::SummaryCache = Arc::new(tokio::sync::RwLock::new(None));
    if let Some(llm_client) = llm::LlmClient::from_env() {
        // Need a service-level BookStack client for reading content
        let summary_token_id = env::var("BSMCP_SUMMARY_TOKEN_ID")
            .or_else(|_| env::var("BSMCP_EMBED_TOKEN_ID"));
        let summary_token_secret = env::var("BSMCP_SUMMARY_TOKEN_SECRET")
            .or_else(|_| env::var("BSMCP_EMBED_TOKEN_SECRET"));

        if let (Ok(tid), Ok(tsec)) = (summary_token_id, summary_token_secret) {
            let bs_client = bsmcp_common::bookstack::BookStackClient::new(
                &bookstack_url,
                &tid,
                &tsec,
                reqwest::Client::new(),
            );
            // BSMCP_SUMMARY_INTERVAL: hours between regenerations (0 = only on startup if no cache)
            let interval_hours: u64 = env::var("BSMCP_SUMMARY_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let interval_secs = interval_hours * 3600;

            if interval_hours > 0 {
                eprintln!("Summary: LLM configured ({:?}), regenerating every {interval_hours}h", llm_client.provider());
            } else {
                eprintln!("Summary: LLM configured ({:?}), one-time generation", llm_client.provider());
            }
            summary::spawn_summary_loop(
                llm_client,
                bs_client,
                semantic_db.clone(),
                summary_cache.clone(),
                interval_secs,
            );
        } else {
            eprintln!("Summary: LLM configured but no BookStack service token (set BSMCP_SUMMARY_TOKEN_ID/SECRET or BSMCP_EMBED_TOKEN_ID/SECRET)");
        }
    }

    // BookStack OAuth configuration (for per-user authentication via BookStack's OAuth provider)
    let auth_method = env::var("BSMCP_BOOKSTACK_AUTH_METHOD")
        .unwrap_or_else(|_| "auto".into())
        .to_lowercase();

    let oauth_config = {
        let client_id = env::var("BSMCP_OAUTH_CLIENT_ID").ok();
        let client_secret = env::var("BSMCP_OAUTH_CLIENT_SECRET").ok();

        if let Some(client_id) = client_id {
            // Build a temporary HTTP client for discovery
            let discovery_client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build discovery HTTP client");

            match oauth::discover_bookstack_oauth(&bookstack_url, &discovery_client).await {
                Ok((authorization_endpoint, token_endpoint)) => {
                    eprintln!("BookStack OAuth: configured (client_id={client_id})");
                    Some(sse::OAuthConfig {
                        client_id,
                        client_secret,
                        authorization_endpoint,
                        token_endpoint,
                    })
                }
                Err(e) => {
                    eprintln!("BookStack OAuth: discovery failed — {e}");
                    if auth_method == "oauth" {
                        panic!("BSMCP_BOOKSTACK_AUTH_METHOD=oauth but OAuth discovery failed: {e}");
                    }
                    eprintln!("BookStack OAuth: falling back to API token authentication");
                    None
                }
            }
        } else {
            if auth_method == "oauth" {
                panic!("BSMCP_BOOKSTACK_AUTH_METHOD=oauth requires BSMCP_OAUTH_CLIENT_ID");
            }
            eprintln!("BookStack OAuth: not configured (set BSMCP_OAUTH_CLIENT_ID to enable)");
            None
        }
    };

    let state = sse::AppState::new(
        bookstack_url,
        db,
        known_urls,
        backup_interval_hours,
        backup_path,
        semantic,
        summary_cache,
        oauth_config,
        auth_method,
    );
    state.spawn_cleanup();
    state.spawn_backup();

    let mut app = Router::new()
        .route("/mcp/sse", get(sse::handle_sse).post(sse::handle_streamable))
        .route("/mcp/messages/", axum::routing::post(sse::handle_message))
        .route("/.well-known/oauth-authorization-server", get(oauth::handle_metadata))
        .route("/.well-known/oauth-protected-resource", get(oauth::handle_resource_metadata))
        .route("/authorize", get(oauth::handle_authorize).post(oauth::handle_authorize_submit))
        .route("/oauth/callback", get(oauth::handle_oauth_callback))
        .route("/token", axum::routing::post(oauth::handle_token))
        .route("/register", axum::routing::post(oauth::handle_register))
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }));

    // Staging upload endpoint for file uploads (50MB limit)
    app = app.route(
        "/stage/upload/{staging_id}",
        axum::routing::post(staging::handle_stage_upload)
            .layer(DefaultBodyLimit::max(50 * 1024 * 1024)),
    );
    eprintln!("Staging: upload endpoint active at POST /stage/upload/:id");

    // Conditional webhook + status routes for semantic search
    if state.semantic.is_some() {
        app = app
            .route("/webhooks/bookstack", axum::routing::post(handle_webhook))
            .route("/status", get(handle_status));
        eprintln!("Semantic: webhook endpoint active at POST /webhooks/bookstack");
        eprintln!("Semantic: status page at GET /status");
    }

    let app = app
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1MB
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::any())
                .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
                .allow_headers([
                    HeaderName::from_static("authorization"),
                    HeaderName::from_static("content-type"),
                    HeaderName::from_static("accept"),
                    HeaderName::from_static("mcp-session-id"),
                    HeaderName::from_static("mcp-protocol-version"),
                    HeaderName::from_static("last-event-id"),
                ])
                .expose_headers([
                    HeaderName::from_static("mcp-session-id"),
                ])
        )
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().unwrap();
    eprintln!("BookStack MCP server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Webhook handler for BookStack page change events.
async fn handle_webhook(
    State(state): State<sse::AppState>,
    headers: axum::http::HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let semantic = match &state.semantic {
        Some(s) => s,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Prefer X-Webhook-Secret header; fall back to ?secret= query param (deprecated)
    let provided_secret = headers
        .get("x-webhook-secret")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            let qs = params.get("secret").cloned();
            if qs.is_some() {
                eprintln!("Webhook: secret via query param is deprecated — use X-Webhook-Secret header instead");
            }
            qs
        });
    let provided_secret = provided_secret.as_deref().unwrap_or("");
    let expected_secret = semantic.webhook_secret();
    if !sse::constant_time_eq(provided_secret, expected_secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let payload: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if let Err(e) = semantic.handle_webhook(&payload).await {
        eprintln!("Webhook error: {e}");
    }

    StatusCode::ACCEPTED.into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn format_timestamp(ts: Option<i64>) -> String {
    match ts {
        Some(epoch) => {
            // Format as relative time + absolute
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let diff = now - epoch;
            let relative = if diff < 60 {
                format!("{diff}s ago")
            } else if diff < 3600 {
                format!("{}m ago", diff / 60)
            } else if diff < 86400 {
                format!("{}h {}m ago", diff / 3600, (diff % 3600) / 60)
            } else {
                format!("{}d ago", diff / 86400)
            };
            relative
        }
        None => "-".to_string(),
    }
}

fn badge_class(status: &str) -> &str {
    match status {
        "running" => "running",
        "completed" => "completed",
        "failed" => "failed",
        "pending" => "pending",
        _ => "none",
    }
}

fn bar_color(status: &str) -> &str {
    match status {
        "running" => "#3b82f6",
        "completed" => "#22c55e",
        "failed" => "#ef4444",
        _ => "#6b7280",
    }
}

async fn handle_status(
    State(state): State<sse::AppState>,
) -> impl IntoResponse {
    let semantic = match &state.semantic {
        Some(s) => s,
        None => return Html("Semantic search not enabled".to_string()).into_response(),
    };

    let stats = match semantic.embedding_status().await {
        Ok(s) => s,
        Err(e) => return Html(format!("Error: {}", html_escape(&e.to_string()))).into_response(),
    };

    let jobs = semantic.list_jobs(10).await.unwrap_or_default();

    let total_pages = stats["total_indexed_pages"].as_i64().unwrap_or(0);
    let total_chunks = stats["total_chunks"].as_i64().unwrap_or(0);

    let has_active = jobs.iter().any(|j| j.status == "running" || j.status == "pending");
    let auto_refresh = if has_active {
        r#"<meta http-equiv="refresh" content="5">"#
    } else {
        ""
    };

    // Build job rows
    let mut job_rows = String::new();
    for job in &jobs {
        let pct = if job.total_pages > 0 {
            (job.done_pages as f64 / job.total_pages as f64) * 100.0
        } else {
            0.0
        };
        let color = bar_color(&job.status);
        let badge = badge_class(&job.status);
        let scope = html_escape(&job.scope);
        let started = format_timestamp(job.started_at);
        let finished = format_timestamp(job.finished_at);
        let error_html = match &job.error {
            Some(e) => format!(r#"<div class="error">Error: {}</div>"#, html_escape(e)),
            None => String::new(),
        };
        let progress_html = if job.status == "running" || job.status == "completed" || job.status == "failed" {
            format!(r#"
      <div class="bar-bg bar-sm">
        <div class="bar-fill" style="width: {pct:.1}%; background: {color};">{done}/{total}</div>
      </div>"#,
                done = job.done_pages,
                total = job.total_pages,
            )
        } else {
            String::new()
        };

        job_rows.push_str(&format!(r#"
    <div class="job-row">
      <div class="job-header">
        <span class="status-badge {badge}">{status}</span>
        <span class="job-scope">{scope}</span>
        <span class="job-id">#{id}</span>
      </div>{progress_html}
      <div class="job-meta">
        <span>Started: {started}</span>
        {finished_span}
      </div>{error_html}
    </div>"#,
            status = html_escape(&job.status),
            id = job.id,
            finished_span = if job.finished_at.is_some() {
                format!("<span>Finished: {finished}</span>")
            } else {
                String::new()
            },
        ));
    }

    if jobs.is_empty() {
        job_rows = r#"<div class="job-row"><span style="color:#64748b">No jobs found</span></div>"#.to_string();
    }

    let pending_count = jobs.iter().filter(|j| j.status == "pending").count();
    let running_count = jobs.iter().filter(|j| j.status == "running").count();
    let queue_summary = if pending_count > 0 || running_count > 0 {
        format!("{running_count} running, {pending_count} pending")
    } else {
        "idle".to_string()
    };

    let html = format!(r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>BookStack MCP — Embedding Status</title>
{auto_refresh}
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background: #0f172a; color: #e2e8f0; padding: 2rem; }}
  .container {{ max-width: 720px; margin: 0 auto; }}
  h1 {{ font-size: 1.25rem; font-weight: 600; margin-bottom: 1.5rem; color: #f8fafc; }}
  h2 {{ font-size: 1rem; font-weight: 600; margin-bottom: 0.75rem; color: #f8fafc; }}
  .card {{ background: #1e293b; border-radius: 0.75rem; padding: 1.5rem; margin-bottom: 1rem; }}
  .stat-row {{ display: flex; justify-content: space-between; padding: 0.5rem 0; border-bottom: 1px solid #334155; }}
  .stat-row:last-child {{ border-bottom: none; }}
  .stat-label {{ color: #94a3b8; }}
  .stat-value {{ font-weight: 500; font-variant-numeric: tabular-nums; }}
  .bar-bg {{ background: #334155; border-radius: 9999px; height: 1.5rem; overflow: hidden; margin: 0.5rem 0; }}
  .bar-sm {{ height: 1.25rem; }}
  .bar-fill {{ height: 100%%; border-radius: 9999px; transition: width 0.5s ease; display: flex; align-items: center; justify-content: center; font-size: 0.7rem; font-weight: 600; min-width: 2.5rem; color: #fff; }}
  .status-badge {{ display: inline-block; padding: 0.125rem 0.5rem; border-radius: 9999px; font-size: 0.7rem; font-weight: 600; text-transform: uppercase; }}
  .running {{ background: #1e3a5f; color: #60a5fa; }}
  .completed {{ background: #14532d; color: #4ade80; }}
  .failed {{ background: #450a0a; color: #f87171; }}
  .pending {{ background: #3f3f46; color: #a1a1aa; }}
  .none {{ background: #27272a; color: #71717a; }}
  .error {{ color: #f87171; font-size: 0.8rem; margin-top: 0.25rem; }}
  .job-row {{ padding: 0.75rem 0; border-bottom: 1px solid #334155; }}
  .job-row:last-child {{ border-bottom: none; }}
  .job-header {{ display: flex; align-items: center; gap: 0.5rem; }}
  .job-scope {{ font-weight: 500; }}
  .job-id {{ color: #64748b; font-size: 0.8rem; margin-left: auto; }}
  .job-meta {{ display: flex; gap: 1rem; font-size: 0.8rem; color: #94a3b8; margin-top: 0.25rem; }}
  .footer {{ text-align: center; color: #475569; font-size: 0.75rem; margin-top: 2rem; }}
</style>
</head><body>
<div class="container">
  <h1>BookStack MCP — Embedding Status</h1>
  <div class="card">
    <div class="stat-row">
      <span class="stat-label">Indexed Pages</span>
      <span class="stat-value">{total_pages}</span>
    </div>
    <div class="stat-row">
      <span class="stat-label">Total Chunks</span>
      <span class="stat-value">{total_chunks}</span>
    </div>
    <div class="stat-row">
      <span class="stat-label">Queue</span>
      <span class="stat-value">{queue_summary}</span>
    </div>
  </div>
  <div class="card">
    <h2>Job Queue</h2>
    {job_rows}
  </div>
  <div class="footer">Auto-refreshes every 5s while jobs are active</div>
</div>
</body></html>"#);

    Html(html).into_response()
}

async fn run_migration(args: &[String]) {
    let usage = "Usage: bsmcp-server migrate --from-sqlite <PATH> --to-postgres <URL>";

    let mut sqlite_path: Option<String> = None;
    let mut postgres_url: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from-sqlite" => {
                i += 1;
                sqlite_path = args.get(i).cloned();
            }
            "--to-postgres" => {
                i += 1;
                postgres_url = args.get(i).cloned();
            }
            "--help" | "-h" => {
                eprintln!("{usage}");
                eprintln!("\nMigrates all data from a SQLite database to PostgreSQL.");
                eprintln!("Encrypted access tokens are copied as-is (same encryption key required).");
                eprintln!("Chunk embeddings are converted from BLOB to pgvector format.");
                return;
            }
            other => {
                eprintln!("Unknown argument: {other}");
                eprintln!("{usage}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let sqlite_path = match sqlite_path {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("Error: --from-sqlite is required");
            eprintln!("{usage}");
            std::process::exit(1);
        }
    };

    let postgres_url = match postgres_url {
        Some(u) => u,
        None => {
            eprintln!("Error: --to-postgres is required");
            eprintln!("{usage}");
            std::process::exit(1);
        }
    };

    if !sqlite_path.exists() {
        eprintln!("Error: SQLite database not found: {}", sqlite_path.display());
        std::process::exit(1);
    }

    match migrate::run(&sqlite_path, &postgres_url).await {
        Ok(()) => {
            eprintln!("\nMigration completed successfully.");
            eprintln!("You can now switch to PostgreSQL by setting:");
            eprintln!("  BSMCP_DB_BACKEND=postgres");
            eprintln!("  BSMCP_DATABASE_URL={}", migrate::redact_url(&postgres_url));
        }
        Err(e) => {
            eprintln!("\nMigration failed: {e}");
            std::process::exit(1);
        }
    }
}
