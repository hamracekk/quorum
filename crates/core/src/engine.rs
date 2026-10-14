//! Event-driven trading engine: one code path for backtests, replay and (from M3) live
//! paper trading.
//!
//! Bars are fed one at a time, in time order. For each timestamp the engine:
//!
//! 1. executes any queued target for the bar's instrument at the bar's **open**,
//! 2. marks the position at the bar's close and asks the strategy for a new target,
//! 3. once every bar of the timestamp has arrived (the next timestamp starts, or
//!    [`finish`](Engine::finish) is called): records equity, updates risk, and queues the
//!    risk-adjusted targets for each instrument's **next** bar.
//!
//! So a decision made on a bar can never trade on that bar's prices. Trades happen only when
//! a risk-adjusted target actually changes, so there is no churn from price drift.
//!
//! Execution goes through the [`Broker`] port: a target becomes a market [`Order`], the
//! broker's [`ExecutionReport`]s move it through the order state machine, and only reported
//! fills change the [`Portfolio`]. Backtests and paper sessions use the [`PaperBroker`].

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::backtest::{BacktestConfig, BacktestResult};
use crate::broker::{Broker, PaperBroker, Portfolio};
use crate::derivatives::FundingPoint;
use crate::events::Event;
use crate::instrument::InstrumentId;
use crate::metrics::Report;
use crate::order::{ExecutionReport, Order, OrderBook, OrderError, OrderStatus};
use crate::risk::RiskManager;
use crate::stops::{Atr, OpenTrade};
use crate::strategy::Strategy;
use crate::types::{Bar, EquityPoint, Fill};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error("bar for {instrument} at {ts} arrived after {latest}; bars must be in time order")]
    OutOfOrder {
        instrument: InstrumentId,
        ts: DateTime<Utc>,
        latest: DateTime<Utc>,
    },
    /// The broker reported something the order state machine refuses: the local view of
    /// orders no longer matches the venue, so trading must stop.
    #[error("broker `{broker}`: {source}")]
    Order {
        broker: String,
        #[source]
        source: OrderError,
    },
}

/// One instrument's paper/sim position as seen by the engine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PositionSnapshot {
    pub instrument: InstrumentId,
    pub qty: f64,
    /// Latest close used to value the position.
    pub mark: f64,
    pub value: f64,
    /// Risk-adjusted target currently held (fraction of equity).
    pub target: f64,
    /// The open trade's stop and take-profit levels (T-2), when stops are on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<f64>,
    /// Who holds the position when it is not the strategy, e.g. `suggestion #3 by scout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<String>,
}

/// An operator-approved trade on one instrument (an accepted suggestion, T-3): it replaces the
/// strategy's target there until the trade closes, with its own stop and take-profit.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualTrade {
    /// Fraction of equity; `0` closes the position.
    pub target: f64,
    /// Stop and take-profit distances as fractions of the entry price.
    pub stop_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    pub label: String,
}

/// An instrument kept flat after a stop or take-profit until the strategy changes its mind.
#[derive(Debug, Clone, Copy)]
struct Blocked {
    /// The strategy's target when the exit happened.
    desired: f64,
    /// Bars since the exit.
    bars: usize,
}

/// Point-in-time state of an engine: what a dashboard or state file needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineSnapshot {
    /// Timestamp of the latest bar fed in.
    pub ts: Option<DateTime<Utc>>,
    pub cash: f64,
    pub equity: f64,
    pub halted: bool,
    /// Share of the targets the portfolio brakes keep (E-5); `1` = no brake.
    pub risk_scale: f64,
    /// True after the kill switch: positions flattened, strategy ignored.
    pub killed: bool,
    pub positions: Vec<PositionSnapshot>,
    /// Targets queued for each instrument's next bar open.
    pub pending: Vec<(InstrumentId, f64)>,
    /// Orders handed to the broker and not yet filled, canceled or rejected.
    pub open_orders: Vec<Order>,
}

#[derive(Debug)]
pub struct Engine<'s> {
    strategy: &'s mut dyn Strategy,
    cfg: BacktestConfig,
    measure_from: Option<DateTime<Utc>>,
    portfolio: Portfolio,
    /// Equity at the previous timestamp's close: what every order of the current timestamp
    /// is sized against (feed order and same-bar closes never change a size).
    sizing_equity: f64,
    broker: Box<dyn Broker>,
    orders: OrderBook,
    risk: RiskManager,
    benchmark: Benchmark,
    desired: BTreeMap<InstrumentId, f64>,
    current: BTreeMap<InstrumentId, f64>,
    pending: BTreeMap<InstrumentId, f64>,
    open_ts: Option<DateTime<Utc>>,
    killed: bool,
    fills: Vec<Fill>,
    equity_curve: Vec<EquityPoint>,
    benchmark_curve: Vec<EquityPoint>,
    atr: BTreeMap<InstrumentId, Atr>,
    trades: BTreeMap<InstrumentId, OpenTrade>,
    blocked: BTreeMap<InstrumentId, Blocked>,
    manual: BTreeMap<InstrumentId, ManualTrade>,
    /// Latest close per instrument and this timestamp's log returns, for the market
    /// volatility the portfolio brake watches (only kept when that brake is on).
    closes: BTreeMap<InstrumentId, f64>,
    stamp_returns: Vec<f64>,
    /// The latest measured close per instrument (the crash stop's reference).
    last_close: BTreeMap<InstrumentId, f64>,
    /// Timestamps in a row without a bar, per held instrument (delisting, R-1).
    missed: BTreeMap<InstrumentId, usize>,
    /// Instruments with a bar in the current timestamp.
    seen: std::collections::BTreeSet<InstrumentId>,
    /// Perpetual funding still to settle, per instrument, oldest first.
    funding: BTreeMap<InstrumentId, std::collections::VecDeque<FundingPoint>>,
    /// Net funding paid so far (negative: received).
    funding_paid: f64,
}

impl<'s> Engine<'s> {
    /// `universe` is the set of instruments the benchmark splits capital across (equal
    /// weight, in this order). Bars before `measure_from` only warm up the strategy: nothing
    /// is executed or recorded for them.
    /// Orders are filled by a [`PaperBroker`] with the configured fee and slippage.
    pub fn new(
        strategy: &'s mut dyn Strategy,
        cfg: BacktestConfig,
        universe: &[InstrumentId],
        measure_from: Option<DateTime<Utc>>,
    ) -> Self {
        let broker = PaperBroker::new(cfg.fee_rate, cfg.slippage_bps).with_impact(cfg.impact);
        Self::with_broker(strategy, cfg, universe, measure_from, Box::new(broker))
    }

