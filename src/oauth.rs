use std::env;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
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
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub redirect_uri: String,
    pub client_id: String,
    /// BookStack credentials from the login form (new flow)
    pub token_id: Option<String>,
    pub token_secret: Option<String>,
    pub created_at: Instant,
}

impl Drop for AuthCode {
    fn drop(&mut self) {
        self.client_id.zeroize();
        if let Some(ref mut t) = self.token_id {
            t.zeroize();
        }
        if let Some(ref mut t) = self.token_secret {
            t.zeroize();
        }
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
pub struct AuthorizeFormSubmit {
    auth_mode: String,
    // Login mode
    email: Option<String>,
    password: Option<String>,
    // Token mode
    token_id: Option<String>,
    token_secret: Option<String>,
    // OAuth params
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn render_login_form(params: &AuthorizeParams, error: Option<&str>, mode: &str) -> String {
    let instance_name = env::var("BSMCP_INSTANCE_NAME").unwrap_or_default();
    let title = if instance_name.is_empty() {
        "BookStack MCP".to_string()
    } else {
        html_escape(&instance_name)
    };

    let error_html = if let Some(msg) = error {
        format!(r#"<div class="error">{}</div>"#, html_escape(msg))
    } else {
        String::new()
    };

    let hidden = |name: &str, value: &str| -> String {
        format!(
            r#"<input type="hidden" name="{}" value="{}">"#,
            html_escape(name),
            html_escape(value)
        )
    };

    let mut hidden_fields = vec![
        hidden("response_type", &params.response_type),
        hidden("client_id", &params.client_id),
        hidden("redirect_uri", &params.redirect_uri),
    ];
    if let Some(ref s) = params.state {
        hidden_fields.push(hidden("state", s));
    }
    if let Some(ref c) = params.code_challenge {
        hidden_fields.push(hidden("code_challenge", c));
    }
    if let Some(ref m) = params.code_challenge_method {
        hidden_fields.push(hidden("code_challenge_method", m));
    }

    let (login_display, token_display) = if mode == "token" {
        ("none", "block")
    } else {
        ("block", "none")
    };

    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in — {title}</title>
<style>
* {{ margin: 0; padding: 0; box-sizing: border-box; }}
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #1a1a2e; color: #e0e0e0; min-height: 100vh; display: flex; align-items: center; justify-content: center; }}
.card {{ background: #16213e; border-radius: 12px; padding: 2.5rem; width: 100%; max-width: 400px; box-shadow: 0 8px 32px rgba(0,0,0,0.3); }}
h1 {{ font-size: 1.4rem; margin-bottom: 0.3rem; color: #fff; }}
.subtitle {{ color: #888; font-size: 0.9rem; margin-bottom: 1.5rem; }}
.error {{ background: #3d1f1f; border: 1px solid #c0392b; color: #e74c3c; padding: 0.75rem; border-radius: 6px; margin-bottom: 1rem; font-size: 0.9rem; }}
label {{ display: block; font-size: 0.85rem; color: #aaa; margin-bottom: 0.3rem; }}
input[type="text"], input[type="password"], input[type="email"] {{ width: 100%; padding: 0.7rem; border: 1px solid #2a3a5c; border-radius: 6px; background: #0f1a30; color: #e0e0e0; font-size: 0.95rem; margin-bottom: 1rem; }}
input:focus {{ outline: none; border-color: #3498db; }}
button {{ width: 100%; padding: 0.75rem; background: #2980b9; color: #fff; border: none; border-radius: 6px; font-size: 1rem; cursor: pointer; }}
button:hover {{ background: #3498db; }}
.toggle {{ margin-top: 1rem; text-align: center; }}
.toggle a {{ color: #3498db; font-size: 0.85rem; text-decoration: none; }}
.toggle a:hover {{ text-decoration: underline; }}
.help {{ margin-top: 0.8rem; font-size: 0.8rem; color: #555; line-height: 1.4; text-align: center; }}
</style>
</head>
<body>
<div class="card">
  <h1>{title}</h1>
  <p class="subtitle" id="subtitle">Sign in to connect Claude to BookStack.</p>
  {error_html}
  <form method="POST" action="/authorize">
    <input type="hidden" name="auth_mode" id="auth_mode" value="{mode}">
    {hidden_fields}

    <div id="login-fields" style="display:{login_display}">
      <label for="email">Email</label>
      <input type="email" id="email" name="email" autocomplete="email">
      <label for="password">Password</label>
      <input type="password" id="password" name="password" autocomplete="current-password">
    </div>

    <div id="token-fields" style="display:{token_display}">
      <label for="token_id">Token ID</label>
      <input type="text" id="token_id" name="token_id" autocomplete="off">
      <label for="token_secret">Token Secret</label>
      <input type="password" id="token_secret" name="token_secret" autocomplete="off">
    </div>

    <button type="submit">Connect</button>
  </form>
  <div class="toggle">
    <a href="#" id="toggle-mode">Use API token instead</a>
  </div>
  <p class="help" id="help-text">An API token will be created automatically in your BookStack account.</p>
</div>
<script>
document.getElementById('toggle-mode').addEventListener('click', function(e) {{
  e.preventDefault();
  var lf = document.getElementById('login-fields');
  var tf = document.getElementById('token-fields');
  var am = document.getElementById('auth_mode');
  var sub = document.getElementById('subtitle');
  var help = document.getElementById('help-text');
  if (am.value === 'login') {{
    lf.style.display = 'none';
    tf.style.display = 'block';
    am.value = 'token';
    this.textContent = 'Use BookStack login instead';
    sub.textContent = 'Enter your BookStack API token to connect.';
    help.textContent = 'Create an API token in BookStack under My Account > API Tokens.';
    document.getElementById('email').removeAttribute('required');
    document.getElementById('password').removeAttribute('required');
  }} else {{
    lf.style.display = 'block';
    tf.style.display = 'none';
    am.value = 'login';
    this.textContent = 'Use API token instead';
    sub.textContent = 'Sign in to connect Claude to BookStack.';
    help.textContent = 'An API token will be created automatically in your BookStack account.';
    document.getElementById('token_id').removeAttribute('required');
    document.getElementById('token_secret').removeAttribute('required');
  }}
}});
</script>
</body>
</html>"##,
        title = title,
        error_html = error_html,
        mode = mode,
        login_display = login_display,
        token_display = token_display,
        hidden_fields = hidden_fields.join("\n    "),
    )
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
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
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

/// Authorization endpoint GET - serves the login form.
pub async fn handle_authorize(Query(params): Query<AuthorizeParams>) -> Response {
    if params.response_type != "code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_response_type",
            Some("Only response_type=code is supported"),
        );
    }
    Html(render_login_form(&params, None, "login")).into_response()
}

/// Authorization endpoint POST - validates BookStack credentials and redirects with auth code.
/// Supports two modes:
/// - "login": email/password → scrapes BookStack web UI to create an API token
/// - "token": direct API token entry
pub async fn handle_authorize_submit(
    State(state): State<AppState>,
    Form(form): Form<AuthorizeFormSubmit>,
) -> Response {
    if form.response_type != "code" {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_response_type",
            Some("Only response_type=code is supported"),
        );
    }

    let params = AuthorizeParams {
        response_type: form.response_type.clone(),
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        state: form.state.clone(),
        code_challenge: form.code_challenge.clone(),
        code_challenge_method: form.code_challenge_method.clone(),
    };

    let (token_id, token_secret) = match form.auth_mode.as_str() {
        "login" => {
            let email = match &form.email {
                Some(e) if !e.is_empty() => e.as_str(),
                _ => return Html(render_login_form(&params, Some("Email is required."), "login")).into_response(),
            };
            let password = match &form.password {
                Some(p) if !p.is_empty() => p.as_str(),
                _ => return Html(render_login_form(&params, Some("Password is required."), "login")).into_response(),
            };

            match crate::web_auth::login_and_create_token(&state.bookstack_url, email, password).await {
                Ok(creds) => creds,
                Err(e) => {
                    eprintln!("OAuth: web login failed: {e}");
                    return Html(render_login_form(&params, Some(&e), "login")).into_response();
                }
            }
        }
        _ => {
            // "token" mode or fallback
            let tid = match &form.token_id {
                Some(t) if !t.is_empty() => t.clone(),
                _ => return Html(render_login_form(&params, Some("Token ID is required."), "token")).into_response(),
            };
            let tsec = match &form.token_secret {
                Some(t) if !t.is_empty() => t.clone(),
                _ => return Html(render_login_form(&params, Some("Token Secret is required."), "token")).into_response(),
            };

            // Validate against BookStack API
            let bs_client = BookStackClient::new(&state.bookstack_url, &tid, &tsec);
            if let Err(e) = bs_client.validate().await {
                eprintln!("OAuth: token validation failed: {e}");
                return Html(render_login_form(&params, Some("Invalid API token. Check your Token ID and Secret."), "token")).into_response();
            }

            (tid, tsec)
        }
    };

    let code = uuid::Uuid::new_v4().to_string();

    {
        let mut codes = state.auth_codes.write().await;
        if codes.len() >= 10000 {
            codes.retain(|_, c| c.created_at.elapsed() < AUTH_CODE_TTL);
        }
        codes.insert(
            code.clone(),
            AuthCode {
                client_id: form.client_id,
                code_challenge: form.code_challenge,
                code_challenge_method: form.code_challenge_method,
                redirect_uri: form.redirect_uri.clone(),
                token_id: Some(token_id),
                token_secret: Some(token_secret),
                created_at: Instant::now(),
            },
        );
    }

    let code_encoded = urlencoding::encode(&code);
    let mut redirect_url = if form.redirect_uri.contains('?') {
        format!("{}&code={code_encoded}", form.redirect_uri)
    } else {
        format!("{}?code={code_encoded}", form.redirect_uri)
    };
    if let Some(ref state_param) = form.state {
        let state_encoded = urlencoding::encode(state_param);
        redirect_url.push_str(&format!("&state={state_encoded}"));
    }

    eprintln!("OAuth: authenticated via {}, issued auth code", form.auth_mode);
    (StatusCode::FOUND, [(header::LOCATION, redirect_url)]).into_response()
}

/// Token endpoint - exchanges authorization code for access token.
/// Supports both form-based auth (credentials stored in auth code) and
/// legacy client_secret auth (credentials in token request).
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

    // Verify redirect_uri matches
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

    // Resolve BookStack credentials: prefer form-stored creds, fall back to client credentials
    let (token_id, token_secret) = if let (Some(tid), Some(tsec)) =
        (auth_code.token_id.clone(), auth_code.token_secret.clone())
    {
        // New flow: credentials came from the login form, already validated
        eprintln!("OAuth: using form-authenticated credentials");
        (tid, tsec)
    } else {
        // Legacy flow: credentials come from client_id/client_secret
        let (client_id, client_secret) = match extract_client_credentials(&headers, &form) {
            Some(creds) => creds,
            None => {
                return oauth_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid_client",
                    Some("No credentials available"),
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

        // Validate against BookStack
        let bs_client =
            BookStackClient::new(&state.bookstack_url, &client_id, &client_secret);
        if let Err(e) = bs_client.validate().await {
            eprintln!("OAuth: BookStack credential validation failed: {e}");
            return oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                Some("Invalid BookStack credentials"),
            );
        }

        eprintln!("OAuth: using legacy client credential flow");
        (client_id, client_secret)
    };

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
                token_id,
                token_secret,
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
