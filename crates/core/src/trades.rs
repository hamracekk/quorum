//! The trade ledger: fills grouped into round trips (flat → position → flat), each with its
//! entry, exit, size, profit after fees, days held, what opened it and why it ended.
//!
//! Built from what the engine already records — fills and the decisions (`Signal` events)
//! behind them — so a backtest and a paper session read the same way and nothing new is
//! persisted. A fill's cause is the instrument's latest decision **before** it (strategy and
//! manual targets execute at the next bar), or a `stops` decision at the same time (stops,
//! take-profits, crash and delisting exits fill on the bar that triggers them). A position
//! closed while its latest decision still wanted it open was closed by the risk layer.
//!
//! Adding to or trimming a position keeps it one trade: the entry price is the
//! quantity-weighted average, profit is realised as it is reduced. A fill that flips the
//! direction closes the trade and opens the next with the rest.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::instrument::InstrumentId;
use crate::ports::Decision;
use crate::types::{Fill, Side};

/// Smallest quantity that still counts as a position.
const DUST: f64 = 1e-12;

/// One round trip.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoundTrip {
    pub instrument: InstrumentId,
    /// `1` long, `-1` short.
    pub direction: f64,
    pub opened: DateTime<Utc>,
    /// `None` while the trade is open.
    pub closed: Option<DateTime<Utc>>,
    /// Quantity-weighted average entry price.
    pub entry_price: f64,
    /// Quantity-weighted average exit price (`None` before any reduction).
    pub exit_price: Option<f64>,
    /// Largest quantity held.
    pub max_qty: f64,
    /// Money at entry prices at the largest size.
    pub max_notional: f64,
    pub fees: f64,
    /// Realised profit after fees; for an open trade also the unrealised part at `mark`.
    pub pnl: f64,
    /// `pnl` over `max_notional`.
    pub return_pct: f64,
    pub days_held: f64,
    /// Price an open trade is valued at (`None` for closed trades or when unknown).
    pub mark: Option<f64>,
    /// Who opened it: the strategy's name, `manual`, `watcher`, …
    pub opened_by: String,
    /// Why it ended: `signal`, `stop`, `take_profit`, `trailing`, `crash`, `delisted`,
    /// `manual`, `risk`, … (`None` while open).
    pub exit_reason: Option<String>,
}

/// Summary of closed round trips.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TradeStats {
    pub closed: usize,
    pub open: usize,
    pub wins: usize,
    pub losses: usize,
    /// Share of closed trades with a positive profit (`0` with none).
    pub win_rate: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    /// Gross profit over gross loss (`0` without losses).
    pub profit_factor: f64,
    /// Average profit per closed trade.
    pub expectancy: f64,
    pub total_pnl: f64,
    pub avg_days_held: f64,
    pub best: f64,
    pub worst: f64,
    /// Closed trades per exit reason.
    pub by_exit: BTreeMap<String, usize>,
}

/// A position being built.
#[derive(Debug)]
struct Open {
    trip: RoundTrip,
    qty: f64,
    exited_qty: f64,
    exit_value: f64,
}

/// Name of whoever caused a fill, as shown in the ledger.
fn actor(decision: &Decision) -> String {
    match decision.strategy.as_str() {
        "suggestion" => "manual".into(),
        "stops" => decision
            .reason
            .as_ref()
            .and_then(|r| r.get("exit"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("stop")
            .to_owned(),
        other => other.to_owned(),
    }
}

/// Why a position ended, given the decision that caused the closing fill.
fn exit_reason(cause: Option<&Decision>) -> String {
    match cause {
        None => "unknown".into(),
        Some(d) if d.target.abs() > DUST && d.strategy != "stops" => "risk".into(),
        Some(d) if d.strategy == "stops" || d.strategy == "suggestion" => actor(d),
        Some(_) => "signal".into(),
    }
}

/// The latest decision for `fill` (see the module docs for the timing rule).
fn cause_of<'a>(decisions: &'a [Decision], fill: &Fill) -> Option<&'a Decision> {
    decisions
        .iter()
        .filter(|d| d.instrument == fill.instrument)
        .filter(|d| d.ts < fill.ts || (d.ts == fill.ts && d.strategy == "stops"))
        .max_by_key(|d| (d.ts, d.strategy == "stops"))
}

fn days(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_seconds() as f64 / 86_400.0
}

impl Open {
    fn start(fill: &Fill, qty: f64, fee: f64, cause: Option<&Decision>) -> Self {
        Self {
            trip: RoundTrip {
                instrument: fill.instrument.clone(),
                direction: qty.signum(),
                opened: fill.ts,
                closed: None,
                entry_price: fill.price,
                exit_price: None,
                max_qty: qty.abs(),
                max_notional: qty.abs() * fill.price,
                fees: fee,
                pnl: -fee,
                return_pct: 0.0,
                days_held: 0.0,
                mark: None,
                opened_by: cause.map_or_else(|| "unknown".into(), actor),
                exit_reason: None,
            },
            qty,
            exited_qty: 0.0,
            exit_value: 0.0,
        }
    }

