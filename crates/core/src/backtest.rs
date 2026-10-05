use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::engine::Engine;
use crate::events::Event;
use crate::instrument::InstrumentId;
use crate::metrics::Report;
use crate::risk::RiskLimits;
use crate::stops::StopRules;
use crate::strategy::Strategy;
use crate::types::{Bar, EquityPoint, Fill, Series};

#[derive(Debug, Clone, Copy)]
pub struct BacktestConfig {
    pub initial_cash: f64,
    /// Commission per trade as a fraction of notional (0.001 = 0.1%).
    pub fee_rate: f64,
    pub slippage_bps: f64,
    pub risk: RiskLimits,
    /// Per-trade exits and risk-per-trade sizing (T-2); off by default.
    pub stops: StopRules,
    /// With funding: whether longs are held as perpetuals and pay it too (`false`: longs are
    /// spot, only shorts — perpetuals — settle funding).
    pub perp_longs: bool,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: 10_000.0,
            fee_rate: 0.001,
            slippage_bps: 2.0,
            risk: RiskLimits::default(),
            stops: StopRules::default(),
            perp_longs: true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestResult {
    pub equity_curve: Vec<EquityPoint>,
    /// Equal-weight buy & hold of the same instruments, without fees or risk limits.
    pub benchmark_curve: Vec<EquityPoint>,
    pub fills: Vec<Fill>,
    pub report: Report,
    /// Net perpetual funding paid (negative: received); `0` on spot.
    pub funding: f64,
}

/// Replay `series` through `strategy` from the first bar.
pub fn run(strategy: &mut dyn Strategy, series: &[Series], cfg: &BacktestConfig) -> BacktestResult {
    run_from(strategy, series, cfg, None)
}

/// Replay `series` through `strategy`, trading and measuring only from `measure_from`.
///
/// Earlier bars are warm-up: the strategy sees them (so indicators are primed), but nothing
/// is executed or recorded. A target set on the last warm-up bar executes at the first
/// measured bar's open. See [`Engine`] for the per-timestamp sequence.
pub fn run_from(
    strategy: &mut dyn Strategy,
    series: &[Series],
    cfg: &BacktestConfig,
    measure_from: Option<DateTime<Utc>>,
) -> BacktestResult {
    run_with_events(strategy, series, cfg, measure_from).0
}

/// Like [`run_from`], also returning every decision event (signals, targets, fills, risk
/// halts) in the order the engine produced them — ready for the journal.
///
/// All series are merged onto one timeline (by time, then by position in `series`) and fed
/// through the same [`Engine`] that replay and live trading use.
pub fn run_with_events(
    strategy: &mut dyn Strategy,
    series: &[Series],
    cfg: &BacktestConfig,
    measure_from: Option<DateTime<Utc>>,
) -> (BacktestResult, Vec<Event>) {
    let mut bars: Vec<(usize, &Bar)> = series
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.bars.iter().map(move |bar| (i, bar)))
        .collect();
    bars.sort_by_key(|&(i, bar)| (bar.ts, i));

    let universe: Vec<InstrumentId> = series
        .iter()
        .filter(|s| !s.bars.is_empty())
        .map(|s| s.instrument.clone())
        .collect();
    let mut engine = Engine::new(strategy, *cfg, &universe, measure_from);
    for s in series.iter().filter(|s| !s.funding.is_empty()) {
        engine.add_funding(&s.instrument, &s.funding);
    }

    let mut events = Vec::new();
    for (i, bar) in bars {
        let emitted = engine
            .on_bar(&series[i].instrument, bar)
            .expect("bars were sorted by time above");
        events.extend(emitted);
    }
    let (result, tail) = engine.finish();
    events.extend(tail);
    (result, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::{BuyAndHold, PerInstrument};

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
        Series::new(
            InstrumentId::spot("test", symbol).expect("valid instrument"),
            bars,
        )
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
            stops: StopRules::default(),
            perp_longs: true,
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
    fn per_instrument_splits_capital() {
        let data = [
            series("A", &[(100.0, 100.0), (100.0, 100.0)]),
            series("B", &[(10.0, 10.0), (10.0, 10.0)]),
        ];
        let mut strategy = PerInstrument::new(BuyAndHold::default(), data.len());
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

    /// Wants full exposure to every instrument on every bar.
    #[derive(Debug)]
    struct AlwaysLong;

    impl Strategy for AlwaysLong {
        fn name(&self) -> &'static str {
            "always-long"
        }

        fn on_bar(&mut self, _instrument: &InstrumentId, _bar: &Bar) -> Option<f64> {
            Some(1.0)
        }
    }

    /// Records the timestamp of every bar it is shown.
    #[derive(Debug, Default)]
    struct Recorder {
        seen: Vec<DateTime<Utc>>,
    }

    impl Strategy for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }

