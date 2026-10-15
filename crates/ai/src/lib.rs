//! Language models for trader (M5b): the [`LlmProvider`](quorum_core::ports::LlmProvider)
//! adapters and the news-event extraction contract.
//!
//! - [`ollama::OllamaProvider`]: local and free, the first pass over every news item.
//! - [`claude::ClaudeProvider`]: the Claude Messages API, for items the first pass marks as
//!   mattering, under a daily token budget enforced by the caller.
//! - [`extract`]: the frozen prompt, the JSON schema, and the strict parser that rejects any
//!   answer that does not conform. Model output is data, never instructions.

pub mod claude;
pub mod extract;
pub mod ollama;
pub mod review;
pub mod themes;

pub use claude::ClaudeProvider;
pub use ollama::OllamaProvider;
