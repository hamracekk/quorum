//! The decisions, as pure functions of the state, the envelope and the request.

use chrono::{DateTime, Utc};
use quorum_envelope::{Envelope, LossLimits, Mode};

use crate::protocol::{OrderCheck, Verdict};
use crate::state::{Halt, RiskState};

/// Slack on top of two intervals before an order's bar counts as stale (processing, a slow feed).
pub const STALE_GRACE_SECS: i64 = 3_600;

/// Leeway on `leverage_max`: fees and slippage leave a fully invested spot book a hair over 1×.
pub const LEVERAGE_ALLOWANCE: f64 = 0.01;

/// A halt that began or ended, for alerts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    Halted { slot: Option<String>, halt: Halt },
    Lifted { slot: Option<String>, halt: Halt },
}

/// Records `equity` for `slot` and halts the slot when a loss limit is reached. A new UTC day
/// measures from the last equity seen before it and lifts a daily halt.
pub fn observe(
    state: &mut RiskState,
    slot: &str,
    equity: f64,
    loss: LossLimits,
    now: DateTime<Utc>,
) -> Vec<Transition> {
    let mut out = Vec::new();
    let risk = state.slots.entry(slot.to_owned()).or_default();
    risk.last_seen = Some(now);
    if !equity.is_finite() || equity <= 0.0 {
        return out;
    }
    let today = now.date_naive();
    if risk.day != Some(today) {
        risk.day = Some(today);
        risk.day_ref = Some(risk.last_equity.unwrap_or(equity));
        if let Some(halt @ Halt::Daily { .. }) = &risk.halt {
            out.push(Transition::Lifted {
                slot: Some(slot.to_owned()),
                halt: halt.clone(),
            });
            risk.halt = None;
        }
    }
    let peak = risk.peak.map_or(equity, |p| p.max(equity));
    risk.peak = Some(peak);
    risk.last_equity = Some(equity);
    if matches!(risk.halt, Some(Halt::Total | Halt::Operator { .. })) {
        return out;
    }
    let halt = if equity <= peak * (1.0 - loss.total_max) {
        Some(Halt::Total)
    } else if risk.halt.is_none()
        && risk
            .day_ref
            .is_some_and(|r| equity <= r * (1.0 - loss.daily_max))
    {
        Some(Halt::Daily { day: today })
    } else {
        None
    };
    if let Some(halt) = halt {
        risk.halt = Some(halt.clone());
        out.push(Transition::Halted {
            slot: Some(slot.to_owned()),
            halt,
        });
    }
    out
}

/// Decides one order and records the slot's equity it carries.
///
/// Risk-reducing orders are always approved (anything may reduce risk); everything else must
/// pass the envelope, the halts and the leverage limit. `envelope` is `None` when the envelope
/// file cannot be read: then only risk-reducing orders pass.
pub fn check(
    state: &mut RiskState,
    envelope: Option<&Envelope>,
    order: &OrderCheck,
    now: DateTime<Utc>,
) -> (Verdict, Vec<Transition>) {
    let transitions = envelope.map_or_else(Vec::new, |e| {
        observe(state, &order.slot, order.equity, e.loss(), now)
    });
    let verdict = decide(state, envelope, order, now);
    let risk = state.slots.entry(order.slot.clone()).or_default();
    if verdict.approved {
        risk.approved += 1;
    } else {
        risk.rejected += 1;
    }
    (verdict, transitions)
}

