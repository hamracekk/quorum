use crate::broker::SimBroker;
use crate::metrics::Report;
use crate::risk::{RiskLimits, RiskManager};
use crate::strategy::Strategy;
use crate::types::{Bar, EquityPoint, Fill};

#[derive(Debug, Clone, Copy)]
pub struct BacktestConfig {
    pub initial_cash: f64,
    /// Commission per trade as a fraction of notional (0.001 = 0.1%).
    pub fee_rate: f64,
    pub slippage_bps: f64,
    pub risk: RiskLimits,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: 10_000.0,
            fee_rate: 0.001,
            slippage_bps: 2.0,
            risk: RiskLimits::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub equity_curve: Vec<EquityPoint>,
    pub fills: Vec<Fill>,
    pub report: Report,
}

/// Replay `bars` through `strategy`.
///
/// Per bar: execute any pending target at the open, mark equity at the close,
/// update risk, then ask the strategy for a new target, which is queued for the
/// next bar. Trades only happen when the risk-adjusted target actually changes,
/// so there is no churn from price drift.
pub fn run(strategy: &mut dyn Strategy, bars: &[Bar], cfg: &BacktestConfig) -> BacktestResult {
    let mut broker = SimBroker::new(cfg.initial_cash, cfg.fee_rate, cfg.slippage_bps);
    let mut risk = RiskManager::new(cfg.risk);

    let mut desired = 0.0;
    let mut current = 0.0;
    let mut pending: Option<f64> = None;
    let mut fills = Vec::new();
    let mut equity_curve = Vec::with_capacity(bars.len());

    for bar in bars {
        if let Some(target) = pending.take() {
            if let Some(fill) = broker.rebalance(bar.ts, bar.open, target) {
                fills.push(fill);
            }
            current = target;
        }

        let equity = broker.equity(bar.close);
        risk.update(bar.ts, equity);
        equity_curve.push(EquityPoint { ts: bar.ts, equity });

        if let Some(target) = strategy.on_bar(bar) {
            desired = target;
        }
        let effective = risk.clamp(desired);
        if (effective - current).abs() > f64::EPSILON {
            pending = Some(effective);
        }
    }

    let report = Report::compute(&equity_curve, &fills, bars);
    BacktestResult {
        equity_curve,
        fills,
        report,
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;
    use crate::strategies::BuyAndHold;

    fn bars(prices: &[(f64, f64)]) -> Vec<Bar> {
        prices
            .iter()
            .enumerate()
            .map(|(i, &(open, close))| Bar {
                ts: DateTime::from_timestamp(i as i64 * 3_600, 0).expect("valid timestamp"),
                open,
                high: open.max(close),
                low: open.min(close),
                close,
                volume: 1.0,
            })
            .collect()
    }

    fn frictionless() -> BacktestConfig {
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
    fn signal_executes_on_next_bar_open() {
        let data = bars(&[(100.0, 100.0), (105.0, 110.0), (110.0, 120.0)]);
        let result = run(&mut BuyAndHold::default(), &data, &frictionless());

        let fill = result.fills.first().expect("one fill");
        assert_eq!(fill.ts, data[1].ts);
        assert!((fill.price - 105.0).abs() < 1e-9);
        assert_eq!(result.fills.len(), 1);

        let expected = 1_000.0 / 105.0 * 120.0;
        assert!((result.report.end_equity - expected).abs() < 1e-6);
    }

    #[test]
    fn empty_input_yields_empty_report() {
        let result = run(&mut BuyAndHold::default(), &[], &frictionless());
        assert!(result.fills.is_empty());
        assert_eq!(result.report.trades, 0);
    }
}
