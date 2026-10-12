use chrono::Duration;

use super::*;

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("test", symbol).expect("valid instrument")
}

const PLAIN: VolTarget = VolTarget {
    target_vol: 0.0,
    vol_bars: 10,
    periods_per_year: 365.0,
    band: 0.0,
};

#[test]
fn solve_matches_a_known_system() {
    let mut a = [[0.0; FEATURES]; FEATURES];
    let mut x = [0.0; FEATURES];
    for i in 0..FEATURES {
        a[i][i] = 2.0;
        if i + 1 < FEATURES {
            a[i][i + 1] = 1.0;
        }
        x[i] = i as f64 - 2.0;
    }
    let mut b = [0.0; FEATURES];
    for i in 0..FEATURES {
        b[i] = (0..FEATURES).map(|k| a[i][k] * x[k]).sum();
    }
    let solved = solve(a, b).expect("regular");
    for i in 0..FEATURES {
        assert!((solved[i] - x[i]).abs() < 1e-9, "{solved:?}");
    }
    assert!(solve([[0.0; FEATURES]; FEATURES], b).is_none());
}

#[test]
fn the_model_ranks_by_the_relationship_it_learned() {
    // The label rises with feature 1 and falls with feature 4.
    let samples: Vec<(Row, f64)> = (0..200)
        .map(|i| {
            let a = f64::from(i % 17) / 17.0;
            let b = f64::from(i % 11) / 11.0;
            let row = [0.0, a, f64::from(i % 5), 0.1, b, 0.0];
            (row, 0.5f64.mul_add(a, -0.3 * b))
        })
        .collect();
    let refs: Vec<&(Row, f64)> = samples.iter().collect();
    let model = Model::fit(&refs).expect("enough samples");
    let high = model.predict(&[0.0, 0.9, 2.0, 0.1, 0.1, 0.0]);
    let low = model.predict(&[0.0, 0.1, 2.0, 0.1, 0.9, 0.0]);
    assert!(high > low, "{high} vs {low}");
    assert!(
        model.beta[1] > 0.0 && model.beta[4] < 0.0,
        "{:?}",
        model.beta
    );
}

/// A day of bars for every coin: `UP*` rise 0.5% a day, `DN*` fall 0.5% a day.
fn feed(s: &mut XsRidge, days: i64, mut seen: impl FnMut(&InstrumentId, Option<f64>)) {
    let coins: Vec<(InstrumentId, f64)> = ["UP1", "UP2", "UP3", "DN1", "DN2", "DN3"]
        .iter()
        .map(|c| (id(c), if c.starts_with("UP") { 1.005 } else { 0.995 }))
        .collect();
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    for day in 0..days {
        for (coin, drift) in &coins {
            // A small wobble so returns and volatility vary.
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

#[test]
fn it_learns_that_trends_continue_and_holds_the_risers() {
    let mut s = XsRidge::new(2, 7, 52, PLAIN);
    let mut last: BTreeMap<InstrumentId, f64> = BTreeMap::new();
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
fn it_waits_for_history_and_labelled_snapshots() {
    let mut s = XsRidge::new(2, 7, 52, PLAIN);
    let mut first_hold = None;
    let mut day = 0_usize;
    let mut count = 0_usize;
    feed(&mut s, 400, |_, target| {
        count += 1;
        if count.is_multiple_of(6) {
            day += 1;
        }
        if first_hold.is_none() && target.is_some_and(|t| t > 0.0) {
            first_hold = Some(day);
        }
    });
    // 181 bars of features, then 12 labelled weekly snapshots (13 rebalances).
    let first = first_hold.expect("trades eventually");
    assert!(first >= 181 + 12 * 7, "first holding on day {first}");
}

#[test]
fn later_bars_cannot_change_earlier_targets() {
    let mut a = XsRidge::new(2, 7, 52, PLAIN);
    let mut b = XsRidge::new(2, 7, 52, PLAIN);
    let mut ta = Vec::new();
    let mut tb = Vec::new();
    feed(&mut a, 300, |c, t| ta.push((c.clone(), t)));
    feed(&mut b, 400, |c, t| tb.push((c.clone(), t)));
    assert_eq!(ta[..], tb[..ta.len()]);
}