    /// Like [`new`](Self::new), but orders go to `broker`.
    pub fn with_broker(
        strategy: &'s mut dyn Strategy,
        cfg: BacktestConfig,
        universe: &[InstrumentId],
        measure_from: Option<DateTime<Utc>>,
        broker: Box<dyn Broker>,
    ) -> Self {
        Self {
            strategy,
            measure_from,
            portfolio: if cfg.risk.allow_short {
                Portfolio::new(cfg.initial_cash)
            } else {
                // Long-only is a spot account: no buying on credit.
                Portfolio::new(cfg.initial_cash).spot(cfg.fee_rate + cfg.slippage_bps / 10_000.0)
            },
            sizing_equity: cfg.initial_cash,
            broker,
            orders: OrderBook::default(),
            risk: RiskManager::new(cfg.risk),
            benchmark: Benchmark::new(universe, cfg.initial_cash),
            desired: BTreeMap::new(),
            current: BTreeMap::new(),
            pending: BTreeMap::new(),
            open_ts: None,
            killed: false,
            fills: Vec::new(),
            equity_curve: Vec::new(),
            benchmark_curve: Vec::new(),
            atr: BTreeMap::new(),
            trades: BTreeMap::new(),
            blocked: BTreeMap::new(),
            manual: BTreeMap::new(),
            closes: BTreeMap::new(),
            stamp_returns: Vec::new(),
            funding: BTreeMap::new(),
            funding_paid: 0.0,
            last_close: BTreeMap::new(),
            missed: BTreeMap::new(),
            seen: std::collections::BTreeSet::new(),
            // Last: the fields above read it first.
            cfg,
        }
    }

    /// Feed one closed bar; returns the decision events it caused (fills of queued targets,
    /// signals, and — when this bar starts a new timestamp — the previous timestamp's
    /// targets and risk halts).
    pub fn on_bar(
        &mut self,
        instrument: &InstrumentId,
        bar: &Bar,
    ) -> Result<Vec<Event>, EngineError> {
        let mut out = Vec::new();
        let latest = self.open_ts;
        match latest {
            Some(latest) if bar.ts < latest => {
                return Err(EngineError::OutOfOrder {
                    instrument: instrument.clone(),
                    ts: bar.ts,
                    latest,
                });
            }
            Some(latest) if bar.ts > latest => {
                self.close_timestamp(latest, &mut out);
                self.settle_funding(bar.ts);
                self.sizing_equity = self.portfolio.equity();
            }
            None => self.sizing_equity = self.portfolio.equity(),
            _ => {}
        }
        self.open_ts = Some(bar.ts);
        self.seen.insert(instrument.clone());
        self.missed.remove(instrument);
        self.drain(&mut out)?;

        if self.measuring(bar.ts) {
            if let Some(target) = self.pending.remove(instrument) {
                let before = self.portfolio.position(instrument);
                self.current.insert(instrument.clone(), target);
                self.execute(instrument, bar.ts, bar.open, target, &mut out)?;
                self.track_trade(instrument, before);
            }
            self.check_crash(instrument, bar, &mut out)?;
            self.check_exit(instrument, bar, &mut out)?;
            self.portfolio.mark(instrument, bar.close);
            self.last_close.insert(instrument.clone(), bar.close);
            self.benchmark.observe(instrument, bar);
        }
        self.observe_bar(instrument, bar);
        // Known from this bar's close on: the next fills are sized against it.
        self.broker.liquidity(instrument, bar.close * bar.volume);

        if self.killed {
            return Ok(out);
        }
        if let Some(target) = self.strategy.on_bar(instrument, bar) {
            let changed = self
                .desired
                .insert(instrument.clone(), target)
                .is_none_or(|previous| (previous - target).abs() > f64::EPSILON);
            if changed {
                out.push(Event::Signal {
                    ts: bar.ts,
                    instrument: instrument.clone(),
                    strategy: self.strategy.name().to_owned(),
                    target,
                    reason: self.strategy.explain(instrument),
                });
            }
        }
        Ok(out)
    }

    /// Apply an operator-approved trade on `instrument` (T-3): from the next timestamp its target
    /// replaces the strategy's there, until the trade is closed (by its stop, take-profit or a
    /// close). Returns the decision event to record.
    pub fn manual_trade(&mut self, instrument: &InstrumentId, trade: ManualTrade) -> Vec<Event> {
        if self.killed {
            return Vec::new();
        }
        self.blocked.remove(instrument);
        let event = Event::Signal {
            ts: self.open_ts.unwrap_or_default(),
            instrument: instrument.clone(),
            strategy: "suggestion".into(),
            target: trade.target,
            reason: Some(serde_json::json!({
                "manual": trade.label, "stop_pct": trade.stop_pct,
                "take_profit_pct": trade.take_profit_pct,
            })),
        };
        self.manual.insert(instrument.clone(), trade);
        vec![event]
    }

    /// Move the stop of the open position on `instrument` closer to the price (T-5): risk only
    /// goes down — for a long the new stop must be above the current one and below the latest
    /// price, for a short the reverse. A position without a stop gets one. Returns the decision
    /// event, or why the move was refused.
    /// Cap every target at `scale` (`0..=0.95`) until `until` — a market-wide risk-reducing
    /// control (`reduce_exposure`); takes effect at the next timestamp, like every target.
    pub fn reduce_exposure(
        &mut self,
        scale: f64,
        until: DateTime<Utc>,
        label: &str,
    ) -> Result<Vec<Event>, String> {
        if !(0.0..=0.95).contains(&scale) {
            return Err(format!("scale {scale} must be between 0 and 0.95"));
        }
        self.risk.cap(scale, until);
        tracing::info!(scale, %until, label, "exposure capped");
        Ok(Vec::new())
    }

    pub fn tighten_stop(
        &mut self,
        instrument: &InstrumentId,
        stop: f64,
        label: &str,
    ) -> Result<Vec<Event>, String> {
        let qty = self.portfolio.position(instrument);
        if self.killed || qty.abs() <= f64::EPSILON {
            return Err(format!("no open position on {instrument}"));
        }
        let mark = self
            .portfolio
            .mark_of(instrument)
            .ok_or_else(|| format!("no price for {instrument} yet"))?;
        let direction = qty.signum();
        if !(stop.is_finite() && stop > 0.0) || direction * (mark - stop) <= 0.0 {
            return Err(format!(
                "a stop at {stop} would trigger at once (latest price {mark})"
            ));
        }
        let previous = self.trades.get(instrument).map(|t| t.stop);
        if previous.is_some_and(|p| direction * (stop - p) <= 0.0) {
            return Err(format!(
                "{stop} is not tighter than the current stop {}",
                previous.unwrap_or_default()
            ));
        }
        let trade = self
            .trades
            .entry(instrument.clone())
            .or_insert_with(|| OpenTrade {
                direction,
                entry: mark,
                distance: (mark - stop).abs(),
                stop,
                take_profit: None,
                best: mark,
                trailed: false,
            });
        trade.stop = stop;
        trade.trailed = true;
        Ok(vec![Event::Signal {
            ts: self.open_ts.unwrap_or_default(),
            instrument: instrument.clone(),
            strategy: "watcher".into(),
            target: self.current.get(instrument).copied().unwrap_or(0.0),
            reason: Some(serde_json::json!({
                "tightened_stop": stop, "previous_stop": previous, "by": label,
            })),
        }])
    }