        fn on_bar(&mut self, _instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
            self.seen.push(bar.ts);
            None
        }
    }

    fn bars_at(symbol: &str, bars: &[(i64, f64, f64)]) -> Series {
        let bars = bars
            .iter()
            .map(|&(hour, open, close)| Bar {
                ts: at(hour),
                open,
                high: open.max(close),
                low: open.min(close),
                close,
                volume: 1.0,
            })
            .collect();
        Series::new(
            InstrumentId::spot("test", symbol).expect("valid instrument"),
            bars,
        )
    }

    #[test]
    fn gross_exposure_cap_splits_capital_between_instruments() {
        let data = [
            series("A", &[(100.0, 100.0), (100.0, 100.0)]),
            series("B", &[(10.0, 10.0), (10.0, 10.0)]),
        ];
        let result = run(&mut AlwaysLong, &data, &frictionless());
        assert_eq!(result.fills.len(), 2);
        for fill in &result.fills {
            assert!(
                fill.qty.mul_add(fill.price, -500.0).abs() < 1e-6,
                "{fill:?}"
            );
        }
    }

    #[test]
    fn kill_switch_flattens_on_the_next_bar() {
        let data = [series(
            "A",
            &[
                (100.0, 100.0),
                (100.0, 100.0),
                (100.0, 85.0),
                (85.0, 85.0),
                (85.0, 85.0),
            ],
        )];
        let cfg = BacktestConfig {
            risk: RiskLimits::default(),
            ..frictionless()
        };
        let result = run(&mut BuyAndHold::default(), &data, &cfg);

        assert_eq!(result.fills.len(), 2, "{:?}", result.fills);
        assert_eq!(result.fills[0].side, crate::types::Side::Buy);
        assert_eq!(result.fills[1].side, crate::types::Side::Sell);
        assert_eq!(result.fills[1].ts, at(3));
    }

    #[test]
    fn late_instrument_is_held_as_cash_in_the_benchmark() {
        let data = [
            bars_at(
                "A",
                &[(0, 100.0, 100.0), (1, 100.0, 100.0), (2, 100.0, 100.0)],
            ),
            bars_at("B", &[(1, 10.0, 10.0), (2, 10.0, 20.0)]),
        ];
        let result = run(&mut Recorder::default(), &data, &frictionless());
        assert!((result.report.benchmark.total_return - 0.5).abs() < 1e-9);
        assert!(result.report.strategy.total_return.abs() < 1e-9);
    }

    #[test]
    fn strategy_sees_bars_in_time_order_across_instruments() {
        let data = [
            bars_at("A", &[(0, 1.0, 1.0), (2, 1.0, 1.0), (4, 1.0, 1.0)]),
            bars_at("B", &[(1, 1.0, 1.0), (2, 1.0, 1.0), (3, 1.0, 1.0)]),
        ];
        let mut recorder = Recorder::default();
        run(&mut recorder, &data, &frictionless());

        assert_eq!(recorder.seen.len(), 6);
        assert!(recorder.seen.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn longs_pay_and_shorts_receive_perpetual_funding() {
        use crate::derivatives::FundingPoint;
        let flat = [(100.0, 100.0); 6];
        let mut perp = series("PERP", &flat);
        let at = |i: usize| perp.bars[i].ts;
        perp.funding = (2..5)
            .map(|i| FundingPoint {
                ts: at(i),
                rate: 0.001,
            })
            .collect();
        let cfg = BacktestConfig {
            fee_rate: 0.0,
            slippage_bps: 0.0,
            ..BacktestConfig::default()
        };
        let long = run(&mut BuyAndHold::default(), &[perp.clone()], &cfg);
        assert!(
            long.funding > 0.0,
            "a long pays positive funding: {}",
            long.funding
        );
        let end = long.equity_curve.last().map_or(0.0, |p| p.equity);
        assert!(
            (end + long.funding - 10_000.0).abs() < 1e-6,
            "equity is lower by what was paid"
        );
        let spot = run(&mut BuyAndHold::default(), &[series("SPOT", &flat)], &cfg);
        assert!(spot.funding.abs() < f64::EPSILON, "no funding on spot");
    }
}
