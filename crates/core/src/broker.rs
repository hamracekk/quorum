use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::types::{Fill, Side};

/// Multi-symbol simulated broker with proportional fees and fixed slippage.
///
/// Positions are valued at the last price passed to [`mark`](Self::mark) or
/// [`rebalance`](Self::rebalance). Fees are paid from cash, so fully-invested targets
/// can leave cash marginally negative (by roughly fee + slippage).
#[derive(Debug, Clone)]
pub struct SimBroker {
    cash: f64,
    positions: BTreeMap<String, f64>,
    marks: BTreeMap<String, f64>,
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

    pub fn position(&self, symbol: &str) -> f64 {
        self.positions.get(symbol).copied().unwrap_or(0.0)
    }

    /// Update the valuation price of `symbol`.
    pub fn mark(&mut self, symbol: &str, price: f64) {
        self.marks.insert(symbol.to_owned(), price);
    }

    /// Cash plus all positions valued at their latest marks.
    pub fn equity(&self) -> f64 {
        self.positions.iter().fold(self.cash, |acc, (symbol, qty)| {
            let price = self.marks.get(symbol).copied().unwrap_or(0.0);
            qty.mul_add(price, acc)
        })
    }

    /// Trade `symbol` toward `target` exposure (fraction of total equity) at `price`.
    /// Returns `None` when the change is below the minimum notional.
    pub fn rebalance(
        &mut self,
        symbol: &str,
        ts: DateTime<Utc>,
        price: f64,
        target: f64,
    ) -> Option<Fill> {
        self.mark(symbol, price);
        let equity = self.equity();
        let target_qty = if equity > 0.0 && price > 0.0 {
            target * equity / price
        } else {
            0.0
        };
        let delta = target_qty - self.position(symbol);
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
        *self.positions.entry(symbol.to_owned()).or_insert(0.0) += delta;

        Some(Fill {
            ts,
            symbol: symbol.to_owned(),
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

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("valid timestamp")
    }

    #[test]
    fn round_trip_without_costs_realises_pnl() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        let buy = broker.rebalance("A", ts(), 100.0, 1.0).expect("buy fill");
        assert_eq!(buy.side, Side::Buy);
        assert!((broker.position("A") - 10.0).abs() < 1e-9);
        assert!(broker.cash().abs() < 1e-9);

        broker.rebalance("A", ts(), 110.0, 0.0).expect("sell fill");
        assert!(broker.position("A").abs() < 1e-9);
        assert!((broker.cash() - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn targets_are_fractions_of_total_equity() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        broker.rebalance("A", ts(), 100.0, 0.5);
        broker.rebalance("B", ts(), 50.0, 0.5);
        assert!((broker.position("A") - 5.0).abs() < 1e-9);
        assert!((broker.position("B") - 10.0).abs() < 1e-9);

        broker.mark("A", 200.0);
        assert!((broker.equity() - 1_500.0).abs() < 1e-9);
    }

    #[test]
    fn fees_and_slippage_reduce_equity() {
        let mut broker = SimBroker::new(1_000.0, 0.001, 10.0);
        broker.rebalance("A", ts(), 100.0, 1.0);
        broker.rebalance("A", ts(), 100.0, 0.0);
        assert!(broker.equity() < 1_000.0);
    }

    #[test]
    fn tiny_changes_are_skipped() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        assert!(broker.rebalance("A", ts(), 100.0, 0.0005).is_none());
    }

    #[test]
    fn zero_price_never_trades() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        assert!(broker.rebalance("A", ts(), 0.0, 1.0).is_none());
    }
}
