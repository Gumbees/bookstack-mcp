use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use subtle::ConstantTimeEq;
use bsmcp_common::bookstack::{BookStackAuth, BookStackClient};
use bsmcp_common::db::DbBackend;

use crate::mcp;
use crate::oauth::{AuthCode, AUTH_CODE_TTL};
use crate::semantic::SemanticState;

const MAX_SESSIONS_PER_TOKEN: usize = 5;
const MAX_TOTAL_SESSIONS: usize = 1000;
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
const MAX_REQUESTS_PER_MINUTE: u32 = 100;

/// OAuth configuration for BookStack's OAuth provider.
/// Populated at startup when BSMCP_OAUTH_CLIENT_ID is set.
#[derive(Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Clone)]
pub struct AppState {
    pub bookstack_url: String,
    pub http_client: Client,
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    pub auth_codes: Arc<RwLock<HashMap<String, AuthCode>>>,
    /// Pending OAuth authorization flows (maps state param → original Claude authorize params)
    pub pending_oauth: Arc<RwLock<HashMap<String, PendingOAuth>>>,
    pub db: Arc<dyn DbBackend>,
    pub known_urls: Vec<String>,
    pub authorize_rate_limit: Arc<Mutex<RateLimit>>,
    pub register_rate_limit: Arc<Mutex<RateLimit>>,
    streamable_rate_limits: Arc<RwLock<HashMap<String, Arc<Mutex<RateLimit>>>>>,
    streamable_sessions: Arc<RwLock<HashMap<String, Instant>>>,
    backup_interval_hours: Option<u64>,
    backup_path: PathBuf,
    pub semantic: Option<Arc<SemanticState>>,
    pub summary_cache: crate::summary::SummaryCache,
    pub staging: crate::staging::StagingStore,
    /// OAuth config for BookStack's OAuth provider (None = token-only mode)
    pub oauth_config: Option<OAuthConfig>,
    /// Auth method: "token", "oauth", or "auto"
    pub auth_method: String,
}

/// In-flight OAuth authorization linking MCP's auth flow to BookStack's OAuth flow.
pub struct PendingOAuth {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub pkce_verifier: String,
    pub created_at: Instant,
}

pub(crate) struct RateLimit {
    count: u32,
    max: u32,
    window_start: Instant,
}

impl RateLimit {
    pub(crate) fn new(max: u32) -> Self {
        Self {
            count: 0,
            max,
            window_start: Instant::now(),
        }
    }

    pub(crate) fn check(&mut self) -> Result<(), ()> {
        if self.window_start.elapsed() > Duration::from_secs(60) {
            self.count = 0;
            self.window_start = Instant::now();
        }
        if self.count >= self.max {
            return Err(());
        }
        self.count += 1;
        Ok(())
    }
}

struct Session {
    tx: mpsc::Sender<Result<Event, Infallible>>,
    client: BookStackClient,
    auth: BookStackAuth,
    created_at: Instant,
    rate_limit: Arc<Mutex<RateLimit>>,
}

impl AppState {
    pub fn new(
        bookstack_url: String,
        db: Arc<dyn DbBackend>,
        known_urls: Vec<String>,
        backup_interval_hours: Option<u64>,
        backup_path: PathBuf,
        semantic: Option<Arc<SemanticState>>,
        summary_cache: crate::summary::SummaryCache,
        oauth_config: Option<OAuthConfig>,
        auth_method: String,
    ) -> Self {
        let http_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client");
        Self {
            bookstack_url: bookstack_url.trim_end_matches('/').to_string(),
            http_client,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            auth_codes: Arc::new(RwLock::new(HashMap::new())),
            pending_oauth: Arc::new(RwLock::new(HashMap::new())),
            db,
            known_urls,
            authorize_rate_limit: Arc::new(Mutex::new(RateLimit::new(20))),
            register_rate_limit: Arc::new(Mutex::new(RateLimit::new(10))),
            streamable_rate_limits: Arc::new(RwLock::new(HashMap::new())),
            streamable_sessions: Arc::new(RwLock::new(HashMap::new())),
            backup_interval_hours,
            backup_path,
            semantic,
            summary_cache,
            staging: crate::staging::new_staging_store(),
            oauth_config,
            auth_method,
        }
    }

    /// Whether the server should use BookStack OAuth for user authentication.
    pub fn use_oauth(&self) -> bool {
        match self.auth_method.as_str() {
            "oauth" => self.oauth_config.is_some(),
            "auto" => self.oauth_config.is_some(),
            _ => false,
        }
    }

