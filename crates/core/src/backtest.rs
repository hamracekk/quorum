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

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub initial_cash: f64,
    /// Commission per trade as a fraction of notional (0.001 = 0.1%).
    pub fee_rate: f64,
    pub slippage_bps: f64,
    /// Market impact (E-3): extra slippage of `impact × √(order notional / the instrument's
    /// latest bar dollar volume)`, the square-root law; `0` = off.
    pub impact: f64,
    pub risk: RiskLimits,
    /// Per-trade exits and risk-per-trade sizing (T-2); off by default.
    pub stops: StopRules,
    /// With funding: whether longs are held as perpetuals and pay it too (`false`: longs are
    /// spot, only shorts — perpetuals — settle funding).
    pub perp_longs: bool,
    /// How much each trade gets, how many at once, per-market budgets; off by default.
    pub sizing: crate::sizing::SizingRules,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            initial_cash: 10_000.0,
            fee_rate: 0.001,
            slippage_bps: 2.0,
            impact: 0.0,
            risk: RiskLimits::default(),
            stops: StopRules::default(),
            perp_longs: true,
            sizing: crate::sizing::SizingRules::default(),
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
    let mut engine = Engine::new(strategy, cfg.clone(), &universe, measure_from);
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
            impact: 0.0,
            risk: RiskLimits {
                max_daily_loss: 1.0,
                ..RiskLimits::default()
            },
            stops: StopRules::default(),
            perp_longs: true,
            sizing: crate::sizing::SizingRules::default(),
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
    fn impact_is_sized_on_the_last_closed_bar() {
        let mut data = [series(
            "A",
            &[(100.0, 100.0), (105.0, 110.0), (110.0, 120.0)],
        )];
        // 100M of dollar volume on the signal bar, none on the fill bar (unknown at its open).
        data[0].bars[0].volume = 1_000_000.0;
        data[0].bars[1].volume = 0.0;
        let cfg = BacktestConfig {
            impact: 0.1,
            ..frictionless()
        };
        let result = run(&mut BuyAndHold::default(), &data, &cfg);
        // Notional ≈ 1,000: 0.1 × √(1,000 / 100M) = 0.1%.
        let expected = 105.0 * 0.1_f64.mul_add((1_000.0_f64 / 1e8).sqrt(), 1.0);
        assert!(
            (result.fills[0].price - expected).abs() < 1e-3,
            "{:?}",
            result.fills[0]
        );
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

    #[test]
    fn the_crash_stop_exits_at_its_level_or_at_a_gapping_open() {
        use chrono::{Duration, TimeZone};
        let day = |i: i64, open: f64, low: f64, close: f64| Bar {
            ts: Utc.timestamp_opt(0, 0).single().expect("ts") + Duration::days(i),
            open,
            high: open.max(close),
            low,
            close,
            volume: 1.0,
        };
        let run_with = |bars: Vec<Bar>| {
            let id = InstrumentId::spot("test", "A").expect("id");
            let mut cfg = BacktestConfig {
                fee_rate: 0.0,
                slippage_bps: 0.0,
                ..BacktestConfig::default()
            };
            cfg.stops.crash_pct = 0.1;
            run(&mut BuyAndHold::default(), &[Series::new(id, bars)], &cfg)
        };
        // Bought at 100; the next day trades down to 85: out at 90 (10% below the close).
        let intraday = run_with(vec![
            day(0, 100.0, 100.0, 100.0),
            day(1, 100.0, 100.0, 100.0),
            day(2, 98.0, 85.0, 95.0),
            day(3, 96.0, 96.0, 97.0),
        ]);
        let exit = intraday.fills.last().expect("exit");
        assert_eq!(exit.side, crate::types::Side::Sell);
        assert!((exit.price - 90.0).abs() < 1e-9, "{}", exit.price);
        assert_eq!(
            intraday.fills.len(),
            2,
            "stays out while the strategy is unchanged"
        );
        // A gap opening at 80 fills at the open, below the level.
        let gap = run_with(vec![
            day(0, 100.0, 100.0, 100.0),
            day(1, 100.0, 100.0, 100.0),
            day(2, 80.0, 78.0, 82.0),
        ]);
        assert!((gap.fills.last().expect("exit").price - 80.0).abs() < 1e-9);
    }

    #[test]
    fn a_stock_is_not_sold_as_delisted_over_weekends() {
        use chrono::{Datelike, Duration, TimeZone};
        // Monday 2024-01-01; the coin trades every day, the stock on weekdays only.
        let start = Utc
            .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .single()
            .expect("ts");
        let day = |i: i64| Bar {
            ts: start + Duration::days(i),
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1.0,
        };
        let weekday = |i: &i64| (start + Duration::days(*i)).weekday().number_from_monday() <= 5;
        let coin = InstrumentId::spot("test", "COIN").expect("id");
        let stock = InstrumentId::new(
            crate::instrument::VenueId::new("test").expect("venue"),
            "NVDA",
            crate::instrument::InstrumentKind::Equity,
        )
        .expect("id");
        let mut cfg = BacktestConfig {
            fee_rate: 0.0,
            slippage_bps: 0.0,
            ..BacktestConfig::default()
        };
        cfg.stops.stale_bars = 2;
        let sold = |stock_days: Vec<i64>| {
            let series = [
                Series::new(coin.clone(), (0..21).map(day).collect()),
                Series::new(stock.clone(), stock_days.into_iter().map(day).collect()),
            ];
            run(
                &mut PerInstrument::new(BuyAndHold::default(), 2),
                &series,
                &cfg,
            )
            .fills
            .iter()
            .any(|f| f.instrument == stock && f.side == crate::types::Side::Sell)
        };
        assert!(
            !sold((0..21).filter(weekday).collect()),
            "three weeks of weekends"
        );
        // It stops trading after Wednesday of week two: two missed weekdays and it is sold.
        assert!(
            sold((0..10).filter(weekday).collect()),
            "a real stop is still caught"
        );
    }

    #[test]
    fn a_coin_that_stops_trading_is_sold_at_its_last_close_less_the_haircut() {
        use chrono::{Duration, TimeZone};
        let day = |i: i64, close: f64| Bar {
            ts: Utc.timestamp_opt(0, 0).single().expect("ts") + Duration::days(i),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        };
        let live = InstrumentId::spot("test", "LIVE").expect("id");
        let dead = InstrumentId::spot("test", "DEAD").expect("id");
        let series = [
            Series::new(live, (0..8).map(|i| day(i, 100.0)).collect()),
            Series::new(dead, (0..3).map(|i| day(i, 50.0)).collect()),
        ];
        let mut cfg = BacktestConfig {
            fee_rate: 0.0,
            slippage_bps: 0.0,
            ..BacktestConfig::default()
        };
        cfg.stops.stale_bars = 2;
        cfg.stops.stale_haircut = 0.5;
        let result = run(
            &mut PerInstrument::new(BuyAndHold::default(), 2),
            &series,
            &cfg,
        );
        let sold = result
            .fills
            .iter()
            .find(|f| f.instrument.symbol() == "DEAD" && f.side == crate::types::Side::Sell)
            .expect("the dead coin is sold");
        assert!(
            (sold.price - 25.0).abs() < 1e-9,
            "last close 50 less 50%: {}",
            sold.price
        );
        let end = result.equity_curve.last().map_or(0.0, |p| p.equity);
        assert!(
            (end - 7_500.0).abs() < 1.0,
            "half of the dead coin's value is lost: {end}"
        );
    }
}
