mod mcp;
mod migrate;
mod oauth;
mod semantic;
mod sse;

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Json};
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
        if let Ok(domain) = env::var("BSMCP_PUBLIC_DOMAIN") {
            let domain = domain.trim().trim_end_matches('/');
            if !domain.is_empty() {
                urls.push(format!("https://{domain}"));
            }
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

    let state = sse::AppState::new(
        bookstack_url,
        db,
        known_urls,
        backup_interval_hours,
        backup_path,
        semantic,
    );
    state.spawn_cleanup();
    state.spawn_backup();

    let mut app = Router::new()
        .route("/mcp/sse", get(sse::handle_sse).post(sse::handle_streamable))
        .route("/mcp/messages/", axum::routing::post(sse::handle_message))
        .route("/.well-known/oauth-authorization-server", get(oauth::handle_metadata))
        .route("/.well-known/oauth-protected-resource", get(oauth::handle_resource_metadata))
        .route("/authorize", get(oauth::handle_authorize).post(oauth::handle_authorize_submit))
        .route("/token", axum::routing::post(oauth::handle_token))
        .route("/register", axum::routing::post(oauth::handle_register))
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }));

    // Conditional webhook route for semantic search
    if state.semantic.is_some() {
        app = app.route("/webhooks/bookstack", axum::routing::post(handle_webhook));
        eprintln!("Semantic: webhook endpoint active at POST /webhooks/bookstack");
    }

    let app = app
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1MB
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::mirror_request())
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
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: String,
) -> impl IntoResponse {
    let semantic = match &state.semantic {
        Some(s) => s,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Constant-time secret verification
    let provided_secret = params.get("secret").map(|s| s.as_str()).unwrap_or("");
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
            eprintln!("  BSMCP_DATABASE_URL={}", postgres_url);
        }
        Err(e) => {
            eprintln!("\nMigration failed: {e}");
            std::process::exit(1);
        }
    }
}
