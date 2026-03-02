use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use zeroize::Zeroize;

// --- Type-safe enums for URL path parameters (defense-in-depth) ---

pub enum ExportFormat {
    Markdown,
    Plaintext,
    Html,
}

impl ExportFormat {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "markdown" => Ok(Self::Markdown),
            "plaintext" => Ok(Self::Plaintext),
            "html" => Ok(Self::Html),
            _ => Err(format!("Invalid export format: '{s}'. Must be one of: markdown, plaintext, html")),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Markdown => "markdown",
            Self::Plaintext => "plaintext",
            Self::Html => "html",
        }
    }
}

pub enum ContentType {
    Page,
    Chapter,
    Book,
    Shelf,
}

impl ContentType {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "page" => Ok(Self::Page),
            "chapter" => Ok(Self::Chapter),
            "book" => Ok(Self::Book),
            "shelf" => Ok(Self::Shelf),
            _ => Err(format!("Invalid content type: '{s}'. Must be one of: page, chapter, book, shelf")),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Page => "page",
            Self::Chapter => "chapter",
            Self::Book => "book",
            Self::Shelf => "shelf",
        }
    }
}

const MAX_RESPONSE_SIZE: usize = 50 * 1024 * 1024; // 50MB
const MAX_ERROR_BODY_SIZE: usize = 4096; // 4KB for error messages

/// Note: Zeroize on Drop clears the current String allocation. Intermediate copies
/// (e.g. from Clone, format!) may remain in freed memory until overwritten by the allocator.
/// This is a best-effort defense-in-depth measure, not a guarantee against memory forensics.
#[derive(Clone)]
pub struct BookStackClient {
    client: Client,
    base_url: String,
    token_id: String,
    token_secret: String,
}

impl Drop for BookStackClient {
    fn drop(&mut self) {
        self.token_id.zeroize();
        self.token_secret.zeroize();
    }
}

impl BookStackClient {
    pub fn new(base_url: &str, token_id: &str, token_secret: &str) -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build()
                .expect("Failed to build HTTP client"),
            base_url: base_url.trim_end_matches('/').to_string(),
            token_id: token_id.to_string(),
            token_secret: token_secret.to_string(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Token {}:{}", self.token_id, self.token_secret)
    }

    /// Fast-reject via Content-Length header before downloading the body.
    fn check_content_length(resp: &reqwest::Response, limit: usize) -> Result<(), String> {
        if let Some(len) = resp.content_length() {
            if len as usize > limit {
                return Err(format!("Response too large: {len} bytes"));
            }
        }
        Ok(())
    }

    /// Read response as JSON, enforcing size limit even for chunked responses.
    async fn read_json(resp: reqwest::Response) -> Result<Value, String> {
        Self::check_content_length(&resp, MAX_RESPONSE_SIZE)?;
        let bytes = resp.bytes().await
            .map_err(|e| { eprintln!("Response read error: {e}"); "Failed to read response".to_string() })?;
        if bytes.len() > MAX_RESPONSE_SIZE {
            return Err(format!("Response too large: {} bytes", bytes.len()));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| { eprintln!("JSON parse error: {e}"); "Invalid response from BookStack".to_string() })
    }

    /// Read response as text, enforcing size limit even for chunked responses.
    async fn read_text(resp: reqwest::Response) -> Result<String, String> {
        Self::check_content_length(&resp, MAX_RESPONSE_SIZE)?;
        let bytes = resp.bytes().await
            .map_err(|e| { eprintln!("Response read error: {e}"); "Failed to read response".to_string() })?;
        if bytes.len() > MAX_RESPONSE_SIZE {
            return Err(format!("Response too large: {} bytes", bytes.len()));
        }
        String::from_utf8(bytes.to_vec())
            .map_err(|e| { eprintln!("UTF-8 decode error: {e}"); "Invalid response encoding".to_string() })
    }