    /// Perpetual funding for `instrument` (settled as cash at each funding time on the position
    /// held then: longs pay a positive rate, shorts receive it).
    pub fn add_funding(&mut self, instrument: &InstrumentId, points: &[FundingPoint]) {
        let queue = self.funding.entry(instrument.clone()).or_default();
        queue.extend(points.iter().copied());
        queue.make_contiguous().sort_by_key(|p| p.ts);
    }

    /// Net funding paid so far (negative: received).
    pub const fn funding_paid(&self) -> f64 {
        self.funding_paid
    }

    /// Settle every funding print up to `now` on the positions held through it.
    fn settle_funding(&mut self, now: DateTime<Utc>) {
        let measuring = self.measuring(now);
        for (instrument, queue) in &mut self.funding {
            while queue.front().is_some_and(|p| p.ts <= now) {
                let Some(point) = queue.pop_front() else {
                    break;
                };
                if !measuring {
                    continue;
                }
                let qty = self.portfolio.position(instrument);
                if qty > 0.0 && !self.cfg.perp_longs {
                    continue; // a spot long pays no funding
                }
                let Some(mark) = self.portfolio.mark_of(instrument) else {
                    continue;
                };
                let paid = qty * mark * point.rate;
                if paid.abs() > 0.0 {
                    self.portfolio.adjust_cash(-paid);
                    self.funding_paid += paid;
                }
            }
        }
    }

    /// Pass external calls (live news readings, …) to the strategy.
    pub fn inform(&mut self, calls: &[crate::strategy::ExternalCall]) {
        self.strategy.inform(calls);
    }

    /// Current state: cash, equity, positions valued at their latest marks, pending targets.
    pub fn snapshot(&self) -> EngineSnapshot {
        let positions = self
            .portfolio
            .positions()
            .filter(|(_, qty)| qty.abs() > f64::EPSILON)
            .map(|(instrument, qty)| {
                let mark = self.portfolio.mark_of(instrument).unwrap_or(0.0);
                PositionSnapshot {
                    instrument: instrument.clone(),
                    qty,
                    mark,
                    value: qty * mark,
                    target: self.current.get(instrument).copied().unwrap_or(0.0),
                    stop: self.trades.get(instrument).map(|t| t.stop),
                    take_profit: self.trades.get(instrument).and_then(|t| t.take_profit),
                    manual: self.manual.get(instrument).map(|m| m.label.clone()),
                }
            })
            .collect();
        EngineSnapshot {
            ts: self.open_ts,
            cash: self.portfolio.cash(),
            equity: self.portfolio.equity(),
            halted: self.risk.halted(),
            risk_scale: self.risk.scale(),
            killed: self.killed,
            positions,
            pending: self
                .pending
                .iter()
                .map(|(instrument, target)| (instrument.clone(), *target))
                .collect(),
            open_orders: self.orders.open().cloned().collect(),
        }
    }

    /// Kill switch: cancel working orders, sell/cover every position **now** at its latest
    /// mark (with slippage and fees), drop all targets, and ignore the strategy from here on.
    /// Returns the resulting order and fill events.
    pub fn flatten(&mut self, ts: DateTime<Utc>) -> Result<Vec<Event>, EngineError> {
        self.killed = true;
        self.desired.clear();
        self.pending.clear();
        self.trades.clear();
        self.manual.clear();
        let mut out = Vec::new();
        let working: Vec<_> = self.orders.open().map(|order| order.id).collect();
        for id in working {
            if let Err(err) = self.broker.cancel(id, ts) {
                tracing::warn!(order = %id, error = %err, "kill switch: cancel failed");
            }
        }
        self.drain(&mut out)?;

        let open: Vec<(InstrumentId, f64)> = self
            .portfolio
            .positions()
            .filter(|(_, qty)| qty.abs() > f64::EPSILON)
            .filter_map(|(instrument, _)| {
                self.portfolio
                    .mark_of(instrument)
                    .map(|mark| (instrument.clone(), mark))
            })
            .collect();
        for (instrument, mark) in open {
            self.current.insert(instrument.clone(), 0.0);
            self.execute(&instrument, ts, mark, 0.0, &mut out)?;
        }
        Ok(out)
    }

    /// Close the last timestamp and produce the result, plus the final decision events.
    pub fn finish(mut self) -> (BacktestResult, Vec<Event>) {
        let mut out = Vec::new();
        if let Some(latest) = self.open_ts.take() {
            self.close_timestamp(latest, &mut out);
        }
        let mut report = Report::compute(
            self.cfg.initial_cash,
            &self.equity_curve,
            &self.benchmark_curve,
            &self.fills,
        );
        report.funding = self.funding_paid;
        let result = BacktestResult {
            equity_curve: self.equity_curve,
            benchmark_curve: self.benchmark_curve,
            fills: self.fills,
            report,
            funding: self.funding_paid,
        };
        (result, out)
    }

    /// Trade `instrument` toward `target` at `price`: size a market order against the
    /// portfolio, hand it to the broker and apply what the broker reports.
    fn execute(
        &mut self,
        instrument: &InstrumentId,
        ts: DateTime<Utc>,
        price: f64,
        target: f64,
        out: &mut Vec<Event>,
    ) -> Result<(), EngineError> {
        self.broker.quote(instrument, ts, price);
        let Some((side, qty)) =
            self.portfolio
                .order_for_equity(instrument, price, target, self.sizing_equity)
        else {
            return Ok(());
        };
        let order = self.orders.create(ts, instrument, side, qty);
        let submitted = self.broker.submit(&order);
        self.drain(out)?;
        if let Err(err) = submitted {
            let rejected = ExecutionReport::Rejected {
                id: order.id,
                ts,
                reason: format!("not submitted: {err}"),
            };
            self.apply_report(&rejected, out)?;
        }
        Ok(())
    }

    /// Apply every report the broker has produced since the last call.
    fn drain(&mut self, out: &mut Vec<Event>) -> Result<(), EngineError> {
        for report in self.broker.poll() {
            self.apply_report(&report, out)?;
        }
        Ok(())
    }

    /// Move the order through its state machine; book fills; emit `fill` and `order` events.
    /// A rejected or canceled order forgets the instrument's held target so the next
    /// timestamp queues it again.
    fn apply_report(
        &mut self,
        report: &ExecutionReport,
        out: &mut Vec<Event>,
    ) -> Result<(), EngineError> {
        let order = self
            .orders
            .apply(report)
            .map_err(|source| EngineError::Order {
                broker: self.broker.name().to_owned(),
                source,
            })?
            .clone();
        if let ExecutionReport::Filled { fill, .. } = report {
            self.portfolio.apply(fill);
            out.push(Event::Fill(fill.clone()));
            self.fills.push(fill.clone());
        }
        if matches!(order.status, OrderStatus::Rejected | OrderStatus::Canceled) {
            tracing::warn!(order = %order.id, instrument = %order.instrument, status = %order.status, reason = order.reason.as_deref().unwrap_or(""), "order not executed");
            self.current.remove(&order.instrument);
        }
        out.push(Event::Order(order));
        Ok(())
    }

