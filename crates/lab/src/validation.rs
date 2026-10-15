//! Overfitting control beyond one path (R-3, E-4): the probability of backtest overfitting and
//! combinatorial purged cross-validation.
//!
//! Walk-forward judges the parameter choice along one history; both checks here re-run that
//! choice over many train/test splits of the same history, using each parameter set's per-bar
//! returns from one full-history backtest.
//!
//! - **PBO (CSCV, Bailey et al.):** cut the history into `blocks`, use every half of them for
//!   choosing the best Sharpe and the other half for judging it. PBO is the share of splits where
//!   the chosen set ranks in the bottom half out of sample; above 0.5 the choice is no better
//!   than picking at random.
//! - **CPCV (López de Prado):** cut the history into `groups`, test on every `test_groups` of
//!   them and choose on the rest, dropping `embargo` bars on both sides of each test group
//!   (serial correlation leaks across the edge). The test pieces assemble into several complete
//!   out-of-sample paths, giving a distribution of Sharpes instead of one number.

use chrono::{DateTime, Utc};
use quorum_core::{BacktestConfig, EquityPoint, Series, run};
use serde::Serialize;

use crate::StrategyFactory;

/// How the history is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationConfig {
    /// CSCV blocks (even, 4–16): every half of them is one split.
    pub blocks: usize,
    /// CPCV groups.
    pub groups: usize,
    /// CPCV groups tested per split.
    pub test_groups: usize,
    /// Bars dropped from training on each side of a test group.
    pub embargo: usize,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            blocks: 16,
            groups: 10,
            test_groups: 2,
            embargo: 5,
        }
    }
}

/// Probability of backtest overfitting.
#[derive(Debug, Clone, Serialize)]
pub struct PboReport {
    /// Share of splits where the in-sample best ranks in the bottom half out of sample.
    pub pbo: f64,
    pub splits: usize,
    /// Median out-of-sample Sharpe of the in-sample best (annualised).
    pub median_oos_sharpe: f64,
    /// Share of splits where the in-sample best lost money out of sample.
    pub prob_oos_loss: f64,
    /// Median in-sample Sharpe of the chosen set, for the drop to out of sample.
    pub median_is_sharpe: f64,
}

/// Combinatorial purged cross-validation.
#[derive(Debug, Clone, Serialize)]
pub struct CpcvReport {
    pub splits: usize,
    /// Annualised Sharpe of each complete out-of-sample path, lowest first.
    pub path_sharpes: Vec<f64>,
    pub median_sharpe: f64,
    /// Share of paths with a positive Sharpe.
    pub share_positive: f64,
}

/// Both checks for one parameter grid.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    /// Parameter sets compared.
    pub configs: usize,
    /// Each set's full-history Sharpe, in grid order with its label: how sensitive the result
    /// is to the parameters (a lone peak among weak neighbours is a warning).
    pub sensitivity: Vec<(String, f64)>,
    /// `None` with fewer than two sets or too little history.
    pub pbo: Option<PboReport>,
    pub cpcv: Option<CpcvReport>,
}

/// Run every parameter set over the whole history and apply both checks to their returns.
pub fn validate<P: std::fmt::Display + Sync>(
    grid: &[P],
    factory: &(impl StrategyFactory<P> + Sync),
    configure: impl Fn(&P, &BacktestConfig) -> BacktestConfig + Sync,
    series: &[Series],
    cfg: &BacktestConfig,
    vc: ValidationConfig,
) -> ValidationReport {
    let curves: Vec<Vec<EquityPoint>> = crate::par_map(grid, |p| {
        let mut strategy = factory.build(p);
        run(strategy.as_mut(), series, &configure(p, cfg)).equity_curve
    });
    let (stamps, returns) = returns_matrix(&curves);
    let per_year = periods_per_year(&stamps);
    let all: Vec<usize> = (0..stamps.len()).collect();
    let sensitivity = grid
        .iter()
        .zip(&returns)
        .map(|(p, r)| (p.to_string(), sharpe(r, &all, per_year)))
        .collect();
    ValidationReport {
        configs: grid.len(),
        sensitivity,
        pbo: pbo(&returns, vc.blocks, per_year),
        cpcv: cpcv(&returns, vc.groups, vc.test_groups, vc.embargo, per_year),
    }
}

