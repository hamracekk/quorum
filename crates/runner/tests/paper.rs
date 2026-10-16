//! End-to-end paper runs on fakes: replay, journaling watermark, resume, gaps, stop, refusals.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};
use quorum_core::fakes::{
    MemoryBarStore, MemoryEventLog, MemoryMarketData, MemoryMarketStream, MemoryNotifier,
};
use quorum_core::{Bar, EventLog, InstrumentId, Journal, VenueId};
use quorum_envelope::Envelope;
use quorum_journal::SqliteJournal;
use quorum_risk::fakes::{FakeGate, FakeMode};
use quorum_runner::{Ports, RunOptions, RunnerError, SlotFiles, Status, request_stop, run};
use quorum_slots::SlotConfig;

const ENVELOPE: &str = r#"
currency = "EUR"
capital_total = 0
leverage_max = 1.0
venues_allowed = ["binance"]
loss = { daily_max = 0.5, total_max = 0.5 }
slot_default = { capital_max = 0, max_mode = "paper" }
"#;

const SLOT: &str = r#"
id = "crypto-spot"
venue = "binance"
interval = "1d"
start_mode = "paper"
paper_capital = 10000
history_days = 60
universe = { kind = "fixed", instruments = ["binance:BTCUSDT", "binance:ETHUSDT"] }
strategy = { kind = "trend-momentum", lookback = 3 }
costs = { fee_rate = 0.001, slippage_bps = 2.0 }
"#;

fn btc() -> InstrumentId {
    InstrumentId::spot("binance", "BTCUSDT").expect("valid")
}

fn eth() -> InstrumentId {
    InstrumentId::spot("binance", "ETHUSDT").expect("valid")
}

fn day(n: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("valid")
        + Duration::days(n)
}

/// Day `n`'s bar: a zig-zag uptrend so trend-momentum enters, exits and re-enters.
fn bar(n: i64, scale: f64) -> Bar {
    let wave = [0.0, 3.0, 5.0, 2.0, -4.0, -6.0, -1.0, 4.0][usize::try_from(n % 8).expect("≥ 0")];
    let close = scale * (100.0 + n as f64 + wave);
    Bar {
        ts: day(n),
        open: close * 0.99,
        high: close * 1.01,
        low: close * 0.98,
        close,
        volume: 1_000.0,
    }
}

fn bars(range: std::ops::Range<i64>, scale: f64) -> Vec<Bar> {
    range.map(|n| bar(n, scale)).collect()
}

/// History up to (not including) day `live_from`; `now` is the end of the day before it.
struct World {
    data: PathBuf,
    store: Arc<MemoryBarStore>,
    journal: Arc<SqliteJournal>,
    log: Arc<MemoryEventLog>,
    notes: Arc<MemoryNotifier>,
    risk: Arc<FakeGate>,
}

