//! Trading slots: hold the best `slots` instruments of the universe, ranked daily.
//!
//! Each rebalance ranks every instrument by a **score**: its trend strength — the `lookback`
//! return over its annualised volatility, the part with a walk-forward record — plus
//! `conviction_weight` times the **evidence-weighted conviction** of the outside sources (news
//! readings, impact-map links, agents' suggestions, prediction markets, followed traders) from an
//! inner [`Ensemble`], which weighs a source only once it has earned an edge walk-forward (no
//! record, no weight). So today the ranking is the trend; AI and news move it as they prove
//! themselves.
//!
//! Slots are kept filled: holdings keep their slot while they rank within `slots + buffer`
//! (no trading in and out over small rank changes), the rest are filled from the top. With
//! `fill_always` off, only instruments with a positive score take a slot. Each slot gets an equal
//! share, volatility-scaled; the portfolio-level brakes wrap it as for any strategy.
//!
//! Like the other cross-sectional strategies, scores use closes up to the **previous** timestamp,
//! computed when the first bar of a new timestamp arrives, so feed order cannot change them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};

use super::ensemble::{Ensemble, EnsembleConfig};
use super::momentum::{VolTarget, momentum, push_close};
use crate::instrument::InstrumentId;
use crate::strategy::{ExternalCall, Strategy};
use crate::types::Bar;

/// How the slots are chosen.
#[derive(Debug, Clone, Copy)]
pub struct SlotsConfig {
    /// Most holdings at once.
    pub slots: usize,
    /// Trend lookback in bars.
    pub lookback: usize,
    /// Re-rank every this many timestamps.
    pub rebalance: usize,
    /// Fill every slot even with a negative score (`false`: positive scores only).
    pub fill_always: bool,
    /// Weight of the sources' conviction (−1…1) next to the trend score.
    pub conviction_weight: f64,
    pub sizing: VolTarget,
}

/// The slots strategy.
#[derive(Debug)]
pub struct Slots {
    cfg: SlotsConfig,
    evidence: Ensemble,
    closes: BTreeMap<InstrumentId, VecDeque<f64>>,
    current: Option<DateTime<Utc>>,
    stamps: usize,
    chosen: BTreeMap<InstrumentId, f64>,
    scores: BTreeMap<InstrumentId, (f64, f64, f64)>,
    ranked: bool,
    held: BTreeMap<InstrumentId, f64>,
    universe: Option<BTreeSet<InstrumentId>>,
}

/// Annualised volatility of the last `bars` log returns in `closes`.
fn volatility(closes: &VecDeque<f64>, bars: usize, per_year: f64) -> Option<f64> {
    let take = bars.min(closes.len().saturating_sub(1));
    if take < 2 {
        return None;
    }
    let recent: Vec<f64> = closes
        .iter()
        .skip(closes.len() - take - 1)
        .copied()
        .collect();
    let returns: Vec<f64> = recent
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    let n = returns.len() as f64;
    if n < 2.0 {
        return None;
    }
    let mean = returns.iter().sum::<f64>() / n;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let vol = (var * per_year).sqrt();
    (vol > 0.0).then_some(vol)
}

impl Slots {
    /// `evidence` configures the inner ensemble that weighs `calls` by their record.
    pub fn new(cfg: SlotsConfig, evidence: EnsembleConfig, calls: Vec<ExternalCall>) -> Self {
        Self {
            cfg: SlotsConfig {
                slots: cfg.slots.max(1),
                lookback: cfg.lookback.max(1),
                rebalance: cfg.rebalance.max(1),
                ..cfg
            },
            evidence: Ensemble::new(evidence, calls),
            closes: BTreeMap::new(),
            current: None,
            stamps: 0,
            chosen: BTreeMap::new(),
            scores: BTreeMap::new(),
            ranked: false,
            held: BTreeMap::new(),
            universe: None,
        }
    }

