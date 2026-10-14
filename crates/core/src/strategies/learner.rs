//! Cross-sectional learners (R-5, M9): a model ranks instruments by predicted return.
//!
//! Every `rebalance` timestamps [`XsLearner`] takes a snapshot of each instrument's features —
//! log returns over 7, 30, 90 and 180 bars, 30-bar volatility and the 30- vs 180-bar
//! dollar-volume trend — and, at the next rebalance, labels the previous snapshot with the
//! return realised since. Only labelled snapshots train the model (refitted at every rebalance
//! on the last `window` of them), so nothing it learns comes from the future. It holds, in equal
//! shares (volatility sized), the `top` instruments with the highest positive prediction.
//!
//! The model is a [`Learner`]: ridge regression ([`super::XsRidge`]) is the honest linear
//! benchmark; other learners (gradient-boosted trees in the `ml` crate) plug in the same way and
//! are judged walk-forward against it.
//!
//! Features use closes up to the **previous** timestamp, computed when the first bar of a new
//! timestamp arrives, so feed order cannot change the result; an instrument that did not trade
//! in that timestamp is skipped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use chrono::{DateTime, Utc};

use super::momentum::VolTarget;
use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// Features per instrument.
pub const FEATURES: usize = 6;
/// Bars of history the features need.
pub const HISTORY: usize = 181;
/// Labelled snapshots needed before the model trades.
pub const MIN_SNAPSHOTS: usize = 12;

/// One instrument's features at a rebalance.
pub type Row = [f64; FEATURES];

/// A model that learns predicted returns from labelled feature rows.
pub trait Learner: fmt::Debug + Send {
    /// The strategy's name, e.g. `xs-ridge`.
    fn name(&self) -> &'static str;
    /// Fit on `(features, realised return)` pairs; `false` when it cannot (too few samples, a
    /// degenerate system) — then it predicts nothing until the next successful fit.
    fn fit(&mut self, samples: &[&(Row, f64)]) -> bool;
    /// Predicted return for `row` (only called after a successful `fit`).
    fn predict(&self, row: &Row) -> f64;
}

/// The six features from closes and dollar volumes (newest last), once 181 bars are known.
pub fn features(closes: &VecDeque<f64>, dollars: &VecDeque<f64>) -> Option<Row> {
    if closes.len() < HISTORY || dollars.len() < HISTORY - 1 {
        return None;
    }
    let at = |back: usize| closes.get(closes.len() - 1 - back).copied();
    let now = at(0)?;
    let ret = |back: usize| -> Option<f64> {
        let then = at(back)?;
        (then > 0.0 && now > 0.0).then(|| (now / then).ln())
    };
    let logs: Vec<f64> = (0..30)
        .filter_map(|i| {
            let (b, a) = (at(i)?, at(i + 1)?);
            (a > 0.0 && b > 0.0).then(|| (b / a).ln())
        })
        .collect();
    let mean = logs.iter().sum::<f64>() / logs.len().max(1) as f64;
    let vol = (logs.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
        / (logs.len().max(2) - 1) as f64)
        .sqrt();
    let avg = |n: usize| dollars.iter().rev().take(n).sum::<f64>() / n as f64;
    let (short, long) = (avg(30), avg(180));
    let volume = if short > 0.0 && long > 0.0 {
        (short / long).ln()
    } else {
        0.0
    };
    Some([ret(7)?, ret(30)?, ret(90)?, ret(180)?, vol, volume])
}

/// One rebalance's features, waiting for (or holding) their labels.
#[derive(Debug, Clone)]
struct Snapshot {
    rows: Vec<(InstrumentId, Row, f64)>,
}

/// A cross-sectional strategy driven by a [`Learner`].
#[derive(Debug)]
pub struct XsLearner {
    model: Box<dyn Learner>,
    top: usize,
    rebalance: usize,
    /// Labelled snapshots kept for training.
    window: usize,
    sizing: VolTarget,
    closes: BTreeMap<InstrumentId, VecDeque<f64>>,
    dollars: BTreeMap<InstrumentId, VecDeque<f64>>,
    seen: BTreeMap<InstrumentId, usize>,
    current: Option<DateTime<Utc>>,
    stamps: usize,
    pending: Option<Snapshot>,
    /// Labelled (features, realised return) pairs, oldest snapshot first.
    training: VecDeque<Vec<(Row, f64)>>,
    chosen: BTreeMap<InstrumentId, f64>,
    predictions: BTreeMap<InstrumentId, f64>,
    ranked: bool,
    held: BTreeMap<InstrumentId, f64>,
    /// Instruments it may train on and choose from (`None` = all).
    universe: Option<BTreeSet<InstrumentId>>,
}

