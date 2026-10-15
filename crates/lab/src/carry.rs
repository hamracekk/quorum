//! Funding carry (ADR 0007, step 1): long spot + short perpetual of the same coin, collecting
//! funding while price moves cancel out.
//!
//! Each `rebalance` interval the simulation holds, in equal notional, up to `top` coins whose
//! funding over the trailing `lookback` clears the entry level; at every funding print a held coin
//! earns `notional × rate` (negative funding is paid). Every change of a coin's notional pays
//! `cost` per leg on both legs (spot and perpetual). Capital covers the spot leg in full plus
//! `margin` of the perpetual leg, so notional = equity / (1 + margin) spread over the held coins.
//!
//! Gating (R-4): a coin is entered only when its trailing funding is at least `min_annual` and
//! pays back the four legs of a round trip (in and out of spot and perpetual) within
//! `hurdle_days`; a held coin stays while it pays at least `exit_annual`, so the gap between the
//! two levels keeps it from churning. With prices, the short leg is **liquidated** when a bar's
//! high rises `margin − maintenance` above its price at the last rebalance (the margin is reset
//! to `margin` at every rebalance): the exchange's fee and the spot sale are paid and the coin is
//! out until the next rebalance. A `holdout_from` date reports the history after it separately.
//!
//! Left out on purpose (documented, not hidden): the spot–perpetual basis (small and mean-
//! reverting for liquid coins) and the exchange itself as a counterparty.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, Utc};
use quorum_core::derivatives::FundingPoint;
use quorum_core::{Bar, EquityPoint, InstrumentId, Report};
use serde::Serialize;

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
    /// Least annualised trailing funding for a held coin to stay (at most the entry level).
    pub exit_annual: f64,
    /// Days of funding that must pay for a round trip's four legs before a coin is entered
    /// (`0` = no cost hurdle).
    pub hurdle_days: f64,
    /// How the short leg is liquidated (needs prices; `None` = never).
    pub liquidation: Option<Liquidation>,
    /// Report the history from here on separately (a hold-out the settings never saw).
    pub holdout_from: Option<DateTime<Utc>>,
}

/// Liquidation of the short perpetual leg.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Liquidation {
    /// Maintenance margin as a fraction of notional (liquidated when the margin left falls to it).
    pub maintenance: f64,
    /// The exchange's liquidation fee as a fraction of notional.
    pub fee: f64,
}

impl CarryConfig {
    /// Least annualised trailing funding to enter a coin: `min_annual`, or what pays the four
    /// legs of a round trip within `hurdle_days` when that is higher.
    pub fn entry_annual(&self) -> f64 {
        if self.hurdle_days > 0.0 {
            self.min_annual
                .max(4.0 * self.cost * 365.0 / self.hurdle_days)
        } else {
            self.min_annual
        }
    }
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
    /// The entry level actually used (annualised).
    pub entry_annual: f64,
    /// Short legs liquidated.
    pub liquidations: usize,
    /// Before `holdout_from` (the whole history without one).
    pub in_sample: Report,
    /// From `holdout_from` on.
    pub holdout: Option<Report>,
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

/// The coins to hold at `at`, best first, at most `top`: held coins stay at `exit_annual`,
/// others need the entry level.
fn choose(
    funding: &BTreeMap<InstrumentId, Vec<FundingPoint>>,
    held: &BTreeMap<InstrumentId, f64>,
    at: DateTime<Utc>,
    cfg: &CarryConfig,
) -> Vec<InstrumentId> {
    let entry = cfg.entry_annual();
    let exit = cfg.exit_annual.min(entry);
    let mut scored: Vec<(&InstrumentId, f64)> = funding
        .iter()
        .filter_map(|(id, points)| Some((id, trailing_annual(points, at, cfg.lookback)?)))
        .filter(|(id, annual)| *annual >= if held.contains_key(*id) { exit } else { entry })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    scored
        .into_iter()
        .take(cfg.top)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Prices walked forward with the funding prints: the last close known and, per held coin, the
/// liquidation level of its short leg.
#[derive(Debug, Default)]
struct Prices<'a> {
    bars: BTreeMap<&'a InstrumentId, (&'a [Bar], Duration, usize)>,
    close: BTreeMap<InstrumentId, f64>,
    /// The highest high per coin among the bars that completed since the last call.
    high: BTreeMap<InstrumentId, f64>,
}

impl<'a> Prices<'a> {
    fn new(prices: &'a BTreeMap<InstrumentId, Vec<Bar>>) -> Self {
        let bars = prices
            .iter()
            .filter(|(_, b)| b.len() >= 2)
            .map(|(id, b)| (id, (b.as_slice(), b[1].ts - b[0].ts, 0)))
            .collect();
        Self {
            bars,
            ..Self::default()
        }
    }

