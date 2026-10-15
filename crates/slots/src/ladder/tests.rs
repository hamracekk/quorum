use chrono::{Duration, TimeZone};

use super::*;

const SLOT: &str = r#"
id = "crypto-spot"
venue = "binance"
interval = "1d"
strategies = ["trend-momentum"]
start_mode = "paper"
universe = { kind = "fixed", instruments = ["binance:BTCUSDT"] }
costs = { fee_rate = 0.001, slippage_bps = 2.0 }
"#;

fn envelope(max_mode: &str) -> Envelope {
    let capital = if max_mode.starts_with("live") {
        1000
    } else {
        0
    };
    Envelope::parse(&format!(
        r#"
currency = "EUR"
capital_total = {capital}
leverage_max = 1.0
venues_allowed = ["binance"]
loss = {{ daily_max = 0.02, total_max = 0.15 }}
slot_default = {{ capital_max = {capital}, max_mode = "{max_mode}" }}
"#
    ))
    .expect("valid envelope")
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0)
        .single()
        .expect("valid date")
}

fn slot(start: &str, env: &Envelope) -> SlotState {
    let config = SlotConfig::parse(&SLOT.replace(
        r#"start_mode = "paper""#,
        &format!("start_mode = \"{start}\""),
    ))
    .expect("valid slot");
    SlotState::new(&config, env, t0())
}

fn research_evidence() -> Evidence {
    Evidence {
        research: ResearchRecord {
            beats_benchmark: true,
            deflated_sharpe: Some(1.1),
            pbo: Some(0.1),
        },
        ..Evidence::default()
    }
}

fn paper_evidence() -> Evidence {
    Evidence {
        paper: PaperRecord {
            tracking_ok: true,
            stress_passed: true,
        },
        ..research_evidence()
    }
}

/// `paper_evidence` with one field of `live` changed.
fn with_live(live: LiveRecord) -> Evidence {
    Evidence {
        live,
        ..paper_evidence()
    }
}

#[test]
fn start_mode_is_capped_by_the_envelope() {
    let state = slot("paper", &envelope("research"));
    assert_eq!(state.mode(), Mode::Research);
    assert_eq!(slot("paper", &envelope("paper")).mode(), Mode::Paper);
}

#[test]
fn research_promotes_to_paper_only_on_full_evidence() {
    let env = envelope("paper");
    let gates = Gates::default();
    let mut state = slot("research", &env);

    let held = state.review(&Evidence::default(), &gates, &env, t0());
    assert_eq!(
        held,
        Transition::Held {
            blockers: vec![
                Blocker::NoBenchmarkEdge,
                Blocker::DeflatedSharpe(None),
                Blocker::Pbo(None)
            ]
        }
    );

    let overfit = Evidence {
        research: ResearchRecord {
            pbo: Some(0.5),
            ..research_evidence().research
        },
        ..research_evidence()
    };
    assert!(matches!(
        state.review(&overfit, &gates, &env, t0()),
        Transition::Held { .. }
    ));

    let promoted = state.review(&research_evidence(), &gates, &env, t0());
    assert_eq!(
        promoted,
        Transition::Promoted {
            from: Mode::Research,
            to: Mode::Paper
        }
    );
    assert_eq!(state.mode(), Mode::Paper);
}

#[test]
fn a_paper_only_envelope_never_lets_a_slot_go_live() {
    let env = envelope("paper");
    let mut state = slot("paper", &env);
    let later = t0() + Duration::weeks(52);
    let held = state.review(&paper_evidence(), &Gates::default(), &env, later);
    assert_eq!(
        held,
        Transition::Held {
            blockers: vec![Blocker::EnvelopeCap(Mode::Paper)]
        }
    );
    assert_eq!(state.mode(), Mode::Paper);
}

#[test]
fn paper_needs_eight_weeks_tracking_and_stress_tests() {
    let env = envelope("live-micro");
    let gates = Gates::default();
    let mut state = slot("paper", &env);

    let early = state.review(&paper_evidence(), &gates, &env, t0() + Duration::weeks(4));
    assert!(matches!(
        early,
        Transition::Held { ref blockers } if matches!(blockers[..], [Blocker::TooSoon { needed, .. }] if (needed - 8.0).abs() < f64::EPSILON)
    ));

    let untested = Evidence {
        paper: PaperRecord {
            tracking_ok: true,
            stress_passed: false,
        },
        ..paper_evidence()
    };
    let held = state.review(&untested, &gates, &env, t0() + Duration::weeks(9));
    assert_eq!(
        held,
        Transition::Held {
            blockers: vec![Blocker::StressTests]
        }
    );

    let at = t0() + Duration::weeks(9);
    let promoted = state.review(&paper_evidence(), &gates, &env, at);
    assert_eq!(
        promoted,
        Transition::Promoted {
            from: Mode::Paper,
            to: Mode::LiveMicro
        }
    );
    assert_eq!(state.since(), at, "the clock restarts in the new mode");
}

