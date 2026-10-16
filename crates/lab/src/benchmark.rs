//! Passive alternatives (DESIGN §11): what the same money would have made in a fixed-weight
//! portfolio of index funds, and whether a strategy's curve beats it after costs.
//!
//! The portfolios are operator-written (`config/benchmark.toml`) and priced from stored bars
//! (adjusted closes: dividends included). A comparison runs on the dates both curves cover,
//! sampling the strategy at the benchmark's dates.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use quorum_core::metrics::Performance;
use quorum_core::{EquityPoint, InstrumentId, Series};
use serde::{Deserialize, Serialize};

use crate::LabError;

/// One holding of a benchmark portfolio.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holding {
    pub instrument: InstrumentId,
    pub weight: f64,
}

/// A fixed-weight portfolio, rebalanced every `rebalance_days` of its trading days.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Portfolio {
    pub name: String,
    pub holdings: Vec<Holding>,
    #[serde(default = "monthly")]
    pub rebalance_days: usize,
}

const fn monthly() -> usize {
    21
}

/// The operator's benchmarks.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkConfig {
    /// The research slot whose stored bars price the holdings.
    pub slot: String,
    /// The portfolio the daily paper summary compares against.
    pub summary: String,
    /// Tax on a trading book's net gain per calendar year (0.15 = 15%); passive portfolios are
    /// taken as held long enough to be exempt.
    #[serde(default)]
    pub trading_tax: f64,
    pub portfolios: Vec<Portfolio>,
}

