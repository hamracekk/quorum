//! Execution: the [`Broker`] port, the simulated [`PaperBroker`] behind it, and the
//! [`Portfolio`] that keeps cash and positions from the fills a broker reports.
//!
//! The engine sizes an order against the portfolio, hands it to the broker, and applies
//! whatever [`ExecutionReport`]s come back through the order state machine
//! ([`crate::order`]). Positions change only on reported fills, never on submission, so the
//! same engine code works against a simulated venue and (M10) a real exchange.

use std::collections::BTreeMap;
use std::fmt;
use std::mem;

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::order::{ExecutionReport, Order, OrderId};
use crate::ports::PortError;
use crate::types::{Fill, Side};

/// Where orders go. Implementations: [`PaperBroker`] (simulated fills), a live exchange
/// adapter in M10.
///
/// The interface is synchronous and non-blocking so the engine stays deterministic: `submit`
/// only hands the order over, and every outcome — acceptance, fills, cancels, rejections —
/// comes back later through [`poll`](Self::poll). A live adapter bridges to its async client
/// internally (e.g. a channel to a background task) and must never block here.
pub trait Broker: fmt::Debug + Send {
    fn name(&self) -> &str;

    /// Latest tradable price of `instrument`. Simulated venues fill market orders against
    /// it; live adapters use the exchange's own prices and ignore it.
    fn quote(&mut self, _instrument: &InstrumentId, _ts: DateTime<Utc>, _price: f64) {}

    /// Dollar volume of `instrument`'s latest closed bar. Simulated venues size market impact
    /// from it; live adapters ignore it.
    fn liquidity(&mut self, _instrument: &InstrumentId, _dollar_volume: f64) {}

    /// Estimated half-spread of `instrument` (a fraction of price) from its latest bars.
    /// Simulated venues fill at least that far from the quote; live adapters ignore it.
    fn spread(&mut self, _instrument: &InstrumentId, _half_spread: f64) {}

    /// Cash added to the account (a contribution; negative: a withdrawal). Simulated books that
    /// track cash count it; live adapters ignore it (the venue reports its own balance).
    fn deposit(&mut self, _amount: f64) {}

    /// Hand over a new order (status `New`). An `Err` means the order never reached the
    /// venue; the engine records it as rejected. Venue-side refusals are reported through
    /// [`poll`](Self::poll) as [`ExecutionReport::Rejected`] instead.
    fn submit(&mut self, order: &Order) -> Result<(), PortError>;

    /// Ask the venue to cancel a working order; the outcome is reported through `poll`.
    fn cancel(&mut self, id: OrderId, ts: DateTime<Utc>) -> Result<(), PortError>;

    /// Reports since the last call, in the order they happened.
    fn poll(&mut self) -> Vec<ExecutionReport>;
}

/// Simulated venue that fills market orders in full against the latest quote.
///
/// Fills are worsened by a fixed slippage plus, with [`with_impact`](Self::with_impact), market
/// impact growing with the square root of the order's share of the latest bar's dollar volume
/// (at most [`MAX_IMPACT`]), and charged a proportional fee. An order submitted before any quote
/// for its instrument rests until the next quote.
#[derive(Debug, Clone)]
pub struct PaperBroker {
    fee_rate: f64,
    slippage: f64,
    impact: f64,
    /// Latest bar dollar volume per instrument.
    volumes: BTreeMap<InstrumentId, f64>,
    /// Estimated half-spread per instrument, when the engine estimates them.
    spreads: BTreeMap<InstrumentId, f64>,
    quotes: BTreeMap<InstrumentId, f64>,
    resting: Vec<Order>,
    reports: Vec<ExecutionReport>,
}

impl PaperBroker {
    pub const fn new(fee_rate: f64, slippage_bps: f64) -> Self {
        Self {
            fee_rate,
            slippage: slippage_bps / 10_000.0,
            impact: 0.0,
            volumes: BTreeMap::new(),
            spreads: BTreeMap::new(),
            quotes: BTreeMap::new(),
            resting: Vec::new(),
            reports: Vec::new(),
        }
    }

    /// With market impact `impact` (see [`BacktestConfig`](crate::BacktestConfig)).
    #[must_use]
    pub const fn with_impact(mut self, impact: f64) -> Self {
        self.impact = impact;
        self
    }

