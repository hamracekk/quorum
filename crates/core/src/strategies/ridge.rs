//! An honest machine-learning benchmark (R-5): a cross-sectional ridge regression.
//!
//! Every `rebalance` timestamps it takes a snapshot of each instrument's features — log returns
//! over 7, 30, 90 and 180 bars, 30-bar volatility and the 30- vs 180-bar dollar-volume trend —
//! and, at the next rebalance, labels the previous snapshot with the return realised since. Only
//! labelled snapshots train the model (refitted at every rebalance on the last `window` of
//! them, features standardised on that training set, an unpenalised intercept, ridge penalty
//! [`LAMBDA`]), so nothing it learns comes from the future. It holds, in equal shares (volatility
//! sized), the `top` instruments with the highest positive predicted return.
//!
//! Like [`XsMomentum`](super::XsMomentum), features use closes up to the **previous**
//! timestamp, computed when the first bar of a new timestamp arrives, so feed order cannot
//! change the result; an instrument that did not trade in that timestamp is skipped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};

use super::momentum::VolTarget;
use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// Features per instrument (an intercept is added to the model).
const FEATURES: usize = 6;
/// Bars of history the features need.
const HISTORY: usize = 181;
/// Ridge penalty on the standardised features.
const LAMBDA: f64 = 1.0;
/// Labelled snapshots needed before the model trades.
const MIN_SNAPSHOTS: usize = 12;

type Row = [f64; FEATURES];

/// One rebalance's features, waiting for (or holding) their labels.
#[derive(Debug, Clone)]
struct Snapshot {
    rows: Vec<(InstrumentId, Row, f64)>,
}

/// Cross-sectional ridge regression on price and volume features.
#[derive(Debug, Clone)]
pub struct XsRidge {
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

impl XsRidge {
    pub fn new(top: usize, rebalance: usize, window: usize, sizing: VolTarget) -> Self {
        Self {
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
            if let Some(model) = Model::fit(&samples) {
                self.predictions = now
                    .rows
                    .iter()
                    .map(|(id, row, _)| (id.clone(), model.predict(row)))
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

/// The six features from closes and dollar volumes (newest last), once 181 bars are known.
fn features(closes: &VecDeque<f64>, dollars: &VecDeque<f64>) -> Option<Row> {
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

/// A fitted ridge model on standardised features.
#[derive(Debug, Clone)]
struct Model {
    mean: Row,
    scale: Row,
    intercept: f64,
    beta: Row,
}

impl Model {
    fn fit(samples: &[&(Row, f64)]) -> Option<Self> {
        let n = samples.len();
        if n < FEATURES * 2 {
            return None;
        }
        let mut mean = [0.0; FEATURES];
        let mut scale = [0.0; FEATURES];
        for j in 0..FEATURES {
            mean[j] = samples.iter().map(|s| s.0[j]).sum::<f64>() / n as f64;
            let var = samples
                .iter()
                .map(|s| (s.0[j] - mean[j]).powi(2))
                .sum::<f64>()
                / n as f64;
            scale[j] = if var > 0.0 { var.sqrt() } else { 1.0 };
        }
        let y_mean = samples.iter().map(|s| s.1).sum::<f64>() / n as f64;
        // Centred data: the intercept is the label mean, the slopes solve
        // (ZᵀZ + λI) β = Zᵀ(y − ȳ).
        let mut a = [[0.0; FEATURES]; FEATURES];
        let mut b = [0.0; FEATURES];
        for (row, y) in samples.iter().map(|s| (&s.0, s.1)) {
            let z: Vec<f64> = (0..FEATURES)
                .map(|j| (row[j] - mean[j]) / scale[j])
                .collect();
            for i in 0..FEATURES {
                b[i] = z[i].mul_add(y - y_mean, b[i]);
                for k in 0..FEATURES {
                    a[i][k] = z[i].mul_add(z[k], a[i][k]);
                }
            }
        }
        for (i, row) in a.iter_mut().enumerate() {
            row[i] += LAMBDA;
        }
        let beta = solve(a, b)?;
        Some(Self {
            mean,
            scale,
            intercept: y_mean,
            beta,
        })
    }

    fn predict(&self, row: &Row) -> f64 {
        (0..FEATURES).fold(self.intercept, |acc, j| {
            ((row[j] - self.mean[j]) / self.scale[j]).mul_add(self.beta[j], acc)
        })
    }
}

/// Solve `a · x = b` by Gaussian elimination with partial pivoting.
fn solve(mut a: [[f64; FEATURES]; FEATURES], mut b: Row) -> Option<Row> {
    for col in 0..FEATURES {
        let pivot = (col..FEATURES).max_by(|&i, &k| a[i][col].abs().total_cmp(&a[k][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..FEATURES {
            let factor = a[row][col] / a[col][col];
            let pivot_row = a[col];
            for (cell, above) in a[row].iter_mut().zip(pivot_row).skip(col) {
                *cell = (-factor).mul_add(above, *cell);
            }
            b[row] = (-factor).mul_add(b[col], b[row]);
        }
    }
    let mut x = [0.0; FEATURES];
    for row in (0..FEATURES).rev() {
        let known = (row + 1..FEATURES).fold(0.0, |acc, k| a[row][k].mul_add(x[k], acc));
        x[row] = (b[row] - known) / a[row][row];
    }
    Some(x)
}

impl Strategy for XsRidge {
    fn name(&self) -> &'static str {
        "xs-ridge"
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

#[cfg(test)]
mod tests;
