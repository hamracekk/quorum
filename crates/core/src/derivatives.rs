//! Derivatives data as sources (E-2): perpetual-futures funding rates turned into calls.
//!
//! A perpetual future's funding rate is what longs pay shorts (positive) or shorts pay longs
//! (negative) every funding period, so it measures how crowded one side is. Two contrarian
//! rules, both firing only when the rate **enters** an extreme (a long spell of extreme funding
//! is one call, not one per period):
//!
//! - **z-score:** the rate is `z` standard deviations from its trailing `window` mean →
//!   a call against the crowd (`-sign(z)`), strength `|z| / 4` capped at 1;
//! - **level:** the rate rises to `high` or above (longs pay a lot) → bearish; falls to `low`
//!   or below (shorts pay) → bullish.
//!
//! A rate is known at its funding time, so that is when a call can first be traded. Whether
//! these calls carry an edge is the scoreboard's job, like any source's.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One settled funding rate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FundingPoint {
    /// Funding time (when the rate settled and became known).
    pub ts: DateTime<Utc>,
    /// Fraction per funding period (`0.0001` = 0.01%).
    pub rate: f64,
}

/// When funding counts as extreme.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FundingRule {
    /// Trailing funding periods behind the z-score.
    pub window: usize,
    /// |z| at or above which the rate is extreme.
    pub z: f64,
    /// Rate at or above which longs are crowded (per period).
    pub high: f64,
    /// Rate at or below which shorts are crowded (per period).
    pub low: f64,
}

impl Default for FundingRule {
    fn default() -> Self {
        Self {
            window: 90,
            z: 2.5,
            high: 0.0005,
            low: -0.0001,
        }
    }
}

/// A call from funding.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FundingCall {
    pub ts: DateTime<Utc>,
    /// `1.0` bullish, `-1.0` bearish.
    pub direction: f64,
    pub strength: f64,
    /// The rate behind the call.
    pub rate: f64,
}

/// Contrarian calls when the rate's trailing z-score enters `|z| >= rule.z`.
pub fn z_calls(points: &[FundingPoint], rule: &FundingRule) -> Vec<FundingCall> {
    let window = rule.window.max(2);
    let mut calls = Vec::new();
    let mut extreme = false;
    for (i, point) in points.iter().enumerate().skip(window) {
        let past = &points[i - window..i];
        let n = past.len() as f64;
        let mean = past.iter().map(|p| p.rate).sum::<f64>() / n;
        let var = past.iter().map(|p| (p.rate - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let sd = var.sqrt();
        let z = if sd > 0.0 {
            (point.rate - mean) / sd
        } else {
            0.0
        };
        let now = z.abs() >= rule.z;
        if now && !extreme {
            calls.push(FundingCall {
                ts: point.ts,
                direction: -z.signum(),
                strength: (z.abs() / 4.0).min(1.0),
                rate: point.rate,
            });
        }
        extreme = now;
    }
    calls
}

/// Contrarian calls when the rate enters `>= rule.high` (bearish) or `<= rule.low` (bullish).
pub fn level_calls(points: &[FundingPoint], rule: &FundingRule) -> Vec<FundingCall> {
    let mut calls = Vec::new();
    let mut zone = 0.0_f64;
    for point in points {
        let now = if point.rate >= rule.high {
            -1.0
        } else if point.rate <= rule.low {
            1.0
        } else {
            0.0
        };
        if now != 0.0 && (now - zone).abs() > f64::EPSILON {
            calls.push(FundingCall {
                ts: point.ts,
                direction: now,
                strength: 1.0,
                rate: point.rate,
            });
        }
        zone = now;
    }
    calls
}

/// The rate as an annual fraction, for display (`periods_per_day` = 3 for 8-hour funding).
pub fn annualised(rate: f64, periods_per_day: f64) -> f64 {
    rate * periods_per_day * 365.0
}

#[cfg(test)]
mod tests;