    fn add(&mut self, fill: &Fill, qty: f64, fee: f64) {
        let total = self.qty.abs() + qty.abs();
        self.trip.entry_price = self
            .trip
            .entry_price
            .mul_add(self.qty.abs(), fill.price * qty.abs())
            / total;
        self.qty += qty;
        self.trip.fees += fee;
        self.trip.pnl -= fee;
        if self.qty.abs() > self.trip.max_qty {
            self.trip.max_qty = self.qty.abs();
            self.trip.max_notional = self.trip.max_qty * self.trip.entry_price;
        }
    }

    /// Reduce by `qty` (positive, at most the position).
    fn reduce(&mut self, fill: &Fill, qty: f64, fee: f64) {
        self.trip.pnl +=
            (self.trip.direction * (fill.price - self.trip.entry_price)).mul_add(qty, -fee);
        self.trip.fees += fee;
        self.exited_qty += qty;
        self.exit_value = qty.mul_add(fill.price, self.exit_value);
        self.trip.exit_price = Some(self.exit_value / self.exited_qty);
        self.qty = self.trip.direction.mul_add(-qty, self.qty);
    }

    fn finish(mut self, closed: Option<DateTime<Utc>>, now: DateTime<Utc>) -> RoundTrip {
        self.trip.closed = closed;
        self.trip.days_held = days(self.trip.opened, closed.unwrap_or(now));
        if self.trip.max_notional > 0.0 {
            self.trip.return_pct = self.trip.pnl / self.trip.max_notional;
        }
        self.trip
    }
}

/// Round trips from `fills` (time order) and the `decisions` behind them; open positions are
/// valued at `marks` (last prices) as of `now`. Oldest trade first.
pub fn round_trips(
    fills: &[Fill],
    decisions: &[Decision],
    marks: &BTreeMap<InstrumentId, f64>,
    now: DateTime<Utc>,
) -> Vec<RoundTrip> {
    let mut open: BTreeMap<InstrumentId, Open> = BTreeMap::new();
    let mut done = Vec::new();
    for fill in fills.iter().filter(|f| f.qty > DUST) {
        let signed = match fill.side {
            Side::Buy => fill.qty,
            Side::Sell => -fill.qty,
        };
        let cause = cause_of(decisions, fill);
        let Some(mut position) = open.remove(&fill.instrument) else {
            open.insert(
                fill.instrument.clone(),
                Open::start(fill, signed, fill.fee, cause),
            );
            continue;
        };
        if signed.signum() * position.trip.direction > 0.0 {
            position.add(fill, signed, fill.fee);
            open.insert(fill.instrument.clone(), position);
            continue;
        }
        let closing = signed.abs().min(position.qty.abs());
        let share = closing / signed.abs();
        position.reduce(fill, closing, fill.fee * share);
        if position.qty.abs() > DUST {
            open.insert(fill.instrument.clone(), position);
            continue;
        }
        position.trip.exit_reason = Some(exit_reason(cause));
        done.push(position.finish(Some(fill.ts), now));
        let rest = signed.abs() - closing;
        if rest > DUST {
            let fee = fill.fee * (1.0 - share);
            let flipped = Open::start(fill, signed.signum() * rest, fee, cause);
            open.insert(fill.instrument.clone(), flipped);
        }
    }
    for (instrument, mut position) in open {
        if let Some(&mark) = marks.get(&instrument) {
            let gain = position.trip.direction * (mark - position.trip.entry_price);
            position.trip.pnl = gain.mul_add(position.qty.abs(), position.trip.pnl);
            position.trip.mark = Some(mark);
        }
        done.push(position.finish(None, now));
    }
    done.sort_by(|a, b| {
        a.opened
            .cmp(&b.opened)
            .then_with(|| a.instrument.cmp(&b.instrument))
    });
    done
}

/// Statistics over the closed trades in `trips`.
pub fn stats(trips: &[RoundTrip]) -> TradeStats {
    let closed: Vec<&RoundTrip> = trips.iter().filter(|t| t.closed.is_some()).collect();
    let mut out = TradeStats {
        closed: closed.len(),
        open: trips.len() - closed.len(),
        ..TradeStats::default()
    };
    if closed.is_empty() {
        return out;
    }
    let n = closed.len() as f64;
    let (mut gross_win, mut gross_loss) = (0.0, 0.0);
    for t in &closed {
        if t.pnl > 0.0 {
            out.wins += 1;
            gross_win += t.pnl;
        } else if t.pnl < 0.0 {
            out.losses += 1;
            gross_loss -= t.pnl;
        }
        let reason = t.exit_reason.clone().unwrap_or_else(|| "unknown".into());
        *out.by_exit.entry(reason).or_default() += 1;
    }
    out.total_pnl = closed.iter().map(|t| t.pnl).sum();
    out.win_rate = out.wins as f64 / n;
    if out.wins > 0 {
        out.avg_win = gross_win / out.wins as f64;
    }
    if out.losses > 0 {
        out.avg_loss = -gross_loss / out.losses as f64;
    }
    if gross_loss > 0.0 {
        out.profit_factor = gross_win / gross_loss;
    }
    out.expectancy = out.total_pnl / n;
    out.avg_days_held = closed.iter().map(|t| t.days_held).sum::<f64>() / n;
    out.best = closed
        .iter()
        .map(|t| t.pnl)
        .fold(f64::NEG_INFINITY, f64::max);
    out.worst = closed.iter().map(|t| t.pnl).fold(f64::INFINITY, f64::min);
    out
}

#[cfg(test)]
mod tests;
