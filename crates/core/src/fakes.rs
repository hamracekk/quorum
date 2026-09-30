//! In-memory implementations of the ports for deterministic tests in any crate.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::instrument::{InstrumentId, VenueId};
use crate::ports::{BarStore, MarketData, PortError};
use crate::types::{Bar, Series};

/// A recorded `MarketData::bars` call: instrument, start, end.
pub type BarsRequest = (InstrumentId, DateTime<Utc>, DateTime<Utc>);

/// Serves fixed bars for one venue, ignoring the interval.
#[derive(Debug)]
pub struct MemoryMarketData {
    venue: VenueId,
    bars: BTreeMap<InstrumentId, Vec<Bar>>,
    requests: Mutex<Vec<BarsRequest>>,
}

impl MemoryMarketData {
    pub const fn new(venue: VenueId) -> Self {
        Self {
            venue,
            bars: BTreeMap::new(),
            requests: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn with_bars(mut self, instrument: InstrumentId, bars: Vec<Bar>) -> Self {
        self.bars.insert(instrument, bars);
        self
    }

    /// Every `(instrument, start, end)` requested so far, in order.
    pub fn requests(&self) -> Vec<BarsRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl MarketData for MemoryMarketData {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn bars(
        &self,
        instrument: &InstrumentId,
        _interval: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Bar>, PortError> {
        if instrument.venue() != &self.venue {
            return Err(PortError::Unsupported(instrument.to_string()));
        }
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((instrument.clone(), start, end));
        Ok(self
            .bars
            .get(instrument)
            .map(|bars| {
                bars.iter()
                    .filter(|b| b.ts >= start && b.ts < end)
                    .copied()
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Keeps series in memory, keyed by instrument and interval.
#[derive(Debug, Default)]
pub struct MemoryBarStore {
    series: Mutex<BTreeMap<(InstrumentId, String), Series>>,
}

impl BarStore for MemoryBarStore {
    fn load(&self, instrument: &InstrumentId, interval: &str) -> Result<Option<Series>, PortError> {
        Ok(self
            .series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(instrument.clone(), interval.to_owned()))
            .cloned())
    }

    fn save(&self, series: &Series, interval: &str) -> Result<(), PortError> {
        if !series.bars.windows(2).all(|w| w[0].ts < w[1].ts) {
            return Err(PortError::Invalid("bars are unsorted or duplicated".into()));
        }
        self.series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (series.instrument.clone(), interval.to_owned()),
                series.clone(),
            );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn bar(hour: i64) -> Bar {
        Bar {
            ts: at(hour),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }
    }

    fn id(venue: &str) -> InstrumentId {
        InstrumentId::spot(venue, "A").expect("valid instrument")
    }

    #[tokio::test]
    async fn market_data_serves_a_half_open_range_and_records_requests() {
        let data = MemoryMarketData::new(VenueId::new("test").expect("valid venue"))
            .with_bars(id("test"), (0..5).map(bar).collect());
        let bars = data
            .bars(&id("test"), "1h", at(1), at(3))
            .await
            .expect("bars");
        let hours: Vec<_> = bars.iter().map(|b| b.ts).collect();
        assert_eq!(hours, vec![at(1), at(2)]);
        assert_eq!(data.requests(), vec![(id("test"), at(1), at(3))]);
    }

    #[tokio::test]
    async fn market_data_rejects_other_venues() {
        let data = MemoryMarketData::new(VenueId::new("test").expect("valid venue"));
        let err = data
            .bars(&id("other"), "1h", at(0), at(1))
            .await
            .expect_err("wrong venue");
        assert!(matches!(err, PortError::Unsupported(_)));
    }

    #[test]
    fn bar_store_round_trips_and_rejects_unsorted_series() {
        let store = MemoryBarStore::default();
        let series = Series::new(id("test"), vec![bar(0), bar(1)]);
        store.save(&series, "1h").expect("save");
        assert_eq!(store.load(&id("test"), "1h").expect("load"), Some(series));
        assert!(store.load(&id("test"), "4h").expect("load").is_none());

        let unsorted = Series::new(id("test"), vec![bar(1), bar(0)]);
        assert!(matches!(
            store.save(&unsorted, "1h"),
            Err(PortError::Invalid(_))
        ));
    }
}
