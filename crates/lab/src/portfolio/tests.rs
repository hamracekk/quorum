use chrono::{DateTime, Duration, Utc};

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
