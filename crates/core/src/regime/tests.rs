use chrono::{Duration, TimeZone};

use super::*;
use crate::instrument::InstrumentId;
use crate::types::Bar;

fn series(name: &str, closes: &[f64]) -> Series {
    let t0 = Utc
        .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
        .single()
        .expect("valid");
    let bars = closes
        .iter()
        .enumerate()
        .map(|(i, &c)| Bar {
            ts: t0 + Duration::days(i64::try_from(i).expect("small")),
            open: c,
            high: c,
            low: c,
            close: c,
            volume: 1.0,
        })
        .collect();
    Series::new(InstrumentId::spot("test", name).expect("valid"), bars)
}

/// Calm, steady rise; then a crash with wild swings.
fn calm_then_crash(name: &str, wiggle: f64) -> Series {
    let mut c = 100.0;
    let mut closes = Vec::new();
    for i in 0..500 {
        c *= if i % 2 == 0 { 1.004 + wiggle } else { 0.998 };
        closes.push(c);
    }
    for i in 0..60 {
        c *= if i % 2 == 0 { 0.90 } else { 1.04 };
        closes.push(c);
    }
    series(name, &closes)
}

fn rule() -> RegimeRule {
    RegimeRule {
        vol_short_bars: 10,
        breadth_bars: 20,
        trend_bars: 40,
        ..RegimeRule::default()
    }
}

#[test]
fn a_calm_rise_is_trending_and_a_violent_fall_is_a_crisis() {
    let universe: Vec<Series> = (0..5)
        .map(|i| calm_then_crash(&format!("A{i}"), 0.0001 * f64::from(i)))
        .collect();
    let points = regimes(&universe, &rule(), 250);
    assert!(!points.is_empty());
    let calm = points
        .iter()
        .find(|p| p.ts == universe[0].bars[400].ts)
        .expect("calm date");
    assert_eq!(calm.regime, Regime::Trending, "{:?}", calm.features);
    let last = points.last().expect("last");
    assert_eq!(last.regime, Regime::Crisis, "{:?}", last.features);
    assert!(last.features.drawdown > 0.5);
}

#[test]
fn labels_are_point_in_time() {
    let universe: Vec<Series> = (0..5)
        .map(|i| calm_then_crash(&format!("A{i}"), 0.0001 * f64::from(i)))
        .collect();
    let cut: Vec<Series> = universe
        .iter()
        .map(|s| Series::new(s.instrument.clone(), s.bars[..450].to_vec()))
        .collect();
    let full = regimes(&universe, &rule(), 250);
    let early = regimes(&cut, &rule(), 250);
    for p in &early {
        let same = full.iter().find(|q| q.ts == p.ts).expect("same date");
        assert_eq!(
            same.regime, p.regime,
            "future bars changed the label at {}",
            p.ts
        );
    }
}

#[test]
fn rules_are_checked_in_order() {
    let r = RegimeRule::default();
    let f = |vol_ratio, drawdown, breadth| RegimeFeatures {
        vol_ratio,
        drawdown,
        breadth,
        trend: 0.0,
    };
    assert_eq!(r.classify(&f(1.6, 0.3, 0.9)), Regime::Crisis);
    assert_eq!(
        r.classify(&f(1.6, 0.1, 0.9)),
        Regime::RiskOff,
        "volatile but not deep"
    );
    assert_eq!(
        r.classify(&f(1.0, 0.0, 0.2)),
        Regime::RiskOff,
        "most instruments falling"
    );
    assert_eq!(r.classify(&f(1.0, 0.0, 0.8)), Regime::Trending);
    assert_eq!(r.classify(&f(1.0, 0.0, 0.5)), Regime::Choppy);
}

#[test]
fn the_market_is_equal_weighted_over_instruments_present() {
    let a = series("A", &[100.0, 110.0, 121.0]);
    let b = series("B", &[100.0, 90.0]);
    let m = market_returns(&[a, b]);
    assert_eq!(m.len(), 2);
    assert!((m[0].1 - 0.0).abs() < 1e-12, "(+10% − 10%) / 2");
    assert_eq!(m[1].2, 1, "only A has the third bar");
    assert!((m[1].1 - 0.1).abs() < 1e-12);
}
