//! Lightweight LLM client for instance summary generation.
//! Supports OpenRouter, Anthropic, OpenAI, and Ollama.

use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub enum LlmProvider {
    OpenRouter,
    Anthropic,
    OpenAI,
    Ollama,
}

#[derive(Clone)]
pub struct LlmClient {
    provider: LlmProvider,
    api_key: String,
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl LlmClient {
    pub fn from_env() -> Option<Self> {
        let provider = match std::env::var("BSMCP_LLM_PROVIDER")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "anthropic" => LlmProvider::Anthropic,
            "openai" => LlmProvider::OpenAI,
            "ollama" => LlmProvider::Ollama,
            "openrouter" => LlmProvider::OpenRouter,
            _ => return None, // No provider configured
        };

        // API key required for cloud providers, optional for Ollama
        let api_key = std::env::var("BSMCP_LLM_API_KEY").unwrap_or_default();
        if api_key.is_empty() && !matches!(provider, LlmProvider::Ollama) {
            return None;
        }

        let default_model = match provider {
            LlmProvider::Anthropic => "claude-sonnet-4-20250514".to_string(),
            LlmProvider::OpenRouter => "anthropic/claude-sonnet-4".to_string(),
            LlmProvider::OpenAI => "gpt-4o".to_string(),
            LlmProvider::Ollama => "llama3.2".to_string(),
        };
        let model = std::env::var("BSMCP_LLM_MODEL").unwrap_or(default_model);

        let default_url = match provider {
            LlmProvider::OpenRouter => "https://openrouter.ai".to_string(),
            LlmProvider::OpenAI => "https://api.openai.com".to_string(),
            LlmProvider::Anthropic => "https://api.anthropic.com".to_string(),
            LlmProvider::Ollama => "http://localhost:11434".to_string(),
        };
        let base_url = std::env::var("BSMCP_LLM_API_URL").unwrap_or(default_url);

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .ok()?;

        Some(Self {
            provider,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            http,
        })
    }

    pub fn provider(&self) -> &LlmProvider {
        &self.provider
    }

    /// Send a single user message and get the assistant's text response.
    pub async fn complete(&self, system: &str, user_message: &str) -> Result<String, String> {
        match self.provider {
            LlmProvider::OpenRouter | LlmProvider::OpenAI => self.complete_openai_compat(
                &format!("{}/v1/chat/completions", self.base_url),
                system,
                user_message,
            ).await,
            LlmProvider::Ollama => self.complete_openai_compat(
                &format!("{}/v1/chat/completions", self.base_url),
                system,
                user_message,
            ).await,
            LlmProvider::Anthropic => self.complete_anthropic(system, user_message).await,
        }
    }

    async fn complete_openai_compat(
        &self,
        url: &str,
        system: &str,
        user_message: &str,
    ) -> Result<String, String> {
        let body = json!({
            "model": self.model,
            "max_tokens": 2048,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user_message },
            ]
        });

        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("LLM request failed: {e}"))?;

        let status = resp.status();
        let json: Value = resp
            .json()
            .await
            .map_err(|e| format!("LLM response parse failed: {e}"))?;

        if !status.is_success() {
            return Err(format!(
                "LLM error {status}: {}",
                json.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
            ));
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "No content in LLM response".to_string())
    }

    async fn complete_anthropic(&self, system: &str, user_message: &str) -> Result<String, String> {
        let body = json!({
            "model": self.model,
            "max_tokens": 2048,
            "system": system,
            "messages": [
                { "role": "user", "content": user_message },
            ]
        });

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Anthropic request failed: {e}"))?;

        let status = resp.status();
        let json: Value = resp
            .json()
            .await
            .map_err(|e| format!("Anthropic response parse failed: {e}"))?;

        if !status.is_success() {
            return Err(format!(
                "Anthropic error {status}: {}",
                json.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
            ));
        }

        // Anthropic returns content as array of blocks
        json["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"] == "text")
                    .and_then(|b| b["text"].as_str())
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| "No text content in Anthropic response".to_string())
    }
}
