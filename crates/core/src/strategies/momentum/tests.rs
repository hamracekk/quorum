use chrono::Duration;

use super::*;

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("test", symbol).expect("valid instrument")
}

fn bar(day: i32, close: f64) -> Bar {
    Bar {
        ts: DateTime::from_timestamp(0, 0).expect("ts") + Duration::days(i64::from(day)),
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
    }
}

const PLAIN: VolTarget = VolTarget {
    target_vol: 0.0,
    vol_bars: 10,
    periods_per_year: 365.0,
    band: 0.0,
};

#[test]
fn trend_momentum_holds_while_the_lookback_return_is_positive() {
    let mut s = TrendMomentum::new(3, PLAIN);
    let a = id("A");
    let closes = [10.0, 11.0, 12.0, 13.0, 14.0, 12.0, 11.0, 10.0];
    let targets: Vec<Option<f64>> = closes
        .iter()
        .zip(0_i32..)
        .map(|(c, day)| s.on_bar(&a, &bar(day, *c)))
        .collect();
    assert_eq!(targets[..3], [None, None, None], "waits for the lookback");
    assert_eq!(targets[3], Some(1.0), "13 > 10: long");
    assert_eq!(targets[4], None, "unchanged target is not repeated");
    assert_eq!(targets[5], Some(0.0), "12 is not above 12: flat");
    assert_eq!(targets[6], None, "still flat");
}

#[test]
fn volatility_targeting_shrinks_wild_assets_and_the_band_skips_noise() {
    let sized = VolTarget {
        target_vol: 0.5,
        vol_bars: 10,
        periods_per_year: 365.0,
        band: 0.2,
    };
    let mut calm = TrendMomentum::new(4, sized);
    let mut wild = TrendMomentum::new(4, sized);
    let (mut last_calm, mut last_wild) = (None, None);
    for day in 0..30_i32 {
        let up = 100.0 * 1.001_f64.powi(day);
        let swing = if day % 2 == 0 { 1.08 } else { 0.95 };
        let jagged = 100.0 * swing * 1.01_f64.powi(day);
        last_calm = calm.on_bar(&id("C"), &bar(day, up)).or(last_calm);
        last_wild = wild.on_bar(&id("W"), &bar(day, jagged)).or(last_wild);
    }
    assert_eq!(last_calm, Some(1.0), "a calm riser is held in full");
    let w = last_wild.expect("held");
    assert!(w > 0.0 && w < 0.6, "a wild riser is scaled down: {w}");
    let held = Some(0.5);
    assert_eq!(sized.banded(held, 0.55), None, "within 20%: keep");
    assert_eq!(sized.banded(held, 0.7), Some(0.7));
    assert_eq!(sized.banded(held, 0.0), Some(0.0), "exits always pass");
}

fn run_xs(order: &[&str]) -> BTreeMap<String, f64> {
    let mut s = XsMomentum::new(3, 1, 1, PLAIN);
    let mut last = BTreeMap::new();
    for day in 0..8_i32 {
        for symbol in order {
            let close = match *symbol {
                "FAST" => 10.0 * 1.10_f64.powi(day),
                "SLOW" => 10.0 * 1.01_f64.powi(day),
                _ => 10.0 * 0.95_f64.powi(day),
            };
            if let Some(t) = s.on_bar(&id(symbol), &bar(day, close)) {
                last.insert((*symbol).to_owned(), t);
            }
        }
    }
    last
}

#[test]
fn cross_sectional_momentum_holds_the_strongest_riser_whatever_the_feed_order() {
    let a = run_xs(&["FAST", "SLOW", "DOWN"]);
    assert_eq!(a.get("FAST"), Some(&1.0), "{a:?}");
    assert_eq!(a.get("SLOW"), Some(&0.0));
    assert_eq!(a.get("DOWN"), Some(&0.0), "falling: never held");
    assert_eq!(
        run_xs(&["DOWN", "SLOW", "FAST"]),
        a,
        "order does not matter"
    );
}
