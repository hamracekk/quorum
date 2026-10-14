use super::*;
use crate::instrument::{InstrumentKind, VenueId};

fn coin(symbol: &str) -> InstrumentId {
    InstrumentId::spot("binance", symbol).expect("instrument")
}

fn stock(symbol: &str) -> InstrumentId {
    InstrumentId::new(
        VenueId::new("ibkr").expect("venue"),
        symbol,
        InstrumentKind::Equity,
    )
    .expect("instrument")
}

fn wanted(pairs: &[(InstrumentId, f64)]) -> BTreeMap<InstrumentId, f64> {
    pairs.iter().cloned().collect()
}

#[test]
fn defaults_pass_targets_through() {
    let w = wanted(&[(coin("BTCUSDT"), 0.04), (coin("ETHUSDT"), 0.0)]);
    assert_eq!(
        SizingRules::default().apply(&w, &BTreeMap::new(), 10_000.0),
        w
    );
}

#[test]
fn fixed_and_equal_modes_size_every_trade_alike() {
    let w = wanted(&[
        (coin("BTCUSDT"), 0.02),
        (coin("ETHUSDT"), 0.07),
        (coin("XRPUSDT"), 0.0),
    ]);
    let fixed = SizingRules {
        mode: SizingMode::Fixed,
        fixed_notional: 500.0,
        ..SizingRules::default()
    };
    let out = fixed.apply(&w, &BTreeMap::new(), 10_000.0);
    assert!((out[&coin("BTCUSDT")] - 0.05).abs() < 1e-12, "$500 of $10k");
    assert!((out[&coin("ETHUSDT")] - 0.05).abs() < 1e-12);
    assert!(
        out[&coin("XRPUSDT")].abs() < 1e-12,
        "no trade stays no trade"
    );
    let equal = SizingRules {
        mode: SizingMode::Equal,
        max_positions: 4,
        ..SizingRules::default()
    };
    let out = equal.apply(&w, &BTreeMap::new(), 10_000.0);
    assert!(
        (out[&coin("ETHUSDT")] - 0.25).abs() < 1e-12,
        "a quarter each for 4 slots"
    );
}

#[test]
fn a_money_cap_and_a_position_cap_keep_open_trades_first() {
    let w = wanted(&[
        (coin("AAA"), 0.30),
        (coin("BBB"), 0.20),
        (coin("CCC"), 0.10),
    ]);
    let rules = SizingRules {
        max_notional: 2_500.0,
        max_positions: 2,
        ..SizingRules::default()
    };
    // CCC is already open: it keeps its place, then the largest (AAA).
    let held = wanted(&[(coin("CCC"), 0.10)]);
    let out = rules.apply(&w, &held, 10_000.0);
    assert!((out[&coin("AAA")] - 0.25).abs() < 1e-12, "capped at $2,500");
    assert!(out[&coin("BBB")].abs() < 1e-12, "third in line: dropped");
    assert!((out[&coin("CCC")] - 0.10).abs() < 1e-12);
}

#[test]
fn each_market_keeps_to_its_budget_and_position_limit() {
    let w = wanted(&[
        (coin("BTCUSDT"), 0.4),
        (coin("ETHUSDT"), 0.4),
        (stock("NVDA"), 0.3),
        (stock("AMD"), 0.2),
    ]);
    let rules = SizingRules {
        markets: vec![
            MarketBudget {
                market: "binance:spot".into(),
                budget: 0.5,
                max_positions: 0,
            },
            MarketBudget {
                market: "ibkr".into(),
                budget: 0.0,
                max_positions: 1,
            },
        ],
        ..SizingRules::default()
    };
    let out = rules.apply(&w, &BTreeMap::new(), 10_000.0);
    let crypto = out[&coin("BTCUSDT")] + out[&coin("ETHUSDT")];
    assert!(
        (crypto - 0.5).abs() < 1e-12,
        "crypto scaled into its 50%: {crypto}"
    );
    assert!(
        (out[&stock("NVDA")] - 0.3).abs() < 1e-12,
        "the larger stock stays"
    );
    assert!(
        out[&stock("AMD")].abs() < 1e-12,
        "one equity position at most"
    );
    assert_eq!(
        market_of(&stock("NVDA")),
        format!("ibkr:{}", InstrumentKind::Equity.code())
    );
}

#[test]
fn a_fixed_trade_keeps_its_entry_size_as_equity_moves() {
    let rules = SizingRules {
        mode: SizingMode::Fixed,
        fixed_notional: 500.0,
        ..SizingRules::default()
    };
    let w = wanted(&[(coin("BTCUSDT"), 0.02), (coin("ETHUSDT"), 0.02)]);
    let held = wanted(&[(coin("BTCUSDT"), 0.05)]);
    // Equity doubled: the open trade is not resized, the new one gets $500 of $20k.
    let out = rules.apply(&w, &held, 20_000.0);
    assert!((out[&coin("BTCUSDT")] - 0.05).abs() < 1e-12);
    assert!((out[&coin("ETHUSDT")] - 0.025).abs() < 1e-12);
}

#[test]
fn equal_shares_split_among_wanted_holdings_only() {
    let rules = SizingRules {
        mode: SizingMode::Equal,
        ..SizingRules::default()
    };
    let w = wanted(&[
        (coin("A"), 0.1),
        (coin("B"), 0.3),
        (coin("C"), 0.0),
        (coin("D"), 0.0),
    ]);
    let out = rules.apply(&w, &BTreeMap::new(), 10_000.0);
    assert!(
        (out[&coin("A")] - 0.5).abs() < 1e-12,
        "two holdings, half each"
    );
    assert!(out[&coin("C")].abs() < 1e-12);
}

#[test]
fn zero_equity_leaves_targets_alone() {
    let rules = SizingRules {
        mode: SizingMode::Fixed,
        fixed_notional: 500.0,
        ..SizingRules::default()
    };
    let w = wanted(&[(coin("A"), 0.1)]);
    assert_eq!(rules.apply(&w, &BTreeMap::new(), 0.0), w);
}