    /// After a target was executed: open, keep or forget the instrument's trade (T-2).
    fn track_trade(&mut self, instrument: &InstrumentId, before: f64) {
        let manual = self.manual.get(instrument).cloned();
        let mut rules = self.cfg.stops;
        if let Some(m) = &manual {
            // A manual trade brings its own levels (no stop: none, whatever `[stops]` says).
            rules = crate::stops::StopRules {
                stop_pct: m.stop_pct.unwrap_or(0.0),
                stop_atr: 0.0,
                take_profit_r: match (m.stop_pct, m.take_profit_pct) {
                    (Some(stop), Some(tp)) if stop > 0.0 => tp / stop,
                    _ => 0.0,
                },
                trailing: false,
                ..rules
            };
        }
        let after = self.portfolio.position(instrument);
        if after.abs() <= f64::EPSILON {
            self.trades.remove(instrument);
            if manual.is_some_and(|m| m.target.abs() <= f64::EPSILON) {
                // A manual close is done: back to the strategy once it changes its mind.
                self.release_manual(instrument);
            }
            return;
        }
        if !rules.active() {
            return;
        }
        let opened = before.abs() <= f64::EPSILON || before.signum() != after.signum();
        if !opened {
            return;
        }
        let entry = self
            .fills
            .last()
            .filter(|f| f.instrument == *instrument)
            .map(|f| f.price);
        let atr = self.atr.get(instrument).and_then(Atr::value);
        match entry.and_then(|price| rules.distance(price, atr).map(|d| (price, d))) {
            Some((price, distance)) => {
                self.trades.insert(
                    instrument.clone(),
                    OpenTrade::open(&rules, after.signum(), price, distance),
                );
            }
            None => {
                self.trades.remove(instrument);
            }
        }
    }

    /// Exit at a stop or take-profit hit by `bar`; otherwise let a trailing stop follow it.
    fn check_exit(
        &mut self,
        instrument: &InstrumentId,
        bar: &Bar,
        out: &mut Vec<Event>,
    ) -> Result<(), EngineError> {
        let Some(trade) = self.trades.get(instrument).copied() else {
            return Ok(());
        };
        if self.portfolio.position(instrument).abs() <= f64::EPSILON {
            self.trades.remove(instrument);
            return Ok(());
        }
        let Some((kind, price)) = trade.check(bar) else {
            if let Some(open) = self.trades.get_mut(instrument) {
                open.trail(&self.cfg.stops, bar);
            }
            return Ok(());
        };
        self.trades.remove(instrument);
        self.pending.remove(instrument);
        self.current.insert(instrument.clone(), 0.0);
        self.release_manual(instrument);
        out.push(Event::Signal {
            ts: bar.ts,
            instrument: instrument.clone(),
            strategy: "stops".into(),
            target: 0.0,
            reason: Some(serde_json::json!({
                "exit": kind, "price": price, "entry": trade.entry, "stop": trade.stop,
                "take_profit": trade.take_profit, "direction": trade.direction,
            })),
        });
        self.execute(instrument, bar.ts, price, 0.0, out)
    }

    /// The crash stop (`[stops] crash_pct`): a position whose price moved `crash_pct` against it
    /// from the previous close is closed at that level (at the open if it gapped through).
    fn check_crash(
        &mut self,
        instrument: &InstrumentId,
        bar: &Bar,
        out: &mut Vec<Event>,
    ) -> Result<(), EngineError> {
        let pct = self.cfg.stops.crash_pct;
        let qty = self.portfolio.position(instrument);
        let Some(prev) = self.last_close.get(instrument).copied() else {
            return Ok(());
        };
        if pct <= 0.0 || qty.abs() <= f64::EPSILON || prev <= 0.0 {
            return Ok(());
        }
        let price = if qty > 0.0 {
            let level = prev * (1.0 - pct);
            (bar.low <= level).then(|| bar.open.min(level))
        } else {
            let level = prev * (1.0 + pct);
            (bar.high >= level).then(|| bar.open.max(level))
        };
        let Some(price) = price else {
            return Ok(());
        };
        self.trades.remove(instrument);
        self.pending.remove(instrument);
        self.current.insert(instrument.clone(), 0.0);
        self.release_manual(instrument);
        out.push(Event::Signal {
            ts: bar.ts,
            instrument: instrument.clone(),
            strategy: "stops".into(),
            target: 0.0,
            reason: Some(serde_json::json!({
                "exit": "crash", "price": price, "previous_close": prev, "crash_pct": pct,
            })),
        });
        self.execute(instrument, bar.ts, price, 0.0, out)
    }

    /// The manual trade on `instrument` is over: the strategy takes over again, but stays flat
    /// until it changes its target (so it does not undo the exit at once).
    fn release_manual(&mut self, instrument: &InstrumentId) {
        self.manual.remove(instrument);
        let desired = self.desired.get(instrument).copied().unwrap_or(0.0);
        self.blocked
            .insert(instrument.clone(), Blocked { desired, bars: 0 });
    }

    /// Track the bars since an exit, and the average true range when stops are on.
    fn observe_bar(&mut self, instrument: &InstrumentId, bar: &Bar) {
        if let Some(blocked) = self.blocked.get_mut(instrument) {
            blocked.bars += 1;
        }
        if self.cfg.risk.portfolio.vol_on()
            && bar.close > 0.0
            && let Some(prev) = self.closes.insert(instrument.clone(), bar.close)
            && prev > 0.0
        {
            self.stamp_returns.push((bar.close / prev).ln());
        }
        let rules = self.cfg.stops;
        if !rules.active() {
            return;
        }
        self.atr
            .entry(instrument.clone())
            .or_insert_with(|| Atr::new(rules.atr_bars))
            .push(bar);
    }

