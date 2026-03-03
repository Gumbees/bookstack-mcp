use std::sync::Arc;

use reqwest::cookie::Jar;

/// Log into BookStack via the web UI and create an API token.
/// Returns (token_id, token_secret) on success.
pub async fn login_and_create_token(
    bookstack_url: &str,
    email: &str,
    password: &str,
) -> Result<(String, String), String> {
    let base = bookstack_url.trim_end_matches('/');
    let jar = Arc::new(Jar::default());
    let client = reqwest::Client::builder()
        .cookie_provider(jar)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    // Step 1: GET /login to get CSRF token
    let login_page = client
        .get(format!("{base}/login"))
        .send()
        .await
        .map_err(|e| format!("Failed to reach BookStack: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Failed to read login page: {e}"))?;

    let csrf = extract_csrf(&login_page)
        .ok_or("Could not find CSRF token on login page. Is this a BookStack instance?")?;

    // Step 2: POST /login with credentials
    let login_resp = client
        .post(format!("{base}/login"))
        .form(&[
            ("email", email),
            ("password", password),
            ("_token", csrf.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Login request failed: {e}"))?;

    // After following redirects, check if we ended up back on /login (= failure)
    let final_url = login_resp.url().to_string();
    let body = login_resp
        .text()
        .await
        .map_err(|e| format!("Failed to read login response: {e}"))?;

    if final_url.contains("/login") {
        // Try to extract a specific error message from the page
        if body.contains("These credentials do not match") {
            return Err("Invalid email or password.".into());
        }
        return Err("Login failed. Check your email and password.".into());
    }

    eprintln!("Web auth: logged into BookStack");

    // Step 3: GET /my-account/auth to find user ID from create-token link
    let auth_page = client
        .get(format!("{base}/my-account/auth"))
        .send()
        .await
        .map_err(|e| format!("Failed to load account auth page: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Failed to read account page: {e}"))?;

    let user_id = extract_user_id(&auth_page).ok_or(
        "Could not determine your user ID. You may not have permission to manage API tokens.",
    )?;

    eprintln!("Web auth: found user ID {user_id}");

    // Step 4: GET the create-token page for a fresh CSRF token
    let create_page = client
        .get(format!("{base}/api-tokens/{user_id}/create"))
        .send()
        .await
        .map_err(|e| format!("Failed to load token creation page: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Failed to read token creation page: {e}"))?;

    let csrf2 = extract_csrf(&create_page).ok_or(
        "Could not find CSRF token on token creation page.",
    )?;

    // Step 5: POST to create the API token
    let create_resp = client
        .post(format!("{base}/api-tokens/{user_id}/create"))
        .form(&[
            ("name", "Claude MCP (auto)"),
            ("expires_at", ""),
            ("_token", csrf2.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Token creation request failed: {e}"))?;

    let token_page = create_resp
        .text()
        .await
        .map_err(|e| format!("Failed to read token creation response: {e}"))?;

    // Step 6: Parse token_id and token_secret from the response
    let token_id = extract_token_id(&token_page)
        .ok_or("Created token but could not parse the Token ID from the response.")?;

    let token_secret = extract_token_secret(&token_page)
        .ok_or("Created token but could not parse the Token Secret from the response.")?;

    eprintln!("Web auth: created API token {token_id}");
    Ok((token_id, token_secret))
}

/// Extract CSRF token from BookStack HTML.
/// Looks for `<meta name="token" content="...">` or `<input name="_token" value="...">`.
fn extract_csrf(html: &str) -> Option<String> {
    // <meta name="token" content="VALUE">
    if let Some(val) = extract_attr_value(html, r#"name="token""#, "content") {
        if !val.is_empty() {
            return Some(val);
        }
    }

    // <input ... name="_token" ... value="VALUE">
    if let Some(val) = extract_input_value(html, "_token") {
        if !val.is_empty() {
            return Some(val);
        }
    }

    None
}

/// Extract user ID from /my-account/auth page by finding `/api-tokens/{userId}/create` link.
fn extract_user_id(html: &str) -> Option<String> {
    // Look for href="/api-tokens/{digits}/create"
    let pattern = "/api-tokens/";
    let mut pos = 0;
    while let Some(idx) = html[pos..].find(pattern) {
        let start = pos + idx + pattern.len();
        if let Some(end) = html[start..].find(|c: char| !c.is_ascii_digit()) {
            let id = &html[start..start + end];
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        pos = start;
    }

    // Fallback: look for /settings/users/{digits} pattern
    let pattern2 = "/settings/users/";
    pos = 0;
    while let Some(idx) = html[pos..].find(pattern2) {
        let start = pos + idx + pattern2.len();
        if let Some(end) = html[start..].find(|c: char| !c.is_ascii_digit()) {
            let id = &html[start..start + end];
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        pos = start;
    }

    None
}

/// Extract the token_id from the token edit page.
/// Looks for `<input name="token_id" ... value="...">`.
fn extract_token_id(html: &str) -> Option<String> {
    extract_input_value(html, "token_id")
}

/// Extract the token secret from the token edit page.
/// The secret is in a readonly input near "Token Secret" text, with no name attribute.
fn extract_token_secret(html: &str) -> Option<String> {
    // Find "Token Secret" or "token-secret" in the page
    let secret_markers = ["Token Secret", "token-secret", "api-token-secret"];

    for marker in &secret_markers {
        if let Some(marker_pos) = html.find(marker) {
            // Search forward from the marker for a readonly input with a value
            let after = &html[marker_pos..];
            // Look within a reasonable window (2000 chars)
            let window = &after[..after.len().min(2000)];

            // Find <input ... readonly ... value="VALUE">
            // or value="VALUE" ... readonly
            if let Some(val) = extract_readonly_input_value(window) {
                if !val.is_empty() && val.len() >= 20 {
                    return Some(val);
                }
            }
        }
    }

    // Fallback: find any readonly text input whose value looks like a 32-char token
    // that isn't the token_id (which has name="token_id")
    let token_id = extract_token_id(html);
    let mut pos = 0;
    while let Some(idx) = html[pos..].find("readonly") {
        let search_start = if pos + idx > 500 { pos + idx - 500 } else { 0 };
        let search_end = (pos + idx + 500).min(html.len());
        let window = &html[search_start..search_end];

        if let Some(val) = find_value_in_tag(window) {
            // Must be 20+ chars and not the token_id
            if val.len() >= 20 {
                if let Some(ref tid) = token_id {
                    if val != *tid {
                        return Some(val);
                    }
                } else {
                    return Some(val);
                }
            }
        }
        pos = pos + idx + 8;
    }

    None
}

/// Extract the value of a named input field: `<input name="NAME" value="VALUE">`.
fn extract_input_value(html: &str, name: &str) -> Option<String> {
    let name_attr = format!(r#"name="{name}""#);

    let mut pos = 0;
    while let Some(idx) = html[pos..].find(&name_attr) {
        let abs = pos + idx;
        // Find the enclosing <input ...> tag
        let tag_start = html[..abs].rfind('<')?;
        let tag_end_offset = html[tag_start..].find('>')?;
        let tag = &html[tag_start..tag_start + tag_end_offset + 1];

        if let Some(val) = find_value_in_tag(tag) {
            return Some(val);
        }
        pos = abs + name_attr.len();
    }

    None
}

/// Find `value="..."` within an HTML tag string.
fn find_value_in_tag(tag: &str) -> Option<String> {
    let marker = r#"value=""#;
    if let Some(vpos) = tag.find(marker) {
        let start = vpos + marker.len();
        if let Some(end) = tag[start..].find('"') {
            let val = &tag[start..start + end];
            return Some(val.to_string());
        }
    }
    None
}

/// Find the value of a readonly input element in an HTML snippet.
fn extract_readonly_input_value(html: &str) -> Option<String> {
    // Find <input tags that contain "readonly"
    let mut pos = 0;
    while let Some(idx) = html[pos..].find("<input") {
        let tag_start = pos + idx;
        if let Some(tag_end_offset) = html[tag_start..].find('>') {
            let tag = &html[tag_start..tag_start + tag_end_offset + 1];
            if tag.contains("readonly") {
                if let Some(val) = find_value_in_tag(tag) {
                    return Some(val);
                }
            }
        }
        pos = tag_start + 6;
    }
    None
}

/// Extract an attribute value from an element that contains a specific marker.
/// e.g., extract_attr_value(html, `name="token"`, "content") finds the content attr
/// of the element containing `name="token"`.
fn extract_attr_value(html: &str, marker: &str, attr: &str) -> Option<String> {
    let pos = html.find(marker)?;
    // Find the enclosing tag
    let tag_start = html[..pos].rfind('<')?;
    let tag_end_offset = html[tag_start..].find('>')?;
    let tag = &html[tag_start..tag_start + tag_end_offset + 1];

    let attr_marker = format!(r#"{attr}=""#);
    let attr_pos = tag.find(&attr_marker)?;
    let start = attr_pos + attr_marker.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}