#[test]
fn live_micro_needs_twelve_weeks_and_slippage_within_model() {
    let env = envelope("live");
    let gates = Gates::default();
    let mut state = slot("paper", &env);
    let at = t0() + Duration::weeks(9);
    state.review(&paper_evidence(), &gates, &env, at);
    assert_eq!(state.mode(), Mode::LiveMicro);

    let slippy = with_live(LiveRecord {
        slippage_ratio: Some(1.8),
        ..LiveRecord::default()
    });
    let held = state.review(&slippy, &gates, &env, at + Duration::weeks(13));
    assert_eq!(
        held,
        Transition::Held {
            blockers: vec![Blocker::Slippage(Some(1.8))]
        }
    );

    let good = with_live(LiveRecord {
        slippage_ratio: Some(1.2),
        ..LiveRecord::default()
    });
    let promoted = state.review(&good, &gates, &env, at + Duration::weeks(13));
    assert_eq!(
        promoted,
        Transition::Promoted {
            from: Mode::LiveMicro,
            to: Mode::Live
        }
    );
    let top = state.review(&good, &gates, &env, at + Duration::weeks(30));
    assert_eq!(
        top,
        Transition::Held {
            blockers: vec![Blocker::TopRung]
        }
    );
}

#[test]
fn drawdown_and_drift_demote_one_rung() {
    let env = envelope("paper");
    let gates = Gates::default();
    let mut state = slot("paper", &env);
    let drawdown = Evidence {
        alarms: Alarms {
            drawdown_over_budget: true,
            drift: false,
        },
        ..paper_evidence()
    };
    let demoted = state.review(&drawdown, &gates, &env, t0());
    assert_eq!(
        demoted,
        Transition::Demoted {
            from: Mode::Paper,
            to: Mode::Research,
            trigger: Trigger::DrawdownOverBudget
        }
    );
    // Research is the floor: drift there demotes nothing, and promotion still needs evidence.
    let drift = Evidence {
        alarms: Alarms {
            drawdown_over_budget: false,
            drift: true,
        },
        ..Evidence::default()
    };
    assert!(matches!(
        state.review(&drift, &gates, &env, t0()),
        Transition::Held { .. }
    ));
}

#[test]
fn costs_over_model_send_live_slots_back_to_paper() {
    let env = envelope("live");
    let gates = Gates::default();
    let mut state = slot("paper", &env);
    state.review(&paper_evidence(), &gates, &env, t0() + Duration::weeks(9));
    assert_eq!(state.mode(), Mode::LiveMicro);
    let costly = with_live(LiveRecord {
        cost_ratio: Some(2.5),
        ..LiveRecord::default()
    });
    let demoted = state.review(&costly, &gates, &env, t0() + Duration::weeks(10));
    assert_eq!(
        demoted,
        Transition::Demoted {
            from: Mode::LiveMicro,
            to: Mode::Paper,
            trigger: Trigger::CostsOverModel
        }
    );
}

#[test]
fn lowering_the_envelope_demotes_at_the_next_review() {
    let gates = Gates::default();
    let live = envelope("live-micro");
    let mut state = slot("paper", &live);
    state.review(&paper_evidence(), &gates, &live, t0() + Duration::weeks(9));
    assert_eq!(state.mode(), Mode::LiveMicro);
    let paper = envelope("paper");
    let demoted = state.review(
        &paper_evidence(),
        &gates,
        &paper,
        t0() + Duration::weeks(10),
    );
    assert_eq!(
        demoted,
        Transition::Demoted {
            from: Mode::LiveMicro,
            to: Mode::Paper,
            trigger: Trigger::EnvelopeLowered
        }
    );
}

#[test]
fn reconciliation_breaks_halt_until_the_operator_resumes() {
    let env = envelope("live-micro");
    let gates = Gates::default();
    let mut state = slot("paper", &env);
    state.review(&paper_evidence(), &gates, &env, t0() + Duration::weeks(9));
    let broken = with_live(LiveRecord {
        reconciliation_breaks: 1,
        ..LiveRecord::default()
    });
    let halted = state.review(&broken, &gates, &env, t0() + Duration::weeks(10));
    assert_eq!(
        halted,
        Transition::Halted {
            trigger: Trigger::ReconciliationBreak
        }
    );
    assert!(state.is_halted());
    let still = state.review(&paper_evidence(), &gates, &env, t0() + Duration::weeks(11));
    assert_eq!(
        still,
        Transition::Held {
            blockers: vec![Blocker::Halted]
        }
    );

    let resumed_at = t0() + Duration::weeks(12);
    state.resume(resumed_at);
    assert!(!state.is_halted());
    assert_eq!(
        state.mode(),
        Mode::Paper,
        "real money goes through the ladder again"
    );
    assert_eq!(state.since(), resumed_at);
}

#[test]
fn state_round_trips_through_json() {
    let env = envelope("paper");
    let state = slot("paper", &env);
    let json = serde_json::to_string(&state).expect("serialize");
    assert!(json.contains(r#""mode":"paper""#), "{json}");
    let back: SlotState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, state);
}
