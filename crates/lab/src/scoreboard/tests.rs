use super::*;

fn at(hours: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_790_000_000 - 1_790_000_000 % 3_600 + hours * 3_600, 0)
        .expect("valid timestamp")
}

fn btc() -> InstrumentId {
    InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
}

/// Hourly bars from hour 0 where bar `i` opens at `prices[i]` and closes at `prices[i + 1]`.
fn series(prices: &[f64]) -> Series {
    let bars = prices
        .windows(2)
        .zip(0_i64..)
        .map(|(pair, hour)| Bar {
            ts: at(hour),
            open: pair[0],
            high: pair[0].max(pair[1]),
            low: pair[0].min(pair[1]),
            close: pair[1],
            volume: 1.0,
        })
        .collect();
    Series {
        instrument: btc(),
        bars,
    }
}

fn call(ts: DateTime<Utc>, direction: f64, strength: f64) -> Occurrence {
    Occurrence {
        instrument: btc(),
        ts,
        direction,
        strength,
    }
}

fn config(now: DateTime<Utc>) -> ScoreConfig {
    ScoreConfig {
        horizons: vec![
            HorizonSpec {
                label: "1h".into(),
                length: Duration::hours(1),
            },
            HorizonSpec {
                label: "2h".into(),
                length: Duration::hours(2),
            },
        ],
        primary: "1h".into(),
        cost: 0.001,
        recent: Duration::hours(3),
        min_outcomes: 2,
        min_weeks: 0.0,
        now,
    }
}

fn news(occurrences: Vec<Occurrence>) -> Source {
    Source {
        id: "news".into(),
        kind: SourceKind::News,
        since: None,
        occurrences,
    }
}

#[test]
fn calls_enter_at_the_next_open_and_exit_at_the_horizon_close() {
    // Bars: h0 100→110, h1 110→121, h2 121→100, h3 100→100.
    let data = series(&[100.0, 110.0, 121.0, 100.0, 100.0]);
    // Known at 00:30 → enters at the h1 open (110); 1h exits at the h1 close (121).
    let board = score(
        &[news(vec![call(at(0) + Duration::minutes(30), 1.0, 1.0)])],
        &[data],
        &config(at(4)),
    );
    let row = &board.rows[0];
    let one = &row.horizons[0];
    assert_eq!((one.count, one.pending), (1, 0));
    assert!((one.mean_return - 0.1).abs() < 1e-12, "{one:?}");
    assert!((one.net - 0.099).abs() < 1e-12);
    // 2h exits at the h2 close (100): 100/110 − 1.
    let two = &row.horizons[1];
    assert!((two.mean_return - (100.0 / 110.0 - 1.0)).abs() < 1e-12);
}

#[test]
fn bearish_calls_profit_from_falls_and_open_windows_are_pending() {
    let data = series(&[100.0, 110.0, 121.0, 100.0, 100.0]);
    let board = score(
        &[news(vec![
            call(at(2), -1.0, 0.5),
            // The h3 window needs a later bar to be closed: pending.
            call(at(3), 1.0, 0.5),
        ])],
        &[data],
        &config(at(4)),
    );
    let one = &board.rows[0].horizons[0];
    assert_eq!((one.count, one.pending), (1, 1));
    assert!((one.mean_return - (1.0 - 100.0 / 121.0)).abs() < 1e-12);
    assert!((one.hit_rate - 1.0).abs() < f64::EPSILON);
}

#[test]
fn verdicts_need_outcomes_weeks_and_a_beaten_baseline() {
    let prices: Vec<f64> = (0..60).map(|i| 100.0 + f64::from(i % 2) * 2.0).collect();
    let data = series(&prices);
    // Bars alternate up (even hours) and down (odd hours): a source that calls each one right
    // wins every time; always long and random do not.
    let right: Vec<Occurrence> = (0..50)
        .map(|h| call(at(h), if h % 2 == 0 { 1.0 } else { -1.0 }, 1.0))
        .collect();
    let mut sources = vec![news(right)];
    sources.extend(baselines(std::slice::from_ref(&data), at(0)));
    let mut cfg = config(at(58));
    cfg.min_outcomes = 30;
    let board = score(&sources, std::slice::from_ref(&data), &cfg);
    let news_row = board.rows.iter().find(|r| r.id == "news").expect("row");
    assert_eq!(news_row.verdict, Verdict::Edge, "{news_row:?}");
    assert!(
        board
            .rows
            .iter()
            .filter(|r| r.kind == SourceKind::Baseline)
            .all(|r| r.verdict == Verdict::Baseline)
    );
    assert_eq!(
        board.rows.last().map(|r| r.kind),
        Some(SourceKind::Baseline)
    );

    cfg.min_weeks = 4.0;
    let board = score(&sources, std::slice::from_ref(&data), &cfg);
    assert_eq!(board.rows[0].verdict, Verdict::Collecting);

    cfg.min_outcomes = 1_000;
    let board = score(&sources, &[data], &cfg);
    assert_eq!(board.rows[0].verdict, Verdict::TooFew);
}