impl World {
    async fn new(name: &str) -> Self {
        let data =
            std::env::temp_dir().join(format!("quorum-runner-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data); // a leftover from an earlier run, if any
        Self {
            data,
            store: Arc::new(MemoryBarStore::default()),
            journal: Arc::new(SqliteJournal::in_memory().await.expect("journal")),
            log: Arc::new(MemoryEventLog::default()),
            notes: Arc::new(MemoryNotifier::default()),
            risk: Arc::new(FakeGate::new(FakeMode::Allow)),
        }
    }

    /// Market data serves days `0..market_until`; the stream yields `live`.
    fn ports(&self, market_until: i64, live: Vec<(InstrumentId, Bar)>) -> Ports {
        let venue = VenueId::new("binance").expect("valid");
        let market = MemoryMarketData::new(venue.clone())
            .with_bars(btc(), bars(0..market_until, 1.0))
            .with_bars(eth(), bars(0..market_until, 0.05));
        Ports {
            market: Arc::new(market),
            stream: Arc::new(MemoryMarketStream::new(venue, live)),
            bars: self.store.clone(),
            journal: self.journal.clone(),
            log: self.log.clone(),
            notifier: self.notes.clone(),
            risk: self.risk.clone(),
        }
    }

    fn options(&self, now: DateTime<Utc>) -> RunOptions {
        RunOptions {
            data: self.data.clone(),
            fresh: false,
            now,
            heartbeat: StdDuration::from_hours(1),
            catch_up_every: StdDuration::from_hours(1),
        }
    }
}

fn live(range: std::ops::Range<i64>) -> Vec<(InstrumentId, Bar)> {
    range
        .flat_map(|n| [(btc(), bar(n, 1.0)), (eth(), bar(n, 0.05))])
        .collect()
}

fn slot() -> SlotConfig {
    SlotConfig::parse(SLOT).expect("valid slot")
}

fn envelope() -> Envelope {
    Envelope::parse(ENVELOPE).expect("valid envelope")
}

async fn decisions(journal: &SqliteJournal, run: &str) -> usize {
    journal
        .decisions(run, 100_000)
        .await
        .expect("decisions")
        .len()
}

#[tokio::test]
async fn a_fresh_session_journals_only_from_its_start() {
    let w = World::new("fresh").await;
    // Now: day 30, just after midnight. Days 0..30 are history; 30..40 arrive live.
    let now = day(30) + Duration::minutes(5);
    let out = run(
        &w.ports(30, live(30..40)),
        &slot(),
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect("run");
    assert_eq!(out.status, Status::StreamEnded);
    assert!(!out.resumed);
    assert_eq!(out.bars, 20, "10 days × 2 instruments");
    assert_eq!(out.journaled_until, Some(day(39)));
    let fills = w.journal.fills(&out.session_id).await.expect("fills");
    assert!(!fills.is_empty(), "the trend strategy traded");
    assert!(
        fills.iter().all(|f| f.ts >= day(30)),
        "warm-up bars never trade"
    );
}

#[tokio::test]
async fn resuming_rebuilds_by_replay_without_duplicates() {
    // One uninterrupted run over days 30..40 …
    let whole = World::new("whole").await;
    let now = day(30) + Duration::minutes(5);
    let reference = run(
        &whole.ports(30, live(30..40)),
        &slot(),
        &envelope(),
        &whole.options(now),
        std::future::pending(),
    )
    .await
    .expect("reference run");

    // … equals a run stopped after day 34 and resumed (the stream repeats day 34).
    let w = World::new("resume").await;
    let first = run(
        &w.ports(30, live(30..35)),
        &slot(),
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect("first run");
    let after_first = decisions(&w.journal, &first.session_id).await;
    let second = run(
        &w.ports(30, live(34..40)),
        &slot(),
        &envelope(),
        &w.options(day(35)),
        std::future::pending(),
    )
    .await
    .expect("resumed run");

    assert!(second.resumed);
    assert_eq!(second.session_id, first.session_id);
    assert_eq!(
        second.bars, 10,
        "day 34 skipped as a duplicate; days 35..40 traded"
    );
    assert!(
        (second.equity - reference.equity).abs() < 1e-9,
        "{} vs {}",
        second.equity,
        reference.equity
    );
    assert_eq!(
        decisions(&w.journal, &first.session_id).await,
        decisions(&whole.journal, &reference.session_id).await,
        "every decision journaled exactly once"
    );
    assert!(decisions(&w.journal, &first.session_id).await > after_first);
    let fills =
        |j: Arc<SqliteJournal>, id: String| async move { j.fills(&id).await.expect("fills").len() };
    assert_eq!(
        fills(w.journal.clone(), first.session_id.clone()).await,
        fills(whole.journal.clone(), reference.session_id.clone()).await
    );
}

#[tokio::test]
async fn bars_stored_while_stopped_are_journaled_on_resume() {
    let w = World::new("offline").await;
    let now = day(30) + Duration::minutes(5);
    let first = run(
        &w.ports(30, live(30..33)),
        &slot(),
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect("first run");
    // Down for days 33..36: the resume syncs them from market data and replays them past the
    // watermark, so their decisions are journaled once.
    let second = run(
        &w.ports(36, Vec::new()),
        &slot(),
        &envelope(),
        &w.options(day(36)),
        std::future::pending(),
    )
    .await
    .expect("resumed run");
    assert_eq!(second.journaled_until, Some(day(35)));
    assert_eq!(
        second.bars, 0,
        "nothing live; the offline days came through replay"
    );
    assert_eq!(second.session_id, first.session_id);
}

#[tokio::test]
async fn a_gap_in_the_stream_is_backfilled_in_order() {
    let w = World::new("gap").await;
    let now = day(30) + Duration::minutes(5);
    // The stream skips days 31 and 32; market data has them.
    let mut stream = live(30..31);
    stream.extend(live(33..35));
    let out = run(
        &w.ports(35, stream),
        &slot(),
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect("run");
    // Day 30 came with the history sync (the fake serves it at `now`); 31..35 live, with 31
    // and 32 backfilled for *both* instruments before day 33 — none skipped as out of order.
    assert_eq!(out.bars, 8, "days 31..35 for both instruments");
    assert_eq!(out.journaled_until, Some(day(34)));
    let logged = w.log.read(day(31), day(33)).expect("event log");
    let backfilled = logged.iter().filter(|e| e.event.kind() == "bar").count();
    assert_eq!(backfilled, 4, "days 31 and 32 for both instruments");
}

#[tokio::test]
async fn a_stop_request_ends_the_run_and_the_session_resumes() {
    let w = World::new("stop").await;
    let files = SlotFiles::new(&w.data, &slot().id);
    let now = day(30) + Duration::minutes(5);
    let venue = VenueId::new("binance").expect("valid");
    let mut ports = w.ports(30, Vec::new());
    ports.stream = Arc::new(MemoryMarketStream::new(venue, Vec::new()).keep_open());

    // A stale marker from before the run is dropped, not obeyed …
    request_stop(&files).expect("stale marker");
    let options = RunOptions {
        heartbeat: StdDuration::from_millis(20),
        ..w.options(now)
    };
    let stopper = {
        let files = files.clone();
        tokio::spawn(async move {
            tokio::time::sleep(StdDuration::from_millis(100)).await;
            request_stop(&files).expect("stop");
        })
    };
    // … and one made during the run stops it at the next heartbeat.
    let out = run(
        &ports,
        &slot(),
        &envelope(),
        &options,
        std::future::pending(),
    )
    .await
    .expect("run");
    stopper.await.expect("stopper");
    assert_eq!(out.status, Status::Stopped);
    let state = files.load_state().expect("state").expect("written");
    assert_eq!(state.status, "stopped");
    let session = files.load_session().expect("session").expect("written");
    assert!(
        session.ended_at.is_none(),
        "a stopped session stays open for the next run"
    );
}

#[tokio::test]
async fn a_changed_configuration_needs_a_fresh_session() {
    let w = World::new("changed").await;
    let now = day(30) + Duration::minutes(5);
    run(
        &w.ports(30, Vec::new()),
        &slot(),
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect("first run");
    let changed = SlotConfig::parse(&SLOT.replace("lookback = 3", "lookback = 5")).expect("valid");
    let err = run(
        &w.ports(30, Vec::new()),
        &changed,
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect_err("refused");
    assert!(matches!(err, RunnerError::Refused(_)), "{err}");
    assert!(err.to_string().contains("--fresh"));

    let fresh = RunOptions {
        fresh: true,
        ..w.options(now)
    };
    let out = run(
        &w.ports(30, Vec::new()),
        &changed,
        &envelope(),
        &fresh,
        std::future::pending(),
    )
    .await
    .expect("fresh run");
    assert!(!out.resumed);
}

#[tokio::test]
async fn a_research_slot_may_not_paper_trade() {
    let w = World::new("research").await;
    let research =
        SlotConfig::parse(&SLOT.replace(r#"start_mode = "paper""#, r#"start_mode = "research""#))
            .expect("valid");
    let err = run(
        &w.ports(30, Vec::new()),
        &research,
        &envelope(),
        &w.options(day(30)),
        std::future::pending(),
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("through the ladder"), "{err}");
}

#[tokio::test]
async fn an_envelope_capped_at_research_demotes_and_refuses() {
    let w = World::new("capped").await;
    run(
        &w.ports(30, Vec::new()),
        &slot(),
        &envelope(),
        &w.options(day(30)),
        std::future::pending(),
    )
    .await
    .expect("paper run");
    let research =
        Envelope::parse(&ENVELOPE.replace(r#"max_mode = "paper""#, r#"max_mode = "research""#))
            .expect("valid");
    let err = run(
        &w.ports(30, Vec::new()),
        &slot(),
        &research,
        &w.options(day(31)),
        std::future::pending(),
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("is in research"), "{err}");
}

#[tokio::test]
async fn a_venue_outside_the_envelope_is_refused() {
    let w = World::new("venue").await;
    let kraken_only =
        Envelope::parse(&ENVELOPE.replace(r#"["binance"]"#, r#"["kraken"]"#)).expect("valid");
    let err = run(
        &w.ports(30, Vec::new()),
        &slot(),
        &kraken_only,
        &w.options(day(30)),
        std::future::pending(),
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("venues_allowed"), "{err}");
}

#[tokio::test]
async fn a_second_process_cannot_run_the_same_slot() {
    let w = World::new("locked").await;
    let files = SlotFiles::new(&w.data, &slot().id);
    std::fs::create_dir_all(&files.dir).expect("dir");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(files.lock_path())
        .expect("lock file");
    holder.try_lock().expect("first writer");
    let err = run(
        &w.ports(30, Vec::new()),
        &slot(),
        &envelope(),
        &w.options(day(30)),
        std::future::pending(),
    )
    .await
    .expect_err("second writer refused");
    assert!(err.to_string().contains("another quorum process"), "{err}");
    drop(holder);
    run(
        &w.ports(30, Vec::new()),
        &slot(),
        &envelope(),
        &w.options(day(30)),
        std::future::pending(),
    )
    .await
    .expect("runs once the lock is free");
}

#[test]
fn only_an_ended_stream_asks_for_a_restart() {
    assert!(Status::StreamEnded.restart());
    assert!(!Status::Stopped.restart());
}

#[tokio::test]
async fn the_operator_hears_of_the_start_each_day_and_the_end() {
    let w = World::new("notes").await;
    let now = day(30) + Duration::minutes(5);
    run(
        &w.ports(30, live(30..33)),
        &slot(),
        &envelope(),
        &w.options(now),
        std::future::pending(),
    )
    .await
    .expect("run");
    let notes = w.notes.sent();
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(
        titles.first(),
        Some(&"paper crypto-spot · session started"),
        "{titles:?}"
    );
    let days = titles.iter().filter(|t| t.contains("· day 2026-")).count();
    assert_eq!(
        days, 3,
        "one summary per completed day (30, 31, 32): {titles:?}"
    );
    assert!(
        notes.iter().any(|n| n.body.contains("since start")),
        "summaries carry the equity change"
    );
    let last = notes.last().expect("end");
    assert_eq!(last.title, "paper crypto-spot · session stream-ended");
    assert_eq!(last.severity, quorum_core::Severity::Warning);

    // Resuming does not repeat a summary for a day already summarized.
    run(
        &w.ports(30, live(32..33)),
        &slot(),
        &envelope(),
        &w.options(day(33)),
        std::future::pending(),
    )
    .await
    .expect("resumed");
    let again = w.notes.sent();
    let days_now = again
        .iter()
        .filter(|n| n.title.contains("· day 2026-"))
        .count();
    assert_eq!(days_now, 3, "day 32 is not summarized twice");
    assert!(again.iter().any(|n| n.title.ends_with("session resumed")));
}

#[tokio::test]
async fn every_order_is_approved_by_the_risk_process_and_recorded() {
    let w = World::new("gate").await;
    let out = run(
        &w.ports(30, live(30..40)),
        &slot(),
        &envelope(),
        &w.options(day(30) + Duration::minutes(5)),
        std::future::pending(),
    )
    .await
    .expect("run");
    let checks = w.risk.checks();
    let fills = w.journal.fills(&out.session_id).await.expect("fills");
    assert!(!fills.is_empty());
    assert!(
        checks.len() >= fills.len(),
        "every fill was an approved order"
    );
    assert!(checks.iter().all(|c| c.slot == "crypto-spot"
        && c.mode == quorum_envelope::Mode::Paper
        && c.equity > 0.0));
    let slot_files = SlotFiles::new(&w.data, &slot().id);
    let recorded = std::fs::read_to_string(slot_files.verdicts()).expect("verdicts");
    assert_eq!(recorded.lines().count(), checks.len());
}

#[tokio::test]
async fn with_the_risk_process_down_nothing_adds_risk_and_the_operator_hears() {
    let w = World::new("gate-down").await;
    w.risk.set(FakeMode::Down);
    let out = run(
        &w.ports(30, live(30..40)),
        &slot(),
        &envelope(),
        &w.options(day(30) + Duration::minutes(5)),
        std::future::pending(),
    )
    .await
    .expect("run");
    assert!(
        w.journal
            .fills(&out.session_id)
            .await
            .expect("fills")
            .is_empty()
    );
    assert!((out.equity - 10_000.0).abs() < 1e-9, "cash untouched");
    assert!(w.notes.sent().iter().any(|n| {
        n.severity == quorum_core::Severity::Critical
            && n.title.contains("risk process unreachable")
    }));
}

#[tokio::test]
async fn replay_repeats_the_recorded_verdicts() {
    // Refused while live: no positions.
    let w = World::new("gate-replay").await;
    w.risk
        .set(FakeMode::RefuseIncreases("kill switch: drill".to_owned()));
    let first = run(
        &w.ports(30, live(30..35)),
        &slot(),
        &envelope(),
        &w.options(day(30) + Duration::minutes(5)),
        std::future::pending(),
    )
    .await
    .expect("first run");
    assert!((first.equity - 10_000.0).abs() < 1e-9);
    let asked = w.risk.checks().len();
    assert!(asked > 0);
    // The kill switch is lifted, but the replayed days keep their refusals: the gate is not
    // asked again about them, and the book is rebuilt exactly as it was.
    w.risk.set(FakeMode::Allow);
    let second = run(
        &w.ports(35, Vec::new()),
        &slot(),
        &envelope(),
        &w.options(day(35)),
        std::future::pending(),
    )
    .await
    .expect("resumed run");
    assert!(second.resumed);
    assert_eq!(second.journaled_until, first.journaled_until);
    assert_eq!(w.risk.checks().len(), asked, "replay asked nothing");
    assert!((second.equity - first.equity).abs() < 1e-9);
}
