use chrono::{Datelike, Duration, TimeZone, Utc, Weekday};
use quorum_core::{Bar, EquityPoint, InstrumentId, Series};

use super::*;

fn id(symbol: &str) -> InstrumentId {
    format!("tiingo:{symbol}:equity").parse().expect("valid")
}

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .expect("valid")
}

/// Weekday closes `price(k)` for `days` calendar days.
fn series(symbol: &str, days: i64, price: impl Fn(i64) -> f64) -> Series {
    let bars = (0..days)
        .map(|k| (k, start() + Duration::days(k)))
        .filter(|(_, d)| !matches!(d.weekday(), Weekday::Sat | Weekday::Sun))
        .map(|(k, ts)| {
            let p = price(k);
            Bar {
                ts,
                open: p,
                high: p,
                low: p,
                close: p,
                volume: 1.0,
            }
        })
        .collect();
    Series::new(id(symbol), bars)
}

fn portfolio(holdings: &[(&str, f64)], rebalance_days: usize) -> Portfolio {
    Portfolio {
        name: "test".to_owned(),
        holdings: holdings
            .iter()
            .map(|(s, w)| Holding {
                instrument: id(s),
                weight: *w,
            })
            .collect(),
        rebalance_days,
    }
}

#[test]
fn the_shipped_config_parses() {
    let cfg = BenchmarkConfig::parse(include_str!("../../../../config/benchmark.toml"))
        .expect("valid config");
    assert!(cfg.portfolio(&cfg.summary).is_some());
}

#[test]
fn weights_must_sum_to_one_and_the_summary_must_exist() {
    let bad = r#"
slot = "multi-asset"
summary = "x"
[[portfolios]]
name = "x"
holdings = [{ instrument = "tiingo:SPY:equity", weight = 0.7 }]
"#;
    assert!(BenchmarkConfig::parse(bad).is_err());
    let missing = bad
        .replace("weight = 0.7", "weight = 1.0")
        .replace("summary = \"x\"", "summary = \"y\"");
    assert!(BenchmarkConfig::parse(&missing).is_err());
}

#[test]
fn one_holding_follows_its_price_less_the_entry_cost() {
    let s = [series("SPY", 30, |k| 100.0 + k as f64)];
    let c = curve(&portfolio(&[("SPY", 1.0)], 21), &s, 1_000.0, 0.001).expect("curve");
    let last = s[0].bars.last().expect("bars").close;
    let expected = 1_000.0 * (1.0 - 0.001) * last / 100.0;
    assert!((c.last().expect("points").equity - expected).abs() < 1e-9);
}

#[test]
fn rebalancing_restores_the_weights() {
    // SPY doubles, TLT flat: after a rebalance the 60/40 split holds again.
    let s = [
        series("SPY", 60, |k| if k < 10 { 100.0 } else { 200.0 }),
        series("TLT", 60, |_| 100.0),
    ];
    let p = portfolio(&[("SPY", 0.6), ("TLT", 0.4)], 21);
    let c = curve(&p, &s, 1_000.0, 0.0).expect("curve");
    // Before the first rebalance: 600 → 1 200 in SPY, 400 in TLT.
    assert!((c[19].equity - 1_600.0).abs() < 1e-9);
    // Without costs a rebalance changes nothing in value.
    assert!((c.last().expect("points").equity - 1_600.0).abs() < 1e-9);
    let costly = curve(&p, &s, 1_000.0, 0.01).expect("curve");
    assert!(costly.last().expect("points").equity < 1_600.0 * 0.99);
}

#[test]
fn a_missing_holding_is_an_error() {
    let s = [series("SPY", 10, |_| 100.0)];
    assert!(curve(&portfolio(&[("TLT", 1.0)], 21), &s, 1.0, 0.0).is_err());
}

fn points(days: i64, growth: impl Fn(i64) -> f64) -> Vec<EquityPoint> {
    (0..days)
        .map(|k| EquityPoint {
            ts: start() + Duration::days(k),
            equity: growth(k),
        })
        .collect()
}

#[test]
fn a_steadily_better_curve_beats_a_noisy_benchmark_and_the_reverse_trails() {
    let wobble = |k: i64| 0.01_f64.mul_add((k * 7 % 5) as f64 - 2.0, 1.0);
    let bench: Vec<EquityPoint> = points(800, |k| {
        100.0 * 1.0002_f64.powi(i32::try_from(k).expect("small")) * wobble(k)
    })
    .into_iter()
    .filter(|p| !matches!(p.ts.weekday(), Weekday::Sat | Weekday::Sun))
    .collect();
    // The subject trades every calendar day; it is sampled on the benchmark's dates.
    let better = points(800, |k| {
        100.0 * 1.001_f64.powi(i32::try_from(k).expect("small")) * wobble(k)
    });
    let c = compare(("book", &better), ("60/40", &bench), 0.3, 252, 2.0).expect("comparison");
    assert_eq!(c.verdict, Verdict::Beats);
    assert!(c.excess_cagr > 0.0 && c.later_excess_cagr > 0.0);
    assert_eq!(c.year_wins, Some(1.0));
    let worse = points(800, |k| {
        100.0 * 0.9995_f64.powi(i32::try_from(k).expect("small")) * wobble(k)
    });
    let c = compare(("book", &worse), ("60/40", &bench), 0.3, 252, 2.0).expect("comparison");
    assert_eq!(c.verdict, Verdict::Trails);
}

#[test]
fn the_same_curve_is_not_significant() {
    let bench = points(300, |k| 100.0 + (k % 9) as f64);
    let c = compare(("book", &bench), ("b", &bench), 0.3, 252, 2.0).expect("comparison");
    assert_eq!(c.verdict, Verdict::NotSignificant);
    assert!(c.t.abs() < 1e-12);
    assert!(compare(("book", &bench[..1]), ("b", &bench), 0.3, 252, 2.0).is_none());
}

#[test]
fn tax_takes_a_share_of_each_winning_year_only() {
    let at = |y: i32| {
        Utc.with_ymd_and_hms(y, 12, 31, 0, 0, 0)
            .single()
            .expect("valid")
    };
    let pre = vec![
        EquityPoint {
            ts: at(2020) - Duration::days(364),
            equity: 100.0,
        },
        EquityPoint {
            ts: at(2020),
            equity: 120.0,
        },
        EquityPoint {
            ts: at(2021),
            equity: 90.0,
        },
        EquityPoint {
            ts: at(2022),
            equity: 99.0,
        },
    ];
    let post = after_tax(&pre, 0.15);
    // 2020: +20 → 3 tax → 117. 2021: −25% → 87.75, no tax. 2022: +10% → 96.525, tax 15% of
    // the gain → 95.20875.
    assert!((post[1].equity - 117.0).abs() < 1e-9);
    assert!((post[2].equity - 87.75).abs() < 1e-9);
    assert!((post[3].equity - 95.208_75).abs() < 1e-9);
    assert_eq!(after_tax(&pre, 0.0), pre);
}
