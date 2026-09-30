use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::broker::SimBroker;
use crate::metrics::Report;
use crate::risk::{RiskLimits, RiskManager};
use crate::strategy::Strategy;
use crate::types::{Bar, EquityPoint, Fill, Series};

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
    /// Equal-weight buy & hold of the same symbols, without fees or risk limits.
    pub benchmark_curve: Vec<EquityPoint>,
    pub fills: Vec<Fill>,
    pub report: Report,
}

/// Replay `series` through `strategy` from the first bar.
pub fn run(strategy: &mut dyn Strategy, series: &[Series], cfg: &BacktestConfig) -> BacktestResult {
    run_from(strategy, series, cfg, None)
}

/// Replay `series` through `strategy`, trading and measuring only from `measure_from`.
///
/// Earlier bars are warm-up: the strategy sees them (so indicators are primed), but
/// nothing is executed or recorded. A target set on the last warm-up bar executes at
/// the first measured bar's open.
///
/// All symbols are merged onto one timeline. Per timestamp: execute pending targets at
/// each symbol's open, mark everything at the close, update risk, record equity, then
/// ask the strategy for new targets, which are queued for each symbol's next bar.
/// Trades only happen when a risk-adjusted target actually changes, so there is no
/// churn from price drift.
pub fn run_from(
    strategy: &mut dyn Strategy,
    series: &[Series],
    cfg: &BacktestConfig,
    measure_from: Option<DateTime<Utc>>,
) -> BacktestResult {
    let mut events: Vec<(usize, &Bar)> = series
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.bars.iter().map(move |bar| (i, bar)))
        .collect();
    events.sort_by_key(|&(i, bar)| (bar.ts, i));

    let symbols: Vec<&str> = series.iter().map(|s| s.symbol.as_str()).collect();
    let mut broker = SimBroker::new(cfg.initial_cash, cfg.fee_rate, cfg.slippage_bps);
    let mut risk = RiskManager::new(cfg.risk);
    let mut benchmark = Benchmark::new(series, cfg.initial_cash);

    let mut desired = BTreeMap::new();
    let mut current: BTreeMap<String, f64> = BTreeMap::new();
    let mut pending: BTreeMap<String, f64> = BTreeMap::new();
    let mut fills = Vec::new();
    let mut equity_curve = Vec::new();
    let mut benchmark_curve = Vec::new();

    for group in events.chunk_by(|a, b| a.1.ts == b.1.ts) {
        let Some(&(_, first)) = group.first() else {
            continue;
        };
        let ts = first.ts;

        if measure_from.is_none_or(|start| ts >= start) {
            for &(i, bar) in group {
                let symbol = symbols[i];
                if let Some(target) = pending.remove(symbol) {
                    fills.extend(broker.rebalance(symbol, bar.ts, bar.open, target));
                    current.insert(symbol.to_owned(), target);
                }
                broker.mark(symbol, bar.close);
                benchmark.observe(i, bar);
            }
            let equity = broker.equity();
            risk.update(ts, equity);
            equity_curve.push(EquityPoint { ts, equity });
            benchmark_curve.push(EquityPoint {
                ts,
                equity: benchmark.value(),
            });
        }

        for &(i, bar) in group {
            if let Some(target) = strategy.on_bar(symbols[i], bar) {
                desired.insert(symbols[i].to_owned(), target);
            }
        }
        for (symbol, target) in risk.apply(&desired) {
            let held = current.get(&symbol).copied().unwrap_or(0.0);
            if (target - held).abs() > f64::EPSILON {
                pending.insert(symbol, target);
            } else {
                pending.remove(&symbol);
            }
        }
    }

    let report = Report::compute(cfg.initial_cash, &equity_curve, &benchmark_curve, &fills);
    BacktestResult {
        equity_curve,
        benchmark_curve,
        fills,
        report,
    }
}

