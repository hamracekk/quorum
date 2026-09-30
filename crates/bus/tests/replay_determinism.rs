//! M2 exit criterion: a recorded day replays to identical decisions, every time, and
//! replaying a recording gives the same decisions as backtesting the same bars directly.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use trader_bus::{Bus, JsonlEventLog, replay};
use trader_core::strategies::{PerInstrument, SmaCross};
use trader_core::{
    BacktestConfig, Bar, Event, EventLog, InstrumentId, Series, Strategy, run_with_events,
};

const fn at(hours: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trader-replay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir); // stale leftovers from an earlier run, if any
    dir
}

/// Two instruments, 48 hourly bars (two UTC days), deterministic zig-zag prices.
fn market() -> Vec<Series> {
    [("BTCUSDT", 100.0, 5), ("ETHUSDT", 50.0, 7)]
        .into_iter()
        .map(|(symbol, start, period)| {
            let mut price = start;
            let bars = (0..48)
                .map(|hour| {
                    let open = price;
                    price *= if hour % period == period - 1 {
                        0.95
                    } else {
                        1.01
                    };
                    Bar {
                        ts: at(hour),
                        open,
                        high: open.max(price),
                        low: open.min(price),
                        close: price,
                        volume: 1.0,
                    }
                })
                .collect();
            Series::new(
                InstrumentId::spot("binance", symbol).expect("valid instrument"),
                bars,
            )
        })
        .collect()
}

fn strategy() -> Box<dyn Strategy> {
    Box::new(PerInstrument::new(SmaCross::new(3, 8, false), 2))
}

/// Publish every bar through the bus, in time order then instrument order — as a live feed would.
fn record(series: &[Series], dir: &PathBuf) -> Arc<JsonlEventLog> {
    let log = Arc::new(JsonlEventLog::open(dir).expect("open log"));
    let bus = Bus::new(log.clone(), Bus::DEFAULT_CAPACITY);
    for hour in 0..48 {
        for s in series {
            let bar = s.bars[usize::try_from(hour).expect("small index")];
            bus.publish(&Event::Bar {
                instrument: s.instrument.clone(),
                bar,
            })
            .expect("publish");
        }
    }
    log
}

#[test]
fn a_recorded_day_replays_to_identical_decisions() {
    let dir = temp_dir("day");
    let log = record(&market(), &dir);
    let cfg = BacktestConfig::default();

    let (first, first_events) =
        replay(log.as_ref(), at(0), at(24), strategy().as_mut(), &cfg).expect("replay 1");
    let (second, second_events) =
        replay(log.as_ref(), at(0), at(24), strategy().as_mut(), &cfg).expect("replay 2");

    assert!(!first_events.is_empty(), "the day should produce decisions");
    assert_eq!(first_events, second_events);
    assert_eq!(first.equity_curve, second.equity_curve);
    assert_eq!(first.fills, second.fills);
}

#[test]
fn replaying_a_recording_matches_backtesting_the_same_bars() {
    let dir = temp_dir("match");
    let series = market();
    let log = record(&series, &dir);
    let cfg = BacktestConfig::default();

    let (replayed, replay_events) =
        replay(log.as_ref(), at(0), at(48), strategy().as_mut(), &cfg).expect("replay");
    let (backtested, backtest_events) = run_with_events(strategy().as_mut(), &series, &cfg, None);

    assert_eq!(replay_events, backtest_events);
    assert_eq!(replayed.equity_curve, backtested.equity_curve);
    assert!((replayed.report.end_equity - backtested.report.end_equity).abs() < 1e-9);
}

#[test]
fn the_recording_spans_one_file_per_day_with_contiguous_sequence_numbers() {
    let dir = temp_dir("files");
    let log = record(&market(), &dir);

    let all = log.read(at(0), at(48)).expect("read");
    let seqs: Vec<u64> = all.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=96).collect::<Vec<u64>>());
    assert!(dir.join("1970-01-01.jsonl").exists());
    assert!(dir.join("1970-01-02.jsonl").exists());

    let reopened = JsonlEventLog::open(&dir).expect("reopen");
    let next = reopened
        .append(&Event::RiskHalt {
            ts: at(47),
            equity: 1.0,
        })
        .expect("append");
    assert_eq!(next.seq, 97);
}