    /// Read error body with a size limit to prevent memory exhaustion from error responses.
    /// Streams chunks to avoid buffering arbitrarily large error responses.
    async fn read_error_body(mut resp: reqwest::Response) -> String {
        // Fast-reject if Content-Length exceeds limit
        if resp.content_length().map_or(false, |len| len as usize > MAX_ERROR_BODY_SIZE) {
            return "[error body too large]".to_string();
        }
        let mut buf = Vec::with_capacity(MAX_ERROR_BODY_SIZE.min(4096));
        while buf.len() < MAX_ERROR_BODY_SIZE {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = MAX_ERROR_BODY_SIZE - buf.len();
                    buf.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                _ => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let resp = self.client
            .get(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .query(query)
            .send()
            .await
            .map_err(|e| { eprintln!("BookStack request error: {e}"); "Request failed".to_string() })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::read_error_body(resp).await;
            eprintln!("BookStack API error {status}: {body}");
            return Err(format!("BookStack API error: {status}"));
        }

        Self::read_json(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let resp = self.client
            .post(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .await
            .map_err(|e| { eprintln!("BookStack request error: {e}"); "Request failed".to_string() })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::read_error_body(resp).await;
            eprintln!("BookStack API error {status}: {body}");
            return Err(format!("BookStack API error: {status}"));
        }

        Self::read_json(resp).await
    }

    async fn put(&self, path: &str, body: &Value) -> Result<Value, String> {
        let resp = self.client
            .put(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .await
            .map_err(|e| { eprintln!("BookStack request error: {e}"); "Request failed".to_string() })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::read_error_body(resp).await;
            eprintln!("BookStack API error {status}: {body}");
            return Err(format!("BookStack API error: {status}"));
        }

        Self::read_json(resp).await
    }

    async fn get_text(&self, path: &str) -> Result<String, String> {
        let resp = self.client
            .get(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| { eprintln!("BookStack request error: {e}"); "Request failed".to_string() })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::read_error_body(resp).await;
            eprintln!("BookStack API error {status}: {body}");
            return Err(format!("BookStack API error: {status}"));
        }

        Self::read_text(resp).await
    }

    async fn delete(&self, path: &str) -> Result<(), String> {
        let resp = self.client
            .delete(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| { eprintln!("BookStack request error: {e}"); "Request failed".to_string() })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = Self::read_error_body(resp).await;
            eprintln!("BookStack API error {status}: {body}");
            return Err(format!("BookStack API error: {status}"));
        }

        Ok(())
    }

    // --- Validation ---

    pub async fn validate(&self) -> Result<Value, String> {
        self.get("books", &[("count", "1")]).await
    }

    // --- Shelves ---

    pub async fn list_shelves(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("shelves", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn get_shelf(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("shelves/{id}"), &[]).await
    }

    pub async fn create_shelf(&self, name: &str, description: &str) -> Result<Value, String> {
        self.post("shelves", &serde_json::json!({
            "name": name, "description": description,
        })).await
    }

    pub async fn update_shelf(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("shelves/{id}"), data).await
    }

