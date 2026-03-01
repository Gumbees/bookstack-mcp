use reqwest::Client;
use serde_json::Value;

pub struct BookStackClient {
    client: Client,
    base_url: String,
    token_id: String,
    token_secret: String,
}

impl BookStackClient {
    pub fn new(base_url: &str, token_id: &str, token_secret: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token_id: token_id.to_string(),
            token_secret: token_secret.to_string(),
        }
    }

    fn auth_header(&self) -> String {
        format!("Token {}:{}", self.token_id, self.token_secret)
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let resp = self.client
            .get(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .query(query)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("BookStack API error: {}", resp.status()));
        }

        resp.json().await.map_err(|e| e.to_string())
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let resp = self.client
            .post(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("BookStack API error: {}", resp.status()));
        }

        resp.json().await.map_err(|e| e.to_string())
    }

    async fn put(&self, path: &str, body: &Value) -> Result<Value, String> {
        let resp = self.client
            .put(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("BookStack API error: {}", resp.status()));
        }

        resp.json().await.map_err(|e| e.to_string())
    }

    async fn delete(&self, path: &str) -> Result<(), String> {
        let resp = self.client
            .delete(format!("{}/api/{}", self.base_url, path))
            .header("Authorization", self.auth_header())
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("BookStack API error: {}", resp.status()));
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
}
