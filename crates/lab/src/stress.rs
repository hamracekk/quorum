//! Stress tests (DESIGN §13.5, the gate before any promotion): how a strategy — or a book of
//! slots — behaves in historical crises and in synthetic shocks, judged against the envelope's
//! loss limit.
//!
//! - **Historical windows** ([`HISTORICAL`]): the equity curve sliced to a crisis, next to buy
//!   & hold over the same days.
//! - **Synthetic shocks**: transforms of the bars on a chosen date — an overnight gap
//!   ([`gap`]), an exchange outage that reopens lower ([`outage`]), a volatility spike
//!   ([`vol_spike`]); a liquidity drought is a run with costs multiplied (the caller's config).
//!
//! A scenario passes when the drawdown inside it stays within the limit.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use quorum_core::{Bar, EquityPoint, Series};
use serde::Serialize;

/// Historical crises: name, first day, last day.
pub const HISTORICAL: [(&str, &str, &str); 6] = [
    ("2008 financial crisis", "2008-09-01", "2009-03-31"),
    ("covid crash 2020", "2020-02-19", "2020-04-30"),
    ("rate shock 2022", "2022-01-01", "2022-10-31"),
    ("LUNA collapse 2022", "2022-05-01", "2022-06-30"),
    ("FTX collapse 2022", "2022-11-01", "2022-12-31"),
    ("carry unwind 2024", "2024-07-29", "2024-08-16"),
];

/// How an equity curve fared inside a window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WindowStats {
    /// From the last equity before the window to its last equity.
    pub total_return: f64,
    /// Deepest fall from a peak inside the window (the entry level counts as a peak).
    pub max_drawdown: f64,
    /// Worst return between consecutive points inside the window.
    pub worst_step: f64,
}

/// Stats of `curve` from `from` to `to` (inclusive days). `None` unless the curve has a point
/// before the window (the entry level) and at least one inside it.
#[must_use]
pub fn window(curve: &[EquityPoint], from: NaiveDate, to: NaiveDate) -> Option<WindowStats> {
    let entry = curve
        .iter()
        .rev()
        .find(|p| p.ts.date_naive() < from)?
        .equity;
    let inside: Vec<f64> = curve
        .iter()
        .filter(|p| (from..=to).contains(&p.ts.date_naive()))
        .map(|p| p.equity)
        .collect();
    let last = *inside.last()?;
    if entry <= 0.0 {
        return None;
    }
    let (mut peak, mut max_drawdown, mut worst_step, mut prev) = (entry, 0.0_f64, 0.0_f64, entry);
    for e in inside {
        peak = peak.max(e);
        max_drawdown = max_drawdown.max(1.0 - e / peak);
        if prev > 0.0 {
            worst_step = worst_step.min(e / prev - 1.0);
        }
        prev = e;
    }
    Some(WindowStats {
        total_return: last / entry - 1.0,
        max_drawdown,
        worst_step,
    })
}

fn scaled(bar: &Bar, factor: f64) -> Bar {
    Bar {
        open: bar.open * factor,
        high: bar.high * factor,
        low: bar.low * factor,
        close: bar.close * factor,
        ..*bar
    }
}

/// Every instrument gaps by `factor` (0.7 = −30%) between the close before `at` and the open
/// on `at`; all later prices keep the new level.
#[must_use]
pub fn gap(series: &[Series], at: DateTime<Utc>, factor: f64) -> Vec<Series> {
    series
        .iter()
        .map(|s| {
            let bars = s
                .bars
                .iter()
                .map(|b| if b.ts >= at { scaled(b, factor) } else { *b })
                .collect();
            Series { bars, ..s.clone() }
        })
        .collect()
}

/// The venue goes dark: no bars for `days` days from `at`, then trading reopens with every
/// price at `factor` of where it would have been.
#[must_use]
pub fn outage(series: &[Series], at: DateTime<Utc>, days: i64, factor: f64) -> Vec<Series> {
    let reopen = at + Duration::days(days);
    series
        .iter()
        .map(|s| {
            let bars = s
                .bars
                .iter()
                .filter(|b| b.ts < at || b.ts >= reopen)
                .map(|b| {
                    if b.ts >= reopen {
                        scaled(b, factor)
                    } else {
                        *b
                    }
                })
                .collect();
            Series { bars, ..s.clone() }
        })
        .collect()
}

/// For `days` days from `at`, each bar's move from the previous close is multiplied by
/// `multiplier` (log terms, so prices stay positive); later bars keep the level reached.
#[must_use]
pub fn vol_spike(series: &[Series], at: DateTime<Utc>, days: i64, multiplier: f64) -> Vec<Series> {
    let end = at + Duration::days(days);
    series
        .iter()
        .map(|s| {
            let mut out: Vec<Bar> = Vec::with_capacity(s.bars.len());
            let mut level = 1.0_f64; // shocked price / original price
            let mut prev_close: Option<f64> = None;
            for b in &s.bars {
                if b.ts >= at
                    && b.ts < end
                    && let Some(prev) = prev_close.filter(|p| *p > 0.0 && b.close > 0.0)
                {
                    let log = (b.close / prev).ln();
                    level *= (log * (multiplier - 1.0)).exp();
                }
                prev_close = Some(b.close);
                out.push(scaled(b, level));
            }
            Series {
                bars: out,
                ..s.clone()
            }
        })
        .collect()
}

