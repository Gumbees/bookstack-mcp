mod bookstack;
mod chunking;
mod db;
mod mcp;
mod oauth;
mod semantic;
mod sse;
mod vector;

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

#[tokio::main]
async fn main() {
    let bookstack_url = env::var("BSMCP_BOOKSTACK_URL")
        .expect("BSMCP_BOOKSTACK_URL is required");

    let host = env::var("BSMCP_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = env::var("BSMCP_PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse()
        .expect("BSMCP_PORT must be a valid port number");

    let db_path = env::var("BSMCP_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/bookstack-mcp.db"));

    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let encryption_key = env::var("BSMCP_ENCRYPTION_KEY")
        .expect("BSMCP_ENCRYPTION_KEY is required (32+ character key for AES-256-GCM token encryption)");
    if encryption_key.len() < 32 {
        panic!("BSMCP_ENCRYPTION_KEY must be at least 32 characters");
    }
    eprintln!("Encryption: enabled (AES-256-GCM)");

    let db = Arc::new(db::Db::open(&db_path, &encryption_key));
    eprintln!("Database: {}", db_path.display());

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
        let model_path = env::var("BSMCP_MODEL_PATH").unwrap_or_else(|_| "/models".into());
        let webhook_secret = env::var("BSMCP_WEBHOOK_SECRET")
            .expect("BSMCP_WEBHOOK_SECRET is required when semantic search is enabled");
        let embed_token_id = env::var("BSMCP_EMBED_TOKEN_ID")
            .expect("BSMCP_EMBED_TOKEN_ID is required when semantic search is enabled");
        let embed_token_secret = env::var("BSMCP_EMBED_TOKEN_SECRET")
            .expect("BSMCP_EMBED_TOKEN_SECRET is required when semantic search is enabled");

        let http_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("Failed to build embed HTTP client");

        let embed_client = bookstack::BookStackClient::new(
            &bookstack_url, &embed_token_id, &embed_token_secret, http_client,
        );

        eprintln!("Semantic: initializing (model_path={model_path})...");
        match semantic::SemanticState::new(db.clone(), &model_path, embed_client, webhook_secret).await {
            Ok(s) => {
                eprintln!("Semantic: ready");
                Some(Arc::new(s))
            }
            Err(e) => {
                eprintln!("Semantic: failed to initialize — {e}");
                eprintln!("Semantic: continuing without semantic search");
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
        // CORS: mirror_request() reflects the Origin header back as Access-Control-Allow-Origin.
        // This is safe because the Bearer token in the Authorization header is the actual
        // security boundary — browsers cannot forge it via CSRF. The MCP protocol requires
        // browser-based OAuth flows that need permissive CORS.
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
/// Secret is passed as query parameter (BookStack doesn't support HMAC signing).
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
    if !constant_time_eq(provided_secret, expected_secret) {
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

/// Constant-time string comparison for webhook secret verification.
fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let a = a.as_bytes();
    let b = b.as_bytes();
    let len = a.len().max(b.len());
    let mut a_padded = vec![0xAAu8; len];
    let mut b_padded = vec![0xBBu8; len];
    a_padded[..a.len()].copy_from_slice(a);
    b_padded[..b.len()].copy_from_slice(b);
    let result: bool = a_padded.ct_eq(&b_padded).into();
    result && a.len() == b.len()
}