    /// `(score, trend, conviction)` of every instrument with enough history, as of `now`.
    fn score_all(&self, now: DateTime<Utc>) -> BTreeMap<InstrumentId, (f64, f64, f64)> {
        let per_year = self.cfg.sizing.periods_per_year;
        self.closes
            .iter()
            .filter(|(id, _)| self.universe.as_ref().is_none_or(|u| u.contains(*id)))
            .filter_map(|(id, closes)| {
                let ret = momentum(closes, self.cfg.lookback)?;
                let vol = volatility(closes, self.cfg.lookback, per_year)?;
                // Annualised return over annualised risk: a Sharpe-like trend strength.
                let years = self.cfg.lookback as f64 / per_year;
                let trend = (ret / years.max(f64::EPSILON)) / vol;
                let (conviction, _) = self.evidence.conviction(id, now);
                let score = self.cfg.conviction_weight.mul_add(conviction, trend);
                Some((id.clone(), (score, trend, conviction)))
            })
            .collect()
    }

    fn rank(&mut self, now: DateTime<Utc>) {
        self.scores = self.score_all(now);
        let mut order: Vec<(&InstrumentId, f64)> =
            self.scores.iter().map(|(id, s)| (id, s.0)).collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let eligible = |score: f64| self.cfg.fill_always || score > 0.0;
        let keep_within = self.cfg.slots + self.cfg.slots.div_ceil(4);
        // Holdings keep their slot while they rank within the buffer…
        let mut picked: Vec<&InstrumentId> = order
            .iter()
            .take(keep_within)
            .filter(|(id, s)| eligible(*s) && self.held.get(*id).is_some_and(|w| *w > 0.0))
            .map(|(id, _)| *id)
            .take(self.cfg.slots)
            .collect();
        // …then the best of the rest fill what is left.
        for (id, score) in &order {
            if picked.len() >= self.cfg.slots {
                break;
            }
            if eligible(*score) && !picked.contains(id) {
                picked.push(id);
            }
        }
        let share = 1.0 / self.cfg.slots as f64;
        self.chosen = picked
            .into_iter()
            .map(|id| {
                let scale = self
                    .closes
                    .get(id)
                    .map_or(1.0, |c| self.cfg.sizing.scale(c));
                (id.clone(), share * scale)
            })
            .collect();
        self.ranked = true;
    }
}

impl Strategy for Slots {
    fn name(&self) -> &'static str {
        "slots"
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if self.current.is_none_or(|ts| bar.ts > ts) {
            // A new timestamp: every instrument's closes are complete up to the previous one.
            if let Some(previous) = self.current {
                if self.stamps.is_multiple_of(self.cfg.rebalance) {
                    self.rank(previous);
                }
                self.stamps += 1;
            }
            self.current = Some(bar.ts);
        }
        // The evidence keeps its own bars and weights; its targets are not used.
        let _ = self.evidence.on_bar(instrument, bar);
        let cap = self.cfg.lookback.max(self.cfg.sizing.vol_bars) + 1;
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
            .cfg
            .sizing
            .banded(self.held.get(instrument).copied(), next)?;
        self.held.insert(instrument.clone(), target);
        Some(target)
    }

    fn inform(&mut self, calls: &[ExternalCall]) {
        self.evidence.inform(calls);
    }

    fn set_universe(&mut self, members: &BTreeSet<InstrumentId>) {
        self.universe = Some(members.clone());
    }

    fn explain(&self, instrument: &InstrumentId) -> Option<serde_json::Value> {
        let (score, trend, conviction) = self.scores.get(instrument).copied().unwrap_or_default();
        let (_, contributors) = self
            .current
            .map(|now| self.evidence.conviction(instrument, now))
            .unwrap_or_default();
        Some(serde_json::json!({
            "slots": self.cfg.slots,
            "score": score,
            "trend": trend,
            "conviction": conviction,
            "contributors": contributors,
            "in_slot": self.chosen.contains_key(instrument),
            "target": self.held.get(instrument),
        }))
    }
}

#[cfg(test)]
mod tests;
