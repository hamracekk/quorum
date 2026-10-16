//! The risk rules on synthetic books, and the process end to end over its socket.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use quorum_core::fakes::MemoryNotifier;
use quorum_core::{InstrumentId, Severity, Side};
use quorum_envelope::{Envelope, Mode};
use quorum_risk::rules::{self, Transition};
use quorum_risk::{
    Halt, OrderCheck, Ping, Request, Response, RiskGate, RiskPaths, RiskState, SocketGate, serve,
};

const ENVELOPE: &str = r#"
currency = "EUR"
capital_total = 0
leverage_max = 1.0
venues_allowed = ["binance"]
loss = { daily_max = 0.05, total_max = 0.15 }
slot_default = { capital_max = 0, max_mode = "paper" }
position_max = 0.6
cluster_max = 0.8
"#;

fn envelope() -> Envelope {
    Envelope::parse(ENVELOPE).expect("valid envelope")
}

fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0)
        .single()
        .expect("valid")
}

/// A buy of `qty` BTC at 100 from a flat book worth `equity`.
fn buy(qty: f64, equity: f64) -> OrderCheck {
    OrderCheck {
        slot: "crypto-spot".to_owned(),
        mode: Mode::Paper,
        instrument: InstrumentId::spot("binance", "BTCUSDT").expect("valid"),
        side: Side::Buy,
        qty,
        price: 100.0,
        position: 0.0,
        gross: 0.0,
        equity,
        cluster: qty * 100.0,
        ts: at(16, 0),
        interval_secs: 0,
    }
}

fn sell(qty: f64, position: f64, equity: f64) -> OrderCheck {
    OrderCheck {
        side: Side::Sell,
        position,
        gross: position * 100.0,
        ..buy(qty, equity)
    }
}

fn verdict(state: &mut RiskState, order: &OrderCheck, now: DateTime<Utc>) -> (bool, String) {
    let (v, _) = rules::check(state, Some(&envelope()), order, now);
    (v.approved, v.reason)
}

#[test]
fn an_order_within_the_envelope_is_approved() {
    let mut state = RiskState::default();
    let (ok, reason) = verdict(&mut state, &buy(50.0, 10_000.0), at(16, 1));
    assert!(ok, "{reason}");
    assert_eq!(state.slots["crypto-spot"].approved, 1);
}

#[test]
fn leverage_over_the_envelope_is_rejected() {
    let mut state = RiskState::default();
    // 150 × 100 = 15 000 on 10 000 equity: 1.5×.
    let (ok, reason) = verdict(&mut state, &buy(150.0, 10_000.0), at(16, 1));
    assert!(!ok);
    assert!(reason.contains("leverage_max"), "{reason}");
}

#[test]
fn reducing_risk_passes_every_halt_and_an_unreadable_envelope() {
    let mut state = RiskState::default();
    rules::halt(&mut state, None, "test");
    let (ok, _) = verdict(&mut state, &sell(10.0, 50.0, 10_000.0), at(16, 1));
    assert!(ok);
    let (v, _) = rules::check(&mut state, None, &sell(10.0, 50.0, 10_000.0), at(16, 1));
    assert!(v.approved);
    // Selling more than held flips the position short: that adds risk.
    let (ok, _) = verdict(&mut state, &sell(60.0, 50.0, 10_000.0), at(16, 1));
    assert!(!ok);
    let (v, _) = rules::check(&mut state, None, &buy(1.0, 10_000.0), at(16, 1));
    assert!(!v.approved);
}

#[test]
fn the_kill_switch_refuses_new_risk_until_resumed() {
    let mut state = RiskState::default();
    rules::halt(&mut state, Some("crypto-spot"), "operator test");
    let (ok, reason) = verdict(&mut state, &buy(1.0, 10_000.0), at(16, 1));
    assert!(!ok);
    assert!(reason.contains("operator test"), "{reason}");
    // Other slots are not affected by a slot's halt.
    let other = OrderCheck {
        slot: "multi-asset".to_owned(),
        ..buy(1.0, 10_000.0)
    };
    assert!(verdict(&mut state, &other, at(16, 1)).0);
    rules::resume(&mut state, Some("crypto-spot"));
    assert!(verdict(&mut state, &buy(1.0, 10_000.0), at(16, 1)).0);
}

