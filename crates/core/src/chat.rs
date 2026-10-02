//! Tool-using conversations with a language model (AG-3b): the types an internal agent's loop
//! exchanges with a [`ChatModel`](crate::ports::ChatModel).
//!
//! The model never acts by itself: it can only *ask* for a tool call, which the agent runner
//! executes through the MCP dispatcher under the agent's permissions, and whose result (untrusted
//! data) goes back into the conversation. Blocks the provider requires to be returned verbatim
//! (e.g. reasoning blocks) are kept opaque in [`ChatBlock::Opaque`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool the model may ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
}

/// One block of a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatBlock {
    Text {
        text: String,
    },
    /// The model asks for a tool call.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The result of a tool call, sent back by the runner.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// A provider block that must be sent back unchanged (reasoning and the like).
    Opaque {
        raw: Value,
    },
}

/// `user` or `assistant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: Vec<ChatBlock>,
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: vec![ChatBlock::Text { text: text.into() }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    /// Fixed instructions (byte-identical across steps so providers can cache them).
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

/// Why the model stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Finished its turn.
    EndTurn,
    /// Waits for tool results.
    ToolUse,
    /// Hit `max_tokens`.
    MaxTokens,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub content: Vec<ChatBlock>,
    pub stop: StopReason,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl ChatReply {
    /// The tool calls asked for, in order.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ChatBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }

    /// The text of the reply (blocks joined).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ChatBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