impl BenchmarkConfig {
    pub fn load(path: &Path) -> Result<Self, LabError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LabError::Benchmark(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, LabError> {
        let cfg: Self = toml::from_str(text).map_err(|e| LabError::Benchmark(e.to_string()))?;
        let invalid = |m: String| Err(LabError::Benchmark(m));
        if !(0.0..1.0).contains(&cfg.trading_tax) {
            return invalid(format!("trading_tax {} must be in [0, 1)", cfg.trading_tax));
        }
        if cfg.portfolios.is_empty() {
            return invalid("no portfolios".to_owned());
        }
        for p in &cfg.portfolios {
            let sum: f64 = p.holdings.iter().map(|h| h.weight).sum();
            if p.holdings
                .iter()
                .any(|h| !h.weight.is_finite() || h.weight <= 0.0)
                || (sum - 1.0).abs() > 1e-9
            {
                return invalid(format!("{}: weights must be positive and sum to 1", p.name));
            }
            if p.rebalance_days == 0 {
                return invalid(format!("{}: rebalance_days must be at least 1", p.name));
            }
            if cfg.portfolios.iter().filter(|q| q.name == p.name).count() > 1 {
                return invalid(format!("{}: duplicate name", p.name));
            }
        }
        if !cfg.portfolios.iter().any(|p| p.name == cfg.summary) {
            return invalid(format!("summary `{}` is not a portfolio", cfg.summary));
        }
        Ok(cfg)
    }

    #[must_use]
    pub fn portfolio(&self, name: &str) -> Option<&Portfolio> {
        self.portfolios.iter().find(|p| p.name == name)
    }
}

/// The portfolio's equity from `initial`, on the dates every holding has a bar. Buying and each
/// rebalance pay `cost` per unit of notional traded (one side).
pub fn curve(
    portfolio: &Portfolio,
    series: &[Series],
    initial: f64,
    cost: f64,
) -> Result<Vec<EquityPoint>, LabError> {
    let closes: Vec<(f64, BTreeMap<DateTime<Utc>, f64>)> = portfolio
        .holdings
        .iter()
        .map(|h| {
            let s = series
                .iter()
                .find(|s| s.instrument == h.instrument)
                .ok_or_else(|| {
                    LabError::Benchmark(format!(
                        "{}: no stored bars for {}",
                        portfolio.name, h.instrument
                    ))
                })?;
            let closes = s
                .bars
                .iter()
                .filter(|b| b.close > 0.0 && b.close.is_finite())
                .map(|b| (b.ts, b.close))
                .collect();
            Ok((h.weight, closes))
        })
        .collect::<Result<_, LabError>>()?;
    let dates: Vec<DateTime<Utc>> = closes
        .first()
        .map(|(_, c)| {
            c.keys()
                .filter(|d| closes.iter().all(|(_, o)| o.contains_key(d)))
                .copied()
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::with_capacity(dates.len());
    let mut units = vec![0.0; closes.len()];
    let mut value = initial;
    for (k, d) in dates.iter().enumerate() {
        let prices: Vec<f64> = closes.iter().map(|(_, c)| c[d]).collect();
        if k > 0 {
            value = units.iter().zip(&prices).map(|(u, p)| u * p).sum();
        }
        if k % portfolio.rebalance_days == 0 {
            let wanted: Vec<f64> = closes
                .iter()
                .zip(&prices)
                .map(|((w, _), p)| value * w / p)
                .collect();
            let traded: f64 = wanted
                .iter()
                .zip(&units)
                .zip(&prices)
                .map(|((w, u), p)| (w - u).abs() * p)
                .sum();
            value = traded.mul_add(-cost, value);
            units = closes
                .iter()
                .zip(&prices)
                .map(|((w, _), p)| value * w / p)
                .collect();
        }
        out.push(EquityPoint {
            ts: *d,
            equity: value,
        });
    }
    Ok(out)
}

/// `curve` after paying `rate` of each calendar year's net gain at the year's last point.
///
/// Nothing is paid in a losing year and no loss is carried forward. The year's whole gain counts
/// as realized: right for a trading book that turns over, an upper bound on the tax otherwise.
#[must_use]
pub fn after_tax(curve: &[EquityPoint], rate: f64) -> Vec<EquityPoint> {
    use chrono::Datelike as _;
    let mut out = Vec::with_capacity(curve.len());
    // After-tax equity = pre-tax equity × `kept`, which shrinks by each year's tax.
    let mut kept = 1.0;
    let mut year_start = curve.first().map_or(0.0, |p| p.equity);
    for (k, point) in curve.iter().enumerate() {
        out.push(EquityPoint {
            ts: point.ts,
            equity: point.equity * kept,
        });
        let year_ends = curve
            .get(k + 1)
            .is_none_or(|next| next.ts.year() != point.ts.year());
        if year_ends && year_start > 0.0 {
            let gain = point.equity / year_start - 1.0;
            if gain > 0.0 {
                kept *= gain.mul_add(1.0 - rate, 1.0) / (1.0 + gain);
                if let Some(last) = out.last_mut() {
                    last.equity = point.equity * kept;
                }
            }
            year_start = point.equity;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Ahead with significance, and still ahead in the later part.
    Beats,
    /// Behind with significance.
    Trails,
    NotSignificant,
}

/// A strategy curve against a benchmark over the dates both cover.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub subject: String,
    pub benchmark: String,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub subject_perf: Performance,
    pub benchmark_perf: Performance,
    /// Subject CAGR minus benchmark CAGR, whole period and later part.
    pub excess_cagr: f64,
    pub later_excess_cagr: f64,
    /// Share of rolling one-year windows in which the subject grew more.
    pub year_wins: Option<f64>,
    /// t of the mean daily return difference.
    pub t: f64,
    pub verdict: Verdict,
}

/// Compares `subject` with `benchmark`; `None` without at least two common dates.
///
/// `year` is the benchmark's trading days in a year; `t_threshold` the |t| a verdict needs
/// (corrected by the caller for how many comparisons it makes).
#[must_use]
pub fn compare(
    subject: (&str, &[EquityPoint]),
    benchmark: (&str, &[EquityPoint]),
    later_share: f64,
    year: usize,
    t_threshold: f64,
) -> Option<Comparison> {
    let (s_first, s_last) = (subject.1.first()?.ts, subject.1.last()?.ts);
    let mut aligned: Vec<(DateTime<Utc>, f64, f64)> = Vec::new();
    let mut cursor = 0;
    for b in benchmark.1 {
        if b.ts < s_first || b.ts > s_last {
            continue;
        }
        while cursor + 1 < subject.1.len() && subject.1[cursor + 1].ts <= b.ts {
            cursor += 1;
        }
        aligned.push((b.ts, subject.1[cursor].equity, b.equity));
    }
    if aligned.len() < 2 {
        return None;
    }
    let side = |pick: fn(&(DateTime<Utc>, f64, f64)) -> f64, part: &[(DateTime<Utc>, f64, f64)]| {
        let points: Vec<EquityPoint> = part
            .iter()
            .map(|p| EquityPoint {
                ts: p.0,
                equity: pick(p),
            })
            .collect();
        Performance::from_curve(points[0].equity, &points)
    };
    let subj = |p: &(DateTime<Utc>, f64, f64)| p.1;
    let bench = |p: &(DateTime<Utc>, f64, f64)| p.2;
    let split = crate::signals::split_index(aligned.len(), later_share).min(aligned.len() - 2);
    let later = &aligned[split..];
    let subject_perf = side(subj, &aligned);
    let benchmark_perf = side(bench, &aligned);
    let later_excess_cagr = side(subj, later).cagr - side(bench, later).cagr;
    let excess: Vec<f64> = aligned
        .windows(2)
        .map(|w| (w[1].1 / w[0].1) - (w[1].2 / w[0].2))
        .collect();
    let count = excess.len() as f64;
    let mean = excess.iter().sum::<f64>() / count;
    let sd =
        (excess.iter().map(|e| (e - mean).powi(2)).sum::<f64>() / (count - 1.0).max(1.0)).sqrt();
    let t_stat = if sd > 0.0 {
        mean / sd * count.sqrt()
    } else {
        0.0
    };
    let year_wins = (aligned.len() > year).then(|| {
        let windows = aligned.len() - year;
        let wins = (0..windows)
            .filter(|&k| {
                let (start, end) = (&aligned[k], &aligned[k + year]);
                end.1 / start.1 > end.2 / start.2
            })
            .count();
        wins as f64 / windows as f64
    });
    let verdict = if t_stat >= t_threshold && later_excess_cagr > 0.0 {
        Verdict::Beats
    } else if t_stat <= -t_threshold {
        Verdict::Trails
    } else {
        Verdict::NotSignificant
    };
    Some(Comparison {
        subject: subject.0.to_owned(),
        benchmark: benchmark.0.to_owned(),
        from: aligned.first().map(|p| p.0),
        to: aligned.last().map(|p| p.0),
        excess_cagr: subject_perf.cagr - benchmark_perf.cagr,
        subject_perf,
        benchmark_perf,
        later_excess_cagr,
        year_wins,
        t: t_stat,
        verdict,
    })
}

#[cfg(test)]
mod tests;
