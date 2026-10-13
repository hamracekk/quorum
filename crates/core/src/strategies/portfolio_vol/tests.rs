use chrono::Duration;

use super::*;
use crate::strategies::{BuyAndHold, PerInstrument};

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("test", symbol).expect("instrument")
}

fn rule(target_vol: f64) -> PortfolioVolRule {
    PortfolioVolRule {
        target_vol,
        window: 30,
        periods_per_year: 365.0,
        band: 0.0,
    }
}

/// Two coins held half each; `together` makes them move identically, else oppositely.
fn run(target: f64, together: bool, days: i64) -> (PortfolioVolTarget, BTreeMap<String, f64>) {
    let inner = Box::new(PerInstrument::new(BuyAndHold::default(), 2));
    let mut s = PortfolioVolTarget::new(inner, rule(target));
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    let (mut a, mut b) = (100.0_f64, 100.0_f64);
    let mut last = BTreeMap::new();
    for day in 0..days {
        // ±5% a day: about 95% annualised volatility per coin.
        let r = if day % 2 == 0 { 1.05 } else { 1.0 / 1.05 };
        a *= r;
        b *= if together { r } else { 1.0 / r };
        for (symbol, close) in [("A", a), ("B", b)] {
            let bar = Bar {
                ts: start + Duration::days(day),
                open: close,
                high: close,
                low: close,
                close,
                volume: 1.0,
            };
            if let Some(t) = s.on_bar(&id(symbol), &bar) {
                last.insert(symbol.to_owned(), t);
            }
        }
    }
    (s, last)
}

#[test]
fn correlated_coins_are_scaled_to_the_portfolio_target() {
    let (s, last) = run(0.3, true, 60);
    // Two perfectly correlated halves: the book's volatility is one coin's (~93%).
    assert!(s.scale() < 0.4 && s.scale() > 0.25, "scale {}", s.scale());
    assert!(
        0.5f64.mul_add(-s.scale(), last["A"]).abs() < 1e-9,
        "{last:?}"
    );
}

#[test]
fn offsetting_coins_are_left_alone() {
    // Perfectly opposite moves cancel: the book barely moves, nothing is cut.
    let (s, last) = run(0.3, false, 60);
    assert!((s.scale() - 1.0).abs() < 1e-12, "scale {}", s.scale());
    assert!((last["A"] - 0.5).abs() < 1e-12);
}

#[test]
fn nothing_is_scaled_before_enough_history() {
    let (s, _) = run(0.3, true, 10);
    assert!((s.scale() - 1.0).abs() < 1e-12);
}