    pub fn spawn_cleanup(&self) {
        let sessions = self.sessions.clone();
        let auth_codes = self.auth_codes.clone();
        let db = self.db.clone();
        let streamable_rate_limits = self.streamable_rate_limits.clone();
        let streamable_sessions = self.streamable_sessions.clone();
        let staging_clone = self.staging.clone();
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
                {
                    let mut srl = streamable_rate_limits.write().await;
                    srl.retain(|_, rl| {
                        let rl = rl.try_lock();
                        match rl {
                            Ok(rl) => rl.window_start.elapsed() < Duration::from_secs(120),
                            Err(_) => true,
                        }
                    });
                }
                {
                    let mut ss = streamable_sessions.write().await;
                    ss.retain(|_, created| created.elapsed() < SESSION_TTL);
                }
                {
                    crate::staging::cleanup_expired_sync(&staging_clone);
                }
                if let Err(e) = db.cleanup_expired_tokens().await {
                    eprintln!("Token cleanup error: {e}");
                }
            }
        });
    }

    pub fn spawn_backup(&self) {
        let Some(interval_hours) = self.backup_interval_hours else {
            return;
        };
        let db = self.db.clone();
        let backup_path = self.backup_path.clone();
        let interval = Duration::from_secs(interval_hours * 3600);
        eprintln!("Backup: enabled every {interval_hours}h to {}", backup_path.display());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                match db.backup(&backup_path).await {
                    Ok(()) => eprintln!("Backup: completed successfully"),
                    Err(e) => eprintln!("Backup: failed — {e}"),
                }
            }
        });
    }
}

/// Constant-time string comparison to prevent timing side-channel attacks.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
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

fn unauthorized(hint: &str, headers: &HeaderMap, known_urls: &[String]) -> Response {
    let base = crate::oauth::derive_base_url(headers, known_urls);
    let body = serde_json::json!({"error": "unauthorized", "hint": hint});
    let mut resp = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    let www_auth = format!(
        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
    );
    resp.headers_mut()
        .insert("WWW-Authenticate", www_auth.parse().unwrap());
    resp
}

pub(crate) async fn resolve_credentials(
    headers: &HeaderMap,
    db: &dyn DbBackend,
    state: &AppState,
) -> Result<BookStackAuth, Response> {
    let known_urls = &state.known_urls;
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !auth.starts_with("Bearer ") {
        eprintln!("Auth: no Bearer token in request");
        return Err(unauthorized("Bearer token required", headers, known_urls));
    }

    let token = auth.trim_start_matches("Bearer ").trim();

    // Legacy format: token_id:token_secret
    if let Some((id, secret)) = token.split_once(':') {
        eprintln!("Auth: legacy token format (token_id:secret)");
        return Ok(BookStackAuth::Token {
            token_id: id.to_string(),
            token_secret: secret.to_string(),
        });
    }

    // MCP access token (from database) — may resolve to Token or OAuth credentials
    match db.get_access_token(token).await {
        Ok(Some(creds)) => {
            if creds.auth_type == "oauth" {
                eprintln!("Auth: OAuth credentials resolved");
                // credential1 = BS access_token, credential2 = BS refresh_token
                if let Some(ref oauth_config) = state.oauth_config {
                    let tokens = bsmcp_common::bookstack::OAuthTokens::new(
                        creds.credential1,
                        creds.credential2,
                        oauth_config.client_id.clone(),
                        oauth_config.client_secret.clone(),
                        oauth_config.token_endpoint.clone(),
                    );
                    return Ok(BookStackAuth::OAuth(tokens));
                } else {
                    eprintln!("Auth: OAuth credentials found but no OAuth config on server");
                    return Err(unauthorized("Server not configured for OAuth", headers, known_urls));
                }
            } else {
                eprintln!("Auth: API token credentials resolved");
                return Ok(BookStackAuth::Token {
                    token_id: creds.credential1,
                    token_secret: creds.credential2,
                });
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("Auth: token lookup error: {e}");
        }
    }

    eprintln!("Auth: token not recognized");
    Err(unauthorized("Invalid or expired token", headers, known_urls))
}

pub async fn handle_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    eprintln!("GET /mcp/sse — SSE connection attempt");
    let auth = match resolve_credentials(&headers, state.db.as_ref(), &state).await {
        Ok(creds) => creds,
        Err(resp) => return resp,
    };

    let client = match &auth {
        BookStackAuth::Token { token_id, token_secret } => {
            BookStackClient::new(&state.bookstack_url, token_id, token_secret, state.http_client.clone())
        }
        BookStackAuth::OAuth(tokens) => {
            BookStackClient::with_oauth(&state.bookstack_url, tokens.clone(), state.http_client.clone())
        }
    };

    if let Err(e) = client.validate().await {
        eprintln!("Credential validation failed: {e}");
        return unauthorized(
            "BookStack credentials are invalid or expired — please re-authenticate",
            &headers,
            &state.known_urls,
        );
    }

    let identity = auth.identity();
    let session_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(32);

    {
        let mut sessions = state.sessions.write().await;

        if sessions.len() >= MAX_TOTAL_SESSIONS {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Server at session capacity"})),
            )
                .into_response();
        }

        let count = sessions.values().filter(|s| s.auth.identity() == identity).count();
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
                auth,
                created_at: Instant::now(),
                rate_limit: Arc::new(Mutex::new(RateLimit::new(MAX_REQUESTS_PER_MINUTE))),
            },
        );
    }

    eprintln!("SSE session {session_id} created");

    let endpoint_url = format!("/mcp/messages/?sessionId={session_id}");
    let _ = tx
        .send(Ok(Event::default().event("endpoint").data(endpoint_url)))
        .await;

    let stream = ReceiverStream::new(rx);
    let mut resp = Sse::new(stream)
        .keep_alive(KeepAlive::default().interval(Duration::from_secs(15)))
        .into_response();

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
    let auth = match resolve_credentials(&headers, state.db.as_ref(), &state).await {
        Ok(creds) => creds,
        Err(resp) => return resp,
    };
    let identity = auth.identity();

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

        if !constant_time_eq(&session.auth.identity(), &identity) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "token does not match session"})),
            )
                .into_response();
        }

        if session.created_at.elapsed() > SESSION_TTL {
            return (
                StatusCode::GONE,
                Json(serde_json::json!({"error": "session expired"})),
            )
                .into_response();
        }

        (session.tx.clone(), session.client.clone(), session.rate_limit.clone())
    };

    {
        let mut rl = rate_limit.lock().await;
        if rl.check().is_err() {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": "Rate limit exceeded"})),
            )
                .into_response();
        }
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

    let semantic = state.semantic.as_deref();
    let response = mcp::handle_request(&request, &client, semantic, &state.summary_cache, &state.staging).await;

    if let Some(response) = response {
        let data = serde_json::to_string(&response).unwrap_or_default();
        if let Err(e) = tx.try_send(Ok(Event::default().event("message").data(data))) {
            eprintln!("SSE send failed for session {session_id}: {e}");
        }
    }

    StatusCode::ACCEPTED.into_response()
}