    /// A risk-adjusted target under the stop rules: flat while blocked after an exit, sized by
    /// risk per trade on entry, and never grown while a trade is open (no drift in size).
    fn stop_adjusted(&mut self, instrument: &InstrumentId, target: f64) -> f64 {
        let rules = self.cfg.stops;
        if self.manual.contains_key(instrument) {
            return target; // sized and stopped by the operator's approval
        }
        if let Some(blocked) = self.blocked.get(instrument).copied() {
            let desired = self.desired.get(instrument).copied().unwrap_or(0.0);
            let changed = (desired - blocked.desired).abs() > f64::EPSILON;
            let cooled = rules.cooldown_bars > 0 && blocked.bars >= rules.cooldown_bars;
            if changed || cooled {
                self.blocked.remove(instrument);
            } else {
                return 0.0;
            }
        }
        if !rules.active() {
            return target;
        }
        let held = self.current.get(instrument).copied().unwrap_or(0.0);
        if self.trades.contains_key(instrument)
            && held.abs() > f64::EPSILON
            && held.signum() == target.signum()
        {
            return target.signum() * target.abs().min(held.abs());
        }
        let Some(mark) = self.portfolio.mark_of(instrument) else {
            return target;
        };
        let distance = rules.distance(mark, self.atr.get(instrument).and_then(Atr::value));
        rules.size(target, mark, distance)
    }

    fn measuring(&self, ts: DateTime<Utc>) -> bool {
        self.measure_from.is_none_or(|start| ts >= start)
    }

    /// Sell positions in instruments that stopped trading (`[stops] stale_bars` timestamps in a
    /// row without a bar) at their last close less the haircut.
    fn exit_delisted(&mut self, ts: DateTime<Utc>, out: &mut Vec<Event>) {
        let rules = self.cfg.stops;
        let seen = std::mem::take(&mut self.seen);
        if rules.stale_bars == 0 || !self.measuring(ts) {
            return;
        }
        let held: Vec<InstrumentId> = self
            .portfolio
            .positions()
            .filter(|(id, qty)| qty.abs() > f64::EPSILON && !seen.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        let weekend = matches!(
            chrono::Datelike::weekday(&ts),
            chrono::Weekday::Sat | chrono::Weekday::Sun
        );
        for instrument in held {
            // A stock has no weekend bars: those days are the calendar, not a delisting.
            if weekend && !instrument.kind().trades_weekends() {
                continue;
            }
            let missed = self.missed.entry(instrument.clone()).or_default();
            *missed += 1;
            if *missed < rules.stale_bars {
                continue;
            }
            let Some(last) = self.last_close.get(&instrument).copied() else {
                continue;
            };
            let price = last * (1.0 - rules.stale_haircut.clamp(0.0, 1.0));
            self.trades.remove(&instrument);
            self.pending.remove(&instrument);
            self.current.insert(instrument.clone(), 0.0);
            out.push(Event::Signal {
                ts,
                instrument: instrument.clone(),
                strategy: "stops".into(),
                target: 0.0,
                reason: Some(serde_json::json!({
                    "exit": "delisted", "last_close": last, "haircut": rules.stale_haircut,
                })),
            });
            if let Err(err) = self.execute(&instrument, ts, price, 0.0, out) {
                tracing::warn!(%instrument, error = %err, "delisted position not sold");
            }
        }
    }

    fn close_timestamp(&mut self, ts: DateTime<Utc>, out: &mut Vec<Event>) {
        self.exit_delisted(ts, out);
        if !self.stamp_returns.is_empty() {
            let mean = self.stamp_returns.iter().sum::<f64>() / self.stamp_returns.len() as f64;
            self.risk.observe_market(mean);
            self.stamp_returns.clear();
        }
        if self.measuring(ts) {
            let equity = self.portfolio.equity();
            let was_halted = self.risk.halted();
            self.risk.update(ts, equity);
            self.risk.observe_equity(equity);
            if self.risk.halted() && !was_halted {
                out.push(Event::RiskHalt { ts, equity });
            }
            self.equity_curve.push(EquityPoint { ts, equity });
            self.benchmark_curve.push(EquityPoint {
                ts,
                equity: self.benchmark.value(),
            });
        }

        if self.killed {
            return;
        }
        // A close asked for while already flat is done at once.
        let done: Vec<InstrumentId> = self
            .manual
            .iter()
            .filter(|(i, m)| {
                m.target.abs() <= f64::EPSILON
                    && self.portfolio.position(i).abs() <= f64::EPSILON
                    && !self.pending.contains_key(*i)
            })
            .map(|(i, _)| i.clone())
            .collect();
        for instrument in done {
            self.release_manual(&instrument);
        }
        let mut wanted =
            self.cfg
                .sizing
                .apply(&self.desired, &self.current, self.portfolio.equity());
        // Manual trades keep the size they were accepted at.
        for (instrument, trade) in &self.manual {
            wanted.insert(instrument.clone(), trade.target);
        }
        for (instrument, target) in self.risk.apply(&wanted) {
            let target = self.stop_adjusted(&instrument, target);
            let held = self.current.get(&instrument).copied().unwrap_or(0.0);
            if (target - held).abs() > f64::EPSILON {
                let already_queued = self
                    .pending
                    .get(&instrument)
                    .is_some_and(|queued| (queued - target).abs() <= f64::EPSILON);
                if !already_queued {
                    out.push(Event::Target {
                        ts,
                        instrument: instrument.clone(),
                        target,
                    });
                }
                self.pending.insert(instrument, target);
            } else {
                self.pending.remove(&instrument);
            }
        }
    }
}

/// Equal-weight buy & hold over the universe: each instrument gets `1/n` of capital at its
/// first measured open; instruments not yet entered are held as cash. No fees, no risk.
#[derive(Debug)]
struct Benchmark {
    initial: f64,
    weight: f64,
    index: BTreeMap<InstrumentId, usize>,
    entry: Vec<Option<f64>>,
    last: Vec<f64>,
}

impl Benchmark {
    fn new(universe: &[InstrumentId], initial: f64) -> Self {
        let index: BTreeMap<InstrumentId, usize> = universe
            .iter()
            .enumerate()
            .map(|(i, instrument)| (instrument.clone(), i))
            .collect();
        let n = universe.len();
        Self {
            initial,
            weight: if n == 0 { 0.0 } else { 1.0 / n as f64 },
            index,
            entry: vec![None; n],
            last: vec![0.0; n],
        }
    }

    /// Bars of instruments outside the universe are ignored.
    fn observe(&mut self, instrument: &InstrumentId, bar: &Bar) {
        let Some(&i) = self.index.get(instrument) else {
            return;
        };
        if self.entry[i].is_none() && bar.open > 0.0 {
            self.entry[i] = Some(bar.open);
        }
        self.last[i] = bar.close;
    }