    /// Extra slippage of an order of `notional` on `instrument`: unknown or zero volume pays the
    /// maximum.
    fn impact_of(&self, instrument: &InstrumentId, notional: f64) -> f64 {
        if self.impact <= 0.0 {
            return 0.0;
        }
        match self.volumes.get(instrument) {
            Some(&volume) if volume > 0.0 => {
                (self.impact * (notional / volume).sqrt()).min(MAX_IMPACT)
            }
            _ => MAX_IMPACT,
        }
    }

    fn execute(&mut self, order: &Order, ts: DateTime<Utc>, price: f64) {
        let spread = self.spreads.get(&order.instrument).copied().unwrap_or(0.0);
        let slip = self.slippage.max(spread) + self.impact_of(&order.instrument, order.qty * price);
        let exec_price = match order.side {
            Side::Buy => price * (1.0 + slip),
            Side::Sell => price * (1.0 - slip),
        };
        let fee = order.qty * exec_price * self.fee_rate;
        self.reports.push(ExecutionReport::Filled {
            id: order.id,
            fill: Fill {
                ts,
                instrument: order.instrument.clone(),
                side: order.side,
                qty: order.qty,
                price: exec_price,
                fee,
            },
        });
    }
}

/// The most market impact one fill pays (10%).
pub const MAX_IMPACT: f64 = 0.1;

impl Broker for PaperBroker {
    fn name(&self) -> &'static str {
        "paper"
    }

    fn liquidity(&mut self, instrument: &InstrumentId, dollar_volume: f64) {
        self.volumes.insert(instrument.clone(), dollar_volume);
    }

    fn spread(&mut self, instrument: &InstrumentId, half_spread: f64) {
        self.spreads.insert(instrument.clone(), half_spread);
    }

    fn quote(&mut self, instrument: &InstrumentId, ts: DateTime<Utc>, price: f64) {
        self.quotes.insert(instrument.clone(), price);
        let (due, keep): (Vec<Order>, Vec<Order>) = mem::take(&mut self.resting)
            .into_iter()
            .partition(|order| &order.instrument == instrument);
        self.resting = keep;
        for order in &due {
            self.execute(order, ts, price);
        }
    }

    fn submit(&mut self, order: &Order) -> Result<(), PortError> {
        let ts = order.created_at;
        if !(order.qty > 0.0 && order.qty.is_finite()) {
            self.reports.push(ExecutionReport::Rejected {
                id: order.id,
                ts,
                reason: format!("invalid quantity {}", order.qty),
            });
            return Ok(());
        }
        self.reports
            .push(ExecutionReport::Accepted { id: order.id, ts });
        match self.quotes.get(&order.instrument).copied() {
            Some(price) if price > 0.0 => self.execute(order, ts, price),
            _ => self.resting.push(order.clone()),
        }
        Ok(())
    }

    fn cancel(&mut self, id: OrderId, ts: DateTime<Utc>) -> Result<(), PortError> {
        let index = self
            .resting
            .iter()
            .position(|order| order.id == id)
            .ok_or_else(|| PortError::Invalid(format!("order {id} is not open")))?;
        self.resting.remove(index);
        self.reports.push(ExecutionReport::Canceled {
            id,
            ts,
            reason: "canceled on request".into(),
        });
        Ok(())
    }

    fn poll(&mut self) -> Vec<ExecutionReport> {
        mem::take(&mut self.reports)
    }
}

/// Cash, positions and valuation marks, changed only by applied fills.
///
/// Fees are paid from cash, so fully-invested targets can leave cash marginally negative
/// (by roughly fee + slippage).
#[derive(Debug, Clone)]
pub struct Portfolio {
    cash: f64,
    positions: BTreeMap<InstrumentId, f64>,
    marks: BTreeMap<InstrumentId, f64>,
    min_notional: f64,
    /// Spot account: buys never spend more than the cash on hand (this is the cost buffer per
    /// unit of notional — fee and slippage). `None`: a margin account (shorts allowed).
    cash_limit: Option<f64>,
}

