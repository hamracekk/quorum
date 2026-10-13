//! Portfolio-level volatility targeting: size the whole book, not each coin alone.
//!
//! Per-coin volatility sizing ignores that crypto coins fall together: in a crash correlations
//! jump towards one and a book of individually "60%-vol" positions behaves like one big bet.
//! [`PortfolioVolTarget`] wraps any strategy; at each new timestamp it estimates the
//! portfolio's annualised volatility from the inner strategy's current weights and the
//! instruments' covariance over the last `window` returns (only timestamps both instruments
//! traded), and scales every target by `min(1, target_vol / portfolio vol)`. The scale moves
//! only when it changes by more than `band` (relative), so it does not churn. Returns use
//! closes up to the previous timestamp, so feed order never matters.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::strategy::{ExternalCall, Strategy};
use crate::types::Bar;

/// How the book is sized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortfolioVolRule {
    /// Annualised portfolio volatility to stay under.
    pub target_vol: f64,
    /// Returns behind the covariance.
    pub window: usize,
    /// Bars per year (365 for daily bars).
    pub periods_per_year: f64,
    /// A new scale is applied only when it differs from the current one by more than this
    /// fraction.
    pub band: f64,
}

/// A strategy whose whole book is scaled to a portfolio volatility target.
#[derive(Debug)]
pub struct PortfolioVolTarget {
    inner: Box<dyn Strategy>,
    rule: PortfolioVolRule,
    /// Per instrument: `(timestamp index, return)`, newest last.
    returns: BTreeMap<InstrumentId, VecDeque<(usize, f64)>>,
    last_close: BTreeMap<InstrumentId, f64>,
    current: Option<DateTime<Utc>>,
    stamps: usize,
    scale: f64,
    desired: BTreeMap<InstrumentId, f64>,
    sent: BTreeMap<InstrumentId, f64>,
}

impl PortfolioVolTarget {
    pub fn new(inner: Box<dyn Strategy>, rule: PortfolioVolRule) -> Self {
        Self {
            inner,
            rule: PortfolioVolRule {
                window: rule.window.max(2),
                ..rule
            },
            returns: BTreeMap::new(),
            last_close: BTreeMap::new(),
            current: None,
            stamps: 0,
            scale: 1.0,
            desired: BTreeMap::new(),
            sent: BTreeMap::new(),
        }
    }

    /// The current scale (1 = untouched).
    pub const fn scale(&self) -> f64 {
        self.scale
    }

    /// Annualised volatility of the current weights, `None` until enough history.
    fn portfolio_vol(&self) -> Option<f64> {
        let held: Vec<(&InstrumentId, f64)> = self
            .desired
            .iter()
            .filter(|(_, w)| w.abs() > f64::EPSILON)
            .map(|(id, w)| (id, *w))
            .collect();
        if held.is_empty() {
            return None;
        }
        let series: Vec<BTreeMap<usize, f64>> = held
            .iter()
            .map(|(id, _)| {
                self.returns
                    .get(*id)
                    .map(|r| r.iter().copied().collect())
                    .unwrap_or_default()
            })
            .collect();
        if series.iter().any(|s| s.len() < self.rule.window / 2) {
            return None;
        }
        let mut variance = 0.0;
        for (i, (_, wi)) in held.iter().enumerate() {
            for (j, (_, wj)) in held.iter().enumerate().skip(i) {
                let cov = covariance(&series[i], &series[j]);
                let pair = if i == j { 1.0 } else { 2.0 };
                variance = (pair * wi * wj).mul_add(cov, variance);
            }
        }
        Some((variance.max(0.0) * self.rule.periods_per_year).sqrt())
    }

    fn rescale(&mut self) {
        let Some(vol) = self.portfolio_vol().filter(|v| *v > 0.0) else {
            return;
        };
        let next = (self.rule.target_vol / vol).min(1.0);
        let moved = (next - self.scale).abs() / self.scale.max(f64::EPSILON);
        if moved > self.rule.band || (next >= 1.0 && self.scale < 1.0) {
            self.scale = next;
        }
    }
}

/// Covariance of two return series over the timestamps both have.
fn covariance(a: &BTreeMap<usize, f64>, b: &BTreeMap<usize, f64>) -> f64 {
    let pairs: Vec<(f64, f64)> = a
        .iter()
        .filter_map(|(t, x)| Some((*x, *b.get(t)?)))
        .collect();
    let n = pairs.len();
    if n < 2 {
        return 0.0;
    }
    let (ma, mb) = pairs
        .iter()
        .fold((0.0, 0.0), |(sa, sb), (x, y)| (sa + x, sb + y));
    let (ma, mb) = (ma / n as f64, mb / n as f64);
    pairs.iter().map(|(x, y)| (x - ma) * (y - mb)).sum::<f64>() / (n as f64 - 1.0)
}

impl Strategy for PortfolioVolTarget {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if self.current.is_none_or(|ts| bar.ts > ts) {
            // A new timestamp: every return up to the previous one is known.
            if self.current.is_some() {
                self.rescale();
                self.stamps += 1;
            }
            self.current = Some(bar.ts);
        }
        if let Some(previous) = self.last_close.insert(instrument.clone(), bar.close)
            && previous > 0.0
            && bar.close > 0.0
        {
            let returns = self.returns.entry(instrument.clone()).or_default();
            returns.push_back((self.stamps, (bar.close / previous).ln()));
            while returns.len() > self.rule.window {
                returns.pop_front();
            }
        }
        if let Some(target) = self.inner.on_bar(instrument, bar) {
            self.desired.insert(instrument.clone(), target);
        }
        let want = self.desired.get(instrument)? * self.scale;
        if self
            .sent
            .get(instrument)
            .is_some_and(|last| (last - want).abs() <= f64::EPSILON)
        {
            return None;
        }
        self.sent.insert(instrument.clone(), want);
        Some(want)
    }

    fn explain(&self, instrument: &InstrumentId) -> Option<serde_json::Value> {
        let mut reason = self
            .inner
            .explain(instrument)
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(map) = reason.as_object_mut() {
            map.insert("portfolio_vol_scale".into(), self.scale.into());
        }
        Some(reason)
    }

    fn inform(&mut self, calls: &[ExternalCall]) {
        self.inner.inform(calls);
    }

    fn set_universe(&mut self, members: &BTreeSet<InstrumentId>) {
        self.inner.set_universe(members);
    }
}

#[cfg(test)]
mod tests;