/// Per-bar returns of every curve on their shared timeline (a curve missing a timestamp keeps
/// its previous equity): one row per curve, plus the timestamp each return ends at.
pub fn returns_matrix(curves: &[Vec<EquityPoint>]) -> (Vec<DateTime<Utc>>, Vec<Vec<f64>>) {
    let mut stamps: Vec<DateTime<Utc>> = curves.iter().flatten().map(|p| p.ts).collect();
    stamps.sort_unstable();
    stamps.dedup();
    let rows = curves
        .iter()
        .map(|curve| {
            let mut points = curve.iter().peekable();
            let mut equity = curve.first().map_or(0.0, |p| p.equity);
            let mut row = Vec::with_capacity(stamps.len().saturating_sub(1));
            for (i, ts) in stamps.iter().enumerate() {
                let before = equity;
                while let Some(p) = points.next_if(|p| p.ts <= *ts) {
                    equity = p.equity;
                }
                if i > 0 {
                    row.push(if before > 0.0 {
                        equity / before - 1.0
                    } else {
                        0.0
                    });
                }
            }
            row
        })
        .collect();
    (stamps.into_iter().skip(1).collect(), rows)
}

/// Bars per year of a timeline (0 when it spans no time).
pub fn periods_per_year(stamps: &[DateTime<Utc>]) -> f64 {
    match (stamps.first(), stamps.last()) {
        (Some(first), Some(last)) if last > first => {
            let years = (*last - *first).num_seconds() as f64 / (365.25 * 86_400.0);
            (stamps.len() - 1) as f64 / years
        }
        _ => 0.0,
    }
}

/// Annualised Sharpe of the returns at `idx`.
fn sharpe(returns: &[f64], idx: &[usize], per_year: f64) -> f64 {
    let n = idx.len();
    if n < 2 {
        return 0.0;
    }
    let mean = idx.iter().map(|&i| returns[i]).sum::<f64>() / n as f64;
    let var = idx
        .iter()
        .map(|&i| (returns[i] - mean).powi(2))
        .sum::<f64>()
        / (n as f64 - 1.0);
    if var <= 0.0 {
        return 0.0;
    }
    mean / var.sqrt() * per_year.sqrt()
}

/// The row with the best Sharpe at `idx` (the first on ties) and that Sharpe.
fn best(returns: &[Vec<f64>], idx: &[usize], per_year: f64) -> (usize, f64) {
    returns
        .iter()
        .map(|r| sharpe(r, idx, per_year))
        .enumerate()
        .fold(
            (0, f64::NEG_INFINITY),
            |top, (i, s)| {
                if s > top.1 { (i, s) } else { top }
            },
        )
}

/// `[start, end)` of each of `parts` equal pieces of `len` bars.
fn bounds(len: usize, parts: usize) -> Vec<(usize, usize)> {
    (0..parts)
        .map(|g| (g * len / parts, (g + 1) * len / parts))
        .collect()
}

/// Every set of `k` out of `n` items as a bit mask, in increasing order.
fn combinations(n: usize, k: usize) -> Vec<u32> {
    (0..1_u32 << n)
        .filter(|m| m.count_ones() as usize == k)
        .collect()
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    }
}

