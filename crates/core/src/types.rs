use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;

/// One OHLCV candle. `ts` is the bar's open time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub ts: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Bars of a single instrument, sorted by time with no duplicates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub instrument: InstrumentId,
    pub bars: Vec<Bar>,
}

impl Series {
    pub const fn new(instrument: InstrumentId, bars: Vec<Bar>) -> Self {
        Self { instrument, bars }
    }

    /// Bars with `start <= ts < end`.
    #[must_use]
    pub fn slice(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        let lo = self.bars.partition_point(|b| b.ts < start);
        let hi = self.bars.partition_point(|b| b.ts < end).max(lo);
        Self {
            instrument: self.instrument.clone(),
            bars: self.bars[lo..hi].to_vec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// An executed trade, including slippage in `price` and the commission in `fee`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub ts: DateTime<Utc>,
    pub instrument: InstrumentId,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub fee: f64,
}

/// Account equity marked at a bar's close.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts: DateTime<Utc>,
    pub equity: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn series(hours: &[i64]) -> Series {
        let bars = hours
            .iter()
            .map(|&h| Bar {
                ts: at(h),
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 1.0,
            })
            .collect();
        Series::new(
            InstrumentId::spot("test", "TEST").expect("valid instrument"),
            bars,
        )
    }

    #[test]
    fn slice_is_half_open() {
        let sliced = series(&[0, 1, 2, 3]).slice(at(1), at(3));
        let hours: Vec<_> = sliced.bars.iter().map(|b| b.ts).collect();
        assert_eq!(hours, vec![at(1), at(2)]);
    }

    #[test]
    fn inverted_slice_is_empty() {
        assert!(series(&[0, 1, 2]).slice(at(2), at(1)).bars.is_empty());
    }
}
