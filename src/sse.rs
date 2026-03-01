use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;

use crate::bookstack::BookStackClient;
use crate::mcp;

#[derive(Clone)]
pub struct AppState {
    bookstack_url: String,
    sessions: Arc<RwLock<HashMap<String, Session>>>,
}

struct Session {
    tx: mpsc::Sender<Result<Event, Infallible>>,
    client: BookStackClient,
}

impl AppState {
    pub fn new(bookstack_url: String) -> Self {
        Self {
            bookstack_url: bookstack_url.trim_end_matches('/').to_string(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

fn extract_credentials(headers: &HeaderMap) -> Result<(String, String), (StatusCode, Json<Value>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !auth.starts_with("Bearer ") {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "hint": "Bearer <token_id>:<token_secret>"
            })),
        ));
    }

    let token = auth.trim_start_matches("Bearer ").trim();
    let (id, secret) = token.split_once(':').ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "invalid token format",
                "hint": "Expected <token_id>:<token_secret>"
            })),
        )
    })?;

    Ok((id.to_string(), secret.to_string()))
}

pub async fn handle_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let (token_id, token_secret) = match extract_credentials(&headers) {
        Ok(creds) => creds,
        Err((status, body)) => return (status, body).into_response(),
    };

    let client = BookStackClient::new(&state.bookstack_url, &token_id, &token_secret);

    // Validate credentials against BookStack
    if let Err(e) = client.validate().await {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": format!("Invalid BookStack credentials: {e}")})),
        )
            .into_response();
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);

    // Send endpoint event
    let endpoint_url = format!("/mcp/messages/?sessionId={session_id}");
    let _ = tx
        .send(Ok(Event::default().event("endpoint").data(endpoint_url)))
        .await;

    // Store session
    state
        .sessions
        .write()
        .await
        .insert(session_id.clone(), Session { tx, client });

    // Clean up on disconnect
    let sessions = state.sessions.clone();
    let sid = session_id.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let should_remove = {
                let sessions = sessions.read().await;
                sessions
                    .get(&sid)
                    .map(|s| s.tx.is_closed())
                    .unwrap_or(true)
            };
            if should_remove {
                sessions.write().await.remove(&sid);
                eprintln!("Session {sid} cleaned up");
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(KeepAlive::default().interval(Duration::from_secs(15)))
        .into_response()
}

pub async fn handle_message(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    let session_id = match params.get("sessionId") {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing sessionId"})),
            )
                .into_response()
        }
    };

    let sessions = state.sessions.read().await;
    let session = match sessions.get(session_id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "unknown session"})),
            )
                .into_response()
        }
    };

    let request: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid JSON: {e}")})),
            )
                .into_response()
        }
    };

    let response = mcp::handle_request(&request, &session.client);

    if let Some(response) = response {
        let data = serde_json::to_string(&response).unwrap_or_default();
        let _ = session
            .tx
            .send(Ok(Event::default().event("message").data(data)))
            .await;
    }

    StatusCode::ACCEPTED.into_response()
}