fn decide(
    state: &RiskState,
    envelope: Option<&Envelope>,
    order: &OrderCheck,
    now: DateTime<Utc>,
) -> Verdict {
    let valid = order.qty.is_finite() && order.qty > 0.0 && order.price.is_finite();
    if valid && order.position.is_finite() && order.reduces_risk() {
        return Verdict::approve("reduces risk");
    }
    let Some(envelope) = envelope else {
        return Verdict::reject("the envelope cannot be read");
    };
    if let Some(halt) = state.halt_for(&order.slot) {
        return Verdict::reject(halt.describe());
    }
    // An order on a bar more than two intervals old was decided on stale data (a gap, a sleep, a
    // stalled feed): no new risk on it.
    if order.interval_secs > 0
        && (now - order.ts).num_seconds() > 2 * order.interval_secs + STALE_GRACE_SECS
    {
        return Verdict::reject(format!(
            "stale data: the order's bar is {} h old",
            (now - order.ts).num_hours()
        ));
    }
    if order.mode < Mode::Paper {
        return Verdict::reject(format!("slot is in {}; it does not trade", order.mode));
    }
    let max = envelope.max_mode(&order.slot);
    if order.mode > max {
        return Verdict::reject(format!(
            "slot is in {} but the envelope allows {max} at most",
            order.mode
        ));
    }
    if order.mode.is_live() {
        return Verdict::reject("live execution is not built; paper only");
    }
    if !envelope.allows_venue(order.instrument.venue()) {
        return Verdict::reject(format!(
            "venue `{}` is not in the envelope",
            order.instrument.venue().as_str()
        ));
    }
    if !valid || order.price <= 0.0 || !order.equity.is_finite() || order.equity <= 0.0 {
        return Verdict::reject("order or book values are not usable");
    }
    let gross = order.gross - (order.position * order.price).abs()
        + (order.position_after() * order.price).abs();
    let leverage = gross / order.equity;
    let limit = envelope.leverage_max() * (1.0 + LEVERAGE_ALLOWANCE);
    if !leverage.is_finite() || leverage > limit {
        return Verdict::reject(format!(
            "gross exposure {leverage:.2}× equity exceeds leverage_max {:.2}×",
            envelope.leverage_max()
        ));
    }
    let after = (order.position_after() * order.price).abs() / order.equity;
    if let Some(max) = envelope.position_max()
        && after > max * (1.0 + LEVERAGE_ALLOWANCE)
    {
        return Verdict::reject(format!(
            "position {:.0}% of equity exceeds position_max {:.0}%",
            after * 100.0,
            max * 100.0
        ));
    }
    let cluster = order.cluster / order.equity;
    if let Some(max) = envelope.cluster_max()
        && cluster > max * (1.0 + LEVERAGE_ALLOWANCE)
    {
        return Verdict::reject(format!(
            "correlated cluster {:.0}% of equity exceeds cluster_max {:.0}%",
            cluster * 100.0,
            max * 100.0
        ));
    }
    Verdict::approve("within the envelope")
}

/// Pulls the kill switch for `slot`, or for every slot.
pub fn halt(state: &mut RiskState, slot: Option<&str>, reason: &str) -> Transition {
    let halt = Halt::Operator {
        reason: reason.to_owned(),
    };
    match slot {
        Some(slot) => {
            state.slots.entry(slot.to_owned()).or_default().halt = Some(halt.clone());
        }
        None => state.kill = Some(halt.clone()),
    }
    Transition::Halted {
        slot: slot.map(str::to_owned),
        halt,
    }
}

/// Lifts the halts of `slot`, or the kill switch and every slot's halts. Loss references
/// restart from the last equity seen, so a lifted slot is not halted again by the same loss.
pub fn resume(state: &mut RiskState, slot: Option<&str>) -> Vec<Transition> {
    let mut out = Vec::new();
    if slot.is_none()
        && let Some(halt) = state.kill.take()
    {
        out.push(Transition::Lifted { slot: None, halt });
    }
    for (name, risk) in &mut state.slots {
        if slot.is_some_and(|s| s != name) {
            continue;
        }
        if let Some(halt) = risk.halt.take() {
            out.push(Transition::Lifted {
                slot: Some(name.clone()),
                halt,
            });
        }
        risk.peak = risk.last_equity;
        risk.day_ref = risk.last_equity;
    }
    out
}
