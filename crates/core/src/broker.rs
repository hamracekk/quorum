use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::types::{Fill, Side};

/// Multi-instrument simulated broker with proportional fees and fixed slippage.
///
/// Positions are valued at the last price passed to [`mark`](Self::mark) or
/// [`rebalance`](Self::rebalance). Fees are paid from cash, so fully-invested targets
/// can leave cash marginally negative (by roughly fee + slippage).
#[derive(Debug, Clone)]
pub struct SimBroker {
    cash: f64,
    positions: BTreeMap<InstrumentId, f64>,
    marks: BTreeMap<InstrumentId, f64>,
    fee_rate: f64,
    slippage: f64,
    min_notional: f64,
}

impl SimBroker {
    pub fn new(cash: f64, fee_rate: f64, slippage_bps: f64) -> Self {
        Self {
            cash,
            positions: BTreeMap::new(),
            marks: BTreeMap::new(),
            fee_rate,
            slippage: slippage_bps / 10_000.0,
            min_notional: 1.0,
        }
    }

    pub const fn cash(&self) -> f64 {
        self.cash
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

    /// Trade `instrument` toward `target` exposure (fraction of total equity) at `price`.
    /// Returns `None` when the change is below the minimum notional.
    pub fn rebalance(
        &mut self,
        instrument: &InstrumentId,
        ts: DateTime<Utc>,
        price: f64,
        target: f64,
    ) -> Option<Fill> {
        self.mark(instrument, price);
        let equity = self.equity();
        let target_qty = if equity > 0.0 && price > 0.0 {
            target * equity / price
        } else {
            0.0
        };
        let delta = target_qty - self.position(instrument);
        if (delta * price).abs() < self.min_notional {
            return None;
        }

        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let exec_price = match side {
            Side::Buy => price * (1.0 + self.slippage),
            Side::Sell => price * (1.0 - self.slippage),
        };
        let fee = delta.abs() * exec_price * self.fee_rate;

        self.cash -= delta.mul_add(exec_price, fee);
        *self.positions.entry(instrument.clone()).or_insert(0.0) += delta;

        Some(Fill {
            ts,
            instrument: instrument.clone(),
            side,
            qty: delta.abs(),
            price: exec_price,
            fee,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(symbol: &str) -> InstrumentId {
        InstrumentId::spot("test", symbol).expect("valid instrument")
    }

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("valid timestamp")
    }

    #[test]
    fn round_trip_without_costs_realises_pnl() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        let buy = broker
            .rebalance(&id("A"), ts(), 100.0, 1.0)
            .expect("buy fill");
        assert_eq!(buy.side, Side::Buy);
        assert!((broker.position(&id("A")) - 10.0).abs() < 1e-9);
        assert!(broker.cash().abs() < 1e-9);

        broker
            .rebalance(&id("A"), ts(), 110.0, 0.0)
            .expect("sell fill");
        assert!(broker.position(&id("A")).abs() < 1e-9);
        assert!((broker.cash() - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn targets_are_fractions_of_total_equity() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        broker.rebalance(&id("A"), ts(), 100.0, 0.5);
        broker.rebalance(&id("B"), ts(), 50.0, 0.5);
        assert!((broker.position(&id("A")) - 5.0).abs() < 1e-9);
        assert!((broker.position(&id("B")) - 10.0).abs() < 1e-9);

        broker.mark(&id("A"), 200.0);
        assert!((broker.equity() - 1_500.0).abs() < 1e-9);
    }

    #[test]
    fn fees_and_slippage_reduce_equity() {
        let mut broker = SimBroker::new(1_000.0, 0.001, 10.0);
        broker.rebalance(&id("A"), ts(), 100.0, 1.0);
        broker.rebalance(&id("A"), ts(), 100.0, 0.0);
        assert!(broker.equity() < 1_000.0);
    }

    #[test]
    fn tiny_changes_are_skipped() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        assert!(broker.rebalance(&id("A"), ts(), 100.0, 0.0005).is_none());
    }

    #[test]
    fn zero_price_never_trades() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        assert!(broker.rebalance(&id("A"), ts(), 0.0, 1.0).is_none());
    }

    #[test]
    fn short_positions_profit_when_price_falls() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        let fill = broker
            .rebalance(&id("A"), ts(), 100.0, -1.0)
            .expect("short fill");
        assert_eq!(fill.side, Side::Sell);
        assert!((broker.position(&id("A")) + 10.0).abs() < 1e-9);
        assert!((broker.cash() - 2_000.0).abs() < 1e-9);

        broker.mark(&id("A"), 90.0);
        assert!((broker.equity() - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn slippage_worsens_price_on_both_sides() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 10.0);
        let buy = broker.rebalance(&id("A"), ts(), 100.0, 1.0).expect("buy");
        let sell = broker.rebalance(&id("A"), ts(), 100.0, 0.0).expect("sell");
        assert!((buy.price - 100.1).abs() < 1e-9);
        assert!((sell.price - 99.9).abs() < 1e-9);
    }

    #[test]
    fn fee_is_charged_on_notional() {
        let mut broker = SimBroker::new(1_000.0, 0.01, 0.0);
        let fill = broker.rebalance(&id("A"), ts(), 100.0, 0.5).expect("buy");
        assert!((fill.fee - 5.0).abs() < 1e-9);
        assert!((broker.cash() - 495.0).abs() < 1e-9);
    }
}
