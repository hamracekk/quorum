//! Market data adapters and local storage.
//!
//! - [`BinanceSpot`](binance::BinanceSpot) implements the `MarketData` port.
//! - [`BinanceTestnet`](binance_testnet::BinanceTestnet): signed spot orders on Binance's
//!   **testnet** only (T-7); it refuses any other host.
//! - [`ParquetStore`] implements the `BarStore` port: one Parquet file per instrument and
//!   interval under `<root>/<venue>/`.
//! - [`sync`] keeps a store current from any `MarketData` source, fetching only bars newer
//!   than the last one stored.

pub mod binance;
pub mod binance_announcements;
pub mod binance_archive;
pub mod binance_futures;
pub mod binance_testnet;
pub mod binance_ws;
mod error;
pub mod indicators;
pub mod quality;
mod store;
pub mod tiingo;

pub use error::{DataError, Result};
pub use store::{ParquetStore, read_parquet, write_parquet};

use chrono::{DateTime, Duration, Utc};
use quorum_core::{BarStore, InstrumentId, MarketData, PortError, Series};

/// Result of a [`sync`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOutcome {
    pub added: usize,
    pub total: usize,
}

/// Bring the stored bars for `instrument`/`interval` up to date as of `until`.
///
/// With nothing stored yet, downloads from `since_if_empty`. Otherwise downloads only bars
/// newer than the last stored one and appends them.
pub async fn sync(
    source: &dyn MarketData,
    store: &dyn BarStore,
    instrument: &InstrumentId,
    interval: &str,
    since_if_empty: DateTime<Utc>,
    until: DateTime<Utc>,
) -> std::result::Result<SyncOutcome, PortError> {
    let mut bars = store
        .load(instrument, interval)?
        .map(|s| s.bars)
        .unwrap_or_default();
    let last = bars.last().map(|b| b.ts);
    let start = last.map_or(since_if_empty, |ts| ts + Duration::milliseconds(1));

    let fresh = source.bars(instrument, interval, start, until).await?;
    let before = bars.len();
    bars.extend(
        fresh
            .into_iter()
            .filter(|b| last.is_none_or(|ts| b.ts > ts)),
    );
    let added = bars.len() - before;

    if added > 0 {
        store.save(&Series::new(instrument.clone(), bars), interval)?;
    }
    let total = before + added;
    tracing::info!(%instrument, interval, added, total, "synced bars");
    Ok(SyncOutcome { added, total })
}

#[cfg(test)]
mod tests {
    use quorum_core::fakes::{MemoryBarStore, MemoryMarketData};
    use quorum_core::{Bar, VenueId};

    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn bars(hours: std::ops::Range<i64>) -> Vec<Bar> {
        hours
            .map(|h| Bar {
                ts: at(h),
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            })
            .collect()
    }

    fn instrument() -> InstrumentId {
        InstrumentId::spot("test", "BTC").expect("valid instrument")
    }

    fn source(hours: std::ops::Range<i64>) -> MemoryMarketData {
        MemoryMarketData::new(VenueId::new("test").expect("valid venue"))
            .with_bars(instrument(), bars(hours))
    }

    #[tokio::test]
    async fn first_sync_downloads_history() {
        let store = MemoryBarStore::default();
        let outcome = sync(&source(0..10), &store, &instrument(), "1h", at(0), at(100))
            .await
            .expect("sync");
        assert_eq!(
            outcome,
            SyncOutcome {
                added: 10,
                total: 10
            }
        );
    }

    #[tokio::test]
    async fn later_syncs_fetch_only_newer_bars() {
        let store = MemoryBarStore::default();
        sync(&source(0..10), &store, &instrument(), "1h", at(0), at(100))
            .await
            .expect("first sync");

        let newer = source(0..12);
        let outcome = sync(&newer, &store, &instrument(), "1h", at(0), at(100))
            .await
            .expect("second sync");

        assert_eq!(
            outcome,
            SyncOutcome {
                added: 2,
                total: 12
            }
        );
        let requests = newer.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].1 > at(9),
            "must start after the last stored bar"
        );
    }

    #[tokio::test]
    async fn unchanged_source_adds_nothing() {
        let store = MemoryBarStore::default();
        let data = source(0..5);
        sync(&data, &store, &instrument(), "1h", at(0), at(100))
            .await
            .expect("first sync");
        let outcome = sync(&data, &store, &instrument(), "1h", at(0), at(100))
            .await
            .expect("second sync");
        assert_eq!(outcome, SyncOutcome { added: 0, total: 5 });
    }

    #[tokio::test]
    async fn wrong_venue_is_rejected() {
        let other = InstrumentId::spot("elsewhere", "BTC").expect("valid instrument");
        let err = sync(
            &source(0..5),
            &MemoryBarStore::default(),
            &other,
            "1h",
            at(0),
            at(100),
        )
        .await
        .expect_err("unsupported venue");
        assert!(matches!(err, PortError::Unsupported(_)));
    }
}
