//! Claude via the Messages API (raw HTTP: there is no official Rust SDK).
//!
//! One request per item: the frozen system prompt (cache-marked), the untrusted item in the
//! user turn, and structured outputs (`output_config.format` with a JSON schema), so the answer
//! is JSON in the schema's shape. Thinking stays adaptive (it cannot be turned off on current
//! Opus models); `output_config.effort` keeps it short for this simple extraction. With
//! `fallbacks` on, a request the model's safety classifiers decline is re-run server-side on
//! Anthropic's recommended fallback model instead of returning a refusal.
//!
//! Retries: 429 and 5xx (incl. 529 overloaded) and connection errors, honouring `retry-after`;
//! 4xx otherwise are returned at once. A refusal or a truncated answer is an error.

use std::time::Duration;

use async_trait::async_trait;
use quorum_core::PortError;
use quorum_core::chat::{ChatBlock, ChatReply, ChatRequest, ChatRole, StopReason};
use quorum_core::ports::{ChatModel, LlmProvider, LlmRequest, LlmResponse};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

pub const DEFAULT_URL: &str = "https://api.anthropic.com";
const VERSION: &str = "2023-06-01";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const TIMEOUT: Duration = Duration::from_mins(2);
const ATTEMPTS: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("request failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("Claude API answered {status}: {message}")]
    Api { status: u16, message: String },
    #[error("the model declined the request (refusal: {})", .0.as_deref().unwrap_or("no category"))]
    Refusal(Option<String>),
    #[error("the answer was cut off at max_tokens")]
    Truncated,
    #[error("unexpected response: {0}")]
    Shape(String),
}

/// Claude Messages API client for structured extraction.
#[derive(Clone)]
pub struct ClaudeProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    effort: String,
    fallbacks: bool,
}

// The key must never reach logs or Debug output.
impl std::fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("fallbacks", &self.fallbacks)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
struct Message {
    content: Vec<Block>,
    stop_reason: Option<String>,
    stop_details: Option<StopDetails>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StopDetails {
    category: Option<String>,
}

#[derive(Debug, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "field names mirror the Claude API usage object"
)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl ClaudeProvider {
    pub fn new(
        api_key: String,
        model: &str,
        effort: &str,
        fallbacks: bool,
    ) -> Result<Self, ClaudeError> {
        Self::with_base_url(DEFAULT_URL, api_key, model, effort, fallbacks)
    }

    /// Against another endpoint (a test server or a proxy).
    pub fn with_base_url(
        base_url: &str,
        api_key: String,
        model: &str,
        effort: &str,
        fallbacks: bool,
    ) -> Result<Self, ClaudeError> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(ClaudeError::Http)?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            model: model.to_owned(),
            effort: effort.to_owned(),
            fallbacks,
        })
    }

    fn body(&self, request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            "system": [{
                "type": "text",
                "text": request.system,
                "cache_control": { "type": "ephemeral" }
            }],
            "messages": [{ "role": "user", "content": request.user }],
            "output_config": {
                "effort": self.effort,
                "format": { "type": "json_schema", "schema": request.schema }
            }
        });
        if self.fallbacks
            && let Some(object) = body.as_object_mut()
        {
            object.insert("fallbacks".into(), json!("default"));
        }
        body
    }

    /// The request body of one tool-using step (AG-3b).
    fn chat_body(&self, request: &ChatRequest) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                json!({
                    "role": match m.role { ChatRole::User => "user", ChatRole::Assistant => "assistant" },
                    "content": m.content.iter().map(block_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
            .collect();
        let mut body = json!({
            "model": self.model,
            "max_tokens": request.max_tokens,
            // Tools and system come first in the prompt: marking the system block caches both.
            "system": [{ "type": "text", "text": request.system, "cache_control": { "type": "ephemeral" } }],
            "messages": messages,
            "tools": tools,
            "output_config": { "effort": self.effort },
        });
        if self.fallbacks
            && let Some(object) = body.as_object_mut()
        {
            object.insert("fallbacks".into(), json!("default"));
        }
        body
    }

    async fn send(&self, body: &Value) -> Result<Message, ClaudeError> {
        self.post(body).await
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, body: &Value) -> Result<T, ClaudeError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut call = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", VERSION)
                .json(body);
            if self.fallbacks {
                call = call.header("anthropic-beta", FALLBACK_BETA);
            }
            let response = match call.send().await {
                Ok(response) => response,
                Err(err) if attempt < ATTEMPTS => {
                    tracing::warn!(attempt, error = %err, "Claude request failed; retrying");
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(err) => return Err(ClaudeError::Http(err)),
            };
            let status = response.status();
            if status.is_success() {
                return response
                    .json::<T>()
                    .await
                    .map_err(|e| ClaudeError::Shape(e.to_string()));
            }
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let message = api_message(response).await;
            if retryable(status) && attempt < ATTEMPTS {
                tracing::warn!(attempt, %status, "Claude API busy; retrying");
                tokio::time::sleep(backoff(attempt, retry_after)).await;
                continue;
            }
            return Err(ClaudeError::Api {
                status: status.as_u16(),
                message,
            });
        }
    }
}