impl Portfolio {
    pub const fn new(cash: f64) -> Self {
        Self {
            cash,
            positions: BTreeMap::new(),
            marks: BTreeMap::new(),
            min_notional: 1.0,
            cash_limit: None,
        }
    }

    /// Make this a spot account: a buy is capped at the cash on hand, allowing `cost` (fee and
    /// slippage per unit of notional) on top.
    #[must_use]
    pub const fn spot(mut self, cost: f64) -> Self {
        self.cash_limit = Some(cost);
        self
    }

    pub const fn cash(&self) -> f64 {
        self.cash
    }

    /// Adds `amount` of cash (a contribution); a negative amount withdraws.
    pub fn deposit(&mut self, amount: f64) {
        if amount.is_finite() {
            self.cash += amount;
        }
    }

    pub fn position(&self, instrument: &InstrumentId) -> f64 {
        self.positions.get(instrument).copied().unwrap_or(0.0)
    }

    /// Latest valuation price of `instrument`, if it has been marked.
    pub fn mark_of(&self, instrument: &InstrumentId) -> Option<f64> {
        self.marks.get(instrument).copied()
    }

    /// Every instrument with a position (including zero after a round trip), in order.
    pub fn positions(&self) -> impl Iterator<Item = (&InstrumentId, f64)> {
        self.positions
            .iter()
            .map(|(instrument, &qty)| (instrument, qty))
    }

    /// Update the valuation price of `instrument`.
    pub fn mark(&mut self, instrument: &InstrumentId, price: f64) {
        self.marks.insert(instrument.clone(), price);
    }

    /// Cash plus all positions valued at their latest marks.
    pub fn equity(&self) -> f64 {
        self.positions
            .iter()
            .fold(self.cash, |acc, (instrument, qty)| {
                let price = self.marks.get(instrument).copied().unwrap_or(0.0);
                qty.mul_add(price, acc)
            })
    }

    /// Mark `instrument` at `price` and size the trade that moves it to `target` exposure
    /// (fraction of total equity). `None` when the change is below the minimum notional.
    pub fn order_for(
        &mut self,
        instrument: &InstrumentId,
        price: f64,
        target: f64,
    ) -> Option<(Side, f64)> {
        self.mark(instrument, price);
        let equity = self.equity();
        self.order_for_equity(instrument, price, target, equity)
    }

    /// [`order_for`](Self::order_for) sized against a given `equity` — the engine passes the
    /// equity at the previous timestamp's close, so every order of a timestamp is sized the
    /// same way whatever order the instruments arrive in, and never against closes of the bar
    /// it fills at the open of.
    pub fn order_for_equity(
        &mut self,
        instrument: &InstrumentId,
        price: f64,
        target: f64,
        equity: f64,
    ) -> Option<(Side, f64)> {
        self.mark(instrument, price);
        let target_qty = if equity > 0.0 && price > 0.0 {
            target * equity / price
        } else {
            0.0
        };
        let mut delta = target_qty - self.position(instrument);
        if let Some(cost) = self.cash_limit
            && delta > 0.0
            && price > 0.0
        {
            // No borrowing on a spot account: what cash can pay for, fees included.
            delta = delta.min(self.cash.max(0.0) / (price * (1.0 + cost)));
        }
        if (delta * price).abs() < self.min_notional {
            return None;
        }
        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        Some((side, delta.abs()))
    }

    /// Book an executed trade: cash pays notional plus fee, the position moves by `qty`.
    /// Pay (`amount < 0`) or receive cash outside a fill, e.g. perpetual funding.
    pub fn adjust_cash(&mut self, amount: f64) {
        self.cash += amount;
    }

    pub fn apply(&mut self, fill: &Fill) {
        let signed = match fill.side {
            Side::Buy => fill.qty,
            Side::Sell => -fill.qty,
        };
        self.cash -= signed.mul_add(fill.price, fill.fee);
        *self.positions.entry(fill.instrument.clone()).or_insert(0.0) += signed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::OrderBook;

    fn id(symbol: &str) -> InstrumentId {
        InstrumentId::spot("test", symbol).expect("valid instrument")
    }

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("valid timestamp")
    }

