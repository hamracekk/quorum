//! An honest machine-learning benchmark (R-5): cross-sectional ridge regression.
//!
//! A [`Learner`] for [`XsLearner`]: features standardised on the training set, an unpenalised
//! intercept, ridge penalty [`LAMBDA`]. Any other learner has to beat it walk-forward.

use super::learner::{FEATURES, Learner, Row, XsLearner};
use super::momentum::VolTarget;

/// Ridge penalty on the standardised features.
const LAMBDA: f64 = 1.0;

/// Ridge regression as a [`Learner`].
#[derive(Debug, Clone, Default)]
pub struct Ridge {
    model: Option<Model>,
}

impl Learner for Ridge {
    fn name(&self) -> &'static str {
        "xs-ridge"
    }

    fn fit(&mut self, samples: &[&(Row, f64)]) -> bool {
        self.model = Model::fit(samples);
        self.model.is_some()
    }

    fn predict(&self, row: &Row) -> f64 {
        self.model.as_ref().map_or(0.0, |m| m.predict(row))
    }
}

/// The cross-sectional ridge strategy.
#[derive(Debug)]
pub struct XsRidge;

impl XsRidge {
    /// Hold the `top` instruments ridge ranks highest, re-ranked every `rebalance` timestamps,
    /// trained on the last `window` labelled snapshots.
    pub fn strategy(top: usize, rebalance: usize, window: usize, sizing: VolTarget) -> XsLearner {
        XsLearner::new(Box::new(Ridge::default()), top, rebalance, window, sizing)
    }
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

#[cfg(test)]
mod tests;
