//! Case files and the shadow study on synthetic bars and decisions.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use quorum_core::regime::Regime;
use quorum_core::{Bar, Event, InstrumentId, Series};
use quorum_evaluator::case::Case;
use quorum_evaluator::study::auc;
use quorum_evaluator::{
    CaseConfig, EXIT, MetaConfig, MetaVerdict, SkipVerdict, StudyConfig, Verdict, cases, shadow,
    study,
};

fn day(n: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0)
        .single()
        .expect("valid")
        + chrono::Duration::days(n)
}

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("binance", symbol).expect("valid")
}

/// Opens at `100 + n` and closes at `101 + n`, with a small wobble so returns vary.
fn rising(symbol: &str, days: i64, wobble: f64) -> Series {
    let bars = (0..days)
        .map(|n| {
            let open = 100.0 + n as f64;
            let close = wobble.mul_add((n * 7 % 5) as f64 - 2.0, open + 1.0);
            Bar {
                ts: day(n),
                open,
                high: open.max(close) + 1.0,
                low: open.min(close) - 1.0,
                close,
                volume: 1_000.0 + (n % 7) as f64,
            }
        })
        .collect();
    Series::new(id(symbol), bars)
}

fn target(n: i64, symbol: &str, target: f64) -> Event {
    Event::Target {
        ts: day(n),
        instrument: id(symbol),
        target,
    }
}

fn cfg() -> CaseConfig {
    CaseConfig::new(0.002, 365.0)
}

#[test]
fn every_entry_is_a_case_labelled_after_costs() {
    let series = vec![rising("AAA", 200, 0.0), rising("BBB", 200, 0.3)];
    let events = vec![
        target(150, "AAA", 0.5),
        target(155, "AAA", 0.4), // a resize, not an entry
        target(160, "AAA", 0.0), // the exit
    ];
    let fills = BTreeSet::from([(id("AAA"), day(151))]);
    let out = cases(&series, &events, &fills, None, &cfg());
    assert_eq!(out.len(), 1);
    let case = &out[0];
    assert!(case.taken);
    assert!(case.direction > 0.0);
    // Entry at day 151's open (251); 5 bars: day 155's close (256).
    let h5 = 256.0 / 251.0 - 1.0 - 0.002;
    assert!((case.outcomes["h5"] - h5).abs() < 1e-12);
    // The exit decided on day 160 fills at day 161's open (261).
    let exit = 261.0 / 251.0 - 1.0 - 0.002;
    assert!((case.outcomes[EXIT] - exit).abs() < 1e-12);
    assert_eq!(case.held_bars, Some(10));
    assert!(case.features.contains_key("f:trend_60@1"));
    assert!(case.features.contains_key("rank:trend_60@1"));
}

#[test]
fn an_entry_never_filled_is_still_followed() {
    let series = vec![rising("AAA", 200, 0.0)];
    let events = vec![target(150, "AAA", 0.5), target(160, "AAA", 0.0)];
    let out = cases(&series, &events, &BTreeSet::new(), None, &cfg());
    assert_eq!(out.len(), 1);
    assert!(!out[0].taken, "refused or halted: a counterfactual");
    assert!(out[0].outcomes.contains_key(EXIT));
}

#[test]
fn features_use_only_bars_up_to_the_decision() {
    let series = vec![rising("AAA", 200, 0.2), rising("BBB", 200, 0.4)];
    let events = vec![target(140, "BBB", 0.5), target(150, "AAA", 0.5)];
    let before = cases(&series, &events, &BTreeSet::new(), None, &cfg());
    let mut changed = series;
    for s in &mut changed {
        for bar in s.bars.iter_mut().skip(151) {
            bar.close *= 3.0;
            bar.volume *= 10.0;
        }
    }
    let after = cases(&changed, &events, &BTreeSet::new(), None, &cfg());
    assert_eq!(before[1].features, after[1].features);
    assert_eq!(before[1].regime, after[1].regime);
    // Positions already held and their correlation are part of the case.
    assert!((before[1].features["fit:held"] - 1.0).abs() < 1e-12);
    assert!(before[1].features.contains_key("fit:corr"));
    assert_ne!(
        before[1].outcomes, after[1].outcomes,
        "labels do look ahead"
    );
}

#[test]
fn a_flip_closes_one_case_and_opens_another() {
    let series = vec![rising("AAA", 200, 0.0)];
    let events = vec![target(150, "AAA", 0.5), target(160, "AAA", -0.5)];
    let out = cases(&series, &events, &BTreeSet::new(), None, &cfg());
    assert_eq!(out.len(), 2);
    assert!(out[0].outcomes.contains_key(EXIT));
    assert!(out[1].direction < 0.0);
    assert!(
        out[1].outcomes["h1"] < 0.0,
        "a short in a rising market loses"
    );
}

#[test]
fn a_session_buys_what_its_warm_up_left_on_its_first_bar() {
    let series = vec![rising("AAA", 200, 0.0), rising("BBB", 200, 0.0)];
    // Warm-up: AAA entered and left, BBB entered; the session trades from day 150.
    let events = vec![
        target(100, "AAA", 0.5),
        target(120, "AAA", 0.0),
        target(130, "BBB", 0.5),
        target(160, "AAA", 0.3),
    ];
    let fills = BTreeSet::from([(id("BBB"), day(150))]);
    let out = cases(&series, &events, &fills, Some(day(150)), &cfg());
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].instrument, id("BBB"));
    assert!(out[0].features["fit:held"].abs() < 1e-12);
    assert_eq!(
        out[0].ts,
        day(149),
        "decided on the bar before the first traded bar"
    );
    assert!(out[0].taken);
    assert_eq!(
        (out[1].instrument.clone(), out[1].ts),
        (id("AAA"), day(160))
    );
}

