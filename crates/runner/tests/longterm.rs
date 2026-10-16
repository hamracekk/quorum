//! The long-term book on paper: contributions, the risk gate, resume by replay, a pinned policy.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use quorum_core::fakes::{MemoryBarStore, MemoryMarketData, MemoryNotifier};
use quorum_core::{Bar, InstrumentId, VenueId};
use quorum_envelope::{Envelope, LONGTERM_BOOK};
use quorum_longterm::LongTermConfig;
use quorum_risk::fakes::{FakeGate, FakeMode};
use quorum_runner::RunnerError;
use quorum_runner::longterm::{BookOptions, BookPorts, BookState, load_state, run_book};

const ENVELOPE: &str = r#"
currency = "EUR"
capital_total = 0
leverage_max = 1.0
venues_allowed = ["tiingo"]
loss = { daily_max = 0.05, total_max = 0.15 }
slot_default = { capital_max = 0, max_mode = "paper" }
position_max = 0.25

[longterm]
capital_max = 0
max_mode = "paper"
position_max = 0.7
"#;

const CONFIG: &str = r#"
prices_from = "multi-asset"
fx_cost = 0.0015
fee_rate = 0.0005
slippage_bps = 2.0

[[policies]]
name = "test"
start_czk = 100000
monthly_czk = 5000
band = 0.05
holdings = [
  { instrument = "tiingo:SPY:equity", weight = 0.6 },
  { instrument = "tiingo:IEF:equity", weight = 0.4 },
]
"#;

fn spy() -> InstrumentId {
    "tiingo:SPY:equity".parse().expect("valid")
}

fn ief() -> InstrumentId {
    "tiingo:IEF:equity".parse().expect("valid")
}

fn day(n: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("valid")
        + Duration::days(n)
}

fn bars(start: f64, drift: f64) -> Vec<Bar> {
    (0..90)
        .map(|n| {
            let close = start * (1.0 + drift).powi(i32::try_from(n).expect("small"));
            Bar {
                ts: day(n),
                open: close,
                high: close,
                low: close,
                close,
                volume: 1e6,
            }
        })
        .collect()
}

struct World {
    data: PathBuf,
    ports: BookPorts,
    gate: Arc<FakeGate>,
    notifier: Arc<MemoryNotifier>,
    rates: BTreeMap<NaiveDate, f64>,
}

impl World {
    fn new(name: &str, mode: FakeMode) -> Self {
        let data =
            std::env::temp_dir().join(format!("quorum-longterm-{name}-{}", uuid::Uuid::now_v7()));
        let market = MemoryMarketData::new(VenueId::new("tiingo").expect("valid"))
            .with_bars(spy(), bars(500.0, 0.002))
            .with_bars(ief(), bars(95.0, 0.0));
        let gate = Arc::new(FakeGate::new(mode));
        let notifier = Arc::new(MemoryNotifier::default());
        let rates =
            std::iter::once((NaiveDate::from_ymd_opt(2025, 12, 1).expect("valid"), 22.0)).collect();
        Self {
            ports: BookPorts {
                market: Arc::new(market),
                bars: Arc::new(MemoryBarStore::default()),
                risk: Arc::clone(&gate) as Arc<dyn quorum_risk::RiskGate>,
                notifier: Arc::clone(&notifier) as Arc<dyn quorum_core::Notifier>,
            },
            data,
            gate,
            notifier,
            rates,
        }
    }

    async fn run(
        &self,
        config: &str,
        envelope: &str,
        now: DateTime<Utc>,
        fresh: bool,
    ) -> Result<BookState, RunnerError> {
        let cfg = LongTermConfig::parse(config).expect("valid config");
        let envelope = Envelope::parse(envelope).expect("valid envelope");
        let options = BookOptions {
            data: self.data.clone(),
            fresh,
            now,
        };
        run_book(&self.ports, &cfg, &envelope, &self.rates, &options).await
    }
}

