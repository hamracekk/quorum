use std::fmt;

use crate::types::Bar;

/// A trading strategy expressed as desired exposure per symbol.
///
/// Strategies never place orders directly. On each *closed* bar of a symbol they may
/// return a target exposure for that symbol as a fraction of **total** equity
/// (`1.0` = fully long, `0.0` = flat, `-1.0` = fully short). Returning `None` keeps
/// the previous target.
///
/// The engine applies risk limits and executes the change at that symbol's **next**
/// bar open, so a strategy cannot trade on a price it has only just seen.
///
/// Single-asset strategies ignore `symbol`; wrap them in
/// [`PerSymbol`](crate::strategies::PerSymbol) to run them across several symbols.
pub trait Strategy: fmt::Debug {
    fn name(&self) -> &str;

    fn on_bar(&mut self, symbol: &str, bar: &Bar) -> Option<f64>;
}
