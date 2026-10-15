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

use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
    pub(super) fn scale(&self, closes: &VecDeque<f64>) -> f64 {
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

    /// `next` unless it is within `band` of `held` on the same side (entries, exits and
    /// flips always pass).
    pub(super) fn banded(&self, held: Option<f64>, next: f64) -> Option<f64> {
        match held {
            Some(h) if h * next > 0.0 && ((next - h).abs() / h.abs()) <= self.band => None,
            Some(h) if (h - next).abs() < f64::EPSILON => None,
            _ => Some(next),
        }
    }
}

/// Recent closes, newest last, at most `cap` of them.
pub(super) fn push_close(closes: &mut VecDeque<f64>, close: f64, cap: usize) {
    closes.push_back(close);
    while closes.len() > cap {
        closes.pop_front();
    }
}

/// Return over `lookback` bars, once known.
pub(super) fn momentum(closes: &VecDeque<f64>, lookback: usize) -> Option<f64> {
    if closes.len() <= lookback {
        return None;
    }
    let now = *closes.back()?;
    let then = *closes.get(closes.len() - 1 - lookback)?;
    (then > 0.0).then(|| now / then - 1.0)
}

/// Time-series momentum on one asset: long (vol-sized) while the `lookback` return is positive;
/// with shorts allowed, short while it is negative (perpetuals, step 1 of ADR 0007).
#[derive(Debug, Clone)]
pub struct TrendMomentum {
    lookback: usize,
    /// When set, the signal is the share of these lookbacks with a positive return (an
    /// ensemble, R-2) instead of the sign over `lookback`.
    lookbacks: Vec<usize>,
    sizing: VolTarget,
    short: bool,
    closes: VecDeque<f64>,
    held: Option<f64>,
}

impl TrendMomentum {
    pub fn new(lookback: usize, sizing: VolTarget) -> Self {
        Self {
            lookback: lookback.max(1),
            lookbacks: Vec::new(),
            sizing,
            short: false,
            closes: VecDeque::new(),
            held: None,
        }
    }

    /// Average the trend over several lookbacks (each `>= 1`): the position is the share of
    /// them with a positive return (minus the share negative, with shorts).
    #[must_use]
    pub fn with_lookbacks(mut self, lookbacks: &[usize]) -> Self {
        self.lookbacks = lookbacks.iter().copied().filter(|l| *l >= 1).collect();
        self
    }

    /// The trend signal in `-1..=1` (`0..=1` without shorts); `None` before any lookback is
    /// known.
    fn signal(&self) -> Option<f64> {
        if self.lookbacks.is_empty() {
            let m = momentum(&self.closes, self.lookback)?;
            return Some(if m > 0.0 {
                1.0
            } else if self.short && m < 0.0 {
                -1.0
            } else {
                0.0
            });
        }
        let known: Vec<f64> = self
            .lookbacks
            .iter()
            .filter_map(|l| momentum(&self.closes, *l))
            .collect();
        if known.is_empty() {
            return None;
        }
        let up = known.iter().filter(|m| **m > 0.0).count() as f64;
        let down = known.iter().filter(|m| **m < 0.0).count() as f64;
        let n = known.len() as f64;
        Some(if self.short { (up - down) / n } else { up / n })
    }

    /// Also hold a short while the trend is down.
    #[must_use]
    pub const fn with_shorts(mut self, short: bool) -> Self {
        self.short = short;
        self
    }
}

impl Strategy for TrendMomentum {
    fn name(&self) -> &'static str {
        "trend-momentum"
    }

    fn on_bar(&mut self, _instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let longest = self.lookbacks.iter().copied().max().unwrap_or(0);
        let cap = self.lookback.max(longest).max(self.sizing.vol_bars) + 1;
        push_close(&mut self.closes, bar.close, cap);
        let signal = self.signal()?;
        let next = if signal == 0.0 {
            0.0
        } else {
            signal * self.sizing.scale(&self.closes)
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
    /// Instruments it may choose from (`None` = all).
    universe: Option<BTreeSet<InstrumentId>>,
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
            universe: None,
        }
    }

    fn rank(&mut self) {
        let mut scored: Vec<(&InstrumentId, f64)> = self
            .closes
            .iter()
            .filter(|(id, _)| self.universe.as_ref().is_none_or(|u| u.contains(*id)))
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

    fn set_universe(&mut self, members: &BTreeSet<InstrumentId>) {
        self.universe = Some(members.clone());
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
