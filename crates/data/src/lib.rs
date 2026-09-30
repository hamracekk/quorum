//! Market data acquisition and local storage.
//!
//! Candles are stored one Parquet file per symbol and interval under a data directory,
//! and [`sync`] keeps them current by downloading only bars newer than the last one
//! on disk.

pub mod binance;
mod error;
mod store;

pub use error::{DataError, Result};
pub use store::{Store, read_parquet, write_parquet};

use chrono::{Duration, Utc};
use trader_core::Series;

/// Result of a [`sync`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    pub added: usize,
    pub total: usize,
}

/// Bring the stored candles for `symbol`/`interval` up to date.
///
/// With nothing stored yet, downloads the last `history`. Otherwise downloads only
/// bars newer than the last stored one and appends them.
pub fn sync(store: &Store, symbol: &str, interval: &str, history: Duration) -> Result<SyncOutcome> {
    let mut bars = store
        .load(symbol, interval)?
        .map(|s| s.bars)
        .unwrap_or_default();
    let last = bars.last().map(|b| b.ts);
    let now = Utc::now();
    let start = last.map_or(now - history, |ts| ts + Duration::milliseconds(1));

    let fresh = binance::fetch_klines(symbol, interval, start, now)?;
    let before = bars.len();
    bars.extend(
        fresh
            .into_iter()
            .filter(|b| last.is_none_or(|ts| b.ts > ts)),
    );
    let added = bars.len() - before;

    if added > 0 {
        store.save(&Series::new(symbol, bars), interval)?;
    }
    let total = before + added;
    tracing::info!(symbol, interval, added, total, "synced candles");
    Ok(SyncOutcome { added, total })
}
