use chrono::{DateTime, Utc};
use serde::Serialize;
use trader_core::{BacktestConfig, BacktestResult, EquityPoint, Report, Series, run, run_from};

use crate::{LabError, StrategyFactory, slice_all};

#[derive(Debug, Clone, Copy)]
pub struct WalkForwardConfig {
    /// Timestamps used to choose parameters in each window.
    pub train_bars: usize,
    /// Timestamps traded out-of-sample after each training window; also the step size.
    pub test_bars: usize,
}

/// One train/test split.
#[derive(Debug, Clone, Serialize)]
pub struct Window<P> {
    pub train_start: DateTime<Utc>,
    pub test_start: DateTime<Utc>,
    pub test_end: DateTime<Utc>,
    /// Parameters that scored best on the training window.
    pub params: P,
    pub train_sharpe: f64,
    /// Out-of-sample result of `params` on the test window.
    pub test: Report,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardReport<P> {
    pub windows: Vec<Window<P>>,
    /// All test windows chained into one out-of-sample equity curve.
    pub out_of_sample: Report,
    /// That stitched curve (for combining strategies); not sent to the dashboard.
    #[serde(skip)]
    pub curve: Vec<EquityPoint>,
    /// Buy & hold over the same windows, stitched the same way.
    #[serde(skip)]
    pub benchmark_curve: Vec<EquityPoint>,
}

/// Rolling walk-forward evaluation.
///
/// For each window: backtest every parameter set on `train_bars`, pick the best Sharpe,
/// then trade it on the next `test_bars` (with the training bars as indicator warm-up
/// only). Windows advance by `test_bars`, so test periods never overlap and no test bar
/// is ever used for choosing parameters.
pub fn walk_forward<P: Clone + Send + Sync>(
    grid: &[P],
    factory: &(impl StrategyFactory<P> + Sync),
    series: &[Series],
    cfg: &BacktestConfig,
    wf: WalkForwardConfig,
) -> Result<WalkForwardReport<P>, LabError> {
    walk_forward_with(grid, factory, |_, base| base.clone(), series, cfg, wf)
}

/// [`walk_forward`] where a parameter set can also change the backtest configuration.
///
/// `configure` maps a parameter set and the base configuration to the configuration it is
/// trained and tested with, e.g. per-trade stops chosen alongside the strategy's parameters.
pub fn walk_forward_with<P: Clone + Send + Sync>(
    grid: &[P],
    factory: &(impl StrategyFactory<P> + Sync),
    configure: impl Fn(&P, &BacktestConfig) -> BacktestConfig + Sync,
    series: &[Series],
    cfg: &BacktestConfig,
    wf: WalkForwardConfig,
) -> Result<WalkForwardReport<P>, LabError> {
    if grid.is_empty() {
        return Err(LabError::EmptyGrid);
    }
    if wf.train_bars == 0 || wf.test_bars == 0 {
        return Err(LabError::EmptyWindow);
    }
    let stamps = timeline(series);
    let needed = wf.train_bars + wf.test_bars;
    if stamps.len() < needed {
        return Err(LabError::NotEnoughData {
            needed,
            available: stamps.len(),
        });
    }

    // Windows are independent: run them across the cores, keep them in order.
    let starts: Vec<usize> = (0..=stamps.len() - needed).step_by(wf.test_bars).collect();
    let (windows, results): (Vec<Window<P>>, Vec<BacktestResult>) =
        crate::par_map(&starts, |&start| {
            let train_start = stamps[start];
            let test_start = stamps[start + wf.train_bars];
            let test_end = stamps[start + needed - 1];
            let after_test = stamps
                .get(start + needed)
                .copied()
                .unwrap_or(DateTime::<Utc>::MAX_UTC);

            let train = slice_all(series, train_start, test_start);
            let (params, train_sharpe) = best_params(grid, factory, &configure, &train, cfg);

            let full = slice_all(series, train_start, after_test);
            let mut strategy = factory.build(&params);
            let chosen = configure(&params, cfg);
            let result = run_from(strategy.as_mut(), &full, &chosen, Some(test_start));
            let window = Window {
                train_start,
                test_start,
                test_end,
                params,
                train_sharpe,
                test: result.report.clone(),
            };
            (window, result)
        })
        .into_iter()
        .unzip();

    let (out_of_sample, curve, benchmark_curve) = stitch(&results, cfg.initial_cash);
    Ok(WalkForwardReport {
        windows,
        out_of_sample,
        curve,
        benchmark_curve,
    })
}

/// Sorted, de-duplicated timestamps across all series.
fn timeline(series: &[Series]) -> Vec<DateTime<Utc>> {
    let mut stamps: Vec<DateTime<Utc>> = series
        .iter()
        .flat_map(|s| s.bars.iter().map(|b| b.ts))
        .collect();
    stamps.sort_unstable();
    stamps.dedup();
    stamps
}

fn best_params<P: Clone>(
    grid: &[P],
    factory: &impl StrategyFactory<P>,
    configure: &impl Fn(&P, &BacktestConfig) -> BacktestConfig,
    train: &[Series],
    cfg: &BacktestConfig,
) -> (P, f64) {
    let mut best: Option<(&P, f64)> = None;
    for params in grid {
        let mut strategy = factory.build(params);
        let sharpe = run(strategy.as_mut(), train, &configure(params, cfg))
            .report
            .strategy
            .sharpe;
        if best.is_none_or(|(_, top)| sharpe > top) {
            best = Some((params, sharpe));
        }
    }
    // `grid` is non-empty (checked by the caller), so `best` is always set.
    best.map_or_else(|| (grid[0].clone(), 0.0), |(p, s)| (p.clone(), s))
}

/// Chain test windows as if the capital rolled from one into the next.
fn stitch(
    results: &[BacktestResult],
    initial: f64,
) -> (Report, Vec<EquityPoint>, Vec<EquityPoint>) {
    let mut curve = Vec::new();
    let mut benchmark = Vec::new();
    let mut fills = Vec::new();
    let (mut scale, mut bench_scale) = (1.0, 1.0);

    for result in results {
        curve.extend(scaled(&result.equity_curve, scale));
        benchmark.extend(scaled(&result.benchmark_curve, bench_scale));
        fills.extend(result.fills.iter().cloned());
        if initial > 0.0 {
            scale *= result.equity_curve.last().map_or(initial, |p| p.equity) / initial;
            bench_scale *= result.benchmark_curve.last().map_or(initial, |p| p.equity) / initial;
        }
    }
    let mut report = Report::compute(initial, &curve, &benchmark, &fills);
    report.funding = results.iter().map(|r| r.funding).sum();
    (report, curve, benchmark)
}

fn scaled(curve: &[EquityPoint], scale: f64) -> impl Iterator<Item = EquityPoint> + '_ {
    curve.iter().map(move |p| EquityPoint {
        ts: p.ts,
        equity: p.equity * scale,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use trader_core::strategies::SmaCross;
    use trader_core::{Bar, InstrumentId, Strategy};

    use super::*;

    /// Deterministic zig-zag uptrend: +1% per bar with a pullback every 7th bar.
    pub(crate) fn trending_series(symbol: &str, len: i64) -> Series {
        let mut price = 100.0;
        let bars = (0..len)
            .map(|i| {
                let open = price;
                price *= if i % 7 == 6 { 0.97 } else { 1.01 };
                Bar {
                    ts: DateTime::from_timestamp(i * 3_600, 0).expect("valid timestamp"),
                    open,
                    high: open.max(price),
                    low: open.min(price),
                    close: price,
                    volume: 1.0,
                }
            })
            .collect();
        Series::new(
            InstrumentId::spot("test", symbol).expect("valid instrument"),
            bars,
        )
    }

    fn factory(&(fast, slow): &(usize, usize)) -> Box<dyn Strategy> {
        Box::new(SmaCross::new(fast, slow, false))
    }

    const GRID: [(usize, usize); 2] = [(2, 5), (3, 10)];

    #[test]
    fn windows_tile_the_data_without_overlap() {
        let data = [trending_series("A", 100)];
        let wf = WalkForwardConfig {
            train_bars: 40,
            test_bars: 20,
        };
        let report = walk_forward(&GRID, &factory, &data, &BacktestConfig::default(), wf)
            .expect("enough data");

        assert_eq!(report.windows.len(), 3);
        for pair in report.windows.windows(2) {
            assert!(pair[0].test_end < pair[1].test_start);
        }
        for window in &report.windows {
            assert!(window.train_start < window.test_start);
        }
    }

    #[test]
    fn test_windows_start_trading_at_test_start() {
        let data = [trending_series("A", 100)];
        let wf = WalkForwardConfig {
            train_bars: 40,
            test_bars: 20,
        };
        let report = walk_forward(&GRID, &factory, &data, &BacktestConfig::default(), wf)
            .expect("enough data");
        let first = &report.windows[0];
        assert_eq!(
            first.test_start,
            DateTime::from_timestamp(40 * 3_600, 0).expect("valid timestamp")
        );
    }

    #[test]
    fn rejects_too_little_data() {
        let data = [trending_series("A", 10)];
        let wf = WalkForwardConfig {
            train_bars: 8,
            test_bars: 5,
        };
        let err = walk_forward(&GRID, &factory, &data, &BacktestConfig::default(), wf)
            .expect_err("too short");
        assert!(matches!(
            err,
            LabError::NotEnoughData {
                needed: 13,
                available: 10
            }
        ));
    }

    #[test]
    fn rejects_empty_grid() {
        let data = [trending_series("A", 100)];
        let wf = WalkForwardConfig {
            train_bars: 40,
            test_bars: 20,
        };
        let empty: [(usize, usize); 0] = [];
        let err = walk_forward(&empty, &factory, &data, &BacktestConfig::default(), wf)
            .expect_err("empty grid");
        assert!(matches!(err, LabError::EmptyGrid));
    }

    #[test]
    fn stitching_compounds_window_returns() {
        let data = [trending_series("A", 100)];
        let wf = WalkForwardConfig {
            train_bars: 40,
            test_bars: 20,
        };
        let report = walk_forward(&GRID, &factory, &data, &BacktestConfig::default(), wf)
            .expect("enough data");
        let compounded: f64 = report
            .windows
            .iter()
            .map(|w| 1.0 + w.test.strategy.total_return)
            .product();
        assert!((report.out_of_sample.strategy.total_return - (compounded - 1.0)).abs() < 1e-9);
    }

    fn wf_40_20() -> WalkForwardConfig {
        WalkForwardConfig {
            train_bars: 40,
            test_bars: 20,
        }
    }

    #[test]
    fn chosen_params_score_best_on_their_training_window() {
        let data = [trending_series("A", 100)];
        let cfg = BacktestConfig::default();
        let report = walk_forward(&GRID, &factory, &data, &cfg, wf_40_20()).expect("enough data");
        for window in &report.windows {
            let train = slice_all(&data, window.train_start, window.test_start);
            let best = crate::sweep(&GRID, &factory, &train, &cfg);
            assert!((best[0].report.strategy.sharpe - window.train_sharpe).abs() < 1e-12);
        }
    }

    #[test]
    fn later_data_cannot_change_earlier_windows() {
        let original = [trending_series("A", 100)];
        let mut altered = original.clone();
        for bar in &mut altered[0].bars[80..] {
            bar.open *= 3.0;
            bar.high *= 3.0;
            bar.low *= 3.0;
            bar.close *= 3.0;
        }
        let cfg = BacktestConfig::default();
        let a = walk_forward(&GRID, &factory, &original, &cfg, wf_40_20()).expect("enough data");
        let b = walk_forward(&GRID, &factory, &altered, &cfg, wf_40_20()).expect("enough data");

        // Windows 0 and 1 end before bar 80; only window 2 may see the change.
        for i in 0..2 {
            assert_eq!(a.windows[i].params, b.windows[i].params);
            assert!((a.windows[i].test.end_equity - b.windows[i].test.end_equity).abs() < 1e-9);
        }
    }
}