    fn value(&self) -> f64 {
        let growth: f64 = self
            .entry
            .iter()
            .zip(&self.last)
            .map(|(entry, last)| self.weight * entry.map_or(1.0, |e| last / e))
            .sum();
        self.initial * growth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::run_with_events;
    use crate::risk::RiskLimits;
    use crate::strategies::BuyAndHold;
    use crate::types::Series;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn id(symbol: &str) -> InstrumentId {
        InstrumentId::spot("test", symbol).expect("valid instrument")
    }

    fn bar(hour: i64, open: f64, close: f64) -> Bar {
        Bar {
            ts: at(hour),
            open,
            high: open.max(close),
            low: open.min(close),
            close,
            volume: 1.0,
        }
    }

    fn cfg() -> BacktestConfig {
        BacktestConfig {
            initial_cash: 1_000.0,
            fee_rate: 0.0,
            slippage_bps: 0.0,
            impact: 0.0,
            risk: RiskLimits {
                max_daily_loss: 1.0,
                ..RiskLimits::default()
            },
            stops: crate::stops::StopRules::default(),
            perp_longs: true,
            sizing: crate::sizing::SizingRules::default(),
        }
    }

    #[test]
    fn rejects_bars_that_go_back_in_time() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        engine
            .on_bar(&id("A"), &bar(2, 1.0, 1.0))
            .expect("first bar");
        let err = engine
            .on_bar(&id("A"), &bar(1, 1.0, 1.0))
            .expect_err("out of order");
        assert!(matches!(err, EngineError::OutOfOrder { .. }));
    }

    #[test]
    fn emits_signal_then_target_then_fill_in_causal_order() {
        let series = [Series::new(
            id("A"),
            vec![
                bar(0, 100.0, 100.0),
                bar(1, 100.0, 110.0),
                bar(2, 110.0, 120.0),
            ],
        )];
        let (_, events) = run_with_events(&mut BuyAndHold::default(), &series, &cfg(), None);

        let kinds: Vec<&str> = events.iter().map(Event::kind).collect();
        assert_eq!(kinds, vec!["signal", "target", "order", "fill", "order"]);
        assert!(events.windows(2).all(|w| w[0].ts() <= w[1].ts()));
        assert_eq!(events[0].ts(), at(0));
        assert_eq!(events[3].ts(), at(1));
        let Event::Order(order) = &events[4] else {
            panic!("expected the filled order, got {events:?}");
        };
        assert_eq!(order.status, OrderStatus::Filled);
        assert_eq!(order.created_at, at(1));
    }

    #[test]
    fn unchanged_targets_are_not_re_emitted() {
        /// Always wants the same exposure.
        #[derive(Debug)]
        struct Constant;
        impl Strategy for Constant {
            fn name(&self) -> &'static str {
                "constant"
            }
            fn on_bar(&mut self, _instrument: &InstrumentId, _bar: &Bar) -> Option<f64> {
                Some(0.5)
            }
        }

        let bars = (0..10).map(|h| bar(h, 100.0, 100.0)).collect();
        let (_, events) =
            run_with_events(&mut Constant, &[Series::new(id("A"), bars)], &cfg(), None);
        let count = |kind: &str| events.iter().filter(|e| e.kind() == kind).count();
        assert_eq!((count("signal"), count("target"), count("fill")), (1, 1, 1));
    }

    #[test]
    fn kill_switch_emits_a_single_risk_halt() {
        let bars = vec![
            bar(0, 100.0, 100.0),
            bar(1, 100.0, 100.0),
            bar(2, 100.0, 80.0),
            bar(3, 80.0, 70.0),
        ];
        let limits = BacktestConfig {
            risk: RiskLimits::default(),
            ..cfg()
        };
        let (_, events) = run_with_events(
            &mut BuyAndHold::default(),
            &[Series::new(id("A"), bars)],
            &limits,
            None,
        );
        let halts: Vec<_> = events.iter().filter(|e| e.kind() == "risk_halt").collect();
        assert_eq!(halts.len(), 1);
        assert_eq!(halts[0].ts(), at(2));
    }

    #[test]
    fn streaming_bars_matches_batch_run() {
        let bars: Vec<Bar> = (0..20)
            .map(|h| bar(h, 100.0 + h as f64, 101.0 + h as f64))
            .collect();
        let series = [Series::new(id("A"), bars.clone())];
        let (batch, batch_events) =
            run_with_events(&mut BuyAndHold::default(), &series, &cfg(), None);

        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        let mut events = Vec::new();
        for b in &bars {
            events.extend(engine.on_bar(&id("A"), b).expect("in order"));
        }
        let (streamed, tail) = engine.finish();
        events.extend(tail);

        assert_eq!(events, batch_events);
        assert_eq!(streamed.equity_curve, batch.equity_curve);
    }