#[tokio::test]
async fn the_book_invests_its_contributions_through_the_risk_process() {
    let w = World::new("invest", FakeMode::Allow);
    // The first day: the start amount arrives at its close; nothing has filled yet.
    let first = w
        .run(CONFIG, ENVELOPE, day(0) + Duration::hours(23), false)
        .await
        .expect("runs");
    assert!((first.contributed_czk - 100_000.0).abs() < 1e-6);
    assert!(first.holdings.is_empty() && first.fills == 0);

    // 45 days later: bought at the next open, and February's contribution invested.
    let later = w
        .run(CONFIG, ENVELOPE, day(45), false)
        .await
        .expect("resumes");
    assert_eq!(later.session_id, first.session_id, "the same session");
    assert!((later.contributed_czk - 105_000.0).abs() < 1e-6);
    assert_eq!(later.holdings.len(), 2);
    for h in &later.holdings {
        assert!((h.weight - h.target).abs() < 0.03, "{h:?}");
    }
    assert!(later.lots.len() >= 2 && later.sells == 0);
    assert!(later.value_czk > 0.95 * later.contributed_czk);
    let checks = w.gate.checks();
    assert!(!checks.is_empty());
    assert!(
        checks
            .iter()
            .all(|c| c.slot == LONGTERM_BOOK && c.interval_secs == 86_400),
        "every order asked as the long-term book"
    );
    assert!(
        checks[0].equity > 4_000.0,
        "the gate knows the contribution: {}",
        checks[0].equity
    );
    let sent = w.notifier.sent();
    assert_eq!(sent.len(), 1, "one note for the new fills");
    assert!(sent[0].title.starts_with("long-term"));
    assert_eq!(load_state(&w.data).expect("readable"), Some(later.clone()));

    // A run with nothing new asks nothing again and announces nothing.
    let again = w.run(CONFIG, ENVELOPE, day(45), false).await.expect("runs");
    assert_eq!(again.fills, later.fills);
    assert_eq!(w.gate.checks().len(), checks.len(), "verdicts replayed");
    assert_eq!(w.notifier.sent().len(), 1);
}

#[tokio::test]
async fn refused_orders_leave_the_book_in_cash() {
    let w = World::new("refused", FakeMode::RefuseIncreases("no".to_owned()));
    w.run(CONFIG, ENVELOPE, day(0) + Duration::hours(23), false)
        .await
        .expect("starts");
    let state = w.run(CONFIG, ENVELOPE, day(10), false).await.expect("runs");
    assert!(state.holdings.is_empty());
    assert!(state.refused > 0);
    assert!(state.cash_czk > 90_000.0);
}

#[tokio::test]
async fn a_changed_policy_needs_a_fresh_session() {
    let w = World::new("pinned", FakeMode::Allow);
    let first = w.run(CONFIG, ENVELOPE, day(5), false).await.expect("runs");
    let changed = CONFIG
        .replace("weight = 0.6", "weight = 0.5")
        .replace("weight = 0.4", "weight = 0.5");
    let err = w
        .run(&changed, ENVELOPE, day(6), false)
        .await
        .expect_err("pinned");
    assert!(err.to_string().contains("--fresh"), "{err}");
    let fresh = w
        .run(&changed, ENVELOPE, day(6), true)
        .await
        .expect("fresh");
    assert_ne!(fresh.session_id, first.session_id);
    assert!(
        w.data
            .join("longterm/sessions")
            .join(format!("{}.json", first.session_id))
            .exists(),
        "the old session is archived"
    );
}

#[tokio::test]
async fn the_envelope_must_bound_the_book() {
    let w = World::new("envelope", FakeMode::Allow);
    let without = ENVELOPE.split("[longterm]").next().expect("head");
    let err = w
        .run(CONFIG, without, day(5), false)
        .await
        .expect_err("no section");
    assert!(err.to_string().contains("[longterm]"), "{err}");
    let tight = ENVELOPE.replace("position_max = 0.7", "position_max = 0.5");
    let err = w
        .run(CONFIG, &tight, day(5), false)
        .await
        .expect_err("a target over the bound");
    assert!(err.to_string().contains("position_max"), "{err}");
}