/// Probability of backtest overfitting by combinatorially symmetric cross-validation.
///
/// `returns` holds one equal-length row of per-bar returns per parameter set; `blocks` is
/// rounded down to an even number in 4..=16.
pub fn pbo(returns: &[Vec<f64>], blocks: usize, per_year: f64) -> Option<PboReport> {
    let blocks = (blocks.clamp(4, 16) / 2) * 2;
    let len = returns.first().map_or(0, Vec::len);
    if returns.len() < 2 || len < blocks * 2 || returns.iter().any(|r| r.len() != len) {
        return None;
    }
    let parts = bounds(len, blocks);
    let indices = |mask: u32, inside: bool| -> Vec<usize> {
        parts
            .iter()
            .enumerate()
            .filter(|(b, _)| (mask >> b & 1 == 1) == inside)
            .flat_map(|(_, &(s, e))| s..e)
            .collect()
    };
    let n = returns.len() as f64;
    let (mut below, mut losses) = (0_usize, 0_usize);
    let (mut oos, mut is) = (Vec::new(), Vec::new());
    for mask in combinations(blocks, blocks / 2) {
        let (train, test) = (indices(mask, true), indices(mask, false));
        let (chosen, in_sample) = best(returns, &train, per_year);
        let scores: Vec<f64> = returns.iter().map(|r| sharpe(r, &test, per_year)).collect();
        let mine = scores[chosen];
        let lower = scores.iter().filter(|s| s.total_cmp(&mine).is_lt()).count() as f64;
        let ties = scores.iter().filter(|s| s.total_cmp(&mine).is_eq()).count() as f64;
        // Relative rank in (0, 1); at or below the middle is "no better than random".
        let omega = (lower + f64::midpoint(ties, 1.0)) / (n + 1.0);
        below += usize::from(omega <= 0.5);
        losses += usize::from(mine < 0.0);
        oos.push(mine);
        is.push(in_sample);
    }
    let splits = oos.len();
    Some(PboReport {
        pbo: below as f64 / splits as f64,
        splits,
        median_oos_sharpe: median(&mut oos),
        prob_oos_loss: losses as f64 / splits as f64,
        median_is_sharpe: median(&mut is),
    })
}

/// Combinatorial purged cross-validation of choosing the best Sharpe.
///
/// Every split tests `test_groups` of `groups` (at most 16) and chooses on the rest minus
/// `embargo` bars on each side of a test group; each group's test pieces, in split order, form
/// `C(groups − 1, test_groups − 1)` complete out-of-sample paths.
pub fn cpcv(
    returns: &[Vec<f64>],
    groups: usize,
    test_groups: usize,
    embargo: usize,
    per_year: f64,
) -> Option<CpcvReport> {
    let len = returns.first().map_or(0, Vec::len);
    if returns.is_empty()
        || !(2..=16).contains(&groups)
        || test_groups == 0
        || test_groups >= groups
        || len < groups * 2
        || returns.iter().any(|r| r.len() != len)
    {
        return None;
    }
    let parts = bounds(len, groups);
    let masks = combinations(groups, test_groups);
    // paths[j][t]: the return at bar t on path j.
    let path_count = masks.len() * test_groups / groups;
    let mut paths = vec![vec![0.0; len]; path_count];
    let mut used = vec![0_usize; groups];
    for &mask in &masks {
        let tested = |g: usize| mask >> g & 1 == 1;
        // Not tested, and at least `embargo` bars away from every tested group.
        let near = |t: usize| {
            parts
                .iter()
                .enumerate()
                .any(|(g, &(s, e))| tested(g) && t + embargo >= s && t < e + embargo)
        };
        let train: Vec<usize> = (0..len).filter(|&t| !near(t)).collect();
        let (chosen, _) = best(returns, &train, per_year);
        for (g, &(s, e)) in parts.iter().enumerate().filter(|(g, _)| tested(*g)) {
            let path = &mut paths[used[g]];
            path[s..e].copy_from_slice(&returns[chosen][s..e]);
            used[g] += 1;
        }
    }
    let all: Vec<usize> = (0..len).collect();
    let mut path_sharpes: Vec<f64> = paths.iter().map(|p| sharpe(p, &all, per_year)).collect();
    path_sharpes.sort_by(f64::total_cmp);
    let positive = path_sharpes.iter().filter(|s| **s > 0.0).count();
    Some(CpcvReport {
        splits: masks.len(),
        share_positive: positive as f64 / path_sharpes.len().max(1) as f64,
        median_sharpe: median(&mut path_sharpes.clone()),
        path_sharpes,
    })
}

#[cfg(test)]
mod tests;