#[test]
fn auc_reads_separation_and_its_significance() {
    let perfect: Vec<(f64, bool)> = (0..40).map(|i| (f64::from(i), i >= 20)).collect();
    let (a, z) = auc(&perfect);
    assert!((a - 1.0).abs() < 1e-12);
    assert!(z > 5.0);
    let flipped: Vec<(f64, bool)> = perfect.iter().map(|(v, w)| (-v, *w)).collect();
    assert!(auc(&flipped).0.abs() < 1e-12);
    assert_eq!(auc(&[(1.0, true), (2.0, true)]), (0.5, 0.0));
}

fn synthetic(n: usize) -> Vec<Case> {
    (0..n)
        .map(|i| {
            let signal = (i % 2) as f64; // wins exactly when the signal is 1
            let noise = ((i * 7919) % 101) as f64;
            let net = if signal > 0.5 { 0.03 } else { -0.02 };
            Case {
                ts: day(i64::try_from(i).expect("small")),
                instrument: id("AAA"),
                direction: 1.0,
                target: 0.1,
                taken: i % 3 != 0,
                regime: None,
                features: BTreeMap::from([
                    ("signal".to_owned(), signal),
                    ("noise".to_owned(), noise),
                ]),
                outcomes: BTreeMap::from([(EXIT.to_owned(), net), ("h1".to_owned(), net)]),
                held_bars: Some(3),
                exit_ts: Some(day(i64::try_from(i).expect("small") + 3)),
            }
        })
        .collect()
}

#[test]
fn the_study_finds_a_separating_feature_and_not_noise() {
    let report = study(&synthetic(300), &StudyConfig::default());
    assert_eq!(report.labelled, 300);
    assert_eq!(report.features_tested, 2);
    assert_eq!(report.baseline.n_in + report.baseline.n_out, 300);
    let row = |name: &str| {
        report
            .features
            .iter()
            .find(|r| r.feature == name)
            .expect("row")
    };
    assert_eq!(row("signal").verdict, Verdict::Separates);
    assert_eq!(row("noise").verdict, Verdict::NoEdge);
    let exit = report
        .horizons
        .iter()
        .find(|h| h.label == EXIT)
        .expect("exit row");
    assert!((exit.win_rate - 0.5).abs() < 1e-12);
    assert_eq!(exit.taken, 200);
}

#[test]
fn too_few_cases_are_not_judged() {
    let report = study(&synthetic(20), &StudyConfig::default());
    assert!(report.features.iter().all(|r| r.verdict == Verdict::TooFew));
}

#[test]
fn a_regime_skip_is_chosen_in_sample_and_judged_out_of_sample() {
    // Crisis entries lose throughout (with some spread); trending ones win; choppy ones lose only
    // in sample, so their rule fails out of sample.
    let mut all = synthetic(400);
    for (i, c) in all.iter_mut().enumerate() {
        let wobble = ((i * 37) % 11) as f64 / 1_000.0;
        let (regime, net) = match i % 3 {
            0 => (Regime::Crisis, -0.05 + wobble),
            1 => (Regime::Trending, 0.04 + wobble),
            _ => (
                Regime::Choppy,
                if i < 280 {
                    -0.03 + wobble
                } else {
                    0.03 + wobble
                },
            ),
        };
        c.regime = Some(regime);
        c.outcomes.insert(EXIT.to_owned(), net);
    }
    let report = study(&all, &StudyConfig::default());
    let skip = |r: Regime| {
        report
            .regime_skips
            .iter()
            .find(|s| s.regime == Some(r))
            .expect("row")
    };
    assert_eq!(skip(Regime::Crisis).verdict, SkipVerdict::Holds);
    assert!(skip(Regime::Crisis).skipping_out > skip(Regime::Crisis).always_out);
    assert_eq!(skip(Regime::Trending).verdict, SkipVerdict::NotChosen);
    assert_eq!(skip(Regime::Choppy).verdict, SkipVerdict::NotSignificant);
}

#[test]
fn the_meta_model_beats_always_take_only_when_a_feature_predicts() {
    // `signal` decides every outcome: a model trained on earlier cases ranks later winners.
    let report = shadow(&synthetic(1_000), &MetaConfig::default());
    assert_eq!(report.verdict, MetaVerdict::BeatsAlwaysTake, "{report:?}");
    assert!(report.auc_out > 0.95);
    assert!(report.taken_mean > report.always_mean);
    assert_eq!(report.weights[0].0, "signal");

    // Without it only noise is left: nothing to learn.
    let noisy: Vec<_> = synthetic(1_000)
        .into_iter()
        .map(|mut c| {
            c.features.remove("signal");
            c
        })
        .collect();
    assert_eq!(
        shadow(&noisy, &MetaConfig::default()).verdict,
        MetaVerdict::NoEdge
    );
    assert_eq!(
        shadow(&synthetic(100), &MetaConfig::default()).verdict,
        MetaVerdict::TooFew
    );
}
