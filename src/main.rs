mod bookstack;
mod mcp;
mod sse;

use std::env;
use std::net::SocketAddr;

use axum::extract::DefaultBodyLimit;
use axum::{Router, routing::get};
use axum::response::Json;
use serde_json::json;
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() {
    let bookstack_url = env::var("BSMCP_BOOKSTACK_URL")
        .expect("BSMCP_BOOKSTACK_URL is required");

    if !bookstack_url.starts_with("https://") {
        eprintln!("WARNING: BSMCP_BOOKSTACK_URL is not HTTPS. Ensure BookStack is on an isolated network.");
    }

    let host = env::var("BSMCP_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = env::var("BSMCP_PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse()
        .expect("BSMCP_PORT must be a valid port number");

    let state = sse::AppState::new(bookstack_url);
    state.spawn_cleanup();

    let app = Router::new()
        .route("/mcp/sse", get(sse::handle_sse))
        .route("/mcp/messages/", axum::routing::post(sse::handle_message))
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .layer(DefaultBodyLimit::max(1024 * 1024)) // 1MB
        .layer(CorsLayer::new()) // deny all origins by default
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().unwrap();
    eprintln!("BookStack MCP server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
