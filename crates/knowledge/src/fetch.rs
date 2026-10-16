//! Fetch every feed, tag items with the instruments they mention, store them deduplicated.
//!
//! Tagging is a keyword match on whole words: an instrument's base asset (`BTC` for
//! `binance:BTCUSDT`, `SPY` for `tiingo:SPY:equity`), well-known coin names, and configured
//! aliases (`GLD = ["gold"]`). It labels items for filtering; it is not a signal.

use std::collections::BTreeMap;
use std::sync::Arc;

use quorum_core::news::{NewsItem, title_key};
use quorum_core::{InstrumentId, NewsSource, NewsStore};
use serde::Serialize;

use crate::Result;

/// Names news uses for common coins, by base asset.
const KNOWN_NAMES: [(&str, &[&str]); 12] = [
    ("BTC", &["bitcoin"]),
    ("ETH", &["ethereum", "ether"]),
    ("SOL", &["solana"]),
    ("XRP", &["ripple"]),
    ("DOGE", &["dogecoin"]),
    ("ADA", &["cardano"]),
    ("BNB", &["binance coin"]),
    ("AVAX", &["avalanche"]),
    ("LINK", &["chainlink"]),
    ("DOT", &["polkadot"]),
    ("LTC", &["litecoin"]),
    ("TON", &["toncoin"]),
];

/// Quote assets stripped from a symbol to find its base asset.
const QUOTES: [&str; 8] = ["USDT", "USDC", "FDUSD", "BUSD", "USD", "EUR", "BTC", "ETH"];

/// `BTCUSDT` → `BTC`, `ETHBTC` → `ETH`, `SPY` → `SPY`.
#[must_use]
pub fn base_asset(symbol: &str) -> &str {
    QUOTES
        .iter()
        .find_map(|quote| symbol.strip_suffix(quote).filter(|base| !base.is_empty()))
        .unwrap_or(symbol)
}

/// The instruments `item` mentions as whole words.
#[must_use]
pub fn tag(
    item: &NewsItem,
    instruments: &[InstrumentId],
    aliases: &BTreeMap<String, Vec<String>>,
) -> Vec<InstrumentId> {
    let text = format!(
        " {} ",
        title_key(&format!("{} {}", item.title, item.summary))
    );
    instruments
        .iter()
        .filter(|instrument| {
            let base = base_asset(instrument.symbol()).to_ascii_uppercase();
            let known = KNOWN_NAMES
                .iter()
                .filter(|(asset, _)| *asset == base)
                .flat_map(|(_, names)| names.iter().map(|n| (*n).to_owned()));
            let configured = aliases.get(&base).into_iter().flatten().cloned();
            std::iter::once(base.clone())
                .chain(known)
                .chain(configured)
                .map(|term| title_key(&term))
                .filter(|term| !term.is_empty())
                .any(|term| text.contains(&format!(" {term} ")))
        })
        .cloned()
        .collect()
}

/// What one source's fetch did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FetchReport {
    pub source: String,
    pub fetched: usize,
    pub inserted: usize,
    pub duplicates: usize,
    /// Why the source could not be read (the others still are).
    pub error: Option<String>,
}

/// Fetches every source once, tags and stores the items. A failing source is reported and
/// logged; it never stops the others.
pub async fn fetch(
    sources: &[Arc<dyn NewsSource>],
    store: &dyn NewsStore,
    instruments: &[InstrumentId],
    aliases: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<FetchReport>> {
    let mut reports = Vec::new();
    for source in sources {
        let name = source.name().to_owned();
        let report = match source.fetch().await {
            Ok(mut items) => {
                for item in &mut items {
                    item.instruments = tag(item, instruments, aliases);
                }
                let stored = store.store_news(&items).await?;
                FetchReport {
                    source: name,
                    fetched: items.len(),
                    inserted: stored.inserted,
                    duplicates: stored.duplicates,
                    error: None,
                }
            }
            Err(err) => {
                tracing::warn!(source = %name, error = %err, "news source failed");
                FetchReport {
                    source: name,
                    fetched: 0,
                    inserted: 0,
                    duplicates: 0,
                    error: Some(err.to_string()),
                }
            }
        };
        reports.push(report);
    }
    let added: usize = reports.iter().map(|r| r.inserted).sum();
    tracing::info!(sources = reports.len(), added, "news fetched");
    Ok(reports)
}
