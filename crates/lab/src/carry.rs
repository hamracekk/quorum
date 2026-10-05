//! Funding carry (ADR 0007, step 1): long spot + short perpetual of the same coin, collecting
//! funding while price moves cancel out.
//!
//! Each `rebalance` interval the simulation holds, in equal notional, up to `top` coins whose
//! funding over the trailing `lookback` was at least `min_annual` (annualised); at every funding
//! print a held coin earns `notional × rate` (negative funding is paid). Every change of a coin's
//! notional pays `cost` per leg on both legs (spot and perpetual). Capital covers the spot leg in
//! full plus `margin` of the perpetual leg, so notional = equity / (1 + margin) spread over the
//! held coins.
//!
//! Left out on purpose (documented, not hidden): the spot–perpetual basis (small and mean-
//! reverting for liquid coins), liquidation of the short leg (sized away by `margin`), and the
//! exchange itself as a counterparty.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, Utc};
use serde::Serialize;
use trader_core::derivatives::FundingPoint;
use trader_core::{EquityPoint, InstrumentId, Report};

/// Funding periods per year on 8-hour funding.
const PERIODS_PER_YEAR: f64 = 3.0 * 365.0;
/// A held coin is resized only when its target notional moves by more than this fraction.
const BAND: f64 = 0.2;

/// Carry rules.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CarryConfig {
    pub initial_cash: f64,
    /// Trailing window the funding level is judged on.
    pub lookback: Duration,
    /// Least annualised trailing funding a coin needs to be held.
    pub min_annual: f64,
    /// Most coins held at once.
    pub top: usize,
    pub rebalance: Duration,
    /// Cost per leg as a fraction of the traded notional (fee + slippage).
    pub cost: f64,
    /// Margin kept for the perpetual leg, as a fraction of its notional.
    pub margin: f64,
}

/// What the simulation produced.
#[derive(Debug, Clone, Serialize)]
pub struct CarryResult {
    pub report: Report,
    pub equity_curve: Vec<EquityPoint>,
    /// Funding earned (net of funding paid).
    pub funding: f64,
    pub costs: f64,
    /// Rebalances that changed the holdings.
    pub changes: usize,
    /// Average number of coins held.
    pub avg_held: f64,
    /// Return per calendar year.
    pub by_year: BTreeMap<i32, f64>,
}

fn trailing_annual(points: &[FundingPoint], at: DateTime<Utc>, lookback: Duration) -> Option<f64> {
    let window: Vec<f64> = points
        .iter()
        .filter(|p| p.ts <= at && p.ts > at - lookback)
        .map(|p| p.rate)
        .collect();
    // At least half the window known: a coin just listed is not judged on a few prints.
    let expected = lookback.num_hours() as f64 / 8.0;
    if (window.len() as f64) < expected / 2.0 {
        return None;
    }
    Some(window.iter().sum::<f64>() / window.len() as f64 * PERIODS_PER_YEAR)
}

/// The coins to hold at `at`: trailing funding at least `min_annual`, best first, at most `top`.
fn choose(
    funding: &BTreeMap<InstrumentId, Vec<FundingPoint>>,
    at: DateTime<Utc>,
    cfg: &CarryConfig,
) -> Vec<InstrumentId> {
    let mut scored: Vec<(&InstrumentId, f64)> = funding
        .iter()
        .filter_map(|(id, points)| Some((id, trailing_annual(points, at, cfg.lookback)?)))
        .filter(|(_, annual)| *annual >= cfg.min_annual)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    scored
        .into_iter()
        .take(cfg.top)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Return per calendar year of `curve` (starting from `initial`).
fn yearly(initial: f64, curve: &[EquityPoint]) -> BTreeMap<i32, f64> {
    let mut ends: BTreeMap<i32, f64> = BTreeMap::new();
    for point in curve {
        ends.insert(point.ts.year(), point.equity);
    }
    let mut start = initial;
    ends.into_iter()
        .map(|(year, end)| {
            let ret = if start > 0.0 { end / start - 1.0 } else { 0.0 };
            start = end;
            (year, ret)
        })
        .collect()
}

/// Notional traded moving from `held` to `target` (both legs counted by the caller).
fn turnover(held: &BTreeMap<InstrumentId, f64>, target: &BTreeMap<InstrumentId, f64>) -> f64 {
    let mut ids: Vec<&InstrumentId> = held.keys().chain(target.keys()).collect();
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let was = held.get(id).copied().unwrap_or(0.0);
            let now = target.get(id).copied().unwrap_or(0.0);
            (now - was).abs()
        })
        .sum()
}

/// Simulate funding carry over `funding` (every instrument's prints, oldest first).
pub fn carry(
    funding: &BTreeMap<InstrumentId, Vec<FundingPoint>>,
    cfg: &CarryConfig,
) -> CarryResult {
    let mut stamps: Vec<DateTime<Utc>> = funding.values().flatten().map(|p| p.ts).collect();
    stamps.sort_unstable();
    stamps.dedup();
    let rates: BTreeMap<(DateTime<Utc>, &InstrumentId), f64> = funding
        .iter()
        .flat_map(|(id, points)| points.iter().map(move |p| ((p.ts, id), p.rate)))
        .collect();
    let start = stamps.first().map(|t| *t + cfg.lookback);
    let mut equity = cfg.initial_cash;
    let mut held: BTreeMap<InstrumentId, f64> = BTreeMap::new();
    let mut next_rebalance = start;
    let (mut earned, mut costs, mut changes, mut held_sum) = (0.0, 0.0, 0, 0.0);
    let mut curve = Vec::new();
    for &ts in stamps.iter().filter(|t| start.is_some_and(|s| **t >= s)) {
        // Funding settles on what was held through the period.
        for (id, notional) in &held {
            if let Some(rate) = rates.get(&(ts, id)) {
                let flow = notional * rate;
                equity += flow;
                earned += flow;
            }
        }
        if next_rebalance.is_some_and(|t| ts >= t) {
            let chosen = choose(funding, ts, cfg);
            let each = if chosen.is_empty() {
                0.0
            } else {
                equity / (1.0 + cfg.margin) / chosen.len() as f64
            };
            // A held coin keeps its size unless the new one differs by more than `BAND`.
            let target: BTreeMap<InstrumentId, f64> = chosen
                .into_iter()
                .map(|id| {
                    let size = match held.get(&id) {
                        Some(&was) if was > 0.0 && ((each - was).abs() / was) <= BAND => was,
                        _ => each,
                    };
                    (id, size)
                })
                .collect();
            let cost = turnover(&held, &target) * cfg.cost * 2.0;
            if cost > 0.0 {
                changes += 1;
            }
            equity -= cost;
            costs += cost;
            held = target;
            next_rebalance = Some(ts + cfg.rebalance);
        }
        held_sum += held.len() as f64;
        curve.push(EquityPoint { ts, equity });
    }
    CarryResult {
        report: Report::compute(cfg.initial_cash, &curve, &[], &[]),
        funding: earned,
        costs,
        changes,
        avg_held: if curve.is_empty() {
            0.0
        } else {
            held_sum / curve.len() as f64
        },
        by_year: yearly(cfg.initial_cash, &curve),
        equity_curve: curve,
    }
}

#[cfg(test)]
mod tests;
