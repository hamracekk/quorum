use chrono::{DateTime, Utc};

use crate::types::{Fill, Side};

/// Single-instrument simulated broker with proportional fees and fixed slippage.
///
/// Fees are paid from cash, so a 100% long target can leave cash marginally
/// negative (by roughly the fee + slippage); treat that as negligible leverage.
#[derive(Debug, Clone)]
pub struct SimBroker {
    cash: f64,
    qty: f64,
    fee_rate: f64,
    slippage: f64,
    min_notional: f64,
}

impl SimBroker {
    pub fn new(cash: f64, fee_rate: f64, slippage_bps: f64) -> Self {
        Self {
            cash,
            qty: 0.0,
            fee_rate,
            slippage: slippage_bps / 10_000.0,
            min_notional: 1.0,
        }
    }

    pub const fn cash(&self) -> f64 {
        self.cash
    }

    pub const fn position(&self) -> f64 {
        self.qty
    }

    pub const fn equity(&self, price: f64) -> f64 {
        self.qty.mul_add(price, self.cash)
    }

    /// Trade toward `target` exposure (fraction of equity) at `price`.
    /// Returns `None` when the change is below the minimum notional.
    pub fn rebalance(&mut self, ts: DateTime<Utc>, price: f64, target: f64) -> Option<Fill> {
        let equity = self.equity(price);
        let target_qty = if equity > 0.0 {
            target * equity / price
        } else {
            0.0
        };
        let delta = target_qty - self.qty;
        if (delta * price).abs() < self.min_notional {
            return None;
        }

        let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
        let exec_price = match side {
            Side::Buy => price * (1.0 + self.slippage),
            Side::Sell => price * (1.0 - self.slippage),
        };
        let fee = delta.abs() * exec_price * self.fee_rate;

        self.cash -= delta * exec_price + fee;
        self.qty += delta;

        Some(Fill {
            ts,
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
        let buy = broker.rebalance(ts(), 100.0, 1.0).expect("buy fill");
        assert_eq!(buy.side, Side::Buy);
        assert!((broker.position() - 10.0).abs() < 1e-9);
        assert!(broker.cash().abs() < 1e-9);

        broker.rebalance(ts(), 110.0, 0.0).expect("sell fill");
        assert!(broker.position().abs() < 1e-9);
        assert!((broker.cash() - 1_100.0).abs() < 1e-9);
    }

    #[test]
    fn fees_and_slippage_reduce_equity() {
        let mut broker = SimBroker::new(1_000.0, 0.001, 10.0);
        broker.rebalance(ts(), 100.0, 1.0);
        broker.rebalance(ts(), 100.0, 0.0);
        assert!(broker.equity(100.0) < 1_000.0);
    }

    #[test]
    fn tiny_changes_are_skipped() {
        let mut broker = SimBroker::new(1_000.0, 0.0, 0.0);
        assert!(broker.rebalance(ts(), 100.0, 0.0005).is_none());
    }
}
