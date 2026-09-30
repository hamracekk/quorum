use std::fmt;

use crate::instrument::InstrumentId;
use crate::types::Bar;

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
}