/// One scenario's result for one subject (a slot or the book).
#[derive(Debug, Clone, Serialize)]
pub struct StressRow {
    pub scenario: String,
    pub subject: String,
    pub strategy: WindowStats,
    /// Buy & hold over the same window, when there is one to compare.
    pub hold: Option<WindowStats>,
    /// The drawdown inside the scenario stayed within `limit`.
    pub passes: bool,
}

impl StressRow {
    #[must_use]
    pub fn new(
        scenario: &str,
        subject: &str,
        strategy: WindowStats,
        hold: Option<WindowStats>,
        limit: f64,
    ) -> Self {
        Self {
            scenario: scenario.to_owned(),
            subject: subject.to_owned(),
            strategy,
            hold,
            passes: strategy.max_drawdown <= limit,
        }
    }
}

/// Parses a [`HISTORICAL`] date.
#[must_use]
pub fn day(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use quorum_core::InstrumentId;

    use super::*;

    fn t(d: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
            .single()
            .expect("valid")
            + Duration::days(d)
    }

    fn flat(n: i64) -> Series {
        let bars = (0..n)
            .map(|d| Bar {
                ts: t(d),
                open: 100.0,
                high: 100.0,
                low: 100.0,
                close: 100.0,
                volume: 1.0,
            })
            .collect();
        Series::new(InstrumentId::spot("test", "A").expect("valid"), bars)
    }

    fn curve(values: &[f64]) -> Vec<EquityPoint> {
        values
            .iter()
            .enumerate()
            .map(|(i, &e)| EquityPoint {
                ts: t(i64::try_from(i).expect("small")),
                equity: e,
            })
            .collect()
    }

    #[test]
    fn window_measures_from_the_entry_level() {
        // Entry 100 (day 0), inside days 1..=3: 90, 120, 108.
        let c = curve(&[100.0, 90.0, 120.0, 108.0, 50.0]);
        let s = window(&c, t(1).date_naive(), t(3).date_naive()).expect("covered");
        assert!((s.total_return - 0.08).abs() < 1e-12);
        assert!(
            (s.max_drawdown - 0.1).abs() < 1e-12,
            "100 → 90 counts; 120 → 108 too"
        );
        assert!((s.worst_step + 0.1).abs() < 1e-12);
        assert!(
            window(&c, t(0).date_naive(), t(2).date_naive()).is_none(),
            "no entry level"
        );
    }

    #[test]
    fn a_gap_moves_every_price_from_the_shock_on() {
        let shocked = gap(&[flat(10)], t(5), 0.7);
        assert!((shocked[0].bars[4].close - 100.0).abs() < 1e-12);
        assert!((shocked[0].bars[5].open - 70.0).abs() < 1e-12);
        assert!((shocked[0].bars[9].close - 70.0).abs() < 1e-12);
    }

    #[test]
    fn an_outage_removes_bars_then_reopens_lower() {
        let shocked = outage(&[flat(20)], t(5), 5, 0.85);
        let days: Vec<i64> = shocked[0]
            .bars
            .iter()
            .map(|b| (b.ts - t(0)).num_days())
            .collect();
        assert!(!days.iter().any(|d| (5..10).contains(d)), "dark days 5..10");
        let reopen = shocked[0]
            .bars
            .iter()
            .find(|b| b.ts == t(10))
            .expect("reopened");
        assert!((reopen.open - 85.0).abs() < 1e-12);
    }

    #[test]
    fn a_vol_spike_amplifies_moves_inside_the_window_only() {
        let mut s = flat(10);
        for (i, b) in s.bars.iter_mut().enumerate() {
            b.close = if i % 2 == 0 { 100.0 } else { 110.0 };
        }
        let shocked = vol_spike(&[s.clone()], t(2), 4, 3.0);
        let original = (s.bars[3].close / s.bars[2].close).ln();
        let amplified = (shocked[0].bars[3].close / shocked[0].bars[2].close).ln();
        assert!(
            3.0_f64.mul_add(-original, amplified).abs() < 1e-9,
            "{amplified} vs 3 × {original}"
        );
        assert!(
            (shocked[0].bars[1].close - s.bars[1].close).abs() < 1e-12,
            "before: untouched"
        );
        assert!(shocked[0].bars.iter().all(|b| b.close > 0.0));
    }

    #[test]
    fn rows_pass_within_the_limit() {
        let ok = WindowStats {
            total_return: -0.1,
            max_drawdown: 0.12,
            worst_step: -0.05,
        };
        assert!(StressRow::new("x", "slot", ok, None, 0.15).passes);
        let bad = WindowStats {
            max_drawdown: 0.2,
            ..ok
        };
        assert!(!StressRow::new("x", "slot", bad, None, 0.15).passes);
        assert!(day(HISTORICAL[0].1).is_some());
    }
}
