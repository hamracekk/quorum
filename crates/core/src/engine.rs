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
use crate::events::Event;
use crate::instrument::InstrumentId;
use crate::metrics::Report;
use crate::order::{ExecutionReport, Order, OrderBook, OrderError, OrderStatus};
use crate::risk::RiskManager;
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
}

/// Point-in-time state of an engine: what a dashboard or state file needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineSnapshot {
    /// Timestamp of the latest bar fed in.
    pub ts: Option<DateTime<Utc>>,
    pub cash: f64,
    pub equity: f64,
    pub halted: bool,
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
        let broker = PaperBroker::new(cfg.fee_rate, cfg.slippage_bps);
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
            cfg,
            measure_from,
            portfolio: Portfolio::new(cfg.initial_cash),
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
            Some(latest) if bar.ts > latest => self.close_timestamp(latest, &mut out),
            _ => {}
        }
        self.open_ts = Some(bar.ts);
        self.drain(&mut out)?;

        if self.measuring(bar.ts) {
            if let Some(target) = self.pending.remove(instrument) {
                self.current.insert(instrument.clone(), target);
                self.execute(instrument, bar.ts, bar.open, target, &mut out)?;
            }
            self.portfolio.mark(instrument, bar.close);
            self.benchmark.observe(instrument, bar);
        }

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
                });
            }
        }
        Ok(out)
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
                }
            })
            .collect();
        EngineSnapshot {
            ts: self.open_ts,
            cash: self.portfolio.cash(),
            equity: self.portfolio.equity(),
            halted: self.risk.halted(),
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
        let report = Report::compute(
            self.cfg.initial_cash,
            &self.equity_curve,
            &self.benchmark_curve,
            &self.fills,
        );
        let result = BacktestResult {
            equity_curve: self.equity_curve,
            benchmark_curve: self.benchmark_curve,
            fills: self.fills,
            report,
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
        let Some((side, qty)) = self.portfolio.order_for(instrument, price, target) else {
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

    fn measuring(&self, ts: DateTime<Utc>) -> bool {
        self.measure_from.is_none_or(|start| ts >= start)
    }

    fn close_timestamp(&mut self, ts: DateTime<Utc>, out: &mut Vec<Event>) {
        if self.measuring(ts) {
            let equity = self.portfolio.equity();
            let was_halted = self.risk.halted();
            self.risk.update(ts, equity);
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
        for (instrument, target) in self.risk.apply(&self.desired) {
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
            risk: RiskLimits {
                max_daily_loss: 1.0,
                ..RiskLimits::default()
            },
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
}
