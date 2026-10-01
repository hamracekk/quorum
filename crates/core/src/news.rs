//! News items (M5a): headlines from public feeds, stored as untrusted data.
//!
//! A [`NewsSource`](crate::ports::NewsSource) adapter fetches and sanitizes items (plain text,
//! bounded lengths, http(s) links only); the service tags them with configured instruments and
//! a [`NewsStore`](crate::ports::NewsStore) keeps them, dropping duplicates: the same canonical
//! URL, or the same headline (see [`title_key`]) within [`DUPLICATE_WINDOW_HOURS`] of another.
//! Nothing in a news item is ever executed or followed as an instruction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;

/// Two items with the same headline key this close in time are one story.
pub const DUPLICATE_WINDOW_HOURS: i64 = 48;

/// One news item. `url` is canonical and identifies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsItem {
    pub url: String,
    /// Configured source name, e.g. `coindesk`.
    pub source: String,
    pub title: String,
    /// Plain-text summary (may be empty).
    pub summary: String,
    pub published_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
    /// Configured instruments the item mentions (keyword match; empty = general market news).
    pub instruments: Vec<InstrumentId>,
}

/// Which items to list. Newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewsQuery {
    /// Full-text search over titles and summaries (plain words).
    pub text: Option<String>,
    pub instrument: Option<InstrumentId>,
    pub source: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
}

/// What storing a batch did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct StoredNews {
    pub inserted: usize,
    pub duplicates: usize,
}

/// Per-source totals for status displays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewsSourceStats {
    pub source: String,
    pub items: usize,
    pub latest: Option<DateTime<Utc>>,
}

/// Which way an event pushes an asset, as the extractor read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Bullish,
    Bearish,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Magnitude {
    Low,
    Medium,
    High,
}

/// How long the extractor expects the effect to matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Horizon {
    Hours,
    Days,
    Weeks,
}

/// What kind of news it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Regulation,
    Security,
    Etf,
    Listing,
    Macro,
    Adoption,
    Technology,
    Flows,
    Legal,
    Other,
}

/// One typed event extracted from a news item (M5b). A reading of untrusted text by a model:
/// an input to scoring (M6) and the ensemble (M8), never an instruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewsEvent {
    /// Canonical URL of the news item it was read from.
    pub url: String,
    pub instrument: InstrumentId,
    pub direction: Direction,
    pub magnitude: Magnitude,
    /// The model's confidence in its reading, 0–1.
    pub confidence: f64,
    pub horizon: Horizon,
    pub event_type: EventType,
    /// One sentence on why (plain text, bounded).
    pub rationale: String,
    /// Publish time of the news item.
    pub published_at: DateTime<Utc>,
    /// `provider/model` that produced it, e.g. `claude/claude-opus-5-5`.
    pub model: String,
    pub extracted_at: DateTime<Utc>,
}

/// How an extraction attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionStatus {
    /// Valid output, stored (possibly with zero events: not market-relevant).
    Ok,
    /// The model answered but the output broke the schema or the rules; discarded.
    Rejected,
    /// No usable answer (provider error, refusal, truncation).
    Failed,
}

/// One extraction attempt of one news item by one provider — also the cache that keeps the
/// same item from being sent to the same model twice (per prompt version).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extraction {
    pub url: String,
    pub prompt_version: String,
    /// `local` (cheap first pass) or `remote` (escalation).
    pub tier: String,
    pub model: String,
    pub ts: DateTime<Utc>,
    pub status: ExtractionStatus,
    pub relevant: bool,
    pub events: Vec<NewsEvent>,
    pub error: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Tokens spent per tier since a moment (the daily budget looks at today).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub local: u64,
    pub remote: u64,
    pub remote_calls: u64,
}

/// A headline reduced to its words: lower-case alphanumerics joined by single spaces, so the
/// same story from two feeds (different punctuation, case, trailing site names aside) matches.
pub fn title_key(title: &str) -> String {
    title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_keys_ignore_case_and_punctuation() {
        assert_eq!(
            title_key("Bitcoin Hits $100K — Again!"),
            title_key("bitcoin hits 100k: again")
        );
        assert_eq!(title_key("  ETH/BTC ratio  "), "eth btc ratio");
        assert_eq!(title_key("…"), "");
    }
}
