use super::*;

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("test", symbol).expect("instrument")
}

fn prints(rate: f64, count: i64) -> Vec<FundingPoint> {
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    (0..count)
        .map(|i| FundingPoint {
            ts: start + Duration::hours(8 * i),
            rate,
        })
        .collect()
}

fn cfg() -> CarryConfig {
    CarryConfig {
        initial_cash: 10_000.0,
        lookback: Duration::days(10),
        min_annual: 0.1,
        top: 5,
        rebalance: Duration::days(7),
        cost: 0.001,
        margin: 0.5,
        exit_annual: 0.1,
        hurdle_days: 0.0,
        liquidation: None,
        holdout_from: None,
    }
}

#[test]
fn carry_holds_coins_paying_enough_funding_and_collects_it() {
    let funding = BTreeMap::from([
        (id("PAYS"), prints(0.0003, 300)),
        (id("NEG"), prints(-0.0002, 300)),
    ]);
    let result = carry(&funding, &cfg());
    assert!(
        (result.avg_held - 1.0).abs() < 0.01,
        "only the coin paying ~33%/yr: {}",
        result.avg_held
    );
    assert!(result.funding > 0.0);
    let notional = 10_000.0 / 1.5;
    let entry_cost = notional * 0.001 * 2.0;
    assert!(
        (result.costs - entry_cost).abs() / entry_cost < 0.05,
        "about one entry on both legs: {}",
        result.costs
    );
    assert!(result.report.strategy.total_return > 0.0);
    assert!(
        result.report.strategy.max_drawdown < 0.01,
        "funding only: a smooth curve"
    );
}

#[test]
fn nothing_is_held_when_funding_is_too_low() {
    let funding = BTreeMap::from([(id("LOW"), prints(0.00005, 300))]);
    let result = carry(&funding, &cfg());
    assert!(result.avg_held.abs() < f64::EPSILON);
    assert!(result.report.strategy.total_return.abs() < f64::EPSILON);
}

/// Flat daily bars at 100 with one spike to `spike_high` on day `spike_day`.
fn bars(days: i64, spike_day: i64, spike_high: f64) -> Vec<Bar> {
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    (0..days)
        .map(|d| Bar {
            ts: start + Duration::days(d),
            open: 100.0,
            high: if d == spike_day { spike_high } else { 100.0 },
            low: 100.0,
            close: 100.0,
            volume: 1.0,
        })
        .collect()
}

#[test]
fn the_cost_hurdle_skips_funding_too_thin_to_pay_four_legs() {
    // ~11%/yr clears `min_annual` (10%) but not four legs at 1% each within 30 days (~49%/yr).
    let funding = BTreeMap::from([(id("THIN"), prints(0.0001, 300))]);
    let open = carry(&funding, &cfg());
    assert!(open.avg_held > 0.9);
    let gated = carry(
        &funding,
        &CarryConfig {
            cost: 0.01,
            hurdle_days: 30.0,
            ..cfg()
        },
    );
    assert!((gated.entry_annual - 4.0 * 0.01 * 365.0 / 30.0).abs() < 1e-12);
    assert!(gated.avg_held.abs() < f64::EPSILON);
}

#[test]
fn a_held_coin_stays_until_funding_falls_below_the_exit_level() {
    // 33%/yr for 20 days, then 5.5%/yr: below entry (10%) but above exit (2%).
    let mut points = prints(0.0003, 60);
    let tail = prints(0.00005, 300);
    let shift = points.last().expect("prints").ts + Duration::hours(8);
    points.extend(tail.into_iter().map(|p| FundingPoint {
        ts: p.ts + (shift - DateTime::from_timestamp(0, 0).expect("ts")),
        rate: p.rate,
    }));
    let funding = BTreeMap::from([(id("FADES"), points)]);
    let strict = carry(&funding, &cfg());
    let sticky = carry(
        &funding,
        &CarryConfig {
            exit_annual: 0.02,
            ..cfg()
        },
    );
    assert!(
        sticky.avg_held > strict.avg_held + 0.5,
        "{} vs {}",
        sticky.avg_held,
        strict.avg_held
    );
}

#[test]
fn a_spike_past_the_margin_liquidates_the_short_leg() {
    let funding = BTreeMap::from([(id("PAYS"), prints(0.0003, 300))]);
    let rule = Liquidation {
        maintenance: 0.01,
        fee: 0.0125,
    };
    let with = CarryConfig {
        liquidation: Some(rule),
        ..cfg()
    };
    // Below the limit (100 × 1.49): nothing happens.
    let calm = carry_with(
        &funding,
        &BTreeMap::from([(id("PAYS"), bars(100, 40, 140.0))]),
        &with,
    );
    assert_eq!(calm.liquidations, 0);
    let spiked = carry_with(
        &funding,
        &BTreeMap::from([(id("PAYS"), bars(100, 40, 160.0))]),
        &with,
    );
    assert_eq!(spiked.liquidations, 1);
    assert!(spiked.costs > calm.costs);
    assert!(
        spiked.funding < calm.funding,
        "out until the next rebalance"
    );
    // Without a rule, prices change nothing.
    let ignored = carry_with(
        &funding,
        &BTreeMap::from([(id("PAYS"), bars(100, 40, 160.0))]),
        &cfg(),
    );
    assert_eq!(ignored.liquidations, 0);
}

#[test]
fn the_holdout_is_reported_on_its_own() {
    let funding = BTreeMap::from([(id("PAYS"), prints(0.0003, 300))]);
    let from = DateTime::from_timestamp(0, 0).expect("ts") + Duration::days(60);
    let result = carry(
        &funding,
        &CarryConfig {
            holdout_from: Some(from),
            ..cfg()
        },
    );
    let holdout = result.holdout.expect("history after the split");
    let whole = 1.0 + result.report.strategy.total_return;
    let parts =
        (1.0 + result.in_sample.strategy.total_return) * (1.0 + holdout.strategy.total_return);
    assert!((whole - parts).abs() < 1e-9, "{whole} vs {parts}");
    assert!(holdout.strategy.total_return > 0.0);
}
