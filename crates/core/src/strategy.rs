use crate::types::Bar;

/// A trading strategy expressed as a desired exposure.
///
/// Strategies never place orders directly. On each *closed* bar they may return a
/// target exposure as a fraction of equity (`1.0` = fully long, `0.0` = flat,
/// `-1.0` = fully short). Returning `None` keeps the previous target.
///
/// The engine applies risk limits and executes the change at the **next** bar's
/// open, so a strategy cannot trade on a price it has only just seen.
pub trait Strategy {
    fn name(&self) -> &str;

    fn on_bar(&mut self, bar: &Bar) -> Option<f64>;
}
