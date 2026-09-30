use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// An executed trade, including slippage in `price` and the commission in `fee`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub ts: DateTime<Utc>,
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