    pub async fn delete_shelf(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("shelves/{id}")).await
    }

    // --- Books ---

    pub async fn list_books(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("books", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn get_book(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("books/{id}"), &[]).await
    }

    pub async fn create_book(&self, name: &str, description: &str) -> Result<Value, String> {
        self.post("books", &serde_json::json!({
            "name": name, "description": description,
        })).await
    }

    pub async fn update_book(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("books/{id}"), data).await
    }

    pub async fn delete_book(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("books/{id}")).await
    }

    // --- Chapters ---

    pub async fn list_chapters(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("chapters", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn get_chapter(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("chapters/{id}"), &[]).await
    }

    pub async fn create_chapter(&self, book_id: i64, name: &str, description: &str) -> Result<Value, String> {
        self.post("chapters", &serde_json::json!({
            "book_id": book_id, "name": name, "description": description,
        })).await
    }

    pub async fn update_chapter(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("chapters/{id}"), data).await
    }

    pub async fn delete_chapter(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("chapters/{id}")).await
    }

    // --- Pages ---

    pub async fn list_pages(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("pages", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn get_page(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("pages/{id}"), &[]).await
    }

    pub async fn create_page(&self, data: &Value) -> Result<Value, String> {
        self.post("pages", data).await
    }

    pub async fn update_page(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("pages/{id}"), data).await
    }

    pub async fn delete_page(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("pages/{id}")).await
    }

    // --- Search ---

    pub async fn search(&self, query: &str, page: i64, count: i64) -> Result<Value, String> {
        self.get("search", &[
            ("query", query),
            ("page", &page.to_string()),
            ("count", &count.to_string()),
        ]).await
    }

    // --- Attachments ---

    pub async fn list_attachments(&self) -> Result<Value, String> {
        self.get("attachments", &[]).await
    }

    pub async fn get_attachment(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("attachments/{id}"), &[]).await
    }

    pub async fn create_attachment(&self, data: &Value) -> Result<Value, String> {
        self.post("attachments", data).await
    }

    pub async fn update_attachment(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("attachments/{id}"), data).await
    }

    pub async fn delete_attachment(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("attachments/{id}")).await
    }

    // --- Exports ---

    pub async fn export_page(&self, id: i64, format: ExportFormat) -> Result<String, String> {
        let fmt = format.as_str();
        self.get_text(&format!("pages/{id}/export/{fmt}")).await
    }

    pub async fn export_chapter(&self, id: i64, format: ExportFormat) -> Result<String, String> {
        let fmt = format.as_str();
        self.get_text(&format!("chapters/{id}/export/{fmt}")).await
    }

    pub async fn export_book(&self, id: i64, format: ExportFormat) -> Result<String, String> {
        let fmt = format.as_str();
        self.get_text(&format!("books/{id}/export/{fmt}")).await
    }

    // --- Comments ---

    pub async fn list_comments(&self, query: &[(&str, &str)]) -> Result<Value, String> {
        self.get("comments", query).await
    }

    pub async fn get_comment(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("comments/{id}"), &[]).await
    }

    pub async fn create_comment(&self, data: &Value) -> Result<Value, String> {
        self.post("comments", data).await
    }

    pub async fn update_comment(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("comments/{id}"), data).await
    }

    pub async fn delete_comment(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("comments/{id}")).await
    }

    // --- Recycle Bin ---

    pub async fn list_recycle_bin(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("recycle-bin", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn restore_recycle_bin_item(&self, id: i64) -> Result<Value, String> {
        self.put(&format!("recycle-bin/{id}"), &serde_json::json!({})).await
    }

    pub async fn destroy_recycle_bin_item(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("recycle-bin/{id}")).await
    }

    // --- Users ---

    pub async fn list_users(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("users", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn get_user(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("users/{id}"), &[]).await
    }

    // --- Audit Log ---

    pub async fn list_audit_log(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("audit-log", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    // --- System ---

    pub async fn get_system_info(&self) -> Result<Value, String> {
        self.get("system", &[]).await
    }

    // --- Image Gallery ---

    pub async fn list_images(&self, count: i64, offset: i64, filter: &[(&str, &str)]) -> Result<Value, String> {
        let mut query: Vec<(&str, &str)> = vec![];
        let count_str = count.to_string();
        let offset_str = offset.to_string();
        query.push(("count", &count_str));
        query.push(("offset", &offset_str));
        query.extend_from_slice(filter);
        self.get("image-gallery", &query).await
    }

    pub async fn get_image(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("image-gallery/{id}"), &[]).await
    }

    pub async fn update_image(&self, id: i64, data: &Value) -> Result<Value, String> {
        self.put(&format!("image-gallery/{id}"), data).await
    }

    pub async fn delete_image(&self, id: i64) -> Result<(), String> {
        self.delete(&format!("image-gallery/{id}")).await
    }

    // --- Content Permissions ---

    pub async fn get_content_permissions(&self, content_type: ContentType, content_id: i64) -> Result<Value, String> {
        let ct = content_type.as_str();
        self.get(&format!("content-permissions/{ct}/{content_id}"), &[]).await
    }

    pub async fn update_content_permissions(&self, content_type: ContentType, content_id: i64, data: &Value) -> Result<Value, String> {
        let ct = content_type.as_str();
        self.put(&format!("content-permissions/{ct}/{content_id}"), data).await
    }

    // --- Roles ---

    pub async fn list_roles(&self, count: i64, offset: i64) -> Result<Value, String> {
        self.get("roles", &[
            ("count", &count.to_string()),
            ("offset", &offset.to_string()),
        ]).await
    }

    pub async fn get_role(&self, id: i64) -> Result<Value, String> {
        self.get(&format!("roles/{id}"), &[]).await
    }
}
