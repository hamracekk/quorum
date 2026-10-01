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