    /// Size, submit and book one trade the way the engine does; returns the fill.
    fn trade(
        portfolio: &mut Portfolio,
        broker: &mut PaperBroker,
        instrument: &InstrumentId,
        price: f64,
        target: f64,
    ) -> Option<Fill> {
        let mut book = OrderBook::default();
        broker.quote(instrument, ts(), price);
        let (side, qty) = portfolio.order_for(instrument, price, target)?;
        let order = book.create(ts(), instrument, side, qty);
        broker.submit(&order).expect("submitted");
        let mut filled = None;
        for report in broker.poll() {
            book.apply(&report).expect("valid report");
            if let ExecutionReport::Filled { fill, .. } = report {
                portfolio.apply(&fill);
                filled = Some(fill);
            }
        }
        filled
    }

    #[test]
    fn round_trip_without_costs_realises_pnl() {
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.0, 0.0));
        let buy = trade(&mut p, &mut b, &id("A"), 100.0, 1.0).expect("buy fill");
        assert_eq!(buy.side, Side::Buy);
        assert!((p.position(&id("A")) - 10.0).abs() < 1e-9);
        assert!(p.cash().abs() < 1e-9);

        trade(&mut p, &mut b, &id("A"), 110.0, 0.0).expect("sell fill");
        assert!(p.position(&id("A")).abs() < 1e-9);
        assert!((p.cash() - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn targets_are_fractions_of_total_equity() {
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.0, 0.0));
        trade(&mut p, &mut b, &id("A"), 100.0, 0.5);
        trade(&mut p, &mut b, &id("B"), 50.0, 0.5);
        assert!((p.position(&id("A")) - 5.0).abs() < 1e-9);
        assert!((p.position(&id("B")) - 10.0).abs() < 1e-9);

        p.mark(&id("A"), 200.0);
        assert!((p.equity() - 1_500.0).abs() < 1e-9);
    }

    #[test]
    fn fees_and_slippage_reduce_equity() {
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.001, 10.0));
        trade(&mut p, &mut b, &id("A"), 100.0, 1.0);
        trade(&mut p, &mut b, &id("A"), 100.0, 0.0);
        assert!(p.equity() < 1_000.0);
    }

    #[test]
    fn tiny_changes_are_skipped() {
        let mut p = Portfolio::new(1_000.0);
        assert!(p.order_for(&id("A"), 100.0, 0.0005).is_none());
    }

    #[test]
    fn a_spot_account_never_buys_on_credit() {
        let mut spot = Portfolio::new(100.0).spot(0.01);
        let (_, qty) = spot.order_for(&id("A"), 1.0, 3.0).expect("buy");
        assert!(
            (qty - 100.0 / 1.01).abs() < 1e-9,
            "capped at cash, fees included: {qty}"
        );
        let mut margin = Portfolio::new(100.0);
        let (_, qty) = margin.order_for(&id("A"), 1.0, 3.0).expect("buy");
        assert!((qty - 300.0).abs() < 1e-9, "a margin account may borrow");
    }

    #[test]
    fn zero_price_never_trades() {
        let mut p = Portfolio::new(1_000.0);
        assert!(p.order_for(&id("A"), 0.0, 1.0).is_none());
    }

    #[test]
    fn short_positions_profit_when_price_falls() {
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.0, 0.0));
        let fill = trade(&mut p, &mut b, &id("A"), 100.0, -1.0).expect("short fill");
        assert_eq!(fill.side, Side::Sell);
        assert!((p.position(&id("A")) + 10.0).abs() < 1e-9);
        assert!((p.cash() - 2_000.0).abs() < 1e-9);

        p.mark(&id("A"), 90.0);
        assert!((p.equity() - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn slippage_worsens_price_on_both_sides() {
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.0, 10.0));
        let buy = trade(&mut p, &mut b, &id("A"), 100.0, 1.0).expect("buy");
        let sell = trade(&mut p, &mut b, &id("A"), 100.0, 0.0).expect("sell");
        assert!((buy.price - 100.1).abs() < 1e-9);
        assert!((sell.price - 99.9).abs() < 1e-9);
    }

    #[test]
    fn impact_grows_with_the_square_root_of_the_order_share() {
        // 1,000 notional against 100,000 of volume: 1% share → 0.1 × √0.01 = 1%.
        let (mut p, mut b) = (
            Portfolio::new(1_000.0),
            PaperBroker::new(0.0, 0.0).with_impact(0.1),
        );
        b.liquidity(&id("A"), 100_000.0);
        let buy = trade(&mut p, &mut b, &id("A"), 100.0, 1.0).expect("buy");
        assert!((buy.price - 101.0).abs() < 1e-9, "{}", buy.price);
        // A quarter of the size pays half the impact.
        let (mut p, mut b) = (
            Portfolio::new(250.0),
            PaperBroker::new(0.0, 0.0).with_impact(0.1),
        );
        b.liquidity(&id("A"), 100_000.0);
        let small = trade(&mut p, &mut b, &id("A"), 100.0, 1.0).expect("buy");
        assert!((small.price - 100.5).abs() < 1e-9, "{}", small.price);
    }

    #[test]
    fn unknown_volume_pays_the_most_impact_and_off_pays_none() {
        let (mut p, mut b) = (
            Portfolio::new(1_000.0),
            PaperBroker::new(0.0, 0.0).with_impact(0.1),
        );
        let sell = trade(&mut p, &mut b, &id("A"), 100.0, -1.0).expect("sell");
        assert!((sell.price - 90.0).abs() < 1e-9, "10% at most");
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.0, 0.0));
        b.liquidity(&id("A"), 1.0);
        let buy = trade(&mut p, &mut b, &id("A"), 100.0, 1.0).expect("buy");
        assert!((buy.price - 100.0).abs() < 1e-9);
    }

    #[test]
    fn fee_is_charged_on_notional() {
        let (mut p, mut b) = (Portfolio::new(1_000.0), PaperBroker::new(0.01, 0.0));
        let fill = trade(&mut p, &mut b, &id("A"), 100.0, 0.5).expect("buy");
        assert!((fill.fee - 5.0).abs() < 1e-9);
        assert!((p.cash() - 495.0).abs() < 1e-9);
    }

    #[test]
    fn paper_orders_are_accepted_then_filled_against_the_quote() {
        let mut broker = PaperBroker::new(0.0, 0.0);
        let mut book = OrderBook::default();
        broker.quote(&id("A"), ts(), 50.0);
        let order = book.create(ts(), &id("A"), Side::Buy, 2.0);
        broker.submit(&order).expect("submit");
        let reports = broker.poll();
        assert!(matches!(reports[0], ExecutionReport::Accepted { .. }));
        let ExecutionReport::Filled { fill, .. } = &reports[1] else {
            panic!("expected a fill, got {reports:?}");
        };
        assert!((fill.price - 50.0).abs() < 1e-12 && (fill.qty - 2.0).abs() < 1e-12);
        assert!(broker.poll().is_empty(), "reports are delivered once");
    }

    #[test]
    fn orders_without_a_quote_rest_until_one_arrives_or_are_canceled() {
        let mut broker = PaperBroker::new(0.0, 0.0);
        let mut book = OrderBook::default();
        let first = book.create(ts(), &id("A"), Side::Buy, 1.0);
        let second = book.create(ts(), &id("B"), Side::Buy, 1.0);
        broker.submit(&first).expect("submit");
        broker.submit(&second).expect("submit");
        assert_eq!(broker.poll().len(), 2, "only acceptances so far");

        broker.quote(&id("A"), ts(), 10.0);
        let reports = broker.poll();
        assert!(
            matches!(&reports[..], [ExecutionReport::Filled { id, .. }] if *id == first.id),
            "{reports:?}"
        );

        broker.cancel(second.id, ts()).expect("cancel resting");
        assert!(matches!(
            &broker.poll()[..],
            [ExecutionReport::Canceled { .. }]
        ));
        assert!(broker.cancel(second.id, ts()).is_err(), "no longer open");
    }

    #[test]
    fn invalid_quantities_are_rejected_by_the_venue() {
        let mut broker = PaperBroker::new(0.0, 0.0);
        let order = Order::new(OrderId(1), ts(), id("A"), Side::Buy, f64::NAN);
        broker.submit(&order).expect("handed over");
        assert!(matches!(
            &broker.poll()[..],
            [ExecutionReport::Rejected { .. }]
        ));
    }
}