#[test]
fn a_daily_loss_halts_the_slot_until_the_next_utc_day() {
    let mut state = RiskState::default();
    let loss = envelope().loss();
    rules::observe(&mut state, "crypto-spot", 10_000.0, loss, at(15, 23));
    // Day 16 measures from 10 000 (the last equity of day 15): −6% breaches the 5% limit.
    let t = rules::observe(&mut state, "crypto-spot", 9_400.0, loss, at(16, 10));
    assert!(matches!(
        t.as_slice(),
        [Transition::Halted {
            halt: Halt::Daily { .. },
            ..
        }]
    ));
    assert!(!verdict(&mut state, &buy(1.0, 9_400.0), at(16, 11)).0);
    // The next day lifts it.
    let t = rules::observe(&mut state, "crypto-spot", 9_400.0, loss, at(17, 0));
    assert!(matches!(t.as_slice(), [Transition::Lifted { .. }]));
    assert!(verdict(&mut state, &buy(1.0, 9_400.0), at(17, 1)).0);
}

#[test]
fn a_total_drawdown_halts_until_the_operator_resumes() {
    let mut state = RiskState::default();
    let loss = envelope().loss();
    // Down 4% a day, never 5% in one day, 15% or more from the peak by the fifth day.
    let mut equity = 10_000.0;
    for day in 10..=14 {
        rules::observe(&mut state, "crypto-spot", equity, loss, at(day, 12));
        equity *= 0.96;
    }
    assert_eq!(state.slots["crypto-spot"].halt, Some(Halt::Total));
    // A new day does not lift it.
    rules::observe(&mut state, "crypto-spot", equity, loss, at(20, 0));
    assert_eq!(state.slots["crypto-spot"].halt, Some(Halt::Total));
    rules::resume(&mut state, None);
    // The peak restarts at the last equity, so the same loss does not halt again.
    rules::observe(&mut state, "crypto-spot", equity, loss, at(20, 1));
    assert_eq!(state.slots["crypto-spot"].halt, None);
}

#[test]
fn modes_above_the_envelope_and_live_modes_are_refused() {
    let mut state = RiskState::default();
    let research = OrderCheck {
        mode: Mode::Research,
        ..buy(1.0, 10_000.0)
    };
    assert!(!verdict(&mut state, &research, at(16, 1)).0);
    let live = OrderCheck {
        mode: Mode::LiveMicro,
        ..buy(1.0, 10_000.0)
    };
    let (ok, reason) = verdict(&mut state, &live, at(16, 1));
    assert!(!ok);
    assert!(reason.contains("allows paper"), "{reason}");
}

#[test]
fn a_position_or_a_correlated_cluster_above_its_limit_is_refused() {
    let mut state = RiskState::default();
    // 70 × 100 = 70% of equity in one coin: over position_max 60%.
    let (ok, reason) = verdict(&mut state, &buy(70.0, 10_000.0), at(16, 1));
    assert!(!ok);
    assert!(reason.contains("position_max"), "{reason}");
    // 50% in this coin, but 90% in coins moving with it: over cluster_max 80%.
    let crowded = OrderCheck {
        cluster: 9_000.0,
        ..buy(50.0, 10_000.0)
    };
    let (ok, reason) = verdict(&mut state, &crowded, at(16, 1));
    assert!(!ok);
    assert!(reason.contains("cluster_max"), "{reason}");
    assert!(verdict(&mut state, &buy(50.0, 10_000.0), at(16, 1)).0);
}

#[test]
fn the_longterm_book_has_its_own_bounds_and_no_loss_halts() {
    let longterm = |order: OrderCheck| OrderCheck {
        slot: quorum_envelope::LONGTERM_BOOK.to_owned(),
        ..order
    };
    let mut state = RiskState::default();
    // Without a [longterm] section the book adds no risk.
    let (ok, reason) = verdict(&mut state, &longterm(buy(10.0, 10_000.0)), at(16, 1));
    assert!(!ok);
    assert!(reason.contains("[longterm]"), "{reason}");

    let bounded = Envelope::parse(&format!(
        "{ENVELOPE}\n[longterm]\ncapital_max = 0\nmax_mode = \"paper\"\nposition_max = 0.7"
    ))
    .expect("valid");
    let ask = |state: &mut RiskState, order: &OrderCheck, now| {
        let (v, transitions) = rules::check(state, Some(&bounded), order, now);
        assert!(transitions.is_empty(), "no loss halts: {transitions:?}");
        (v.approved, v.reason)
    };
    // 65% in one fund: over the slots' 60%, within the book's 70%; the cluster is not capped.
    let (ok, reason) = ask(&mut state, &longterm(buy(65.0, 10_000.0)), at(16, 1));
    assert!(ok, "{reason}");
    let (ok, reason) = ask(&mut state, &longterm(buy(75.0, 10_000.0)), at(16, 1));
    assert!(!ok);
    assert!(reason.contains("position_max 70%"), "{reason}");
    // A 40% fall in a day halts no long-term book.
    assert!(ask(&mut state, &longterm(buy(1.0, 6_000.0)), at(16, 2)).0);
    // The kill switch still does.
    rules::halt(&mut state, None, "operator");
    assert!(!ask(&mut state, &longterm(buy(1.0, 6_000.0)), at(16, 3)).0);
}

