use std::fmt;

use serde::Serialize;

use crate::types::{Bar, EquityPoint, Fill};

const SECONDS_PER_YEAR: f64 = 365.25 * 86_400.0;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub start_equity: f64,
    pub end_equity: f64,
    pub total_return: f64,
    pub buy_hold_return: f64,
    pub cagr: f64,
    pub max_drawdown: f64,
    pub sharpe: f64,
    pub trades: usize,
    pub fees: f64,
}

impl Report {
    pub fn compute(curve: &[EquityPoint], fills: &[Fill], bars: &[Bar]) -> Self {
        let (Some(first), Some(last)) = (curve.first(), curve.last()) else {
            return Self::default();
        };
        let start = first.equity;
        let end = last.equity;
        let years = (last.ts - first.ts).num_seconds() as f64 / SECONDS_PER_YEAR;

        let buy_hold_return = match (bars.first(), bars.last()) {
            (Some(a), Some(b)) if a.open > 0.0 => b.close / a.open - 1.0,
            _ => 0.0,
        };
        let cagr = if years > 0.0 && start > 0.0 && end > 0.0 {
            (end / start).powf(1.0 / years) - 1.0
        } else {
            0.0
        };
        let bars_per_year = if years > 0.0 {
            (curve.len() - 1) as f64 / years
        } else {
            0.0
        };

        Self {
            start_equity: start,
            end_equity: end,
            total_return: if start > 0.0 { end / start - 1.0 } else { 0.0 },
            buy_hold_return,
            cagr,
            max_drawdown: max_drawdown(curve),
            sharpe: sharpe(curve, bars_per_year),
            trades: fills.len(),
            fees: fills.iter().map(|f| f.fee).sum(),
        }
    }
}

/// Largest peak-to-trough decline, as a positive fraction.
pub fn max_drawdown(curve: &[EquityPoint]) -> f64 {
    let mut peak = f64::MIN;
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
    if std == 0.0 {
        0.0
    } else {
        mean / std * bars_per_year.sqrt()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "  equity        {:>12.2} -> {:.2}",
            self.start_equity, self.end_equity
        )?;
        writeln!(f, "  total return  {:>11.2}%", self.total_return * 100.0)?;
        writeln!(f, "  buy & hold    {:>11.2}%", self.buy_hold_return * 100.0)?;
        writeln!(f, "  CAGR          {:>11.2}%", self.cagr * 100.0)?;
        writeln!(f, "  max drawdown  {:>11.2}%", self.max_drawdown * 100.0)?;
        writeln!(f, "  sharpe        {:>12.2}", self.sharpe)?;
        writeln!(f, "  trades        {:>12}", self.trades)?;
        write!(f, "  fees paid     {:>12.2}", self.fees)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    fn curve(values: &[f64]) -> Vec<EquityPoint> {
        values
            .iter()
            .enumerate()
            .map(|(i, &equity)| EquityPoint {
                ts: DateTime::from_timestamp(i as i64 * 3_600, 0).expect("valid timestamp"),
                equity,
            })
            .collect()
    }

    #[test]
    fn drawdown_uses_running_peak() {
        let dd = max_drawdown(&curve(&[100.0, 120.0, 90.0, 130.0]));
        assert!((dd - 0.25).abs() < 1e-12);
    }

    #[test]
    fn flat_curve_has_zero_sharpe() {
        assert_eq!(sharpe(&curve(&[100.0, 100.0, 100.0]), 8_760.0), 0.0);
    }
}
