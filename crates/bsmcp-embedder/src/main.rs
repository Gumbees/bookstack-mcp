mod pipeline;

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Json};
use axum::{Router, routing::get};
use serde::Deserialize;
use serde_json::json;

use bsmcp_common::bookstack::BookStackClient;
use bsmcp_common::config::DbBackendType;
use bsmcp_common::db::SemanticDb;

use pipeline::EmbedModel;

struct AppState {
    model: Arc<EmbedModel>,
    db: Arc<dyn SemanticDb>,
}

#[derive(Deserialize)]
struct EmbedRequest {
    texts: Vec<String>,
}

#[tokio::main]
async fn main() {
    let encryption_key = env::var("BSMCP_ENCRYPTION_KEY")
        .expect("BSMCP_ENCRYPTION_KEY is required");
    if encryption_key.len() < 32 {
        panic!("BSMCP_ENCRYPTION_KEY must be at least 32 characters");
    }

    // Select database backend
    let backend_type = DbBackendType::from_env();
    let db: Arc<dyn SemanticDb> = match backend_type {
        DbBackendType::Sqlite => {
            let db_path = env::var("BSMCP_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/data/bookstack-mcp.db"));
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            eprintln!("Database: SQLite ({})", db_path.display());
            Arc::new(bsmcp_db_sqlite::SqliteDb::open(&db_path, &encryption_key))
        }
        DbBackendType::Postgres => {
            let database_url = env::var("BSMCP_DATABASE_URL")
                .expect("BSMCP_DATABASE_URL is required when BSMCP_DB_BACKEND=postgres");
            eprintln!("Database: PostgreSQL");
            Arc::new(
                bsmcp_db_postgres::PostgresDb::new(&database_url, &encryption_key)
                    .await
                    .expect("Failed to connect to PostgreSQL"),
            )
        }
    };

    // Initialize semantic tables
    db.init_semantic_tables().await.expect("Failed to initialize semantic tables");

    // Load embedding model
    let model_path = env::var("BSMCP_MODEL_PATH").unwrap_or_else(|_| "/data/models".into());
    let model_name = env::var("BSMCP_EMBED_MODEL").unwrap_or_else(|_| "BAAI/bge-large-en-v1.5".into());
    let embed_threads: usize = env::var("BSMCP_EMBED_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Set OMP_NUM_THREADS before model init to limit ONNX Runtime CPU usage
    if embed_threads > 0 {
        env::set_var("OMP_NUM_THREADS", embed_threads.to_string());
        eprintln!("Embedder: thread limit set to {embed_threads}");
    }

    eprintln!("Embedder: loading model {model_name} (cache={model_path})...");

    let model = EmbedModel::new(&model_path).expect("Failed to load embedding model");
    let model = Arc::new(model);
    eprintln!("Embedder: model ready");

    // Start HTTP server for /embed endpoint
    let host = env::var("BSMCP_EMBED_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = env::var("BSMCP_EMBED_PORT")
        .unwrap_or_else(|_| "8081".into())
        .parse()
        .expect("BSMCP_EMBED_PORT must be a valid port number");

    let state = Arc::new(AppState {
        model: model.clone(),
        db: db.clone(),
    });

    let app = Router::new()
        .route("/embed", axum::routing::post(handle_embed))
        .route("/health", get(handle_health))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse().unwrap();

    // Spawn job queue worker
    let worker_db = db.clone();
    let worker_model = model.clone();
    tokio::spawn(async move {
        job_queue_worker(worker_db, worker_model).await;
    });

    eprintln!("Embedder: HTTP server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn handle_embed(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmbedRequest>,
) -> impl IntoResponse {
    if req.texts.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "texts array must not be empty"})),
        )
            .into_response();
    }

    if req.texts.len() > 100 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "maximum 100 texts per request"})),
        )
            .into_response();
    }

    let model = state.model.clone();
    let texts = req.texts;
    let result = tokio::task::spawn_blocking(move || {
        model.embed(texts)
    })
    .await;

    match result {
        Ok(Ok(embeddings)) => {
            Json(json!({ "embeddings": embeddings })).into_response()
        }
        Ok(Err(e)) => {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Embedding failed: {e}")})),
            )
                .into_response()
        }
        Err(e) => {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Task failed: {e}")})),
            )
                .into_response()
        }
    }
}

async fn handle_health(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let stats = state.db.get_stats().await.ok();
    Json(json!({
        "status": "ok",
        "model": "BAAI/bge-large-en-v1.5",
        "dimensions": 1024,
        "stats": stats.map(|s| json!({
            "total_pages": s.total_pages,
            "total_chunks": s.total_chunks,
            "latest_job": s.latest_job.map(|j| json!({
                "id": j.id,
                "scope": j.scope,
                "status": j.status,
                "done_pages": j.done_pages,
                "total_pages": j.total_pages,
            })),
        })),
    }))
}

/// Background job queue worker. Polls for pending embed jobs and processes them.
async fn job_queue_worker(db: Arc<dyn SemanticDb>, model: Arc<EmbedModel>) {
    let poll_interval: u64 = env::var("BSMCP_EMBED_POLL_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let delay_ms: u64 = env::var("BSMCP_EMBED_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let batch_size: usize = env::var("BSMCP_EMBED_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);

    let bookstack_url = env::var("BSMCP_BOOKSTACK_URL")
        .expect("BSMCP_BOOKSTACK_URL is required");
    let embed_token_id = env::var("BSMCP_EMBED_TOKEN_ID")
        .expect("BSMCP_EMBED_TOKEN_ID is required");
    let embed_token_secret = env::var("BSMCP_EMBED_TOKEN_SECRET")
        .expect("BSMCP_EMBED_TOKEN_SECRET is required");

    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .expect("Failed to build HTTP client");

    let client = BookStackClient::new(
        &bookstack_url,
        &embed_token_id,
        &embed_token_secret,
        http_client,
    );

    eprintln!("Embedder: job queue worker started (poll={}s, delay={}ms, batch={})",
        poll_interval, delay_ms, batch_size);

    loop {
        match db.claim_next_job().await {
            Ok(Some(job)) => {
                eprintln!("Embedder: claimed job {} (scope={})", job.id, job.scope);
                let result = pipeline::run_pipeline(
                    &db, &model, &client,
                    job.id, &job.scope,
                    delay_ms, batch_size,
                ).await;
                match result {
                    Ok(()) => {
                        if let Err(e) = db.complete_job(job.id, None).await {
                            eprintln!("Embedder: failed to mark job {} complete: {e}", job.id);
                        }
                        eprintln!("Embedder: job {} completed", job.id);
                    }
                    Err(e) => {
                        eprintln!("Embedder: job {} failed: {e}", job.id);
                        if let Err(e2) = db.complete_job(job.id, Some(&e)).await {
                            eprintln!("Embedder: failed to mark job {} failed: {e2}", job.id);
                        }
                    }
                }
            }
            Ok(None) => {
                // No pending jobs, sleep
                tokio::time::sleep(Duration::from_secs(poll_interval)).await;
            }
            Err(e) => {
                eprintln!("Embedder: job queue poll error: {e}");
                tokio::time::sleep(Duration::from_secs(poll_interval)).await;
            }
        }
    }
}
