mod bookstack;
mod db;
mod mcp;
mod oauth;
mod sse;

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::{Router, routing::get};
use axum::response::Json;
use serde_json::json;
use axum::http::{HeaderName, Method};
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

    let db = Arc::new(db::Db::open(&db_path));
    eprintln!("Database: {}", db_path.display());

    let state = sse::AppState::new(bookstack_url, db);
    state.spawn_cleanup();

    let app = Router::new()
        .route("/mcp/sse", get(sse::handle_sse).post(sse::handle_streamable))
        .route("/mcp/messages/", axum::routing::post(sse::handle_message))
        .route("/.well-known/oauth-authorization-server", get(oauth::handle_metadata))
        .route("/.well-known/oauth-protected-resource", get(oauth::handle_resource_metadata))
        .route("/authorize", get(oauth::handle_authorize).post(oauth::handle_authorize_submit))
        .route("/token", axum::routing::post(oauth::handle_token))
        .route("/register", axum::routing::post(oauth::handle_register))
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
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
