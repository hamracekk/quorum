//! Bid-ask spread estimated from daily highs and lows (Corwin & Schultz, *J. Finance* 2012).
//!
//! The cost of crossing the spread grows as an instrument gets less liquid, which a fixed
//! slippage hides. Point in time: an estimate uses the two bars up to the latest close.

use std::collections::{BTreeMap, VecDeque};

use crate::instrument::InstrumentId;
use crate::types::Bar;

/// The largest half-spread an estimate may report (5%).
pub const MAX_HALF_SPREAD: f64 = 0.05;

/// The full spread (as a fraction of price) implied by two consecutive bars.
///
/// `None` when their prices cannot give one; negative estimates (noise) are 0.
#[must_use]
pub fn corwin_schultz(a: &Bar, b: &Bar) -> Option<f64> {
    let ok = |bar: &Bar| bar.low > 0.0 && bar.high >= bar.low && bar.high.is_finite();
    if !ok(a) || !ok(b) {
        return None;
    }
    let (ra, rb) = ((a.high / a.low).ln(), (b.high / b.low).ln());
    let beta = ra.mul_add(ra, rb * rb);
    let gamma = (a.high.max(b.high) / a.low.min(b.low)).ln().powi(2);
    let k = 2.0_f64.mul_add(-std::f64::consts::SQRT_2, 3.0);
    let alpha = ((2.0 * beta).sqrt() - beta.sqrt()) / k - (gamma / k).sqrt();
    let spread = 2.0 * alpha.exp_m1() / (1.0 + alpha.exp());
    spread.is_finite().then(|| spread.max(0.0))
}

/// Each instrument's half-spread: the mean of its last `window` two-bar estimates, capped at
/// [`MAX_HALF_SPREAD`].
#[derive(Debug, Clone)]
pub struct SpreadEstimator {
    window: usize,
    last: BTreeMap<InstrumentId, Bar>,
    recent: BTreeMap<InstrumentId, VecDeque<f64>>,
}

impl SpreadEstimator {
    #[must_use]
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            last: BTreeMap::new(),
            recent: BTreeMap::new(),
        }
    }

    /// Records `bar` and returns the instrument's half-spread estimate, once it has one.
    pub fn observe(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let previous = self.last.insert(instrument.clone(), *bar);
        let recent = self.recent.entry(instrument.clone()).or_default();
        if let Some(spread) = previous.and_then(|p| corwin_schultz(&p, bar)) {
            recent.push_back(spread / 2.0);
            if recent.len() > self.window {
                recent.pop_front();
            }
        }
        (!recent.is_empty())
            .then(|| (recent.iter().sum::<f64>() / recent.len() as f64).min(MAX_HALF_SPREAD))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    fn bar(day: u32, high: f64, low: f64) -> Bar {
        Bar {
            ts: Utc
                .with_ymd_and_hms(2025, 1, day, 0, 0, 0)
                .single()
                .expect("valid"),
            open: low,
            high,
            low,
            close: high,
            volume: 1.0,
        }
    }

    #[test]
    fn wider_daily_ranges_with_no_drift_imply_wider_spreads() {
        // Same range both days and no move between them: the range is read as spread.
        let tight = corwin_schultz(&bar(1, 100.2, 99.8), &bar(2, 100.2, 99.8)).expect("spread");
        let wide = corwin_schultz(&bar(1, 102.0, 98.0), &bar(2, 102.0, 98.0)).expect("spread");
        assert!(tight > 0.0 && wide > tight, "{tight} vs {wide}");
        assert!(corwin_schultz(&bar(1, 1.0, 0.0), &bar(2, 1.0, 0.5)).is_none());
    }

    #[test]
    fn the_estimate_averages_a_window_and_is_capped() {
        let id = InstrumentId::spot("binance", "AUSDT").expect("valid");
        let mut est = SpreadEstimator::new(3);
        assert!(
            est.observe(&id, &bar(1, 100.2, 99.8)).is_none(),
            "one bar: nothing yet"
        );
        let first = est.observe(&id, &bar(2, 100.2, 99.8)).expect("estimate");
        assert!(first > 0.0);
        let mut crazy = SpreadEstimator::new(3);
        crazy.observe(&id, &bar(1, 200.0, 50.0));
        let capped = crazy.observe(&id, &bar(2, 200.0, 50.0)).expect("estimate");
        assert!((capped - MAX_HALF_SPREAD).abs() < 1e-12);
    }
}