    /// Take in every bar completed by `ts`.
    fn advance(&mut self, ts: DateTime<Utc>) {
        self.high.clear();
        for (id, (bars, interval, next)) in &mut self.bars {
            while let Some(bar) = bars.get(*next).filter(|b| b.ts + *interval <= ts) {
                self.close.insert((*id).clone(), bar.close);
                let high = self.high.entry((*id).clone()).or_insert(bar.high);
                *high = high.max(bar.high);
                *next += 1;
            }
        }
    }
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

/// Simulate funding carry over `funding` (every instrument's prints, oldest first) without
/// prices, so without liquidations.
pub fn carry(
    funding: &BTreeMap<InstrumentId, Vec<FundingPoint>>,
    cfg: &CarryConfig,
) -> CarryResult {
    carry_with(funding, &BTreeMap::new(), cfg)
}

/// What the loop accumulates.
#[derive(Debug, Default)]
struct Book {
    equity: f64,
    /// Notional per held coin.
    held: BTreeMap<InstrumentId, f64>,
    /// Liquidation price of each held coin's short leg.
    limit: BTreeMap<InstrumentId, f64>,
    earned: f64,
    costs: f64,
    changes: usize,
    liquidations: usize,
}

impl Book {
    fn settle(&mut self, rates: &BTreeMap<(DateTime<Utc>, &InstrumentId), f64>, ts: DateTime<Utc>) {
        for (id, notional) in &self.held {
            if let Some(rate) = rates.get(&(ts, id)) {
                let flow = notional * rate;
                self.equity += flow;
                self.earned += flow;
            }
        }
    }

    /// Close every short leg whose liquidation price the latest bars reached.
    fn liquidate(&mut self, prices: &Prices<'_>, cfg: &CarryConfig) {
        let Some(rule) = cfg.liquidation else { return };
        let hit: Vec<InstrumentId> = self
            .limit
            .iter()
            .filter(|(id, limit)| prices.high.get(*id).is_some_and(|h| h >= *limit))
            .map(|(id, _)| id.clone())
            .collect();
        for id in hit {
            let notional = self.held.remove(&id).unwrap_or(0.0);
            self.limit.remove(&id);
            // The exchange's fee on the perpetual, our sale of the spot leg.
            let cost = notional * (rule.fee + cfg.cost);
            self.equity -= cost;
            self.costs += cost;
            self.liquidations += 1;
        }
    }

    fn rebalance(&mut self, chosen: Vec<InstrumentId>, prices: &Prices<'_>, cfg: &CarryConfig) {
        let each = if chosen.is_empty() {
            0.0
        } else {
            self.equity / (1.0 + cfg.margin) / chosen.len() as f64
        };
        // A held coin keeps its size unless the new one differs by more than `BAND`.
        let target: BTreeMap<InstrumentId, f64> = chosen
            .into_iter()
            .map(|id| {
                let size = match self.held.get(&id) {
                    Some(&was) if was > 0.0 && ((each - was).abs() / was) <= BAND => was,
                    _ => each,
                };
                (id, size)
            })
            .collect();
        let cost = turnover(&self.held, &target) * cfg.cost * 2.0;
        if cost > 0.0 {
            self.changes += 1;
        }
        self.equity -= cost;
        self.costs += cost;
        // The perpetual's margin is topped back up to `margin` of its notional.
        self.limit = cfg.liquidation.map_or_else(BTreeMap::new, |rule| {
            target
                .keys()
                .filter_map(|id| {
                    let close = prices.close.get(id)?;
                    Some((id.clone(), close * (1.0 + cfg.margin - rule.maintenance)))
                })
                .collect()
        });
        self.held = target;
    }
}

/// [`carry`] with each instrument's bars (oldest first), for liquidating the short leg.
pub fn carry_with(
    funding: &BTreeMap<InstrumentId, Vec<FundingPoint>>,
    prices: &BTreeMap<InstrumentId, Vec<Bar>>,
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
    let mut prices = Prices::new(prices);
    let mut book = Book {
        equity: cfg.initial_cash,
        ..Book::default()
    };
    let mut next_rebalance = start;
    let mut held_sum = 0.0;
    let mut curve = Vec::new();
    for &ts in &stamps {
        prices.advance(ts);
        if start.is_none_or(|s| ts < s) {
            continue;
        }
        // Funding settles on what was held through the period, then the bars since are checked.
        book.settle(&rates, ts);
        book.liquidate(&prices, cfg);
        if next_rebalance.is_some_and(|t| ts >= t) {
            let chosen = choose(funding, &book.held, ts, cfg);
            book.rebalance(chosen, &prices, cfg);
            next_rebalance = Some(ts + cfg.rebalance);
        }
        held_sum += book.held.len() as f64;
        curve.push(EquityPoint {
            ts,
            equity: book.equity,
        });
    }
    let (in_sample, holdout) = split(cfg.initial_cash, &curve, cfg.holdout_from);
    CarryResult {
        report: Report::compute(cfg.initial_cash, &curve, &[], &[]),
        funding: book.earned,
        costs: book.costs,
        changes: book.changes,
        avg_held: if curve.is_empty() {
            0.0
        } else {
            held_sum / curve.len() as f64
        },
        by_year: yearly(cfg.initial_cash, &curve),
        entry_annual: cfg.entry_annual(),
        liquidations: book.liquidations,
        in_sample,
        holdout,
        equity_curve: curve,
    }
}

/// Reports before and from `from` on (the second one starting from the equity reached).
fn split(
    initial: f64,
    curve: &[EquityPoint],
    from: Option<DateTime<Utc>>,
) -> (Report, Option<Report>) {
    let Some(from) = from else {
        return (Report::compute(initial, curve, &[], &[]), None);
    };
    let at = curve.partition_point(|p| p.ts < from);
    let (before, after) = curve.split_at(at);
    let restart = before.last().map_or(initial, |p| p.equity);
    let holdout = (!after.is_empty()).then(|| Report::compute(restart, after, &[], &[]));
    (Report::compute(initial, before, &[], &[]), holdout)
}

#[cfg(test)]
mod tests;