#[test]
fn the_information_coefficient_rewards_confidence_that_ranks_moves() {
    let data = series(&[100.0, 101.0, 104.0, 102.0, 110.0, 110.0]);
    // Moves: +1%, +2.97%, −1.92%, +7.84%. Confidence ranks them correctly.
    let board = score(
        &[news(vec![
            call(at(0), 1.0, 0.2),
            call(at(1), 1.0, 0.6),
            call(at(2), -1.0, 0.7),
            call(at(3), 1.0, 0.9),
        ])],
        &[data],
        &config(at(5)),
    );
    let ic = board.rows[0].horizons[0].ic.expect("defined");
    assert!((ic - 1.0).abs() < 1e-12, "{ic}");
}

#[test]
fn spearman_handles_ties_and_constant_inputs() {
    assert_eq!(spearman(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]), None);
    assert_eq!(spearman(&[1.0, 2.0], &[1.0, 2.0]), None);
    let rho = spearman(&[1.0, 2.0, 3.0, 4.0], &[10.0, 20.0, 20.0, 40.0]).expect("defined");
    assert!(rho > 0.9 && rho < 1.0, "{rho}");
    let inverse = spearman(&[1.0, 2.0, 3.0], &[3.0, 2.0, 1.0]).expect("defined");
    assert!((inverse + 1.0).abs() < 1e-12);
}

#[test]
fn baselines_are_deterministic_and_the_random_one_is_mixed() {
    let data = series(
        &(0..200)
            .map(f64::from)
            .map(|p| 100.0 + p)
            .collect::<Vec<_>>(),
    );
    let first = baselines(std::slice::from_ref(&data), at(10));
    let second = baselines(std::slice::from_ref(&data), at(10));
    assert_eq!(first[1].occurrences, second[1].occurrences);
    assert_eq!(first[0].occurrences.len(), 189);
    assert!(first[0].occurrences.iter().all(|o| o.direction > 0.0));
    let longs = first[1]
        .occurrences
        .iter()
        .filter(|o| o.direction > 0.0)
        .count();
    assert!((60..130).contains(&longs), "{longs}");
}

#[test]
fn technical_signals_are_known_when_their_bar_closes() {
    let mut prices = vec![100.0; 40];
    prices.extend((0..40).map(|i| 100.0 - f64::from(i)));
    prices.extend((0..40).map(|i| f64::from(i).mul_add(2.0, 60.0)));
    let data = series(&prices);
    let sources = technical_sources(std::slice::from_ref(&data), at(0), SignalConfig::default());
    assert!(sources.iter().all(|s| s.kind == SourceKind::Technical));
    let signal_bars: Vec<(usize, Signal)> = occurrences(&data.bars, SignalConfig::default());
    let (i, signal) = signal_bars
        .iter()
        .find(|(i, s)| s.bias() != Bias::Neutral && *i + 1 < data.bars.len())
        .copied()
        .expect("some directional signal");
    let source = sources
        .iter()
        .find(|s| s.id == signal.to_string())
        .expect("source");
    assert!(
        source
            .occurrences
            .iter()
            .any(|o| o.ts == data.bars[i + 1].ts)
    );
}

#[test]
fn riding_the_trend_is_not_an_edge() {
    // Rising every bar, by 1.5% and 0.5% in turn: always long earns ~1% per bar.
    let mut prices = vec![100.0];
    for i in 0..80 {
        let last = prices.last().copied().unwrap_or(100.0);
        prices.push(last * if i % 2 == 0 { 1.015 } else { 1.005 });
    }
    let data = series(&prices);
    let every_bar: Vec<Occurrence> = (0..70).map(|h| call(at(h), 1.0, 1.0)).collect();
    let mut sources = vec![news(every_bar)];
    sources.extend(baselines(std::slice::from_ref(&data), at(0)));
    let mut cfg = config(at(79));
    cfg.min_outcomes = 30;
    let board = score(&sources, &[data], &cfg);
    let row = board.rows.iter().find(|r| r.id == "news").expect("row");
    let one = &row.horizons[0];
    assert!(
        one.net > 0.0 && one.t_stat > 2.0,
        "profitable and 'significant': {one:?}"
    );
    assert!((one.bias - 1.0).abs() < f64::EPSILON);
    assert!(one.excess.abs() < 1e-3, "{one:?}");
    assert_eq!(row.verdict, Verdict::NoEdge, "{one:?}");
}

#[test]
fn calls_before_a_source_was_chosen_are_in_sample_and_kept_out_of_the_record() {
    let data = series(&[100.0, 110.0, 121.0, 100.0, 100.0, 100.0]);
    let mut source = news(vec![call(at(0), 1.0, 1.0), call(at(2), -1.0, 1.0)]);
    source.since = Some(at(1));
    let board = score(&[source], &[data], &config(at(5)));
    let row = &board.rows[0];
    assert_eq!(row.occurrences, 1, "only the call after it was chosen");
    assert_eq!(row.since, Some(at(1)));
    let before = row.in_sample.as_ref().expect("in-sample");
    assert_eq!(before.count, 1);
    assert!((before.mean_return - 0.1).abs() < 1e-12, "{before:?}");
    assert!(
        row.horizons[0].mean_return > 0.0,
        "the bearish call after: 121 → 100"
    );
}
