//! Momentum (E-1): time-series and cross-sectional, with volatility targeting.
//!
//! - [`TrendMomentum`] (single asset; run it with
//!   [`PerInstrument`](super::PerInstrument)): long while the close is above the close
//!   `lookback` bars ago, flat otherwise — the classic time-series momentum rule.
//! - [`XsMomentum`] (the whole universe): every `rebalance` bars, rank instruments by their
//!   `lookback` return and hold the best `top` of them, only those that are rising
//!   (absolute-momentum filter), in equal shares.
//!
//! Both size by **volatility targeting** when `target_vol > 0`: a holding's weight is scaled by
//! `min(1, target_vol / realised_vol)` (annualised from `vol_bars` returns), so calm coins get
//! more and wild coins less, and the portfolio's risk stays near the target. A `band` keeps a
//! held weight unless it moved by more than that fraction, so daily volatility noise does not
//! turn into fees.

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// Sizing shared by both momentum strategies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolTarget {
    /// Annualised volatility a holding is sized to (`0` = no scaling).
    pub target_vol: f64,
    /// Returns behind the realised volatility.
    pub vol_bars: usize,
    /// Bars per year of the interval (365 for daily bars), to annualise.
    pub periods_per_year: f64,
    /// A held weight changes only when the new one differs by more than this fraction.
    pub band: f64,
}

impl VolTarget {
    /// The scale for a holding with recent closes `closes` (newest last); `1` without a target
    /// or before `vol_bars` returns are known.
    fn scale(&self, closes: &VecDeque<f64>) -> f64 {
        if self.target_vol <= 0.0 || closes.len() <= self.vol_bars {
            return 1.0;
        }
        let recent: Vec<f64> = closes
            .iter()
            .skip(closes.len() - self.vol_bars - 1)
            .copied()
            .collect();
        let returns: Vec<f64> = recent
            .windows(2)
            .filter(|w| w[0] > 0.0 && w[1] > 0.0)
            .map(|w| (w[1] / w[0]).ln())
            .collect();
        if returns.len() < 2 {
            return 1.0;
        }
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let vol = (var * self.periods_per_year).sqrt();
        if vol > 0.0 {
            (self.target_vol / vol).min(1.0)
        } else {
            1.0
        }
    }

    /// `next` unless it is within `band` of `held` (entries and exits always pass).
    fn banded(&self, held: Option<f64>, next: f64) -> Option<f64> {
        match held {
            Some(h) if h > 0.0 && next > 0.0 && ((next - h).abs() / h) <= self.band => None,
            Some(h) if (h - next).abs() < f64::EPSILON => None,
            _ => Some(next),
        }
    }
}

/// Recent closes, newest last, at most `cap` of them.
fn push_close(closes: &mut VecDeque<f64>, close: f64, cap: usize) {
    closes.push_back(close);
    while closes.len() > cap {
        closes.pop_front();
    }
}

/// Return over `lookback` bars, once known.
fn momentum(closes: &VecDeque<f64>, lookback: usize) -> Option<f64> {
    if closes.len() <= lookback {
        return None;
    }
    let now = *closes.back()?;
    let then = *closes.get(closes.len() - 1 - lookback)?;
    (then > 0.0).then(|| now / then - 1.0)
}

/// Time-series momentum on one asset: long (vol-sized) while the `lookback` return is positive.
#[derive(Debug, Clone)]
pub struct TrendMomentum {
    lookback: usize,
    sizing: VolTarget,
    closes: VecDeque<f64>,
    held: Option<f64>,
}

impl TrendMomentum {
    pub fn new(lookback: usize, sizing: VolTarget) -> Self {
        Self {
            lookback: lookback.max(1),
            sizing,
            closes: VecDeque::new(),
            held: None,
        }
    }
}

impl Strategy for TrendMomentum {
    fn name(&self) -> &'static str {
        "trend-momentum"
    }

    fn on_bar(&mut self, _instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let cap = self.lookback.max(self.sizing.vol_bars) + 1;
        push_close(&mut self.closes, bar.close, cap);
        let rising = momentum(&self.closes, self.lookback)? > 0.0;
        let next = if rising {
            self.sizing.scale(&self.closes)
        } else {
            0.0
        };
        let target = self.sizing.banded(self.held, next)?;
        self.held = Some(target);
        Some(target)
    }

    fn explain(&self, _instrument: &InstrumentId) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "lookback": self.lookback,
            "momentum": momentum(&self.closes, self.lookback),
            "target": self.held,
        }))
    }
}

/// Cross-sectional momentum: hold the `top` rising instruments with the best `lookback`
/// return, re-ranked every `rebalance` bars.
///
/// Ranking uses every instrument's close up to the **previous** timestamp, computed when the
/// first bar of a new timestamp arrives, so the result never depends on the order instruments
/// are fed in (one bar of extra delay, negligible for multi-week lookbacks).
#[derive(Debug, Clone)]
pub struct XsMomentum {
    lookback: usize,
    top: usize,
    rebalance: usize,
    sizing: VolTarget,
    closes: BTreeMap<InstrumentId, VecDeque<f64>>,
    current: Option<DateTime<Utc>>,
    stamps: usize,
    /// Chosen weights since the last ranking (missing = 0).
    chosen: BTreeMap<InstrumentId, f64>,
    ranked: bool,
    held: BTreeMap<InstrumentId, f64>,
}

impl XsMomentum {
    pub fn new(lookback: usize, top: usize, rebalance: usize, sizing: VolTarget) -> Self {
        Self {
            lookback: lookback.max(1),
            top: top.max(1),
            rebalance: rebalance.max(1),
            sizing,
            closes: BTreeMap::new(),
            current: None,
            stamps: 0,
            chosen: BTreeMap::new(),
            ranked: false,
            held: BTreeMap::new(),
        }
    }

    fn rank(&mut self) {
        let mut scored: Vec<(&InstrumentId, f64)> = self
            .closes
            .iter()
            .filter_map(|(id, closes)| Some((id, momentum(closes, self.lookback)?)))
            .filter(|(_, m)| *m > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let share = 1.0 / self.top as f64;
        self.chosen = scored
            .into_iter()
            .take(self.top)
            .map(|(id, _)| {
                let scale = self.closes.get(id).map_or(1.0, |c| self.sizing.scale(c));
                (id.clone(), share * scale)
            })
            .collect();
        self.ranked = true;
    }
}

impl Strategy for XsMomentum {
    fn name(&self) -> &'static str {
        "xs-momentum"
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if self.current.is_none_or(|ts| bar.ts > ts) {
            // A new timestamp: every instrument's closes are complete up to the previous one.
            if self.current.is_some() {
                if self.stamps.is_multiple_of(self.rebalance) {
                    self.rank();
                }
                self.stamps += 1;
            }
            self.current = Some(bar.ts);
        }
        let cap = self.lookback.max(self.sizing.vol_bars) + 1;
        push_close(
            self.closes.entry(instrument.clone()).or_default(),
            bar.close,
            cap,
        );
        if !self.ranked {
            return None;
        }
        let next = self.chosen.get(instrument).copied().unwrap_or(0.0);
        let target = self
            .sizing
            .banded(self.held.get(instrument).copied(), next)?;
        self.held.insert(instrument.clone(), target);
        Some(target)
    }

    fn explain(&self, instrument: &InstrumentId) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "lookback": self.lookback,
            "top": self.top,
            "momentum": self.closes.get(instrument).and_then(|c| momentum(c, self.lookback)),
            "chosen": self.chosen.contains_key(instrument),
            "target": self.held.get(instrument),
        }))
    }
}

#[cfg(test)]
mod tests;