#[test]
fn an_order_on_stale_data_adds_no_risk() {
    let mut state = RiskState::default();
    let daily = OrderCheck {
        interval_secs: 86_400,
        ..buy(1.0, 10_000.0)
    };
    // The bar of 16 Oct 00:00 is current until about 18 Oct 01:00.
    assert!(verdict(&mut state, &daily, at(17, 12)).0);
    let (ok, reason) = verdict(&mut state, &daily, at(19, 0));
    assert!(!ok);
    assert!(reason.contains("stale"), "{reason}");
    let selling = OrderCheck {
        side: Side::Sell,
        position: 5.0,
        ..daily
    };
    assert!(
        verdict(&mut state, &selling, at(19, 0)).0,
        "reducing still passes"
    );
}

#[test]
fn venues_outside_the_envelope_are_refused() {
    let mut state = RiskState::default();
    let order = OrderCheck {
        instrument: InstrumentId::spot("kraken", "BTCEUR").expect("valid"),
        ..buy(1.0, 10_000.0)
    };
    let (ok, reason) = verdict(&mut state, &order, at(16, 1));
    assert!(!ok);
    assert!(reason.contains("kraken"), "{reason}");
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quorum-risk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Calls the blocking client off the runtime's threads.
async fn ask(gate: &SocketGate, request: Request) -> Response {
    let gate = gate.clone();
    tokio::task::spawn_blocking(move || gate.request(&request))
        .await
        .expect("joined")
        .expect("answer")
}

#[tokio::test]
async fn the_process_answers_over_its_socket_and_keeps_halts_across_restarts() {
    let dir = temp("socket");
    let envelope_path = dir.join("envelope.toml");
    std::fs::write(&envelope_path, ENVELOPE).expect("envelope written");
    let paths = RiskPaths::new(&dir.join("data"), &envelope_path);
    let notes = Arc::new(MemoryNotifier::default());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(paths.clone(), notes.clone(), async {
        let _ = stopped.await;
    }));
    let gate = SocketGate::new(paths.socket());
    let mut tries = 0;
    while !paths.socket().exists() {
        tries += 1;
        assert!(tries < 200, "socket never appeared");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let order = buy(1.0, 10_000.0);
    let checked = {
        let gate = gate.clone();
        let order = order.clone();
        tokio::task::spawn_blocking(move || gate.check(&order))
            .await
            .expect("joined")
            .expect("verdict")
    };
    assert!(checked.approved, "{}", checked.reason);

    let halted = ask(
        &gate,
        Request::Halt {
            slot: None,
            reason: "drill".to_owned(),
        },
    )
    .await;
    assert_eq!(halted, Response::Done);
    let Response::Verdict(v) = ask(&gate, Request::Check(order.clone())).await else {
        panic!("expected a verdict");
    };
    assert!(!v.approved);
    let pong = ask(
        &gate,
        Request::Ping(Ping {
            slot: "crypto-spot".to_owned(),
            mode: Mode::Paper,
            equity: 10_000.0,
        }),
    )
    .await;
    assert!(matches!(pong, Response::Pong { halt: Some(_) }));
    assert!(
        notes
            .sent()
            .iter()
            .any(|n| n.severity == Severity::Critical)
    );

    // A request that is not one is answered with an error, not a crash.
    let bad = {
        let path = paths.socket();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, BufReader, Write};
            let mut s = std::os::unix::net::UnixStream::connect(path).expect("connect");
            s.write_all(b"{\"kind\":\"launch\"}\n").expect("write");
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).expect("read");
            line
        })
        .await
        .expect("joined")
    };
    assert!(bad.contains("error"), "{bad}");

    stop.send(()).expect("server running");
    server.await.expect("joined").expect("served");
    assert!(!paths.socket().exists());
    // The kill switch survived in the state file.
    let state = RiskState::load(&paths.state()).expect("state");
    assert!(state.kill.is_some());
    // No process: the gate is unreachable, and says so.
    let down = tokio::task::spawn_blocking(move || gate.check(&order))
        .await
        .expect("joined");
    assert!(down.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
