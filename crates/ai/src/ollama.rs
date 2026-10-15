//! Ollama: a local model server (`/api/chat`) for the cheap first pass over every news item.
//!
//! Free and private. `format` carries the JSON schema so the answer is structured;
//! temperature 0 keeps readings repeatable.

use std::time::Duration;

use async_trait::async_trait;
use quorum_core::PortError;
use quorum_core::ports::{LlmProvider, LlmRequest, LlmResponse};
use serde::Deserialize;
use serde_json::json;

pub const DEFAULT_URL: &str = "http://127.0.0.1:11434";
const TIMEOUT: Duration = Duration::from_mins(3);

#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    #[error("request to {url} failed: {source} (is Ollama running? `ollama serve`)")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("Ollama answered {status}: {message}")]
    Api { status: u16, message: String },
    #[error("the answer was cut off (done_reason: length)")]
    Truncated,
    #[error("unexpected response: {0}")]
    Shape(String),
}

#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[derive(Debug, Deserialize)]
struct Chat {
    message: ChatMessage,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: u64,
    #[serde(default)]
    eval_count: u64,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

impl OllamaProvider {
    pub fn new(base_url: &str, model: &str) -> Result<Self, OllamaError> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|source| OllamaError::Http {
                url: base_url.to_owned(),
                source,
            })?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
        })
    }

    async fn chat(&self, request: &LlmRequest) -> Result<LlmResponse, OllamaError> {
        let url = format!("{}/api/chat", self.base_url);
        let body = json!({
            "model": self.model,
            "stream": false,
            "format": request.schema,
            "options": { "temperature": 0, "num_predict": request.max_tokens },
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user }
            ]
        });
        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|source| OllamaError::Http {
                url: url.clone(),
                source,
            })?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(OllamaError::Api {
                status: status.as_u16(),
                message: text.chars().take(300).collect(),
            });
        }
        let chat: Chat = response
            .json()
            .await
            .map_err(|e| OllamaError::Shape(e.to_string()))?;
        if chat.done_reason.as_deref() == Some("length") {
            return Err(OllamaError::Truncated);
        }
        Ok(LlmResponse {
            text: chat.message.content,
            input_tokens: chat.prompt_eval_count,
            output_tokens: chat.eval_count,
        })
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, PortError> {
        self.chat(request)
            .await
            .map_err(|e| PortError::adapter("ollama", e))
    }
}
