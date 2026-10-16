use chrono::{DateTime, Duration, TimeZone, Utc};

use super::*;

fn curve(daily: &[f64]) -> Vec<EquityPoint> {
    let start: DateTime<Utc> = DateTime::from_timestamp(0, 0).expect("ts");
    let mut equity = 100.0;
    let mut out = vec![EquityPoint { ts: start, equity }];
    for (r, day) in daily.iter().zip(1_i64..) {
        equity *= 1.0 + r;
        out.push(EquityPoint {
            ts: start + Duration::days(day),
            equity,
        });
    }
    out
}

fn config(weighting: Weighting) -> CombineConfig {
    CombineConfig {
        tilts: Vec::new(),
        initial_cash: 1_000.0,
        weighting,
        vol_window_days: 10,
        rebalance_days: 5,
    }
}

#[test]
fn two_opposite_streams_cancel_into_a_smoother_portfolio() {
    let up: Vec<f64> = (0..40)
        .map(|i| if i % 2 == 0 { 0.03 } else { -0.02 })
        .collect();
    let down: Vec<f64> = up.iter().map(|r| r.mul_add(-0.5, 0.004)).collect();
    let streams = [
        Stream {
            name: "a".into(),
            curve: curve(&up),
        },
        Stream {
            name: "b".into(),
            curve: curve(&down),
        },
    ];
    let combined = combine(&streams, &config(Weighting::Fixed(vec![0.5, 0.5])));
    assert_eq!(combined.curve.len(), 40);
    assert!(
        combined.correlation[0][1] < -0.9,
        "{:?}",
        combined.correlation
    );
    let a = &combined.streams[0].report.strategy;
    assert!(
        combined.report.strategy.max_drawdown < a.max_drawdown,
        "diversified drawdown {} vs {}",
        combined.report.strategy.max_drawdown,
        a.max_drawdown
    );
    assert!((combined.streams[0].avg_weight + combined.streams[1].avg_weight - 1.0).abs() < 1e-9);
}

#[test]
fn inverse_volatility_is_capped_so_a_calm_stream_cannot_take_everything() {
    let wild: Vec<f64> = (0..30)
        .map(|i| if i % 2 == 0 { 0.05 } else { -0.04 })
        .collect();
    let calm: Vec<f64> = vec![0.0001; 30];
    let history = vec![wild, calm];
    let w = weights(&history, &config(Weighting::InverseVol { max_weight: 0.7 }));
    assert!((w[1] - 0.7).abs() < 1e-9, "calm stream capped: {w:?}");
    assert!((w[0] - 0.3).abs() < 1e-9);
}

#[test]
fn tilts_hold_part_of_a_stream_in_cash_from_the_next_rebalance() {
    let start = Utc
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .expect("valid");
    let rising = |name: &str| Stream {
        name: name.into(),
        curve: (0..40)
            .map(|d| EquityPoint {
                ts: start + Duration::days(d),
                equity: 100.0 * 1.01_f64.powi(i32::try_from(d).expect("small")),
            })
            .collect(),
    };
    let streams = [rising("a"), rising("b")];
    let base = CombineConfig {
        weighting: Weighting::Fixed(vec![0.5, 0.5]),
        rebalance_days: 1,
        ..config(Weighting::Fixed(vec![0.5, 0.5]))
    };
    let full = combine(&streams, &base);
    // Stream b held at half weight from day 10 on: less of the rise is earned.
    let half: BTreeMap<NaiveDate, f64> = [((start + Duration::days(10)).date_naive(), 0.5)].into();
    let tilted = combine(
        &streams,
        &CombineConfig {
            tilts: vec![BTreeMap::new(), half],
            ..base
        },
    );
    let (f, t) = (
        full.curve.last().expect("f").equity,
        tilted.curve.last().expect("t").equity,
    );
    assert!(t < f, "{t} vs {f}");
    // Before the tilt day the curves are identical (point in time).
    assert!((full.curve[5].equity - tilted.curve[5].equity).abs() < 1e-9);
}
