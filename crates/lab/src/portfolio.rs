//! Portfolio combiner (ADR 0007, step 2): several strategies' out-of-sample equity curves
//! combined into one, by fixed capital shares or by risk budget.
//!
//! Each stream is turned into daily returns (the last equity of each UTC day); the portfolio
//! starts when every stream has data. Weights are set every `rebalance_days` from **trailing**
//! data only: `fixed` shares, or inverse volatility over `vol_window_days` capped at
//! `max_weight` (a near-riskless stream such as carry would otherwise take everything). Between
//! rebalances each stream's capital drifts with its own returns.
//!
//! **Tilts** (optional, per stream): at each rebalance a stream's weight is multiplied by its
//! tilt on the latest day *before* the rebalance day (point in time), e.g. the exposure its
//! market's regime allows; what the tilts hold back stays in cash at zero return.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use quorum_core::{EquityPoint, Report};
use serde::Serialize;

/// How the streams share capital.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Weighting {
    /// Fixed shares (normalised to sum to 1).
    Fixed(Vec<f64>),
    /// Inverse trailing volatility, each weight capped at `max_weight`.
    InverseVol { max_weight: f64 },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CombineConfig {
    pub initial_cash: f64,
    pub weighting: Weighting,
    pub vol_window_days: usize,
    pub rebalance_days: usize,
    /// Per stream (in stream order): day → weight multiplier in `[0, 1]`. Empty: no tilt; a
    /// stream without an entry before a rebalance day keeps its full weight.
    pub tilts: Vec<BTreeMap<NaiveDate, f64>>,
}

/// A stream's tilt on the latest day before `day`; 1 without one.
fn tilt(tilts: &[BTreeMap<NaiveDate, f64>], stream: usize, day: NaiveDate) -> f64 {
    tilts
        .get(stream)
        .and_then(|t| t.range(..day).next_back())
        .map_or(1.0, |(_, v)| v.clamp(0.0, 1.0))
}

/// One input stream.
#[derive(Debug, Clone)]
pub struct Stream {
    pub name: String,
    pub curve: Vec<EquityPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStats {
    pub name: String,
    pub report: Report,
    /// Average weight over the combined period.
    pub avg_weight: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Combined {
    pub report: Report,
    pub curve: Vec<EquityPoint>,
    /// Each stream alone over the same period.
    pub streams: Vec<StreamStats>,
    /// Correlation of daily returns, in stream order.
    pub correlation: Vec<Vec<f64>>,
}

/// Daily returns by date (last equity per UTC day).
fn daily(curve: &[EquityPoint]) -> BTreeMap<NaiveDate, f64> {
    let mut last: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for p in curve {
        last.insert(p.ts.date_naive(), p.equity);
    }
    let mut out = BTreeMap::new();
    let mut prev: Option<f64> = None;
    for (day, equity) in last {
        if let Some(p) = prev.filter(|p| *p > 0.0) {
            out.insert(day, equity / p - 1.0);
        }
        prev = Some(equity);
    }
    out
}

fn stdev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()
}

fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n < 2 {
        return 0.0;
    }
    let ma = a[..n].iter().sum::<f64>() / n as f64;
    let mb = b[..n].iter().sum::<f64>() / n as f64;
    let cov: f64 = (0..n).map(|i| (a[i] - ma) * (b[i] - mb)).sum();
    let sa = a[..n].iter().map(|x| (x - ma).powi(2)).sum::<f64>().sqrt();
    let sb = b[..n].iter().map(|x| (x - mb).powi(2)).sum::<f64>().sqrt();
    if sa > 0.0 && sb > 0.0 {
        cov / (sa * sb)
    } else {
        0.0
    }
}

/// Cap every weight at `cap`, handing the excess to the uncapped ones.
fn capped(mut w: Vec<f64>, cap: f64) -> Vec<f64> {
    for _ in 0..w.len() {
        let excess: f64 = w.iter().map(|x| (x - cap).max(0.0)).sum();
        if excess <= 1e-12 {
            break;
        }
        let free: f64 = w.iter().filter(|x| **x < cap).sum();
        w = w
            .iter()
            .map(|x| {
                if *x >= cap {
                    cap
                } else if free > 0.0 {
                    x + excess * x / free
                } else {
                    *x
                }
            })
            .collect();
    }
    w
}

