//! A policy run over history on the one engine, in CZK.
//!
//! The start amount and monthly contributions are converted at ČNB rates, fills are booked as tax
//! lots, and the result is measured the way an investor feels it (money-weighted) and the way
//! funds are compared (time-weighted).

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Months, NaiveDate, TimeZone, Utc};
use quorum_core::metrics::Performance;
use quorum_core::risk::RiskLimits;
use quorum_core::{BacktestConfig, Bar, Engine, EquityPoint, Event, InstrumentId, Series, Side};
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
struct Flows {
    cash: Vec<(DateTime<Utc>, f64)>,
    contributed: f64,
    units: f64,
    nav: Vec<EquityPoint>,
}

/// The policy's instruments, its start and its bars in order.
type Timeline<'a> = (
    Vec<InstrumentId>,
    DateTime<Utc>,
    Vec<(&'a InstrumentId, &'a Bar)>,
);

/// The policy's instruments, the first date every holding and a rate exist, and every bar from
/// then on in time order (holdings in policy order within a date).
fn timeline<'a>(
    policy: &Policy,
    series: &'a [Series],
    rates: &BTreeMap<NaiveDate, f64>,
) -> Result<Timeline<'a>, LongTermError> {
    let instruments: Vec<InstrumentId> = policy
        .holdings
        .iter()
        .map(|h| h.instrument.clone())
        .collect();
    let held: Vec<&Series> = instruments
        .iter()
        .map(|id| {
            series.iter().find(|s| &s.instrument == id).ok_or_else(|| {
                LongTermError::Data(format!("{}: no stored bars for {id}", policy.name))
            })
        })
        .collect::<Result<_, _>>()?;
    // From the first date every holding and a rate exist.
    let start = held
        .iter()
        .filter_map(|s| s.bars.first().map(|b| b.ts))
        .chain(
            rates
                .keys()
                .next()
                .map(|d| d.and_time(chrono::NaiveTime::MIN).and_utc()),
        )
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
    Ok((
        instruments,
        start,
        bars.into_iter().map(|(_, id, bar)| (id, bar)).collect(),
    ))
}

/// Runs `policy` over `series` (USD prices) with `rates` (CZK per USD).
pub fn simulate(
    policy: &Policy,
    series: &[Series],
    rates: &BTreeMap<NaiveDate, f64>,
    costs: SimCosts,
) -> Result<SimReport, LongTermError> {
    let (instruments, start, bars) = timeline(policy, series, rates)?;
    let first_rate = rate_at(rates, start)
        .ok_or_else(|| LongTermError::Data("no exchange rate at the start".to_owned()))?;

    let cfg = BacktestConfig {
        initial_cash: policy.start_czk / first_rate * (1.0 - costs.fx_cost),
        fee_rate: costs.fee_rate,
        slippage_bps: costs.slippage_bps,
        risk: RiskLimits {
            // A long-term book rides out a bad day; the daily kill switch is for trading.
            max_daily_loss: 1.0,
            ..RiskLimits::default()
        },
        ..BacktestConfig::default()
    };
    let mut strategy = PolicyStrategy::new(policy.clone());
    let mut engine = Engine::new(&mut strategy, cfg, &instruments, None);

    let mut ledger = Ledger::default();
    let mut f = Flows {
        cash: vec![(start, -policy.start_czk)],
        contributed: policy.start_czk,
        units: policy.start_czk,
        nav: Vec::new(),
    };
    let mut next_contribution = month_start(start) + Months::new(1);
    let mut value = policy.start_czk;
    let mut current: Option<DateTime<Utc>> = None;
    let (mut trades, mut sells) = (0, 0);
    let mut last_close: BTreeMap<InstrumentId, f64> = BTreeMap::new();

    for (instrument, bar) in bars {
        let rate = rate_at(rates, bar.ts).unwrap_or(first_rate);
        if current.is_none_or(|c| bar.ts > c) {
            if let Some(c) = current {
                // The timestamp before this one is complete: value the book.
                value = engine.snapshot().equity * rate;
                f.nav.push(EquityPoint {
                    ts: c,
                    equity: value / f.units,
                });
            }
            if bar.ts >= next_contribution && policy.monthly_czk > 0.0 {
                let nav = (value / f.units).max(f64::MIN_POSITIVE);
                f.units += policy.monthly_czk / nav;
                engine.deposit(policy.monthly_czk / rate * (1.0 - costs.fx_cost));
                f.cash.push((bar.ts, -policy.monthly_czk));
                f.contributed += policy.monthly_czk;
                value += policy.monthly_czk;
                while next_contribution <= bar.ts {
                    next_contribution = next_contribution + Months::new(1);
                }
            }
            current = Some(bar.ts);
        }
        last_close.insert(instrument.clone(), bar.close);
        let events = engine
            .on_bar(instrument, bar)
            .map_err(|e| LongTermError::Data(e.to_string()))?;
        for event in &events {
            if let Event::Fill(fill) = event {
                ledger.apply(fill, rate_at(rates, fill.ts).unwrap_or(rate));
                trades += 1;
                sells += usize::from(fill.side == Side::Sell);
            }
        }
    }
    let end = current.unwrap_or(start);
    let end_rate = rate_at(rates, end).unwrap_or(first_rate);
    let value_czk = engine.snapshot().equity * end_rate * (1.0 - costs.fx_cost);
    f.nav.push(EquityPoint {
        ts: end,
        equity: value_czk / f.units,
    });
    let tax_paid: f64 = ledger.tax_by_year(costs.tax_rate).values().sum();
    let if_sold = ledger.if_sold(&last_close, end_rate, end);
    let after_sale = if_sold
        .taxable_gain
        .max(0.0)
        .mul_add(-costs.tax_rate, value_czk - tax_paid);
    let mut pre = f.cash.clone();
    pre.push((end, value_czk - tax_paid));
    let mut post = f.cash;
    post.push((end, after_sale));
    let unit = f.nav.first().map_or_else(Performance::default, |p| {
        Performance::from_curve(p.equity, &f.nav)
    });
    Ok(SimReport {
        policy: policy.name.clone(),
        from: Some(start),
        to: current,
        contributed_czk: f.contributed,
        value_czk,
        tax_paid_czk: tax_paid,
        if_sold,
        after_sale_tax_czk: after_sale,
        money_weighted: xirr(&pre),
        money_weighted_after_tax: xirr(&post),
        time_weighted: unit.cagr,
        max_drawdown: unit.max_drawdown,
        trades,
        sells,
    })
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
