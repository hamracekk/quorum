//! The knowledge plane (DESIGN §8): news in, typed events out.
//!
//! 1. **Fetch** every configured feed ([`fetch`]), tag items with the instruments of the
//!    configured slots (a keyword label, not a signal), store them deduplicated.
//! 2. **Extract** ([`extract`]) typed events with two tiers of Claude: a cheap **first pass** on
//!    every new item, an **escalation** for items the first pass reads as mattering — each tier
//!    within its own daily token budget. Then the **impact-map** pass: themes from the operator's
//!    map (`config/impact.toml`), expanded through its links into implied moves across markets.
//!
//! **News is untrusted.** Items are inert data in prompts; model output is validated against a
//! closed schema (`quorum_ai`) and discarded if it does not conform. A reading is data for
//! scoring: it never reaches an order. Every attempt is journaled, which is also the cache — an
//! item is never sent to the same tier twice for the same prompt version.

mod config;
mod extract;
mod fetch;

pub use config::{NewsConfig, NewsConfigError, Source, Tier, TierConfig, load_impact};
pub use extract::{ExtractReport, LLMs, TierReport, assets, escalates, extract, final_readings};
pub use fetch::{FetchReport, base_asset, fetch, tag};

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error(transparent)]
    Config(#[from] NewsConfigError),
    #[error(transparent)]
    Port(#[from] quorum_core::PortError),
}

pub type Result<T> = std::result::Result<T, KnowledgeError>;
