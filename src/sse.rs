use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::bookstack::BookStackClient;
use crate::db::Db;
use crate::mcp;
use crate::oauth::{AuthCode, AUTH_CODE_TTL};

const MAX_SESSIONS_PER_TOKEN: usize = 5;
const MAX_TOTAL_SESSIONS: usize = 1000;
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
const MAX_REQUESTS_PER_MINUTE: u32 = 100;

#[derive(Clone)]
pub struct AppState {
    pub bookstack_url: String,
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    pub auth_codes: Arc<RwLock<HashMap<String, AuthCode>>>,
    pub db: Arc<Db>,
}

struct RateLimit {
    count: u32,
    window_start: Instant,
}

struct Session {
    tx: mpsc::Sender<Result<Event, Infallible>>,
    client: BookStackClient,
    token_id: String,
    token_secret: String,
    created_at: Instant,
    rate_limit: Arc<Mutex<RateLimit>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.token_id.zeroize();
        self.token_secret.zeroize();
    }
}

impl AppState {
    pub fn new(bookstack_url: String, db: Arc<Db>) -> Self {
        Self {
            bookstack_url: bookstack_url.trim_end_matches('/').to_string(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            auth_codes: Arc::new(RwLock::new(HashMap::new())),
            db,
        }
    }

    /// Spawn a single shared cleanup task that periodically removes expired/disconnected sessions
    /// and expired OAuth codes/tokens.
    pub fn spawn_cleanup(&self) {
        let sessions = self.sessions.clone();
        let auth_codes = self.auth_codes.clone();
        let db = self.db.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                {
                    let mut sessions = sessions.write().await;
                    sessions.retain(|sid, s| {
                        let expired = s.tx.is_closed() || s.created_at.elapsed() > SESSION_TTL;
                        if expired {
                            eprintln!("Session {sid} cleaned up");
                        }
                        !expired
                    });
                }
                {
                    let mut codes = auth_codes.write().await;
                    codes.retain(|_, c| c.created_at.elapsed() < AUTH_CODE_TTL);
                }
                db.cleanup_expired_tokens();
            }
        });
    }
}

/// Constant-time string comparison to prevent timing side-channel attacks.
/// Uses the `subtle` crate which handles length differences without leaking timing info.
fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Build a 401 response with WWW-Authenticate header for OAuth discovery.
/// Includes resource_metadata URL per MCP 2025-06-18 / RFC 9728.
fn unauthorized(hint: &str, headers: &HeaderMap) -> Response {
    let base = crate::oauth::derive_base_url(headers);
    let body = serde_json::json!({"error": "unauthorized", "hint": hint});
    let mut resp = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    let www_auth = format!(
        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
    );
    resp.headers_mut()
        .insert("WWW-Authenticate", www_auth.parse().unwrap());
    resp
}

/// Resolve Bearer token to BookStack credentials.
/// Supports both legacy `token_id:token_secret` format and OAuth access tokens (from SQLite).
fn resolve_credentials(
    headers: &HeaderMap,
    db: &Db,
) -> Result<(String, String), Response> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !auth.starts_with("Bearer ") {
        eprintln!("Auth: no Bearer token in request");
        return Err(unauthorized("Bearer token required", headers));
    }

    let token = auth.trim_start_matches("Bearer ").trim();

    // Legacy format: token_id:token_secret
    if let Some((id, secret)) = token.split_once(':') {
        eprintln!("Auth: legacy token format (token_id:secret)");
        return Ok((id.to_string(), secret.to_string()));
    }

    // OAuth access token (from SQLite)
    let token_count = db.count_tokens();
    if let Some((token_id, token_secret)) = db.get_access_token(token) {
        eprintln!("Auth: OAuth token resolved (db has {token_count} tokens)");
        return Ok((token_id, token_secret));
    }

    eprintln!("Auth: token not found in db ({token_count} tokens stored, token_len={})", token.len());
    Err(unauthorized("Invalid or expired token", headers))
}