/// Equal-weight buy & hold: each symbol gets `1/n` of capital at its first measured open.
#[derive(Debug)]
struct Benchmark {
    initial: f64,
    weight: f64,
    entry: Vec<Option<f64>>,
    last: Vec<f64>,
}

impl Benchmark {
    fn new(series: &[Series], initial: f64) -> Self {
        let active = series.iter().filter(|s| !s.bars.is_empty()).count();
        Self {
            initial,
            weight: if active == 0 {
                0.0
            } else {
                1.0 / active as f64
            },
            entry: vec![None; series.len()],
            last: vec![0.0; series.len()],
        }
    }

    fn observe(&mut self, i: usize, bar: &Bar) {
        if self.entry[i].is_none() && bar.open > 0.0 {
            self.entry[i] = Some(bar.open);
        }
        self.last[i] = bar.close;
    }

    /// Symbols not yet entered are held as cash.
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
    use crate::strategies::{BuyAndHold, PerSymbol};

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn series(symbol: &str, prices: &[(f64, f64)]) -> Series {
        let bars = prices
            .iter()
            .zip(0_i64..)
            .map(|(&(open, close), i)| Bar {
                ts: at(i),
                open,
                high: open.max(close),
                low: open.min(close),
                close,
                volume: 1.0,
            })
            .collect();
        Series::new(symbol, bars)
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
        let data = [series(
            "A",
            &[(100.0, 100.0), (105.0, 110.0), (110.0, 120.0)],
        )];
        let result = run(&mut BuyAndHold::default(), &data, &frictionless());

        assert_eq!(result.fills.len(), 1);
        let fill = &result.fills[0];
        assert_eq!(fill.ts, at(1));
        assert!((fill.price - 105.0).abs() < 1e-9);

        let expected = 1_000.0 / 105.0 * 120.0;
        assert!((result.report.end_equity - expected).abs() < 1e-6);
    }

    #[test]
    fn benchmark_ignores_fees_and_risk() {
        let data = [series("A", &[(100.0, 100.0), (100.0, 50.0), (50.0, 150.0)])];
        let cfg = BacktestConfig {
            fee_rate: 0.01,
            risk: RiskLimits::default(),
            ..frictionless()
        };
        let result = run(&mut BuyAndHold::default(), &data, &cfg);
        assert!((result.report.benchmark.total_return - 0.5).abs() < 1e-9);
        assert!(result.report.strategy.total_return < result.report.benchmark.total_return);
    }

    #[test]
    fn per_symbol_splits_capital() {
        let data = [
            series("A", &[(100.0, 100.0), (100.0, 100.0)]),
            series("B", &[(10.0, 10.0), (10.0, 10.0)]),
        ];
        let mut strategy = PerSymbol::new(BuyAndHold::default(), data.len());
        let result = run(&mut strategy, &data, &frictionless());

        assert_eq!(result.fills.len(), 2);
        for fill in &result.fills {
            assert!(
                fill.qty.mul_add(fill.price, -500.0).abs() < 1e-6,
                "{fill:?}"
            );
        }
    }

    #[test]
    fn warm_up_bars_are_not_traded_or_recorded() {
        let data = [series(
            "A",
            &[
                (100.0, 100.0),
                (100.0, 100.0),
                (200.0, 200.0),
                (200.0, 220.0),
            ],
        )];
        let result = run_from(
            &mut BuyAndHold::default(),
            &data,
            &frictionless(),
            Some(at(2)),
        );

        assert_eq!(result.equity_curve.first().map(|p| p.ts), Some(at(2)));
        assert_eq!(result.fills.len(), 1);
        assert_eq!(result.fills[0].ts, at(2));
        assert!((result.fills[0].price - 200.0).abs() < 1e-9);
    }

    #[test]
    fn empty_input_yields_empty_report() {
        let result = run(&mut BuyAndHold::default(), &[], &frictionless());
        assert!(result.fills.is_empty());
        assert_eq!(result.report.trades, 0);
        assert!((result.report.end_equity - 1_000.0).abs() < 1e-9);
    }
}