/// Weights from the trailing returns `history[stream]` (oldest first).
fn weights(history: &[Vec<f64>], cfg: &CombineConfig) -> Vec<f64> {
    let n = history.len();
    let raw: Vec<f64> = match &cfg.weighting {
        Weighting::Fixed(shares) => (0..n)
            .map(|i| shares.get(i).copied().unwrap_or(0.0).max(0.0))
            .collect(),
        Weighting::InverseVol { .. } => history
            .iter()
            .map(|h| {
                let vol = stdev(&h[h.len().saturating_sub(cfg.vol_window_days)..]);
                if vol > 0.0 { 1.0 / vol } else { 0.0 }
            })
            .collect(),
    };
    let total: f64 = raw.iter().sum();
    let w: Vec<f64> = if total > 0.0 {
        raw.iter().map(|r| r / total).collect()
    } else {
        vec![1.0 / n as f64; n]
    };
    match cfg.weighting {
        Weighting::InverseVol { max_weight } => capped(w, max_weight.clamp(1.0 / n as f64, 1.0)),
        Weighting::Fixed(_) => w,
    }
}

/// The days every stream covers and each stream's return on them (0 when it has none).
fn aligned(streams: &[Stream]) -> (Vec<NaiveDate>, Vec<Vec<f64>>) {
    let returns: Vec<BTreeMap<NaiveDate, f64>> = streams.iter().map(|s| daily(&s.curve)).collect();
    let start = returns
        .iter()
        .filter_map(|r| r.keys().next().copied())
        .max();
    let end = returns
        .iter()
        .filter_map(|r| r.keys().next_back().copied())
        .min();
    let mut days: Vec<NaiveDate> = match (start, end) {
        (Some(s), Some(e)) if s <= e => returns
            .iter()
            .flat_map(|r| r.keys().copied())
            .filter(|d| *d >= s && *d <= e)
            .collect(),
        _ => Vec::new(),
    };
    days.sort_unstable();
    days.dedup();
    let series = returns
        .iter()
        .map(|r| {
            days.iter()
                .map(|d| r.get(d).copied().unwrap_or(0.0))
                .collect()
        })
        .collect();
    (days, series)
}

fn curve_of(days: &[NaiveDate], equity: &[f64]) -> Vec<EquityPoint> {
    days.iter()
        .zip(equity)
        .filter_map(|(d, e)| {
            Some(EquityPoint {
                ts: d.and_hms_opt(0, 0, 0)?.and_utc(),
                equity: *e,
            })
        })
        .collect()
}

/// Combine `streams` from the first day all of them have data.
pub fn combine(streams: &[Stream], cfg: &CombineConfig) -> Combined {
    let (days, series) = aligned(streams);
    let n = streams.len();
    let mut capital: Vec<f64> = vec![0.0; n];
    let mut cash = 0.0;
    let mut equity = cfg.initial_cash;
    let mut path = Vec::with_capacity(days.len());
    let mut weight_sum = vec![0.0; n];
    for t in 0..days.len() {
        if t % cfg.rebalance_days.max(1) == 0 {
            let history: Vec<Vec<f64>> = series.iter().map(|s| s[..t].to_vec()).collect();
            capital = weights(&history, cfg)
                .iter()
                .enumerate()
                .map(|(i, w)| w * tilt(&cfg.tilts, i, days[t]) * equity)
                .collect();
            cash = equity - capital.iter().sum::<f64>();
        }
        for (c, s) in capital.iter_mut().zip(&series) {
            *c *= 1.0 + s[t];
        }
        equity = capital.iter().sum::<f64>() + cash;
        for (sum, c) in weight_sum.iter_mut().zip(&capital) {
            *sum += if equity > 0.0 { c / equity } else { 0.0 };
        }
        path.push(equity);
    }
    let streams_alone = streams
        .iter()
        .zip(&series)
        .zip(&weight_sum)
        .map(|((s, returns), sum)| {
            let mut e = cfg.initial_cash;
            let alone: Vec<f64> = returns
                .iter()
                .map(|r| {
                    e *= 1.0 + r;
                    e
                })
                .collect();
            StreamStats {
                name: s.name.clone(),
                report: Report::compute(cfg.initial_cash, &curve_of(&days, &alone), &[], &[]),
                avg_weight: if days.is_empty() {
                    0.0
                } else {
                    sum / days.len() as f64
                },
            }
        })
        .collect();
    let correlation = series
        .iter()
        .map(|a| series.iter().map(|b| correlation(a, b)).collect())
        .collect();
    let curve = curve_of(&days, &path);
    Combined {
        report: Report::compute(cfg.initial_cash, &curve, &[], &[]),
        curve,
        streams: streams_alone,
        correlation,
    }
}

#[cfg(test)]
mod tests;