impl XsLearner {
    /// Hold the `top` instruments `model` ranks highest, re-ranked every `rebalance`
    /// timestamps, trained on the last `window` labelled snapshots.
    pub fn new(
        model: Box<dyn Learner>,
        top: usize,
        rebalance: usize,
        window: usize,
        sizing: VolTarget,
    ) -> Self {
        Self {
            model,
            top: top.max(1),
            rebalance: rebalance.max(1),
            window: window.max(MIN_SNAPSHOTS),
            sizing,
            closes: BTreeMap::new(),
            dollars: BTreeMap::new(),
            seen: BTreeMap::new(),
            current: None,
            stamps: 0,
            pending: None,
            training: VecDeque::new(),
            chosen: BTreeMap::new(),
            predictions: BTreeMap::new(),
            ranked: false,
            held: BTreeMap::new(),
            universe: None,
        }
    }

    /// Features of every instrument that traded in the timestamp just completed.
    fn snapshot(&self) -> Snapshot {
        let rows = self
            .closes
            .iter()
            .filter(|(id, _)| self.seen.get(*id).is_some_and(|s| s + 1 >= self.stamps))
            .filter(|(id, _)| self.universe.as_ref().is_none_or(|u| u.contains(*id)))
            .filter_map(|(id, closes)| {
                let row = features(closes, self.dollars.get(id)?)?;
                Some((id.clone(), row, *closes.back()?))
            })
            .collect();
        Snapshot { rows }
    }

    fn rebalance_now(&mut self) {
        let now = self.snapshot();
        // Label the previous snapshot with what each instrument returned since — from its last
        // known close, so a coin that left the universe or stopped trading still counts.
        if let Some(previous) = self.pending.take() {
            let labelled: Vec<(Row, f64)> = previous
                .rows
                .iter()
                .filter_map(|(id, row, then)| {
                    let close = self.closes.get(id)?.back()?;
                    (*then > 0.0).then(|| (*row, close / then - 1.0))
                })
                .collect();
            if !labelled.is_empty() {
                self.training.push_back(labelled);
            }
            while self.training.len() > self.window {
                self.training.pop_front();
            }
        }
        self.predictions.clear();
        if self.training.len() >= MIN_SNAPSHOTS {
            let samples: Vec<&(Row, f64)> = self.training.iter().flatten().collect();
            if self.model.fit(&samples) {
                self.predictions = now
                    .rows
                    .iter()
                    .map(|(id, row, _)| (id.clone(), self.model.predict(row)))
                    .collect();
            }
        }
        let mut scored: Vec<(&InstrumentId, f64)> = self
            .predictions
            .iter()
            .filter(|(_, p)| **p > 0.0)
            .map(|(id, p)| (id, *p))
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
        self.pending = Some(now);
        self.ranked = true;
    }
}

impl Strategy for XsLearner {
    fn name(&self) -> &'static str {
        self.model.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if self.current.is_none_or(|ts| bar.ts > ts) {
            // A new timestamp: every instrument's history is complete up to the previous one.
            if self.current.is_some() {
                if self.stamps.is_multiple_of(self.rebalance) {
                    self.rebalance_now();
                }
                self.stamps += 1;
            }
            self.current = Some(bar.ts);
        }
        let cap = HISTORY.max(self.sizing.vol_bars + 1);
        let closes = self.closes.entry(instrument.clone()).or_default();
        closes.push_back(bar.close);
        while closes.len() > cap {
            closes.pop_front();
        }
        let dollars = self.dollars.entry(instrument.clone()).or_default();
        dollars.push_back(bar.close * bar.volume);
        while dollars.len() > HISTORY {
            dollars.pop_front();
        }
        self.seen.insert(instrument.clone(), self.stamps);
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
            "top": self.top,
            "predicted": self.predictions.get(instrument),
            "snapshots": self.training.len(),
            "chosen": self.chosen.contains_key(instrument),
            "target": self.held.get(instrument),
        }))
    }
}
