//! Robustness of an out-of-sample result: how wide its range of outcomes is, and where it
//! comes from.
//!
//! - **Block bootstrap:** the curve's per-bar returns resampled in blocks (keeping short-range
//!   dependence) into `samples` alternative histories of the same length; percentiles of CAGR,
//!   Sharpe and drawdown show how much of the single number is luck of the path. Seeded, so
//!   the same curve always gives the same report.
//! - **Regimes:** every bar is labelled by the benchmark's trailing return over a quarter of a
//!   year — bull above +20%, bear below −20%, sideways between — and the strategy and the
//!   benchmark are scored within each: does the edge come from one kind of market only?

use quorum_core::EquityPoint;
use serde::Serialize;

use crate::validation::{periods_per_year, returns_matrix};

/// 5th, 50th and 95th percentiles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Range {
    pub p5: f64,
    pub p50: f64,
    pub p95: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BootstrapReport {
    pub samples: usize,
    pub block: usize,
    pub cagr: Range,
    pub sharpe: Range,
    pub max_drawdown: Range,
    /// Share of the resampled histories that lost money.
    pub prob_loss: f64,
}

/// One kind of market.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegimeStats {
    /// `bull`, `bear` or `sideways`.
    pub regime: &'static str,
    /// Share of the bars in it.
    pub share: f64,
    /// Annualised mean return and Sharpe within it.
    pub strategy_return: f64,
    pub strategy_sharpe: f64,
    pub benchmark_return: f64,
    pub benchmark_sharpe: f64,
}

/// Deterministic pseudo-random numbers (a 64-bit linear congruential generator).
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    // Nearest rank: the index is rounded from a share of the length.
    let last = sorted.len() - 1;
    let at = (0..=last)
        .min_by(|a, b| {
            let target = q * last as f64;
            (*a as f64 - target)
                .abs()
                .total_cmp(&(*b as f64 - target).abs())
        })
        .unwrap_or(0);
    sorted[at]
}

fn range(mut values: Vec<f64>) -> Range {
    values.sort_by(f64::total_cmp);
    Range {
        p5: percentile(&values, 0.05),
        p50: percentile(&values, 0.5),
        p95: percentile(&values, 0.95),
    }
}

/// Annualised mean and Sharpe of `returns`.
fn stats(returns: &[f64], per_year: f64) -> (f64, f64) {
    let n = returns.len();
    if n < 2 {
        return (0.0, 0.0);
    }
    let mean = returns.iter().sum::<f64>() / n as f64;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    let sharpe = if var > 0.0 {
        mean / var.sqrt() * per_year.sqrt()
    } else {
        0.0
    };
    (mean * per_year, sharpe)
}

/// CAGR, Sharpe and maximum drawdown of a path of returns.
fn path_stats(returns: &[f64], per_year: f64) -> (f64, f64, f64) {
    let (mut equity, mut peak, mut worst) = (1.0_f64, 1.0_f64, 0.0_f64);
    for r in returns {
        equity *= 1.0 + r;
        peak = peak.max(equity);
        worst = worst.max(1.0 - equity / peak);
    }
    let years = returns.len() as f64 / per_year.max(1.0);
    let cagr = if years > 0.0 && equity > 0.0 {
        equity.powf(1.0 / years) - 1.0
    } else {
        -1.0
    };
    (cagr, stats(returns, per_year).1, worst)
}

/// Block bootstrap of `curve`'s returns (`None` with fewer than two blocks of history).
pub fn bootstrap(
    curve: &[EquityPoint],
    block: usize,
    samples: usize,
    seed: u64,
) -> Option<BootstrapReport> {
    let (stamps, rows) = returns_matrix(&[curve.to_vec()]);
    let returns = rows.into_iter().next()?;
    let block = block.max(1);
    if returns.len() < block * 2 || samples == 0 {
        return None;
    }
    let per_year = periods_per_year(&stamps);
    let mut rng = Lcg(seed);
    let (mut cagr, mut sharpe, mut dd) = (Vec::new(), Vec::new(), Vec::new());
    let mut losses = 0;
    for _ in 0..samples {
        let mut path = Vec::with_capacity(returns.len() + block);
        while path.len() < returns.len() {
            let start = rng.below(returns.len() - block + 1);
            path.extend_from_slice(&returns[start..start + block]);
        }
        let (c, s, d) = path_stats(&path[..returns.len()], per_year);
        losses += usize::from(c < 0.0);
        cagr.push(c);
        sharpe.push(s);
        dd.push(d);
    }
    Some(BootstrapReport {
        samples,
        block,
        cagr: range(cagr),
        sharpe: range(sharpe),
        max_drawdown: range(dd),
        prob_loss: losses as f64 / samples as f64,
    })
}

/// The strategy and the benchmark within bull, bear and sideways markets (labelled by the
/// benchmark's trailing quarter-year return, known at each bar).
pub fn regimes(curve: &[EquityPoint], benchmark: &[EquityPoint]) -> Vec<RegimeStats> {
    let (stamps, rows) = returns_matrix(&[curve.to_vec(), benchmark.to_vec()]);
    let (Some(strategy), Some(bench)) = (rows.first(), rows.get(1)) else {
        return Vec::new();
    };
    let per_year = periods_per_year(&stamps);
    // A quarter of a year in bars (the largest whole number of bars not above it, at least 1).
    let window = (1..=bench.len().max(1))
        .take_while(|n| (*n as f64) <= (per_year / 4.0).max(1.0))
        .last()
        .unwrap_or(1);
    let mut groups: [(Vec<f64>, Vec<f64>); 3] = Default::default();
    let mut level = 1.0_f64;
    let mut levels = Vec::with_capacity(bench.len());
    for (i, r) in bench.iter().enumerate() {
        // The label uses the trailing window up to the previous bar.
        let label = if i >= window {
            let trailing = levels[i - 1] / levels[i - window] - 1.0;
            if trailing > 0.2 {
                0
            } else if trailing < -0.2 {
                1
            } else {
                2
            }
        } else {
            2
        };
        groups[label].0.push(strategy[i]);
        groups[label].1.push(*r);
        level *= 1.0 + r;
        levels.push(level);
    }
    let total = bench.len().max(1) as f64;
    ["bull", "bear", "sideways"]
        .into_iter()
        .zip(groups)
        .filter(|(_, (s, _))| !s.is_empty())
        .map(|(regime, (s, b))| {
            let (sr, ss) = stats(&s, per_year);
            let (br, bs) = stats(&b, per_year);
            RegimeStats {
                regime,
                share: s.len() as f64 / total,
                strategy_return: sr,
                strategy_sharpe: ss,
                benchmark_return: br,
                benchmark_sharpe: bs,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
