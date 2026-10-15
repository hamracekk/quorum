use chrono::{Duration, TimeZone};
use quorum_core::Bar;
use quorum_core::factors::Momentum;

use super::*;

/// `n` instruments over `days` days. With `persistent`, instrument `i` drifts at its own steady
/// rate, so past returns rank the future perfectly; otherwise each period's returns are
/// scrambled by a fixed-seed generator.
fn universe(n: usize, days: usize, persistent: bool) -> Vec<Series> {
    let t0 = Utc
        .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
        .single()
        .expect("valid");
    let mut seed: u64 = 42;
    let mut noise = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((seed >> 33) as f64 / f64::from(u32::MAX)) - 0.5
    };
    (0..n)
        .map(|i| {
            let mut close = 100.0;
            let bars = (0..days)
                .map(|d| {
                    let drift = if persistent {
                        0.0001_f64.mul_add(noise(), 0.0005 * i as f64)
                    } else {
                        0.04 * noise()
                    };
                    let open = close;
                    close *= 1.0 + drift;
                    Bar {
                        ts: t0 + Duration::days(i64::try_from(d).expect("small")),
                        open,
                        high: open.max(close),
                        low: open.min(close),
                        close,
                        volume: 1_000.0,
                    }
                })
                .collect();
            Series::new(
                InstrumentId::spot("binance", &format!("C{i}USDT")).expect("valid"),
                bars,
            )
        })
        .collect()
}

fn cfg() -> FactorStudyConfig {
    FactorStudyConfig {
        cost: 0.0005,
        ..FactorStudyConfig::default()
    }
}

#[test]
fn a_factor_that_ranks_the_future_is_promising() {
    let series = universe(15, 500, true);
    let factors: Vec<Box<dyn PriceFactor>> = vec![Box::new(Momentum { lookback: 20 })];
    let report = study(&factors, &series, &cfg());
    let row = &report.rows[0];
    assert_eq!(row.direction, 1);
    assert!(row.in_sample.ic > 0.9, "{}", row.in_sample.ic);
    assert!(row.out_of_sample.ic_t > T_OUT_OF_SAMPLE);
    assert!(row.out_of_sample.long_excess_net > 0.0);
    assert_eq!(row.verdict, Verdict::Promising);
}

#[test]
fn noise_is_not_promising() {
    let series = universe(15, 700, false);
    let factors: Vec<Box<dyn PriceFactor>> = vec![
        Box::new(Momentum { lookback: 5 }),
        Box::new(Momentum { lookback: 20 }),
        Box::new(Momentum { lookback: 60 }),
    ];
    let report = study(&factors, &series, &cfg());
    assert_eq!(report.factors_tested, 3);
    for row in &report.rows {
        assert_ne!(row.verdict, Verdict::Promising, "{} on noise", row.factor);
    }
}

#[test]
fn rebalance_dates_never_overlap_and_costs_count() {
    let series = universe(15, 500, true);
    let factors: Vec<Box<dyn PriceFactor>> = vec![Box::new(Momentum { lookback: 20 })];
    let cheap = study(&factors, &series, &cfg());
    let dear = study(
        &factors,
        &series,
        &FactorStudyConfig {
            cost: 0.05,
            ..cfg()
        },
    );
    let dates = cheap.rows[0].in_sample.dates + cheap.rows[0].out_of_sample.dates;
    assert!(dates <= 500 / 7, "one date per horizon: {dates}");
    assert!(dear.rows[0].in_sample.long_excess_net < cheap.rows[0].in_sample.long_excess_net);
}

#[test]
fn a_perfectly_consistent_ic_is_strong_evidence_not_none() {
    assert!(mean_t(&[0.5, 0.5, 0.5]).1 >= f64::MAX);
    assert!(mean_t(&[0.0, 0.0]).1.abs() < f64::EPSILON);
    assert!(mean_t(&[-0.2, -0.2]).1 < 0.0);
}

#[test]
fn too_few_dates_are_called_too_few() {
    let series = universe(15, 120, true);
    let factors: Vec<Box<dyn PriceFactor>> = vec![Box::new(Momentum { lookback: 20 })];
    assert_eq!(
        study(&factors, &series, &cfg()).rows[0].verdict,
        Verdict::TooFew
    );
}

#[test]
fn thin_cross_sections_are_skipped() {
    let series = universe(5, 400, true);
    let factors: Vec<Box<dyn PriceFactor>> = vec![Box::new(Momentum { lookback: 20 })];
    let row = &study(&factors, &series, &cfg()).rows[0];
    assert_eq!(
        row.in_sample.dates + row.out_of_sample.dates,
        0,
        "5 names < min_names 10"
    );
}
