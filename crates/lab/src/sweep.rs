use quorum_core::{BacktestConfig, Report, Series, run};
use serde::Serialize;

use crate::StrategyFactory;

#[derive(Debug, Clone, Serialize)]
pub struct SweepRow<P> {
    pub params: P,
    pub report: Report,
}

/// Backtest every parameter set over the full period, best Sharpe first.
///
/// In-sample only: the top row is the luckiest fit, not a forecast. Use
/// [`walk_forward`](crate::walk_forward) to judge parameters honestly.
pub fn sweep<P: Clone>(
    grid: &[P],
    factory: &impl StrategyFactory<P>,
    series: &[Series],
    cfg: &BacktestConfig,
) -> Vec<SweepRow<P>> {
    let mut rows: Vec<SweepRow<P>> = grid
        .iter()
        .map(|params| {
            let mut strategy = factory.build(params);
            SweepRow {
                params: params.clone(),
                report: run(strategy.as_mut(), series, cfg).report,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.report
            .strategy
            .sharpe
            .total_cmp(&a.report.strategy.sharpe)
    });
    rows
}

#[cfg(test)]
mod tests {
    use quorum_core::Strategy;
    use quorum_core::strategies::SmaCross;

    use super::*;
    use crate::walk_forward::tests::trending_series;

    #[test]
    fn rows_are_sorted_by_sharpe_and_cover_the_grid() {
        let grid = [(2, 5), (3, 8), (5, 13)];
        let factory = |&(fast, slow): &(usize, usize)| -> Box<dyn Strategy> {
            Box::new(SmaCross::new(fast, slow, false))
        };
        let data = [trending_series("A", 200)];
        let rows = sweep(&grid, &factory, &data, &BacktestConfig::default());

        assert_eq!(rows.len(), grid.len());
        assert!(
            rows.windows(2)
                .all(|w| w[0].report.strategy.sharpe >= w[1].report.strategy.sharpe)
        );
    }

    #[test]
    fn empty_grid_yields_no_rows() {
        let factory = |&(fast, slow): &(usize, usize)| -> Box<dyn Strategy> {
            Box::new(SmaCross::new(fast, slow, false))
        };
        let empty: [(usize, usize); 0] = [];
        let data = [trending_series("A", 50)];
        assert!(sweep(&empty, &factory, &data, &BacktestConfig::default()).is_empty());
    }
}