/// One conversation block as the Messages API expects it.
fn block_json(block: &ChatBlock) -> Value {
    match block {
        ChatBlock::Text { text } => json!({ "type": "text", "text": text }),
        ChatBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ChatBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result", "tool_use_id": tool_use_id, "content": content,
            "is_error": is_error,
        }),
        ChatBlock::Opaque { raw } => raw.clone(),
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    content: Vec<Value>,
    stop_reason: Option<String>,
    stop_details: Option<StopDetails>,
    usage: Usage,
}

/// A response block: text and tool calls are typed; reasoning and anything else is kept
/// verbatim (it must be sent back unchanged in the next step).
fn parse_block(raw: Value) -> Result<ChatBlock, ClaudeError> {
    match raw.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ChatBlock::Text {
            text: raw
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        Some("tool_use") => {
            let field = |name: &str| {
                raw.get(name)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| ClaudeError::Shape(format!("tool_use without {name}")))
            };
            Ok(ChatBlock::ToolUse {
                id: field("id")?,
                name: field("name")?,
                input: raw.get("input").cloned().unwrap_or_else(|| json!({})),
            })
        }
        _ => Ok(ChatBlock::Opaque { raw }),
    }
}

fn chat_reply(response: ChatResponse) -> Result<ChatReply, ClaudeError> {
    let stop = match response.stop_reason.as_deref() {
        Some("refusal") => {
            return Err(ClaudeError::Refusal(
                response.stop_details.and_then(|d| d.category),
            ));
        }
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    };
    let usage = response.usage;
    Ok(ChatReply {
        content: response
            .content
            .into_iter()
            .map(parse_block)
            .collect::<Result<_, _>>()?,
        stop,
        input_tokens: usage.input_tokens
            + usage.cache_creation_input_tokens
            + usage.cache_read_input_tokens,
        output_tokens: usage.output_tokens,
    })
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    retry_after.map_or_else(
        || Duration::from_secs(u64::from(attempt).pow(2)),
        |seconds| Duration::from_secs(seconds.min(60)),
    )
}

/// The `error.message` of an error body, or the raw body (bounded) when it is not JSON.
async fn api_message(response: reqwest::Response) -> String {
    let text = response.text().await.unwrap_or_default();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| text.chars().take(300).collect())
}

/// The answer text of a finished message (thinking blocks skipped), or why there is none.
fn answer(message: Message) -> Result<LlmResponse, ClaudeError> {
    match message.stop_reason.as_deref() {
        Some("refusal") => {
            return Err(ClaudeError::Refusal(
                message.stop_details.and_then(|d| d.category),
            ));
        }
        Some("max_tokens") => return Err(ClaudeError::Truncated),
        _ => {}
    }
    let text: String = message
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect();
    if text.trim().is_empty() {
        return Err(ClaudeError::Shape("no text in the answer".into()));
    }
    let usage = message.usage;
    Ok(LlmResponse {
        text,
        input_tokens: usage.input_tokens
            + usage.cache_creation_input_tokens
            + usage.cache_read_input_tokens,
        output_tokens: usage.output_tokens,
    })
}

#[async_trait]
impl LlmProvider for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, PortError> {
        let message = self
            .send(&self.body(request))
            .await
            .map_err(|e| PortError::adapter("claude", e))?;
        answer(message).map_err(|e| PortError::adapter("claude", e))
    }
}

#[async_trait]
impl ChatModel for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatReply, PortError> {
        let response: ChatResponse = self
            .post(&self.chat_body(request))
            .await
            .map_err(|e| PortError::adapter("claude", e))?;
        chat_reply(response).map_err(|e| PortError::adapter("claude", e))
    }
}

#[cfg(test)]
mod tests;