pub async fn handle_streamable(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    eprintln!("POST /mcp/sse — Streamable HTTP request");
    let auth = match resolve_credentials(&headers, state.db.as_ref(), &state).await {
        Ok(creds) => creds,
        Err(resp) => return resp,
    };
    let identity = auth.identity();

    {
        let rate_limits = state.streamable_rate_limits.read().await;
        if let Some(rl) = rate_limits.get(&identity) {
            let mut rl = rl.lock().await;
            if rl.check().is_err() {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({"error": "Rate limit exceeded"})),
                )
                    .into_response();
            }
        } else {
            drop(rate_limits);
            let mut rate_limits = state.streamable_rate_limits.write().await;
            let rl = rate_limits
                .entry(identity.clone())
                .or_insert_with(|| Arc::new(Mutex::new(RateLimit::new(MAX_REQUESTS_PER_MINUTE))));
            let mut rl = rl.lock().await;
            let _ = rl.check();
        }
    }

    let client = match &auth {
        BookStackAuth::Token { token_id, token_secret } => {
            BookStackClient::new(&state.bookstack_url, token_id, token_secret, state.http_client.clone())
        }
        BookStackAuth::OAuth(tokens) => {
            BookStackClient::with_oauth(&state.bookstack_url, tokens.clone(), state.http_client.clone())
        }
    };

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

    if method == "initialize" {
        if let Err(e) = client.validate().await {
            eprintln!("Streamable: credential validation failed: {e}");
            return unauthorized(
                "BookStack credentials are invalid or expired — please re-authenticate",
                &headers,
                &state.known_urls,
            );
        }
    }

    if request.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    let semantic = state.semantic.as_deref();
    let response = mcp::handle_request(&request, &client, semantic, &state.summary_cache, &state.staging).await;

    match response {
        Some(resp) => {
            let incoming_session_id = headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            let mut http_resp = Json(resp).into_response();

            if method == "initialize" {
                let new_session_id = uuid::Uuid::new_v4().to_string();
                eprintln!("Streamable session {new_session_id} created");
                {
                    let mut ss = state.streamable_sessions.write().await;
                    ss.insert(new_session_id.clone(), Instant::now());
                }
                http_resp.headers_mut().insert(
                    "Mcp-Session-Id",
                    new_session_id.parse().unwrap(),
                );
            } else if let Some(ref sid) = incoming_session_id {
                let ss = state.streamable_sessions.read().await;
                if ss.contains_key(sid) {
                    http_resp.headers_mut().insert(
                        "Mcp-Session-Id",
                        sid.parse().unwrap(),
                    );
                }
            }

            http_resp
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}
