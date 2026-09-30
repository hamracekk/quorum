use std::fmt;

use serde::Serialize;

use crate::types::{EquityPoint, Fill};

const SECONDS_PER_YEAR: f64 = 365.25 * 86_400.0;

/// Return and risk statistics of one equity curve.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Performance {
    pub total_return: f64,
    pub cagr: f64,
    pub max_drawdown: f64,
    pub sharpe: f64,
}

impl Performance {
    /// Statistics of `curve` relative to the starting capital `initial`.
    pub fn from_curve(initial: f64, curve: &[EquityPoint]) -> Self {
        let (Some(first), Some(last)) = (curve.first(), curve.last()) else {
            return Self::default();
        };
        let years = (last.ts - first.ts).num_seconds() as f64 / SECONDS_PER_YEAR;
        let growth = if initial > 0.0 {
            last.equity / initial
        } else {
            1.0
        };
        let cagr = if years > 0.0 && growth > 0.0 {
            growth.powf(1.0 / years) - 1.0
        } else {
            0.0
        };
        let bars_per_year = if years > 0.0 {
            (curve.len() - 1) as f64 / years
        } else {
            0.0
        };
        Self {
            total_return: growth - 1.0,
            cagr,
            max_drawdown: max_drawdown(initial, curve),
            sharpe: sharpe(curve, bars_per_year),
        }
    }
}

/// Outcome of a backtest: the strategy next to the pure buy & hold benchmark.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub initial_equity: f64,
    pub end_equity: f64,
    pub strategy: Performance,
    /// Equal-weight buy & hold of the same symbols: no fees, no risk limits.
    pub benchmark: Performance,
    pub trades: usize,
    pub fees: f64,
}

impl Report {
    pub fn compute(
        initial: f64,
        curve: &[EquityPoint],
        benchmark_curve: &[EquityPoint],
        fills: &[Fill],
    ) -> Self {
        Self {
            initial_equity: initial,
            end_equity: curve.last().map_or(initial, |p| p.equity),
            strategy: Performance::from_curve(initial, curve),
            benchmark: Performance::from_curve(initial, benchmark_curve),
            trades: fills.len(),
            fees: fills.iter().map(|f| f.fee).sum(),
        }
    }
}

/// Largest peak-to-trough decline as a positive fraction, starting from `initial`.
pub fn max_drawdown(initial: f64, curve: &[EquityPoint]) -> f64 {
    let mut peak = initial;
    let mut worst = 0.0_f64;
    for point in curve {
        peak = peak.max(point.equity);
        if peak > 0.0 {
            worst = worst.max((peak - point.equity) / peak);
        }
    }
    worst
}

/// Annualised Sharpe ratio of per-bar returns (risk-free rate = 0).
pub fn sharpe(curve: &[EquityPoint], bars_per_year: f64) -> f64 {
    let returns: Vec<f64> = curve
        .windows(2)
        .filter(|w| w[0].equity > 0.0)
        .map(|w| w[1].equity / w[0].equity - 1.0)
        .collect();
    if returns.len() < 2 {
        return 0.0;
    }
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std = variance.sqrt();
    if std > 0.0 {
        mean / std * bars_per_year.sqrt()
    } else {
        0.0
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (s, b) = (&self.strategy, &self.benchmark);
        writeln!(f, "                  strategy   buy & hold")?;
        writeln!(
            f,
            "  total return  {:>9.2}%  {:>9.2}%",
            s.total_return * 100.0,
            b.total_return * 100.0
        )?;
        writeln!(
            f,
            "  CAGR          {:>9.2}%  {:>9.2}%",
            s.cagr * 100.0,
            b.cagr * 100.0
        )?;
        writeln!(
            f,
            "  max drawdown  {:>9.2}%  {:>9.2}%",
            s.max_drawdown * 100.0,
            b.max_drawdown * 100.0
        )?;
        writeln!(f, "  sharpe        {:>10.2}  {:>10.2}", s.sharpe, b.sharpe)?;
        writeln!(
            f,
            "  equity        {:.2} -> {:.2}",
            self.initial_equity, self.end_equity
        )?;
        write!(
            f,
            "  trades        {}  (fees {:.2})",
            self.trades, self.fees
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    fn curve(values: &[f64]) -> Vec<EquityPoint> {
        values
            .iter()
            .zip(0_i64..)
            .map(|(&equity, i)| EquityPoint {
                ts: DateTime::from_timestamp(i * 3_600, 0).expect("valid timestamp"),
                equity,
            })
            .collect()
    }

    #[test]
    fn drawdown_uses_running_peak() {
        let dd = max_drawdown(100.0, &curve(&[100.0, 120.0, 90.0, 130.0]));
        assert!((dd - 0.25).abs() < 1e-12);
    }

    #[test]
    fn drawdown_counts_loss_from_initial_capital() {
        let dd = max_drawdown(100.0, &curve(&[80.0, 90.0]));
        assert!((dd - 0.2).abs() < 1e-12);
    }

    #[test]
    fn flat_curve_has_zero_sharpe() {
        assert!(sharpe(&curve(&[100.0, 100.0, 100.0]), 8_760.0).abs() < 1e-12);
    }

    #[test]
    fn total_return_is_relative_to_initial() {
        let perf = Performance::from_curve(100.0, &curve(&[110.0, 120.0]));
        assert!((perf.total_return - 0.2).abs() < 1e-12);
    }

    #[test]
    fn empty_curve_is_neutral() {
        let perf = Performance::from_curve(100.0, &[]);
        assert!(perf.total_return.abs() < 1e-12);
    }

    #[test]
    fn cagr_over_exactly_one_year_equals_total_return() {
        let year = 31_557_600; // 365.25 days in seconds
        let curve = [
            EquityPoint {
                ts: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
                equity: 100.0,
            },
            EquityPoint {
                ts: DateTime::from_timestamp(year, 0).expect("valid timestamp"),
                equity: 110.0,
            },
        ];
        let perf = Performance::from_curve(100.0, &curve);
        assert!((perf.cagr - 0.1).abs() < 1e-9);
    }

    #[test]
    fn report_prints_strategy_next_to_benchmark() {
        let report = Report::compute(100.0, &curve(&[100.0, 110.0]), &curve(&[100.0, 105.0]), &[]);
        let text = report.to_string();
        assert!(text.contains("strategy") && text.contains("buy & hold"));
        assert!(text.contains("10.00%") && text.contains("5.00%"), "{text}");
    }
}
