use super::*;

fn at(hours: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_790_035_200 + hours * 3_600, 0).expect("valid timestamp") // midnight
}

fn btc() -> InstrumentId {
    InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
}

/// Hourly bars: day k rises if k is even, falls if odd, by a varying hourly rate.
fn market(days: i64) -> Vec<Bar> {
    let mut price = 100.0_f64;
    (0..days * 24)
        .map(|h| {
            let day = h / 24;
            let rate = 0.002 * ((day % 3) as f64).mul_add(0.5, 1.0);
            let open = price;
            price *= if day % 2 == 0 { 1.0 + rate } else { 1.0 - rate };
            Bar {
                ts: at(h),
                open,
                high: open.max(price),
                low: open.min(price),
                close: price,
                volume: 1.0,
            }
        })
        .collect()
}

/// A source that calls each day's direction at its start.
fn oracle(days: i64) -> Vec<ExternalCall> {
    (0..days)
        .map(|day| ExternalCall {
            source: "test:oracle".into(),
            instrument: btc(),
            ts: at(day * 24),
            direction: if day % 2 == 0 { 1.0 } else { -1.0 },
            strength: 1.0,
        })
        .collect()
}

fn config() -> EnsembleConfig {
    EnsembleConfig {
        min_outcomes: 5,
        vol_bars: 48,
        technical: false,
        ..EnsembleConfig::default()
    }
}

/// Feed `bars`; return the target in force after each bar.
fn run(ensemble: &mut Ensemble, bars: &[Bar]) -> Vec<f64> {
    let mut target = 0.0;
    bars.iter()
        .map(|bar| {
            if let Some(t) = ensemble.on_bar(&btc(), bar) {
                target = t;
            }
            target
        })
        .collect()
}

#[test]
fn without_a_record_nothing_weighs_in_and_the_ensemble_stays_flat() {
    let mut ensemble = Ensemble::new(config(), Vec::new());
    let targets = run(&mut ensemble, &market(20));
    assert!(targets.iter().all(|t| *t == 0.0));
    let why = ensemble.explain(&btc()).expect("explained");
    assert_eq!(why["note"], "no source has earned weight yet: flat");
}

#[test]
fn a_source_earns_weight_walk_forward_and_its_calls_move_the_target_with_reasons() {
    let bars = market(30);
    let mut ensemble = Ensemble::new(config(), oracle(30));
    let targets = run(&mut ensemble, &bars);
    // Days 0–5: fewer than 5 outcomes closed, so no weight, so flat.
    assert!(
        targets.iter().take(5 * 24).all(|t| *t == 0.0),
        "no look-ahead"
    );
    // Day 20 is a rising day: the oracle's bullish call is active, the ensemble is long.
    let day20 = targets.get(20 * 24 + 2).copied().unwrap_or_default();
    assert!(day20 > 0.0, "{day20}");
    // Day 21 falls: bearish call, long-only → flat.
    let day21 = targets.get(21 * 24 + 2).copied().unwrap_or_default();
    assert!(day21.abs() < 1e-12, "{day21}");
    let weights = ensemble.weights();
    let oracle_weight = weights
        .iter()
        .find(|w| w.source == "test:oracle")
        .expect("evaluated");
    assert!(
        oracle_weight.weight >= 2.0 && oracle_weight.outcomes >= 5,
        "{oracle_weight:?}"
    );
}

#[test]
fn explanations_name_the_contributors_and_live_calls_are_taken_in_once() {
    let bars = market(22);
    let mut ensemble = Ensemble::new(config(), oracle(20));
    let (before, last_day) = bars.split_at(21 * 24);
    run(&mut ensemble, before);
    // Day 21 was not in the initial calls: it arrives live (twice, counted once).
    let live = ExternalCall {
        source: "test:oracle".into(),
        instrument: btc(),
        ts: at(21 * 24),
        direction: -1.0,
        strength: 1.0,
    };
    ensemble.inform(&[live.clone(), live]);
    run(&mut ensemble, last_day.get(..3).unwrap_or_default());
    let why = ensemble.explain(&btc()).expect("explained");
    let contributors = why["contributors"].as_array().expect("list");
    assert_eq!(contributors.len(), 1);
    assert_eq!(contributors[0]["source"], "test:oracle");
    assert_eq!(contributors[0]["direction"], -1.0);
    assert!(why["conviction"].as_f64().is_some_and(|c| c < 0.0));
    assert_eq!(ensemble.calls.len(), 21);
}

#[test]
fn outcomes_count_only_once_their_window_has_closed() {
    let bars = market(3);
    let step = Duration::hours(1);
    let day = Duration::hours(24);
    assert_eq!(forward(&bars, at(0), day, step, at(23)), None, "still open");
    let closed = forward(&bars, at(0), day, step, at(24)).expect("closed");
    let expected = bars[23].close / bars[0].open - 1.0;
    assert!((closed - expected).abs() < 1e-12);
}

#[test]
fn the_bar_rises_with_the_number_of_sources_compared() {
    assert!((normal_quantile(0.975) - 1.959_964).abs() < 1e-5);
    assert!((normal_quantile_cdf(1.959_964) - 0.975).abs() < 1e-6);
    assert!((bonferroni(2.0, 1) - 2.0).abs() < 1e-12);
    let twenty = bonferroni(2.0, 20);
    assert!((twenty - 3.0518).abs() < 1e-3, "{twenty}");
}
