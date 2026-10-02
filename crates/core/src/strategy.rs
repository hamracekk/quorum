use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;
use crate::types::Bar;

/// A directional call from outside the bar stream (a news reading, a prediction-market move, a
/// followed trader), known at `ts`. Strategies that weigh evidence take these in; others
/// ignore them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalCall {
    /// Scoreboard id of the source, e.g. `news:final` or `trader:hyperliquid:whale`.
    pub source: String,
    pub instrument: InstrumentId,
    pub ts: DateTime<Utc>,
    /// `1.0` bullish, `-1.0` bearish.
    pub direction: f64,
    /// Confidence or size in `0..=1`.
    pub strength: f64,
}

/// A trading strategy expressed as desired exposure per instrument.
///
/// Strategies never place orders directly. On each *closed* bar of an instrument they
/// may return a target exposure for that instrument as a fraction of **total** equity
/// (`1.0` = fully long, `0.0` = flat, `-1.0` = fully short). Returning `None` keeps
/// the previous target.
///
/// The engine applies risk limits and executes the change at that instrument's **next**
/// bar open, so a strategy cannot trade on a price it has only just seen.
///
/// Single-asset strategies ignore `instrument`; wrap them in
/// [`PerInstrument`](crate::strategies::PerInstrument) to run them across several
/// instruments.
///
/// Strategies are `Send` so the daemon can run them on async tasks.
pub trait Strategy: fmt::Debug + Send {
    fn name(&self) -> &str;

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64>;

    /// Why the latest target for `instrument` was chosen, recorded with the signal (M8).
    fn explain(&self, _instrument: &InstrumentId) -> Option<serde_json::Value> {
        None
    }

    /// New external calls (live news readings and the like). Ignored by default.
    fn inform(&mut self, _calls: &[ExternalCall]) {}
}
