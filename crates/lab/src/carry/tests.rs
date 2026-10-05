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