    #[test]
    fn instruments_outside_the_universe_do_not_distort_the_benchmark() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        engine.on_bar(&id("A"), &bar(0, 100.0, 100.0)).expect("bar");
        engine.on_bar(&id("B"), &bar(0, 10.0, 50.0)).expect("bar");
        let (result, _) = engine.finish();
        assert!(result.report.benchmark.total_return.abs() < 1e-9);
    }

    #[test]
    fn snapshot_reports_positions_equity_and_targets() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        engine.on_bar(&id("A"), &bar(0, 100.0, 100.0)).expect("bar");
        engine.on_bar(&id("A"), &bar(1, 100.0, 110.0)).expect("bar");
        let snap = engine.snapshot();

        assert_eq!(snap.ts, Some(at(1)));
        assert_eq!(snap.positions.len(), 1);
        let position = &snap.positions[0];
        assert!((position.qty - 10.0).abs() < 1e-9);
        assert!((position.mark - 110.0).abs() < 1e-9);
        assert!((position.target - 1.0).abs() < 1e-9);
        assert!((snap.equity - 1_100.0).abs() < 1e-9);
        assert!(!snap.killed);
    }

    #[test]
    fn flatten_sells_everything_now_and_ignores_the_strategy_afterwards() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        engine.on_bar(&id("A"), &bar(0, 100.0, 100.0)).expect("bar");
        engine.on_bar(&id("A"), &bar(1, 100.0, 120.0)).expect("bar");

        let events = engine.flatten(at(1)).expect("flatten");
        let fills: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Fill(fill) => Some(fill),
                _ => None,
            })
            .collect();
        assert_eq!(fills.len(), 1);
        let fill = fills[0];
        assert_eq!(fill.side, crate::types::Side::Sell);
        assert!((fill.price - 120.0).abs() < 1e-9, "sold at the latest mark");

        let later = engine.on_bar(&id("A"), &bar(2, 120.0, 130.0)).expect("bar");
        let later_tail = engine.on_bar(&id("A"), &bar(3, 130.0, 140.0)).expect("bar");
        assert!(
            later.is_empty() && later_tail.is_empty(),
            "{later:?} {later_tail:?}"
        );
        let snap = engine.snapshot();
        assert!(snap.killed && snap.positions.is_empty() && snap.pending.is_empty());
        assert!((snap.equity - 1_200.0).abs() < 1e-9);
    }

    /// Venue whose behaviour each test scripts: accept and hold orders, refuse to take them,
    /// or report nonsense.
    #[derive(Debug, Default)]
    struct Scripted {
        mode: Mode,
        reports: Vec<ExecutionReport>,
        submitted: Vec<Order>,
    }

    #[derive(Debug, Default, Clone, Copy)]
    enum Mode {
        /// Accept and never fill.
        #[default]
        Hold,
        /// `submit` fails: the order never reaches the venue.
        Unreachable,
        /// Report a fill for an order it never accepted.
        Bogus,
    }

    impl Broker for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn submit(&mut self, order: &Order) -> Result<(), crate::ports::PortError> {
            self.submitted.push(order.clone());
            match self.mode {
                Mode::Hold => self.reports.push(ExecutionReport::Accepted {
                    id: order.id,
                    ts: order.created_at,
                }),
                Mode::Unreachable => {
                    return Err(crate::ports::PortError::Invalid("venue down".into()));
                }
                Mode::Bogus => self.reports.push(ExecutionReport::Filled {
                    id: order.id,
                    fill: Fill {
                        ts: order.created_at,
                        instrument: order.instrument.clone(),
                        side: order.side,
                        qty: order.qty,
                        price: 1.0,
                        fee: 0.0,
                    },
                }),
            }
            Ok(())
        }

        fn cancel(
            &mut self,
            id: crate::order::OrderId,
            ts: DateTime<Utc>,
        ) -> Result<(), crate::ports::PortError> {
            self.reports.push(ExecutionReport::Canceled {
                id,
                ts,
                reason: "requested".into(),
            });
            Ok(())
        }

        fn poll(&mut self) -> Vec<ExecutionReport> {
            std::mem::take(&mut self.reports)
        }
    }

    fn scripted(mode: Mode) -> Box<dyn Broker> {
        Box::new(Scripted {
            mode,
            ..Scripted::default()
        })
    }

    #[test]
    fn positions_change_only_on_reported_fills() {
        let mut strategy = BuyAndHold::default();
        let mut engine =
            Engine::with_broker(&mut strategy, cfg(), &[id("A")], None, scripted(Mode::Hold));
        engine.on_bar(&id("A"), &bar(0, 100.0, 100.0)).expect("bar");
        let events = engine.on_bar(&id("A"), &bar(1, 100.0, 110.0)).expect("bar");

        assert!(events.iter().all(|e| e.kind() != "fill"), "{events:?}");
        let snap = engine.snapshot();
        assert!(snap.positions.is_empty());
        assert!((snap.equity - 1_000.0).abs() < 1e-9);
        assert_eq!(snap.open_orders.len(), 1);
        assert_eq!(snap.open_orders[0].status, OrderStatus::Accepted);
    }

    #[test]
    fn orders_that_never_reach_the_venue_are_rejected_and_retried() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::with_broker(
            &mut strategy,
            cfg(),
            &[id("A")],
            None,
            scripted(Mode::Unreachable),
        );
        let mut events = Vec::new();
        for h in 0..4 {
            events.extend(engine.on_bar(&id("A"), &bar(h, 100.0, 100.0)).expect("bar"));
        }
        let rejected: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Order(order) => Some(order),
                _ => None,
            })
            .collect();
        assert_eq!(rejected.len(), 3, "one attempt per bar after the first");
        assert!(rejected.iter().all(|o| o.status == OrderStatus::Rejected));
        assert!(
            rejected[0]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("venue down"))
        );
        assert!(engine.snapshot().open_orders.is_empty());
    }

    #[test]
    fn reports_that_break_the_state_machine_stop_the_engine() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::with_broker(
            &mut strategy,
            cfg(),
            &[id("A")],
            None,
            scripted(Mode::Bogus),
        );
        engine.on_bar(&id("A"), &bar(0, 100.0, 100.0)).expect("bar");
        let err = engine
            .on_bar(&id("A"), &bar(1, 100.0, 100.0))
            .expect_err("fill before acceptance");
        assert!(
            matches!(&err, EngineError::Order { broker, .. } if broker == "scripted"),
            "{err}"
        );
    }

    #[test]
    fn the_kill_switch_cancels_working_orders() {
        let mut strategy = BuyAndHold::default();
        let mut engine =
            Engine::with_broker(&mut strategy, cfg(), &[id("A")], None, scripted(Mode::Hold));
        engine.on_bar(&id("A"), &bar(0, 100.0, 100.0)).expect("bar");
        engine.on_bar(&id("A"), &bar(1, 100.0, 100.0)).expect("bar");
        assert_eq!(engine.snapshot().open_orders.len(), 1);

        let events = engine.flatten(at(1)).expect("flatten");
        let Some(Event::Order(order)) = events.first() else {
            panic!("expected the canceled order, got {events:?}");
        };
        assert_eq!(order.status, OrderStatus::Canceled);
        assert!(engine.snapshot().open_orders.is_empty());
    }

    #[test]
    fn flattening_an_empty_book_is_a_no_op() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        assert!(engine.flatten(at(0)).expect("flatten").is_empty());
    }

    fn ohlc(hour: i64, open: f64, high: f64, low: f64, close: f64) -> Bar {
        Bar {
            ts: at(hour),
            open,
            high,
            low,
            close,
            volume: 1.0,
        }
    }

    fn with_stops(stops: crate::stops::StopRules) -> BacktestConfig {
        BacktestConfig { stops, ..cfg() }
    }

    fn five_percent() -> crate::stops::StopRules {
        crate::stops::StopRules {
            stop_pct: 0.05,
            ..crate::stops::StopRules::default()
        }
    }

    fn exits(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Signal {
                    strategy, reason, ..
                } if strategy == "stops" => reason
                    .as_ref()
                    .and_then(|r| r["exit"].as_str())
                    .map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_stop_loss_exits_at_the_stop_and_the_same_signal_does_not_re_enter() {
        let bars = vec![
            ohlc(0, 100.0, 100.0, 100.0, 100.0),
            ohlc(1, 100.0, 101.0, 99.0, 100.0), // enters at 100, stop 95
            ohlc(2, 99.0, 99.0, 93.0, 94.0),    // stopped at 95
            ohlc(3, 94.0, 120.0, 94.0, 120.0),
            ohlc(4, 120.0, 130.0, 120.0, 130.0),
        ];
        let (result, events) = run_with_events(
            &mut BuyAndHold::default(),
            &[Series::new(id("A"), bars)],
            &with_stops(five_percent()),
            None,
        );
        assert_eq!(exits(&events), ["stop_loss"]);
        assert_eq!(result.fills.len(), 2, "{:?}", result.fills);
        assert!((result.fills[1].price - 95.0).abs() < 1e-9);
        let end = result.report.end_equity;
        assert!((end - 950.0).abs() < 1e-6, "lost 5% and stayed out: {end}");
    }

    #[test]
    fn take_profit_locks_in_the_gain() {
        let rules = crate::stops::StopRules {
            take_profit_r: 2.0,
            ..five_percent()
        };
        let bars = vec![
            ohlc(0, 100.0, 100.0, 100.0, 100.0),
            ohlc(1, 100.0, 101.0, 99.0, 100.0),
            ohlc(2, 104.0, 112.0, 103.0, 108.0), // target 110
            ohlc(3, 108.0, 108.0, 80.0, 80.0),
        ];
        let (result, events) = run_with_events(
            &mut BuyAndHold::default(),
            &[Series::new(id("A"), bars)],
            &with_stops(rules),
            None,
        );
        assert_eq!(exits(&events), ["take_profit"]);
        assert!((result.report.end_equity - 1_100.0).abs() < 1e-6);
    }

    #[test]
    fn after_the_cooldown_the_strategy_may_enter_again() {
        let rules = crate::stops::StopRules {
            cooldown_bars: 2,
            ..five_percent()
        };
        let bars = vec![
            ohlc(0, 100.0, 100.0, 100.0, 100.0),
            ohlc(1, 100.0, 101.0, 99.0, 100.0),
            ohlc(2, 99.0, 99.0, 93.0, 94.0), // stop
            ohlc(3, 94.0, 95.0, 93.0, 94.0),
            ohlc(4, 94.0, 95.0, 93.0, 94.0),
            ohlc(5, 94.0, 96.0, 94.0, 95.0), // re-entered at this open
            ohlc(6, 95.0, 96.0, 94.0, 95.0),
        ];
        let (result, _) = run_with_events(
            &mut BuyAndHold::default(),
            &[Series::new(id("A"), bars)],
            &with_stops(rules),
            None,
        );
        assert_eq!(result.fills.len(), 3, "{:?}", result.fills);
    }

    #[test]
    fn risk_per_trade_sizes_the_position_from_the_stop() {
        let rules = crate::stops::StopRules {
            risk_per_trade: 0.01,
            ..five_percent()
        };
        let bars = vec![
            ohlc(0, 100.0, 100.0, 100.0, 100.0),
            ohlc(1, 100.0, 101.0, 99.0, 100.0),
            ohlc(2, 99.0, 99.0, 93.0, 94.0),
        ];
        let (result, _) = run_with_events(
            &mut BuyAndHold::default(),
            &[Series::new(id("A"), bars)],
            &with_stops(rules),
            None,
        );
        // 1% risk with a 5% stop → 20% of equity; the stop loses 1% of equity.
        assert!(result.fills[0].qty.mul_add(100.0, -200.0).abs() < 1e-6);
        assert!((result.report.end_equity - 990.0).abs() < 1e-6);
    }

    /// A strategy that never wants a position (so only manual trades trade).
    #[derive(Debug)]
    struct Flat;

    impl Strategy for Flat {
        fn name(&self) -> &'static str {
            "flat"
        }

        fn on_bar(&mut self, _instrument: &InstrumentId, _bar: &Bar) -> Option<f64> {
            Some(0.0)
        }
    }

    fn feed(engine: &mut Engine<'_>, bars: &[Bar]) -> Vec<Event> {
        bars.iter()
            .flat_map(|b| engine.on_bar(&id("A"), b).expect("bar"))
            .collect()
    }

    #[test]
    fn a_manual_trade_overrides_the_strategy_until_its_stop_then_hands_back() {
        let mut strategy = Flat;
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        feed(&mut engine, &[ohlc(0, 100.0, 100.0, 100.0, 100.0)]);
        let events = engine.manual_trade(
            &id("A"),
            ManualTrade {
                target: 0.5,
                stop_pct: Some(0.05),
                take_profit_pct: Some(0.10),
                label: "suggestion #1 by scout".into(),
            },
        );
        assert!(matches!(&events[0], Event::Signal { strategy, .. } if strategy == "suggestion"));
        feed(
            &mut engine,
            &[
                ohlc(1, 100.0, 101.0, 99.0, 100.0), // enters 50% at 100, stop 95, target 110
            ],
        );
        let open = engine.snapshot();
        assert_eq!(open.positions.len(), 1);
        assert_eq!(
            open.positions[0].manual.as_deref(),
            Some("suggestion #1 by scout")
        );
        assert_eq!(open.positions[0].stop, Some(95.0));
        assert_eq!(open.positions[0].take_profit, Some(110.0));
        let after = feed(
            &mut engine,
            &[
                ohlc(2, 99.0, 99.0, 94.0, 95.0), // stopped at 95
                ohlc(3, 95.0, 96.0, 95.0, 96.0),
            ],
        );
        assert_eq!(exits(&after), ["stop_loss"]);
        let flat = engine.snapshot();
        assert!(flat.positions.is_empty(), "{flat:?}");
        let (result, _) = engine.finish();
        assert!(
            (result.report.end_equity - 975.0).abs() < 1e-6,
            "lost 5% of half"
        );
    }

    #[test]
    fn a_manual_close_flattens_and_the_strategy_waits_for_a_new_signal() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        feed(
            &mut engine,
            &[
                ohlc(0, 100.0, 100.0, 100.0, 100.0),
                ohlc(1, 100.0, 100.0, 100.0, 100.0), // buy & hold enters
            ],
        );
        assert_eq!(engine.snapshot().positions.len(), 1);
        engine.manual_trade(
            &id("A"),
            ManualTrade {
                target: 0.0,
                stop_pct: None,
                take_profit_pct: None,
                label: "suggestion #2 by risk".into(),
            },
        );
        feed(
            &mut engine,
            &[
                ohlc(2, 100.0, 100.0, 100.0, 100.0), // closed at this open
                ohlc(3, 100.0, 100.0, 100.0, 100.0),
                ohlc(4, 100.0, 100.0, 100.0, 100.0),
            ],
        );
        assert!(
            engine.snapshot().positions.is_empty(),
            "buy & hold does not re-enter on the same signal"
        );
    }

    #[test]
    fn a_stop_can_only_be_tightened_and_then_exits_there() {
        let mut strategy = BuyAndHold::default();
        let mut engine = Engine::new(&mut strategy, cfg(), &[id("A")], None);
        feed(
            &mut engine,
            &[
                ohlc(0, 100.0, 100.0, 100.0, 100.0),
                ohlc(1, 100.0, 110.0, 100.0, 108.0), // long from 100, marked at 108
            ],
        );
        assert!(
            engine.tighten_stop(&id("A"), 109.0, "w").is_err(),
            "above the price"
        );
        let events = engine
            .tighten_stop(&id("A"), 102.0, "watcher")
            .expect("tightened");
        assert!(matches!(&events[0], Event::Signal { strategy, .. } if strategy == "watcher"));
        assert!(
            engine.tighten_stop(&id("A"), 101.0, "w").is_err(),
            "looser is refused"
        );
        assert_eq!(engine.snapshot().positions[0].stop, Some(102.0));
        let after = feed(&mut engine, &[ohlc(2, 107.0, 107.0, 101.0, 103.0)]);
        assert_eq!(exits(&after), ["trailing_stop"]);
        assert!(engine.snapshot().positions.is_empty());
        assert!(
            engine.tighten_stop(&id("A"), 99.0, "w").is_err(),
            "nothing to protect"
        );
    }
}
