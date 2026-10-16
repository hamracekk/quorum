//! The long-term book on synthetic prices: the planner, the tax lots, and a whole policy run.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use quorum_core::strategy::BookView;
use quorum_core::{Bar, Fill, InstrumentId, Series, Side};
use quorum_longterm::{Holding, Ledger, LongTermConfig, Policy, SimCosts, plan, simulate, xirr};

fn id(t: &str) -> InstrumentId {
    format!("tiingo:{t}:equity").parse().expect("valid")
}

fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0)
        .single()
        .expect("valid")
}

fn policy() -> Policy {
    Policy {
        name: "test".to_owned(),
        start_czk: 100_000.0,
        monthly_czk: 5_000.0,
        band: 0.05,
        cash_buffer: 0.0,
        holdings: vec![
            Holding {
                instrument: id("EQ"),
                weight: 0.6,
            },
            Holding {
                instrument: id("BD"),
                weight: 0.4,
            },
        ],
    }
}

fn book(equity: f64, cash: f64, eq: f64, bd: f64) -> BookView {
    BookView {
        ts: at(2024, 1, 1),
        equity,
        cash,
        values: BTreeMap::from([(id("EQ"), eq), (id("BD"), bd)]),
    }
}

#[test]
fn the_shipped_config_parses() {
    let cfg =
        LongTermConfig::parse(include_str!("../../../config/longterm.toml")).expect("valid config");
    assert!(!cfg.policies.is_empty());
}

#[test]
fn the_first_plan_buys_the_targets() {
    let t = plan(&policy(), &book(100.0, 100.0, 0.0, 0.0), true, true).expect("targets");
    assert!((t[&id("EQ")] - 0.6).abs() < 1e-12 && (t[&id("BD")] - 0.4).abs() < 1e-12);
}

#[test]
fn new_money_goes_to_the_underweight_holding_without_selling() {
    // Equity 55 (target 60), bonds 35 (target 40), 10 new cash: the two gaps take it all.
    let t = plan(&policy(), &book(100.0, 10.0, 55.0, 35.0), false, true).expect("targets");
    assert!((t[&id("EQ")] - 0.60).abs() < 1e-9, "{t:?}");
    assert!((t[&id("BD")] - 0.40).abs() < 1e-9, "{t:?}");
    // Equity over target (65 vs 60): new money goes to bonds only, equity is not sold.
    let t = plan(&policy(), &book(100.0, 4.0, 65.0, 31.0), false, true).expect("targets");
    assert!(
        !t.contains_key(&id("EQ")),
        "equity is neither bought nor sold: {t:?}"
    );
    assert!((t[&id("BD")] - 0.35).abs() < 1e-9, "{t:?}");
}

#[test]
fn a_band_breach_without_new_money_rebalances_and_drift_inside_it_does_not() {
    assert!(plan(&policy(), &book(100.0, 0.0, 63.0, 37.0), false, false).is_none());
    let t = plan(&policy(), &book(100.0, 0.0, 70.0, 30.0), false, false).expect("rebalance");
    assert!((t[&id("EQ")] - 0.6).abs() < 1e-12);
}

fn fill(side: Side, ts: DateTime<Utc>, qty: f64, price: f64) -> Fill {
    Fill {
        ts,
        instrument: id("EQ"),
        side,
        qty,
        price,
        fee: 0.0,
    }
}

#[test]
fn lots_are_sold_oldest_first_and_exempt_after_three_years() {
    let mut ledger = Ledger::default();
    ledger.apply(&fill(Side::Buy, at(2020, 1, 1), 10.0, 10.0), 20.0);
    ledger.apply(&fill(Side::Buy, at(2022, 1, 1), 10.0, 20.0), 20.0);
    // 2023-06: the 2020 lot is over three years old, the 2022 lot is not.
    ledger.apply(&fill(Side::Sell, at(2023, 6, 1), 15.0, 30.0), 20.0);
    let r = ledger.realized();
    assert_eq!(r.len(), 2);
    assert!(r[0].exempt && (r[0].gain_czk - 4_000.0).abs() < 1e-9);
    assert!(!r[1].exempt && (r[1].gain_czk - 1_000.0).abs() < 1e-9);
    // Only the non-exempt gain is taxed: 1 000 CZK × 15%.
    let tax = ledger.tax_by_year(0.15);
    assert!((tax[&2023] - 150.0).abs() < 1e-9);
    let left = ledger.if_sold(&BTreeMap::from([(id("EQ"), 30.0)]), 20.0, at(2023, 6, 1));
    assert!((left.taxable_gain - 1_000.0).abs() < 1e-9);
    // The 2022 lot frees up in January 2025: more than a year away.
    assert!(left.exempt_within_year.abs() < 1e-12);
    let later = ledger.if_sold(&BTreeMap::from([(id("EQ"), 30.0)]), 20.0, at(2024, 3, 1));
    assert!(later.exempt_within_year > 0.0);
}

#[test]
fn the_money_weighted_return_of_a_simple_flow() {
    let flows = [(at(2020, 1, 1), -100.0), (at(2021, 1, 1), 110.0)];
    assert!((xirr(&flows) - 0.0996).abs() < 0.001);
}

fn rising(t: &str, growth: f64) -> Series {
    let bars = (0..900)
        .map(|d| {
            let p = 100.0 * growth.powi(d);
            Bar {
                ts: at(2020, 1, 1) + Duration::days(i64::from(d)),
                open: p,
                high: p,
                low: p,
                close: p,
                volume: 1e6,
            }
        })
        .collect();
    Series::new(id(t), bars)
}

#[test]
fn a_policy_run_contributes_monthly_and_grows_in_czk() {
    let series = [rising("EQ", 1.0004), rising("BD", 1.0001)];
    let rates: BTreeMap<NaiveDate, f64> = BTreeMap::from([(at(2019, 12, 1).date_naive(), 20.0)]);
    let costs = SimCosts {
        fee_rate: 0.0,
        slippage_bps: 0.0,
        fx_cost: 0.0,
        tax_rate: 0.15,
    };
    let r = simulate(&policy(), &series, &rates, costs).expect("report");
    // About 29 months of 5 000 on top of 100 000.
    assert!(
        r.contributed_czk > 240_000.0 && r.contributed_czk < 255_000.0,
        "{r:?}"
    );
    assert!(r.value_czk > r.contributed_czk, "{r:?}");
    assert!(r.money_weighted > 0.0 && r.time_weighted > 0.0);
    assert_eq!(r.sells, 0, "steady drift inside the band never sells");
    assert!(r.trades > 2);
}
