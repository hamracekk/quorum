//! Research tooling on top of the backtester: parameter sweeps and walk-forward
//! (out-of-sample) evaluation.
//!
//! Sweeps show how sensitive a strategy is to its parameters; they overfit by design.
//! Walk-forward is the honest number: parameters are chosen on a training window and
//! judged only on the following window, which they have never seen.

pub mod carry;
mod error;
pub mod scoreboard;
pub mod signals;
mod sweep;
pub mod tournament;
mod walk_forward;

pub use error::LabError;
pub use sweep::{SweepRow, sweep};
pub use walk_forward::{
    WalkForwardConfig, WalkForwardReport, Window, walk_forward, walk_forward_with,
};

use chrono::{DateTime, Utc};
use trader_core::{Series, Strategy};

/// Builds a fresh strategy instance from a parameter set.
pub trait StrategyFactory<P> {
    fn build(&self, params: &P) -> Box<dyn Strategy>;
}

impl<P, F> StrategyFactory<P> for F
where
    F: Fn(&P) -> Box<dyn Strategy>,
{
    fn build(&self, params: &P) -> Box<dyn Strategy> {
        self(params)
    }
}

/// Every series restricted to `start <= ts < end`.
fn slice_all(series: &[Series], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<Series> {
    series.iter().map(|s| s.slice(start, end)).collect()
}
