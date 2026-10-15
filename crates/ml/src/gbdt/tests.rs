use chrono::{DateTime, Duration, Utc};
use quorum_core::strategies::Ridge;
use quorum_core::{Bar, InstrumentId, Strategy};

use super::*;

/// Deterministic pseudo-random numbers in [-1, 1).
fn noise(seed: u64, n: usize) -> Vec<f64> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as f64 / f64::from(1_u32 << 31)).mul_add(2.0, -1.0)
        })
        .collect()
}

/// Labels that rise with |feature 0| — a U shape no straight line can follow.
fn u_shaped(n: usize) -> Vec<(Row, f64)> {
    let a = noise(1, n);
    let b = noise(2, n);
    (0..n)
        .map(|i| {
            let row = [a[i], b[i], 0.0, 0.0, 0.0, 0.0];
            (row, a[i].abs() - 0.5)
        })
        .collect()
}

fn refs(samples: &[(Row, f64)]) -> Vec<&(Row, f64)> {
    samples.iter().collect()
}

#[test]
fn trees_learn_a_shape_a_line_cannot() {
    let samples = u_shaped(600);
    let mut trees = Gbdt::new(BoostParams::default());
    assert!(trees.fit(&refs(&samples)));
    let edge = trees.predict(&[0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let other_edge = trees.predict(&[-0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let middle = trees.predict(&[0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert!(
        edge > middle + 0.3 && other_edge > middle + 0.3,
        "{edge} {other_edge} {middle}"
    );

    let mut line = Ridge::default();
    assert!(line.fit(&refs(&samples)));
    let line_edge = line.predict(&[0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let line_middle = line.predict(&[0.0; 6]);
    assert!(
        (line_edge - line_middle).abs() < 0.1,
        "a line through a symmetric U is flat: {line_edge} vs {line_middle}"
    );
}

#[test]
fn the_same_data_gives_the_same_trees() {
    let samples = u_shaped(300);
    let mut a = Gbdt::new(BoostParams::default());
    let mut b = Gbdt::new(BoostParams::default());
    a.fit(&refs(&samples));
    b.fit(&refs(&samples));
    for row in [
        [0.3, -0.2, 0.0, 0.0, 0.0, 0.0],
        [-0.7, 0.9, 0.0, 0.0, 0.0, 0.0],
    ] {
        assert_eq!(a.predict(&row).to_bits(), b.predict(&row).to_bits());
    }
}

#[test]
fn too_few_samples_fit_nothing() {
    let samples = u_shaped(30);
    let mut trees = Gbdt::new(BoostParams::default());
    assert!(!trees.fit(&refs(&samples)), "min_leaf 20 needs 40 samples");
}

#[test]
fn one_extreme_label_does_not_shape_the_trees() {
    let mut samples = u_shaped(400);
    let calm = {
        let mut trees = Gbdt::new(BoostParams::default());
        trees.fit(&refs(&samples));
        trees.predict(&[0.05, 0.0, 0.0, 0.0, 0.0, 0.0])
    };
    // A coin that went up 50× in one week, right in the middle.
    samples[0] = ([0.05, 0.0, 0.0, 0.0, 0.0, 0.0], 50.0);
    let mut trees = Gbdt::new(BoostParams::default());
    trees.fit(&refs(&samples));
    let shocked = trees.predict(&[0.05, 0.0, 0.0, 0.0, 0.0, 0.0]);
    assert!(
        (shocked - calm).abs() < 0.2,
        "winsorised: {calm} → {shocked}"
    );
}

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("test", symbol).expect("valid instrument")
}

const PLAIN: VolTarget = VolTarget {
    target_vol: 0.0,
    vol_bars: 10,
    periods_per_year: 365.0,
    band: 0.0,
};

/// A day of bars for every coin: `UP*` rise 0.5% a day, `DN*` fall 0.5% a day.
fn feed(s: &mut XsLearner, days: i64, mut seen: impl FnMut(&InstrumentId, Option<f64>)) {
    let coins: Vec<(InstrumentId, f64)> = ["UP1", "UP2", "UP3", "DN1", "DN2", "DN3"]
        .iter()
        .map(|c| (id(c), if c.starts_with("UP") { 1.005 } else { 0.995 }))
        .collect();
    let start = DateTime::<Utc>::from_timestamp(0, 0).expect("ts");
    for day in 0..days {
        for (coin, drift) in &coins {
            let wobble = if day % 3 == 0 { 1.002 } else { 0.999 };
            let close = 100.0 * drift.powf(day as f64) * wobble;
            let bar = Bar {
                ts: start + Duration::days(day),
                open: close,
                high: close,
                low: close,
                close,
                volume: 1_000.0,
            };
            let target = s.on_bar(coin, &bar);
            seen(coin, target);
        }
    }
}

fn small() -> BoostParams {
    BoostParams {
        trees: 30,
        min_leaf: 5,
        ..BoostParams::default()
    }
}

#[test]
fn as_a_strategy_it_holds_the_risers() {
    let mut s = XsBoost::strategy(small(), 2, 7, 52, PLAIN);
    assert_eq!(s.name(), "xs-boost");
    let mut last = std::collections::BTreeMap::new();
    feed(&mut s, 400, |coin, target| {
        if let Some(t) = target {
            last.insert(coin.clone(), t);
        }
    });
    let held: Vec<&InstrumentId> = last
        .iter()
        .filter(|(_, t)| **t > 0.0)
        .map(|(c, _)| c)
        .collect();
    assert_eq!(held.len(), 2, "{last:?}");
    assert!(
        held.iter().all(|c| c.symbol().starts_with("UP")),
        "{held:?}"
    );
}

#[test]
fn later_bars_cannot_change_earlier_targets() {
    let mut a = XsBoost::strategy(small(), 2, 7, 52, PLAIN);
    let mut b = XsBoost::strategy(small(), 2, 7, 52, PLAIN);
    let mut ta = Vec::new();
    let mut tb = Vec::new();
    feed(&mut a, 300, |c, t| ta.push((c.clone(), t)));
    feed(&mut b, 400, |c, t| tb.push((c.clone(), t)));
    assert_eq!(ta[..], tb[..ta.len()]);
}
