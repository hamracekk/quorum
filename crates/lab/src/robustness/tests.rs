use chrono::{DateTime, Duration, Utc};

use super::*;

fn curve(daily: impl Fn(usize) -> f64, days: usize) -> Vec<EquityPoint> {
    let start: DateTime<Utc> = DateTime::from_timestamp(0, 0).expect("ts");
    let mut equity = 10_000.0;
    let mut ts = start;
    (0..=days)
        .map(|d| {
            if d > 0 {
                equity *= 1.0 + daily(d);
                ts += Duration::days(1);
            }
            EquityPoint { ts, equity }
        })
        .collect()
}

#[test]
fn the_bootstrap_brackets_the_actual_result_and_is_repeatable() {
    // +0.2% most days, −1% every fifth.
    let c = curve(|d| if d % 5 == 0 { -0.01 } else { 0.002 }, 730);
    let a = bootstrap(&c, 20, 300, 7).expect("enough history");
    let b = bootstrap(&c, 20, 300, 7).expect("enough history");
    assert_eq!(a, b, "seeded");
    assert!(a.cagr.p5 <= a.cagr.p50 && a.cagr.p50 <= a.cagr.p95);
    assert!(a.max_drawdown.p5 <= a.max_drawdown.p95);
    let steady = bootstrap(&curve(|_| 0.001, 730), 20, 100, 1).expect("enough history");
    assert!(steady.prob_loss.abs() < 1e-12 && steady.max_drawdown.p95.abs() < 1e-12);
    assert!(bootstrap(&curve(|_| 0.001, 10), 20, 100, 1).is_none());
}

#[test]
fn regimes_split_bars_by_the_benchmarks_trailing_quarter() {
    // Benchmark: up 1% a day for 200 days, then down 1% a day for 200 days.
    let bench = curve(|d| if d <= 200 { 0.01 } else { -0.01 }, 400);
    // Strategy: flat in the fall.
    let strat = curve(|d| if d <= 200 { 0.01 } else { 0.0 }, 400);
    let split = regimes(&strat, &bench);
    let bull = split.iter().find(|r| r.regime == "bull").expect("bull");
    let bear = split.iter().find(|r| r.regime == "bear").expect("bear");
    assert!(bull.strategy_return > 0.0 && bull.benchmark_return > 0.0);
    assert!(bear.benchmark_return < 0.0);
    assert!(bear.strategy_return.abs() < 1e-9, "flat in the bear market");
    let shares: f64 = split.iter().map(|r| r.share).sum();
    assert!((shares - 1.0).abs() < 1e-9);
}