pub async fn handle_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    eprintln!("GET /mcp/sse — SSE connection attempt");
    let (token_id, token_secret) = match resolve_credentials(&headers, &state.db) {
        Ok(creds) => creds,
        Err(resp) => return resp,
    };

    let client = BookStackClient::new(&state.bookstack_url, &token_id, &token_secret);

    // Validate credentials against BookStack
    if let Err(e) = client.validate().await {
        eprintln!("Credential validation failed: {e}");
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Invalid BookStack credentials"})),
        )
            .into_response();
    }

    // Atomically check session limit and insert under write lock (fixes TOCTOU)
    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);

    // Insert session BEFORE sending endpoint event (prevents race where
    // client receives endpoint URL and sends a message before session exists)
    {
        let mut sessions = state.sessions.write().await;

        // Global session limit
        if sessions.len() >= MAX_TOTAL_SESSIONS {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Server at session capacity"})),
            )
                .into_response();
        }

        // Per-token session limit
        let count = sessions.values().filter(|s| s.token_id == token_id).count();
        if count >= MAX_SESSIONS_PER_TOKEN {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": "Too many sessions for this token"})),
            )
                .into_response();
        }
        sessions.insert(
            session_id.clone(),
            Session {
                tx: tx.clone(),
                client,
                token_id: token_id.clone(),
                token_secret: token_secret.clone(),
                created_at: Instant::now(),
                rate_limit: Arc::new(Mutex::new(RateLimit {
                    count: 0,
                    window_start: Instant::now(),
                })),
            },
        );
    }

    eprintln!("SSE session {session_id} created");

    // Send endpoint event after session is stored
    let endpoint_url = format!("/mcp/messages/?sessionId={session_id}");
    let _ = tx
        .send(Ok(Event::default().event("endpoint").data(endpoint_url)))
        .await;

    // Session cleanup is handled by the shared cleanup task (AppState::spawn_cleanup)

    let stream = ReceiverStream::new(rx);
    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::default().interval(Duration::from_secs(15)))
        .into_response();

    // Prevent Cloudflare and reverse proxies from buffering SSE
    let hdrs = resp.headers_mut();
    hdrs.insert(header::CACHE_CONTROL, "no-cache, no-store".parse().unwrap());
    hdrs.insert("X-Accel-Buffering", "no".parse().unwrap());

    resp
}

pub async fn handle_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
    body: String,
) -> Response {
    eprintln!("POST /mcp/messages/ — message request");
    // Authenticate the request (both token_id and token_secret)
    let (token_id, token_secret) = match resolve_credentials(&headers, &state.db) {
        Ok(creds) => creds,
        Err(resp) => return resp,
    };

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

    // Clone what we need out of the session, then release the lock
    let (tx, client, rate_limit) = {
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

        // Verify the full token (id AND secret) matches the session owner
        // Uses constant-time comparison to prevent timing side-channel attacks
        if !constant_time_eq(&session.token_id, &token_id) || !constant_time_eq(&session.token_secret, &token_secret) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "token does not match session"})),
            )
                .into_response();
        }

        // Check TTL
        if session.created_at.elapsed() > SESSION_TTL {
            return (
                StatusCode::GONE,
                Json(serde_json::json!({"error": "session expired"})),
            )
                .into_response();
        }

        (session.tx.clone(), session.client.clone(), session.rate_limit.clone())
    }; // RwLock released here

    // Rate limit check
    {
        let mut rl = rate_limit.lock().await;
        if rl.window_start.elapsed() > Duration::from_secs(60) {
            rl.count = 0;
            rl.window_start = Instant::now();
        }
        if rl.count >= MAX_REQUESTS_PER_MINUTE {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": "Rate limit exceeded"})),
            )
                .into_response();
        }
        rl.count += 1;
    }

    let request: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid JSON"})),
            )
                .into_response()
        }
    };

    let response = mcp::handle_request(&request, &client).await;

    if let Some(response) = response {
        let data = serde_json::to_string(&response).unwrap_or_default();
        // try_send to avoid blocking if the SSE client is slow
        if let Err(e) = tx.try_send(Ok(Event::default().event("message").data(data))) {
            eprintln!("SSE send failed for session {session_id}: {e}");
        }
    }

    StatusCode::ACCEPTED.into_response()
}

/// Streamable HTTP transport (MCP 2025-03-26).
/// Client POSTs JSON-RPC directly to the endpoint and gets JSON responses.
/// No persistent SSE connection needed.
pub async fn handle_streamable(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    eprintln!("POST /mcp/sse — Streamable HTTP request");
    let (token_id, token_secret) = match resolve_credentials(&headers, &state.db) {
        Ok(creds) => creds,
        Err(resp) => return resp,
    };

    let client = BookStackClient::new(&state.bookstack_url, &token_id, &token_secret);

    let request: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid JSON"})),
            )
                .into_response()
        }
    };

    let method = request["method"].as_str().unwrap_or("");

    // For initialize, validate credentials against BookStack
    if method == "initialize" {
        if let Err(e) = client.validate().await {
            eprintln!("Streamable: credential validation failed: {e}");
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "Invalid BookStack credentials"})),
            )
                .into_response();
        }
    }

    // Notifications have no response
    if request.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    let response = mcp::handle_request(&request, &client).await;

    match response {
        Some(resp) => {
            let session_id = headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none");

            let mut http_resp = Json(resp).into_response();

            // On initialize, issue a new session ID
            if method == "initialize" {
                let new_session_id = uuid::Uuid::new_v4().to_string();
                eprintln!("Streamable session {new_session_id} created");
                http_resp.headers_mut().insert(
                    "Mcp-Session-Id",
                    new_session_id.parse().unwrap(),
                );
            } else if session_id != "none" {
                http_resp.headers_mut().insert(
                    "Mcp-Session-Id",
                    session_id.parse().unwrap(),
                );
            }

            http_resp
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}
