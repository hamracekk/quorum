//! Data quality checks: a bad bar silently corrupts every backtest that reads it.
//!
//! [`check`] scans one series for missing bars (gaps longer than one interval), impossible
//! candles (high below low, open or close outside the range), non-positive prices, outlier
//! moves (a close more than [`OUTLIER_MOVE`] away from the previous one in one bar) and stale
//! runs (at least [`STALE_RUN`] identical closes with zero volume — a feed that stopped
//! updating). It reports; it never edits data.

use chrono::{DateTime, Duration, Utc};
use quorum_core::{InstrumentId, Series};
use serde::Serialize;

/// A single-bar move this large (as a fraction) is flagged.
pub const OUTLIER_MOVE: f64 = 0.5;
/// Identical closes on zero volume this many bars in a row are flagged.
pub const STALE_RUN: usize = 5;

/// What one series looks like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualityReport {
    pub instrument: InstrumentId,
    pub bars: usize,
    pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>,
    /// Bars missing between stored ones.
    pub missing_bars: usize,
    /// The longest gap: where it starts and how many bars are missing.
    pub largest_gap: Option<(DateTime<Utc>, usize)>,
    pub bad_candles: usize,
    pub non_positive: usize,
    pub outliers: usize,
    pub stale_runs: usize,
    pub zero_volume: usize,
}

impl QualityReport {
    /// No errors (gaps count: a missing bar changes indicators and fills). Large moves are
    /// not errors — crypto has real ±50% days — but are listed to look at.
    pub const fn clean(&self) -> bool {
        self.missing_bars == 0
            && self.bad_candles == 0
            && self.non_positive == 0
            && self.stale_runs == 0
    }
}

/// Check `series`, whose bars are `interval` apart.
pub fn check(series: &Series, interval: Duration) -> QualityReport {
    let bars = &series.bars;
    let mut report = QualityReport {
        instrument: series.instrument.clone(),
        bars: bars.len(),
        first: bars.first().map(|b| b.ts),
        last: bars.last().map(|b| b.ts),
        missing_bars: 0,
        largest_gap: None,
        bad_candles: 0,
        non_positive: 0,
        outliers: 0,
        stale_runs: 0,
        zero_volume: 0,
    };
    let step = interval.num_seconds().max(1);
    let mut run = 0_usize;
    for (i, bar) in bars.iter().enumerate() {
        let prices = [bar.open, bar.high, bar.low, bar.close];
        if prices.iter().any(|p| *p <= 0.0 || !p.is_finite()) {
            report.non_positive += 1;
        }
        if bar.high < bar.low
            || bar.open > bar.high
            || bar.open < bar.low
            || bar.close > bar.high
            || bar.close < bar.low
        {
            report.bad_candles += 1;
        }
        if bar.volume <= 0.0 {
            report.zero_volume += 1;
        }
        let Some(previous) = i.checked_sub(1).map(|p| &bars[p]) else {
            continue;
        };
        let missing = usize::try_from((bar.ts - previous.ts).num_seconds() / step - 1).unwrap_or(0);
        if missing > 0 {
            report.missing_bars += missing;
            if report.largest_gap.is_none_or(|(_, worst)| missing > worst) {
                report.largest_gap = Some((previous.ts, missing));
            }
        }
        if previous.close > 0.0 && ((bar.close / previous.close) - 1.0).abs() > OUTLIER_MOVE {
            report.outliers += 1;
        }
        if bar.volume <= 0.0 && (bar.close - previous.close).abs() <= f64::EPSILON {
            run += 1;
            if run + 1 == STALE_RUN {
                report.stale_runs += 1;
            }
        } else {
            run = 0;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use quorum_core::Bar;

    use super::*;

    fn bar(day: i64, open: f64, high: f64, low: f64, close: f64, volume: f64) -> Bar {
        Bar {
            ts: DateTime::from_timestamp(day * 86_400, 0).expect("ts"),
            open,
            high,
            low,
            close,
            volume,
        }
    }

    #[test]
    fn every_kind_of_problem_is_counted() {
        let mut bars = vec![
            bar(0, 10.0, 11.0, 9.0, 10.0, 5.0),
            bar(1, 10.0, 9.0, 11.0, 10.0, 5.0),  // high below low
            bar(4, 10.0, 11.0, 9.0, 10.0, 5.0),  // two bars missing before it
            bar(5, 10.0, 30.0, 9.0, 25.0, 5.0),  // +150% in a bar
            bar(6, -1.0, 26.0, -1.0, 25.0, 5.0), // negative price
        ];
        for d in 7..13 {
            bars.push(bar(d, 25.0, 25.0, 25.0, 25.0, 0.0)); // a dead feed
        }
        let r = check(
            &Series::new(InstrumentId::spot("test", "A").expect("id"), bars),
            Duration::days(1),
        );
        assert_eq!(r.missing_bars, 2);
        assert_eq!(r.largest_gap.map(|(_, n)| n), Some(2));
        assert_eq!(r.bad_candles, 1);
        assert_eq!(r.outliers, 1);
        assert_eq!(r.non_positive, 1);
        assert_eq!(r.stale_runs, 1);
        assert_eq!(r.zero_volume, 6);
        assert!(!r.clean());
    }

    #[test]
    fn a_clean_series_is_clean() {
        let bars = (0..30)
            .map(|d| bar(d, 10.0, 11.0, 9.0, 10.5, 1.0))
            .collect();
        let r = check(
            &Series::new(InstrumentId::spot("test", "A").expect("id"), bars),
            Duration::days(1),
        );
        assert!(r.clean(), "{r:?}");
    }
}
