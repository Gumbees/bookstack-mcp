use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Form;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use zeroize::Zeroize;

use crate::bookstack::BookStackClient;
use crate::sse::AppState;

pub const AUTH_CODE_TTL: Duration = Duration::from_secs(300); // 5 minutes
pub const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(86400); // 24 hours

pub struct AuthCode {
    pub client_id: String,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub redirect_uri: String,
    pub created_at: Instant,
}

impl Drop for AuthCode {
    fn drop(&mut self) {
        self.client_id.zeroize();
    }
}

pub struct OAuthAccessToken {
    pub token_id: String,
    pub token_secret: String,
    pub created_at: Instant,
}

impl Drop for OAuthAccessToken {
    fn drop(&mut self) {
        self.token_id.zeroize();
        self.token_secret.zeroize();
    }
}

#[derive(Deserialize)]
pub struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: String,
    code: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
}

fn derive_base_url(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

/// RFC 8414 Authorization Server Metadata (MCP 2025-03-26 spec)
pub async fn handle_metadata(headers: HeaderMap) -> Json<Value> {
    let base = derive_base_url(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
    }))
}

/// RFC 9728 Protected Resource Metadata (MCP 2025-06-18 spec)
pub async fn handle_resource_metadata(headers: HeaderMap) -> Json<Value> {
    let base = derive_base_url(&headers);
    Json(json!({
        "resource": base,
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
    }))
}

/// Authorization endpoint - generates auth code and redirects.
/// No consent UI needed: the user already "authorized" by providing their
/// BookStack API credentials as the OAuth client credentials.
pub async fn handle_authorize(
    State(state): State<AppState>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    if params.response_type != "code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_response_type",
            Some("Only response_type=code is supported"),
        );
    }

    let code = uuid::Uuid::new_v4().to_string();

    {
        let mut codes = state.auth_codes.write().await;
        if codes.len() >= 10000 {
            codes.retain(|_, c| c.created_at.elapsed() < AUTH_CODE_TTL);
        }
        codes.insert(
            code.clone(),
            AuthCode {
                client_id: params.client_id,
                code_challenge: params.code_challenge,
                code_challenge_method: params.code_challenge_method,
                redirect_uri: params.redirect_uri.clone(),
                created_at: Instant::now(),
            },
        );
    }

    let code_encoded = urlencoding::encode(&code);
    let mut redirect_url = if params.redirect_uri.contains('?') {
        format!("{}&code={code_encoded}", params.redirect_uri)
    } else {
        format!("{}?code={code_encoded}", params.redirect_uri)
    };
    if let Some(ref state_param) = params.state {
        let state_encoded = urlencoding::encode(state_param);
        redirect_url.push_str(&format!("&state={state_encoded}"));
    }

    eprintln!("OAuth: issued auth code, redirecting");
    (StatusCode::FOUND, [(header::LOCATION, redirect_url)]).into_response()
}

/// Token endpoint - exchanges authorization code for access token.
/// Validates BookStack credentials (client_id:client_secret) and PKCE.
pub async fn handle_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    if form.grant_type != "authorization_code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            Some("Only authorization_code is supported"),
        );
    }

    let code = match &form.code {
        Some(c) => c.clone(),
        None => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                Some("Missing code"),
            )
        }
    };

    let (client_id, client_secret) = match extract_client_credentials(&headers, &form) {
        Some(creds) => creds,
        None => return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", None),
    };

    // Consume auth code (single-use)
    let auth_code = {
        let mut codes = state.auth_codes.write().await;
        codes.remove(&code)
    };

    let auth_code = match auth_code {
        Some(c) if c.created_at.elapsed() < AUTH_CODE_TTL => c,
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                Some("Invalid or expired authorization code"),
            )
        }
    };

    if auth_code.client_id != client_id {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            Some("Client ID mismatch"),
        );
    }

    if let Some(ref redirect_uri) = form.redirect_uri {
        if *redirect_uri != auth_code.redirect_uri {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                Some("Redirect URI mismatch"),
            );
        }
    }

    // Verify PKCE
    if let Some(ref challenge) = auth_code.code_challenge {
        let verifier = match &form.code_verifier {
            Some(v) => v,
            None => {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    Some("Missing code_verifier"),
                )
            }
        };

        let method = auth_code
            .code_challenge_method
            .as_deref()
            .unwrap_or("S256");
        if method != "S256" {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                Some("Unsupported code_challenge_method"),
            );
        }

        if compute_s256_challenge(verifier) != *challenge {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                Some("PKCE verification failed"),
            );
        }
    }

    // Validate credentials against BookStack
    let bs_client = BookStackClient::new(&state.bookstack_url, &client_id, &client_secret);
    if let Err(e) = bs_client.validate().await {
        eprintln!("OAuth: BookStack credential validation failed: {e}");
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            Some("Invalid BookStack credentials"),
        );
    }

    // Issue access token
    let access_token = uuid::Uuid::new_v4().to_string();
    let expires_in = ACCESS_TOKEN_TTL.as_secs();

    {
        let mut tokens = state.access_tokens.write().await;
        if tokens.len() >= 10000 {
            tokens.retain(|_, t| t.created_at.elapsed() < ACCESS_TOKEN_TTL);
        }
        tokens.insert(
            access_token.clone(),
            OAuthAccessToken {
                token_id: client_id,
                token_secret: client_secret,
                created_at: Instant::now(),
            },
        );
    }

    eprintln!("OAuth: issued access token");

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({
            "access_token": access_token,
            "token_type": "bearer",
            "expires_in": expires_in,
        })),
    )
        .into_response()
}

/// Extract client credentials from form body (client_secret_post) or
/// Authorization header (client_secret_basic).
fn extract_client_credentials(
    headers: &HeaderMap,
    form: &TokenForm,
) -> Option<(String, String)> {
    // client_secret_post
    if let (Some(id), Some(secret)) = (&form.client_id, &form.client_secret) {
        if !id.is_empty() && !secret.is_empty() {
            return Some((id.clone(), secret.clone()));
        }
    }

    // client_secret_basic
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(basic) = auth.strip_prefix("Basic ") {
            if let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(basic.trim())
            {
                if let Ok(decoded_str) = String::from_utf8(decoded) {
                    if let Some((id, secret)) = decoded_str.split_once(':') {
                        return Some((id.to_string(), secret.to_string()));
                    }
                }
            }
        }
    }

    None
}

fn compute_s256_challenge(verifier: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

fn oauth_error(status: StatusCode, error: &str, description: Option<&str>) -> Response {
    let mut body = json!({"error": error});
    if let Some(desc) = description {
        body["error_description"] = json!(desc);
    }
    (status, Json(body)).into_response()
}
