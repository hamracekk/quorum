//! A policy run over history on the one engine, in CZK.
//!
//! The start amount and monthly contributions are converted at ČNB rates, fills are booked as tax
//! lots, and the result is measured the way an investor feels it (money-weighted) and the way
//! funds are compared (time-weighted).

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Utc};
use quorum_core::metrics::Performance;
use quorum_core::risk::RiskLimits;
use quorum_core::{
    BacktestConfig, Bar, Engine, EquityPoint, Event, Fill, InstrumentId, Series, Side,
};
use serde::Serialize;

use crate::LongTermError;
use crate::ledger::{IfSold, Ledger};
use crate::policy::{Policy, PolicyStrategy};

/// Costs of running a policy.
#[derive(Debug, Clone, Copy)]
pub struct SimCosts {
    pub fee_rate: f64,
    pub slippage_bps: f64,
    pub fx_cost: f64,
    /// Tax on each year's non-exempt net gain.
    pub tax_rate: f64,
}

/// How a policy did.
#[derive(Debug, Clone, Serialize)]
pub struct SimReport {
    pub policy: String,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub contributed_czk: f64,
    /// Converted back to CZK at the end (after the conversion cost), before any sale tax.
    pub value_czk: f64,
    /// Tax owed on gains realised along the way (band rebalances).
    pub tax_paid_czk: f64,
    /// Selling everything at the end.
    pub if_sold: IfSold,
    /// `value_czk` less the tax along the way and on selling everything at the end.
    pub after_sale_tax_czk: f64,
    /// Money-weighted annual return in CZK, before and after selling everything.
    pub money_weighted: f64,
    pub money_weighted_after_tax: f64,
    /// Time-weighted (contributions removed): annual return and drawdown of a unit of the book.
    pub time_weighted: f64,
    pub max_drawdown: f64,
    pub trades: usize,
    pub sells: usize,
}

/// The latest rate on or before `ts`.
fn rate_at(rates: &BTreeMap<NaiveDate, f64>, ts: DateTime<Utc>) -> Option<f64> {
    rates.range(..=ts.date_naive()).next_back().map(|(_, r)| *r)
}

