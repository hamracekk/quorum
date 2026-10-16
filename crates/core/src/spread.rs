//! Bid-ask spread estimated from trading volume: thin markets cost more to cross than a fixed
//! slippage assumes, liquid ones less.
//!
//! Calibrated 2026-10-16 on Binance spot: the median quoted spread of 25 USDT pairs (the
//! recorder's book snapshots, one day) against their 24-hour dollar volume ($2M–$1.4B) fits
//! `spread_bps = e^16.526 × volume^−0.946` (R² 0.63): ~0.4 bp at $100M a day, ~3.6 bp at $10M,
//! ~32 bp at $1M. Below $2M it is an extrapolation; other venues need their own calibration.
//!
//! A high/low estimator (Corwin & Schultz) was tried first and rejected: on volatile daily bars it
//! reads volatility as spread and put Binance pairs at ~1% where their books show 0.5–12 bp.
//! Point in time: an estimate uses volumes up to the latest close.

use std::collections::{BTreeMap, VecDeque};

use crate::instrument::InstrumentId;
use crate::types::Bar;

/// `ln` of the calibration's scale (bp at $1 a day).
const LN_SCALE: f64 = 16.526;
/// The calibration's volume exponent.
const EXPONENT: f64 = -0.946;
/// The largest half-spread an estimate may report (5%).
pub const MAX_HALF_SPREAD: f64 = 0.05;

/// Half the quoted spread (a fraction of price) of a market trading `daily_dollar_volume`.
#[must_use]
pub fn half_spread(daily_dollar_volume: f64) -> f64 {
    if !(daily_dollar_volume.is_finite() && daily_dollar_volume > 0.0) {
        return MAX_HALF_SPREAD;
    }
    let bps = EXPONENT.mul_add(daily_dollar_volume.ln(), LN_SCALE).exp();
    (bps / 2.0 / 10_000.0).min(MAX_HALF_SPREAD)
}

/// Each instrument's half-spread from its mean daily dollar volume over the last `window` bars
/// (bars of any interval, scaled to a day by their spacing).
#[derive(Debug, Clone)]
pub struct SpreadEstimator {
    window: usize,
    last_ts: BTreeMap<InstrumentId, chrono::DateTime<chrono::Utc>>,
    daily: BTreeMap<InstrumentId, VecDeque<f64>>,
}

impl SpreadEstimator {
    #[must_use]
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            last_ts: BTreeMap::new(),
            daily: BTreeMap::new(),
        }
    }

    /// Records `bar` and returns the instrument's half-spread estimate, once it has one.
    pub fn observe(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let previous = self.last_ts.insert(instrument.clone(), bar.ts);
        let daily = self.daily.entry(instrument.clone()).or_default();
        if let Some(previous) = previous {
            let secs = (bar.ts - previous).num_seconds();
            if secs > 0 {
                let per_day = 86_400.0 / secs as f64;
                daily.push_back(bar.close * bar.volume * per_day);
                if daily.len() > self.window {
                    daily.pop_front();
                }
            }
        }
        (!daily.is_empty()).then(|| half_spread(daily.iter().sum::<f64>() / daily.len() as f64))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::*;

    #[test]
    fn thinner_markets_cost_more_to_cross() {
        let liquid = half_spread(1e8);
        let thin = half_spread(1e6);
        // ~0.4 bp and ~32 bp full spread at $100M and $1M a day.
        let bps = |half: f64| half * 20_000.0;
        assert!((bps(liquid) - 0.41).abs() < 0.05, "{liquid}");
        assert!((bps(thin) - 31.8).abs() < 1.0, "{thin}");
        assert!(
            (half_spread(10.0) - MAX_HALF_SPREAD).abs() < 1e-12,
            "capped"
        );
        assert!((half_spread(0.0) - MAX_HALF_SPREAD).abs() < 1e-12);
    }

    #[test]
    fn volume_is_scaled_to_a_day_by_bar_spacing() {
        let id = InstrumentId::spot("binance", "AUSDT").expect("valid");
        let start = Utc
            .with_ymd_and_hms(2025, 1, 1, 0, 0, 0)
            .single()
            .expect("valid");
        // Hourly bars of $1M: $24M a day.
        let bar = |h: i64| Bar {
            ts: start + Duration::hours(h),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1e6,
        };
        let mut est = SpreadEstimator::new(30);
        assert!(
            est.observe(&id, &bar(0)).is_none(),
            "spacing needs two bars"
        );
        let h = est.observe(&id, &bar(1)).expect("estimate");
        assert!((h - half_spread(24e6)).abs() < 1e-15);
    }
}
