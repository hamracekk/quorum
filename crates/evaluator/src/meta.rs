//! The meta model in shadow (DESIGN §10.1): does anything in the case files predict which
//! proposed entries win, well enough to beat "always take" out of sample?
//!
//! A regularised logistic regression on every case feature plus the regime, judged walk-forward:
//! the latest `out_of_sample` share of closed cases is split into `folds` in time order, and each
//! fold is predicted by a model trained only on cases whose exit filled before the fold began
//! (purged: no label overlaps the cases it predicts). The policy "take only entries the model
//! rates above the earlier win rate" is then compared with taking every entry. Gradient-boosted
//! trees replace the regression once this shows there is something to learn.

use std::collections::BTreeSet;

use quorum_core::regime::Regime;
use serde::Serialize;

use crate::case::{Case, EXIT};
use crate::study::auc;

#[derive(Debug, Clone, Copy)]
pub struct MetaConfig {
    pub out_of_sample: f64,
    pub folds: usize,
    /// L2 penalty on the standardised weights.
    pub l2: f64,
    pub iterations: usize,
    pub learning_rate: f64,
    /// Fewest training cases a fold needs.
    pub min_train: usize,
    /// One-sided z the out-of-sample evidence needs (a single model is tested).
    pub z: f64,
}

impl Default for MetaConfig {
    fn default() -> Self {
        Self {
            out_of_sample: 0.3,
            folds: 3,
            l2: 1.0,
            iterations: 400,
            learning_rate: 0.1,
            min_train: 200,
            z: 1.645,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetaVerdict {
    /// Ranks winners out of sample, and filtering on it beat always taking.
    BeatsAlwaysTake,
    NoEdge,
    /// Not enough closed cases to train and test.
    TooFew,
}

/// How the meta model did on the cases it never trained on.
#[derive(Debug, Clone, Serialize)]
pub struct MetaReport {
    pub features: Vec<String>,
    /// Closed cases predicted out of sample.
    pub n_out: usize,
    pub auc_out: f64,
    pub z_out: f64,
    /// Mean net at the exit of every entry predicted.
    pub always_mean: f64,
    pub taken_n: usize,
    pub taken_mean: f64,
    pub skipped_n: usize,
    pub skipped_mean: f64,
    /// t of the skipped entries' mean (negative: they lost).
    pub skipped_t: f64,
    /// The last fold's weights by feature, largest magnitude first (standardised units).
    pub weights: Vec<(String, f64)>,
    pub verdict: MetaVerdict,
}

/// Trains and tests the meta model on `cases` (any order).
#[must_use]
pub fn shadow(cases: &[Case], cfg: &MetaConfig) -> MetaReport {
    let mut closed: Vec<(&Case, f64)> = cases
        .iter()
        .filter_map(|c| c.outcomes.get(EXIT).map(|r| (c, *r)))
        .collect();
    closed.sort_by_key(|(c, _)| c.ts);
    let names: Vec<String> = closed
        .iter()
        .flat_map(|(c, _)| c.features.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .chain(Regime::ALL.iter().map(|r| format!("regime:{}", r.as_str())))
        .collect();
    let x: Vec<Vec<Option<f64>>> = closed.iter().map(|(c, _)| row(c, &names)).collect();
    let n = closed.len();
    let start = n - held_out(n, cfg.out_of_sample);
    let fold_len = (n - start).div_ceil(cfg.folds.max(1)).max(1);

    let mut predicted: Vec<(f64, f64)> = Vec::new(); // (probability, net)
    let mut weights = Vec::new();
    let mut k = start;
    while k < n {
        let end = (k + fold_len).min(n);
        let fold_start = closed[k].0.ts;
        let train: Vec<usize> = (0..k)
            .filter(|&i| closed[i].0.exit_ts.is_some_and(|e| e < fold_start))
            .collect();
        if train.len() >= cfg.min_train {
            let model = Model::fit(&x, &closed, &train, cfg);
            predicted.extend((k..end).map(|i| (model.predict(&x[i]), closed[i].1)));
            weights = model.named_weights(&names);
        }
        k = end;
    }

    let scored: Vec<(f64, bool)> = predicted.iter().map(|(p, r)| (*p, *r > 0.0)).collect();
    let (auc_out, z_out) = auc(&scored);
    let always: Vec<f64> = predicted.iter().map(|(_, r)| *r).collect();
    // The bar: the win rate before the first fold, known to every model trained.
    let earlier: Vec<f64> = closed[..start].iter().map(|(_, r)| *r).collect();
    let bar = earlier.iter().filter(|r| **r > 0.0).count() as f64 / earlier.len().max(1) as f64;
    let taken: Vec<f64> = predicted
        .iter()
        .filter(|(p, _)| *p >= bar)
        .map(|(_, r)| *r)
        .collect();
    let skipped: Vec<f64> = predicted
        .iter()
        .filter(|(p, _)| *p < bar)
        .map(|(_, r)| *r)
        .collect();
    let skipped_t = t_stat(&skipped);
    let verdict = if predicted.len() < cfg.min_train / 2 || taken.is_empty() || skipped.is_empty() {
        MetaVerdict::TooFew
    } else if z_out >= cfg.z && mean(&taken) > mean(&always) && skipped_t <= -cfg.z {
        MetaVerdict::BeatsAlwaysTake
    } else {
        MetaVerdict::NoEdge
    };
    MetaReport {
        features: names,
        n_out: predicted.len(),
        auc_out,
        z_out,
        always_mean: mean(&always),
        taken_n: taken.len(),
        taken_mean: mean(&taken),
        skipped_n: skipped.len(),
        skipped_mean: mean(&skipped),
        skipped_t,
        weights,
        verdict,
    }
}

/// How many of `n` time-ordered cases are held out.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a count between 0 and n, rounded"
)]
fn held_out(n: usize, share: f64) -> usize {
    (((n as f64) * share.clamp(0.0, 1.0)).round() as usize).min(n)
}

/// A case's feature values in `names` order; regimes one-hot.
fn row(case: &Case, names: &[String]) -> Vec<Option<f64>> {
    names
        .iter()
        .map(|name| {
            name.strip_prefix("regime:").map_or_else(
                || case.features.get(name).copied().filter(|v| v.is_finite()),
                |r| {
                    Some(f64::from(u8::from(
                        case.regime.is_some_and(|g| g.as_str() == r),
                    )))
                },
            )
        })
        .collect()
}

/// Logistic regression on standardised features; missing values sit at the training mean.
struct Model {
    means: Vec<f64>,
    sds: Vec<f64>,
    weights: Vec<f64>,
    bias: f64,
}

impl Model {
    fn fit(
        x: &[Vec<Option<f64>>],
        closed: &[(&Case, f64)],
        train: &[usize],
        cfg: &MetaConfig,
    ) -> Self {
        let width = x.first().map_or(0, Vec::len);
        let mut means = vec![0.0; width];
        let mut sds = vec![1.0; width];
        for j in 0..width {
            let values: Vec<f64> = train.iter().filter_map(|&i| x[i][j]).collect();
            if values.len() > 1 {
                let m = mean(&values);
                let var =
                    values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (values.len() - 1) as f64;
                means[j] = m;
                sds[j] = if var > 0.0 { var.sqrt() } else { 1.0 };
            }
        }
        let mut model = Self {
            means,
            sds,
            weights: vec![0.0; width],
            bias: 0.0,
        };
        let rows: Vec<Vec<f64>> = train.iter().map(|&i| model.standardise(&x[i])).collect();
        let labels: Vec<f64> = train
            .iter()
            .map(|&i| f64::from(u8::from(closed[i].1 > 0.0)))
            .collect();
        let count = rows.len() as f64;
        for _ in 0..cfg.iterations {
            let mut grad = vec![0.0; width];
            let mut grad_bias = 0.0;
            for (z, y) in rows.iter().zip(&labels) {
                let error = model.probability(z) - y;
                for (g, v) in grad.iter_mut().zip(z) {
                    *g = error.mul_add(*v, *g);
                }
                grad_bias += error;
            }
            for (w, g) in model.weights.iter_mut().zip(&grad) {
                *w -= cfg.learning_rate * cfg.l2.mul_add(*w, *g) / count;
            }
            model.bias -= cfg.learning_rate * grad_bias / count;
        }
        model
    }

    fn standardise(&self, row: &[Option<f64>]) -> Vec<f64> {
        row.iter()
            .zip(self.means.iter().zip(&self.sds))
            .map(|(v, (m, s))| v.map_or(0.0, |v| (v - m) / s))
            .collect()
    }

    fn probability(&self, z: &[f64]) -> f64 {
        let logit = self
            .weights
            .iter()
            .zip(z)
            .fold(self.bias, |acc, (w, v)| w.mul_add(*v, acc));
        1.0 / (1.0 + (-logit).exp())
    }

    fn predict(&self, row: &[Option<f64>]) -> f64 {
        self.probability(&self.standardise(row))
    }

    fn named_weights(&self, names: &[String]) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = names.iter().cloned().zip(self.weights.clone()).collect();
        out.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()));
        out
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn t_stat(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let count = xs.len() as f64;
    let sd = (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (count - 1.0)).sqrt();
    if sd > 0.0 { m / sd * count.sqrt() } else { 0.0 }
}