fn month_start(ts: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(ts.year(), ts.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(ts)
}

/// Contributions and the unit value of the book while it runs.
#[derive(Debug, Clone, Default)]
struct Flows {
    cash: Vec<(DateTime<Utc>, f64)>,
    contributed: f64,
    units: f64,
    nav: Vec<EquityPoint>,
}

/// The policy's bars in time order (holdings in policy order within a date), from the first
/// date every holding and a rate exist and not before `from`.
fn timeline<'a>(
    policy: &Policy,
    series: &'a [Series],
    rates: &BTreeMap<NaiveDate, f64>,
    from: Option<DateTime<Utc>>,
) -> Result<Vec<(&'a InstrumentId, &'a Bar)>, LongTermError> {
    let held: Vec<&Series> = policy
        .holdings
        .iter()
        .map(|h| {
            series
                .iter()
                .find(|s| s.instrument == h.instrument)
                .ok_or_else(|| {
                    LongTermError::Data(format!(
                        "{}: no stored bars for {}",
                        policy.name, h.instrument
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let start = held
        .iter()
        .filter_map(|s| s.bars.first().map(|b| b.ts))
        .chain(
            rates
                .keys()
                .next()
                .map(|d| d.and_time(chrono::NaiveTime::MIN).and_utc()),
        )
        .chain(from)
        .max()
        .ok_or_else(|| LongTermError::Data("no data".to_owned()))?;
    let mut bars: Vec<(usize, &InstrumentId, &Bar)> = held
        .iter()
        .enumerate()
        .flat_map(|(rank, s)| {
            s.bars
                .iter()
                .filter(move |b| b.ts >= start)
                .map(move |b| (rank, &s.instrument, b))
        })
        .collect();
    bars.sort_by_key(|(rank, _, b)| (b.ts, *rank));
    Ok(bars.into_iter().map(|(_, id, bar)| (id, bar)).collect())
}

/// What driving a policy through its bars produced.
#[derive(Debug, Clone)]
pub struct Driven {
    pub ledger: Ledger,
    /// Every fill, in order.
    pub fills: Vec<Fill>,
    /// The first and the last timestamp processed; `None` before the first bar.
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Latest close of each holding (USD).
    pub last_close: BTreeMap<InstrumentId, f64>,
    /// CZK per USD at `to`.
    pub rate: f64,
    flows: Flows,
}

impl Driven {
    /// Paid in so far, in CZK.
    #[must_use]
    pub const fn contributed_czk(&self) -> f64 {
        self.flows.contributed
    }
}

/// The policy's instruments, in policy order.
#[must_use]
pub fn instruments(policy: &Policy) -> Vec<InstrumentId> {
    policy
        .holdings
        .iter()
        .map(|h| h.instrument.clone())
        .collect()
}

/// The engine settings a policy runs with: no cash until the first contribution, and no daily
/// kill switch (that is for trading; a long-term book rides out a bad day).
#[must_use]
pub fn engine_config(fee_rate: f64, slippage_bps: f64) -> BacktestConfig {
    BacktestConfig {
        initial_cash: 0.0,
        fee_rate,
        slippage_bps,
        risk: RiskLimits {
            max_daily_loss: 1.0,
            ..RiskLimits::default()
        },
        ..BacktestConfig::default()
    }
}

/// Feeds `policy`'s bars from `from` on through `engine`, with its contributions.
///
/// The engine is built on a [`PolicyStrategy`] with [`engine_config`]. The start amount goes in
/// on the first timestamp and a contribution on the first of every later month (CZK at the ČNB
/// rate, less the conversion cost); fills are booked as lots.
pub fn drive(
    engine: &mut Engine<'_>,
    policy: &Policy,
    series: &[Series],
    rates: &BTreeMap<NaiveDate, f64>,
    fx_cost: f64,
    from: Option<DateTime<Utc>>,
) -> Result<Driven, LongTermError> {
    let bars = timeline(policy, series, rates, from)?;
    let mut d = Driven {
        ledger: Ledger::default(),
        fills: Vec::new(),
        from: None,
        to: None,
        last_close: BTreeMap::new(),
        rate: rates.values().next_back().copied().unwrap_or(0.0),
        flows: Flows::default(),
    };
    let mut next_contribution: Option<DateTime<Utc>> = None;
    let mut value = 0.0;
    for (instrument, bar) in bars {
        let Some(rate) = rate_at(rates, bar.ts) else {
            continue; // before the first rate: timeline() starts after it
        };
        if d.to.is_none_or(|c| bar.ts > c) {
            if let Some(c) = d.to {
                // The timestamp before this one is complete: value the book.
                value = engine.snapshot().equity * d.rate;
                d.flows.nav.push(EquityPoint {
                    ts: c,
                    equity: value / d.flows.units,
                });
            }
            let amount = match next_contribution {
                None => Some(policy.start_czk),
                Some(due) if bar.ts >= due => Some(policy.monthly_czk),
                Some(_) => None,
            };
            if let Some(amount) = amount.filter(|a| *a > 0.0) {
                let nav = if d.flows.units > 0.0 {
                    (value / d.flows.units).max(f64::MIN_POSITIVE)
                } else {
                    1.0
                };
                d.flows.units += amount / nav;
                engine.deposit(amount / rate * (1.0 - fx_cost));
                d.flows.cash.push((bar.ts, -amount));
                d.flows.contributed += amount;
                value += amount;
            }
            let mut due = next_contribution.unwrap_or_else(|| month_start(bar.ts));
            while due <= bar.ts {
                due = due + Months::new(1);
            }
            next_contribution = Some(due);
            d.from.get_or_insert(bar.ts);
            d.to = Some(bar.ts);
            d.rate = rate;
        }
        d.last_close.insert(instrument.clone(), bar.close);
        let events = engine
            .on_bar(instrument, bar)
            .map_err(|e| LongTermError::Data(e.to_string()))?;
        for event in events {
            if let Event::Fill(fill) = event {
                d.ledger
                    .apply(&fill, rate_at(rates, fill.ts).unwrap_or(rate));
                d.fills.push(fill);
            }
        }
    }
    Ok(d)
}

/// The report on a driven policy whose book is worth `equity_usd` now.
#[must_use]
pub fn report(policy: &Policy, d: &Driven, equity_usd: f64, costs: SimCosts) -> SimReport {
    let end = d.to.unwrap_or_else(Utc::now);
    let value_czk = equity_usd * d.rate * (1.0 - costs.fx_cost);
    let mut nav = d.flows.nav.clone();
    if d.flows.units > 0.0 {
        nav.push(EquityPoint {
            ts: end,
            equity: value_czk / d.flows.units,
        });
    }
    let tax_paid: f64 = d.ledger.tax_by_year(costs.tax_rate).values().sum();
    let if_sold = d.ledger.if_sold(&d.last_close, d.rate, end);
    let after_sale = if_sold
        .taxable_gain
        .max(0.0)
        .mul_add(-costs.tax_rate, value_czk - tax_paid);
    let mut pre = d.flows.cash.clone();
    pre.push((end, value_czk - tax_paid));
    let mut post = d.flows.cash.clone();
    post.push((end, after_sale));
    let unit = nav.first().map_or_else(Performance::default, |p| {
        Performance::from_curve(p.equity, &nav)
    });
    SimReport {
        policy: policy.name.clone(),
        from: d.from,
        to: d.to,
        contributed_czk: d.flows.contributed,
        value_czk,
        tax_paid_czk: tax_paid,
        if_sold,
        after_sale_tax_czk: after_sale,
        money_weighted: xirr(&pre),
        money_weighted_after_tax: xirr(&post),
        time_weighted: unit.cagr,
        max_drawdown: unit.max_drawdown,
        trades: d.fills.len(),
        sells: d.fills.iter().filter(|f| f.side == Side::Sell).count(),
    }
}

/// Runs `policy` over `series` (USD prices) with `rates` (CZK per USD).
pub fn simulate(
    policy: &Policy,
    series: &[Series],
    rates: &BTreeMap<NaiveDate, f64>,
    costs: SimCosts,
) -> Result<SimReport, LongTermError> {
    if rates.is_empty() {
        return Err(LongTermError::Data("no exchange rates".to_owned()));
    }
    let mut strategy = PolicyStrategy::new(policy.clone());
    let mut engine = Engine::new(
        &mut strategy,
        engine_config(costs.fee_rate, costs.slippage_bps),
        &instruments(policy),
        None,
    );
    let driven = drive(&mut engine, policy, series, rates, costs.fx_cost, None)?;
    Ok(report(policy, &driven, engine.snapshot().equity, costs))
}

/// The annual rate that makes the cash flows' present value zero (bisection); 0 when none.
#[must_use]
pub fn xirr(flows: &[(DateTime<Utc>, f64)]) -> f64 {
    let Some(&(t0, _)) = flows.first() else {
        return 0.0;
    };
    let npv = |r: f64| -> f64 {
        flows
            .iter()
            .map(|(t, cf)| {
                let years = (*t - t0).num_seconds() as f64 / (365.25 * 86_400.0);
                cf / (1.0 + r).powf(years)
            })
            .sum()
    };
    let (mut lo, mut hi) = (-0.99, 10.0);
    if npv(lo).signum() == npv(hi).signum() {
        return 0.0;
    }
    for _ in 0..200 {
        let mid = f64::midpoint(lo, hi);
        if npv(mid).signum() == npv(lo).signum() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    f64::midpoint(lo, hi)
}
