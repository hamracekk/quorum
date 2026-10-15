//! Market regimes (DESIGN §7.3): a small, interpretable label per market and date — trending,
//! choppy, risk-off, crisis — from the market's own bars.
//!
//! The market is a slot's universe, equal-weighted, so the same rules label crypto, equities,
//! bonds or a cross-asset book. Features at date `t` use bars up to `t` only. The thresholds
//! are fixed in advance ([`RegimeRule::default`]), not fitted, so a study of whether the label
//! helps is not a study of how well it was tuned.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::Series;

/// A market's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Regime {
    /// Most instruments rising, volatility normal.
    Trending,
    /// No clear direction.
    Choppy,
    /// Volatility elevated or most instruments falling.
    RiskOff,
    /// Volatility far above normal and the market deep below its peak.
    Crisis,
}

impl Regime {
    pub const ALL: [Self; 4] = [Self::Trending, Self::Choppy, Self::RiskOff, Self::Crisis];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trending => "trending",
            Self::Choppy => "choppy",
            Self::RiskOff => "risk-off",
            Self::Crisis => "crisis",
        }
    }
}

/// Fixed thresholds and windows. Checked in order: crisis, risk-off, trending, else choppy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegimeRule {
    /// Bars behind the short volatility.
    pub vol_short_bars: usize,
    /// Bars behind breadth: an instrument is "up" if its close is above the close this many
    /// bars ago.
    pub breadth_bars: usize,
    /// Bars behind the trend (the market's return).
    pub trend_bars: usize,
    /// Crisis: short/long volatility at least this …
    pub crisis_vol_ratio: f64,
    /// … and the market at least this far below its one-year peak.
    pub crisis_drawdown: f64,
    /// Risk-off: short/long volatility at least this, or breadth at most `risk_off_breadth`.
    pub risk_off_vol_ratio: f64,
    pub risk_off_breadth: f64,
    /// Trending: breadth at least this.
    pub trending_breadth: f64,
}

impl Default for RegimeRule {
    fn default() -> Self {
        Self {
            vol_short_bars: 20,
            breadth_bars: 60,
            trend_bars: 120,
            crisis_vol_ratio: 1.5,
            crisis_drawdown: 0.2,
            risk_off_vol_ratio: 1.25,
            risk_off_breadth: 0.35,
            trending_breadth: 0.65,
        }
    }
}

/// What the label is read from, at one date.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RegimeFeatures {
    /// Short over one-year volatility of the equal-weighted market's returns.
    pub vol_ratio: f64,
    /// Fraction the market sits below its one-year peak (0 = at the peak).
    pub drawdown: f64,
    /// Share of instruments above their close `breadth_bars` ago.
    pub breadth: f64,
    /// The market's return over `trend_bars`.
    pub trend: f64,
}

impl RegimeRule {
    #[must_use]
    pub fn classify(&self, f: &RegimeFeatures) -> Regime {
        if f.vol_ratio >= self.crisis_vol_ratio && f.drawdown >= self.crisis_drawdown {
            Regime::Crisis
        } else if f.vol_ratio >= self.risk_off_vol_ratio || f.breadth <= self.risk_off_breadth {
            Regime::RiskOff
        } else if f.breadth >= self.trending_breadth {
            Regime::Trending
        } else {
            Regime::Choppy
        }
    }
}

/// One labelled date.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RegimePoint {
    pub ts: DateTime<Utc>,
    /// The equal-weighted market's return into this date (close to close).
    pub market_return: f64,
    pub features: RegimeFeatures,
    pub regime: Regime,
}

/// The equal-weighted market of `series` on the union of their timestamps: per date, the mean
/// close-to-close return of the instruments with a bar on it and on the date before.
#[must_use]
pub fn market_returns(series: &[Series]) -> Vec<(DateTime<Utc>, f64, usize)> {
    let calendar: Vec<DateTime<Utc>> = series
        .iter()
        .flat_map(|s| s.bars.iter().map(|b| b.ts))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let closes: Vec<BTreeMap<DateTime<Utc>, f64>> = series
        .iter()
        .map(|s| s.bars.iter().map(|b| (b.ts, b.close)).collect())
        .collect();
    calendar
        .windows(2)
        .map(|w| {
            let (sum, n) = closes
                .iter()
                .filter_map(|c| match (c.get(&w[0]), c.get(&w[1])) {
                    (Some(&a), Some(&b)) if a > 0.0 && b > 0.0 => Some(b / a - 1.0),
                    _ => None,
                })
                .fold((0.0, 0_usize), |(s, n), r| (s + r, n + 1));
            (w[1], if n > 0 { sum / n as f64 } else { 0.0 }, n)
        })
        .collect()
}

fn std_dev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

/// Labels every date of `series` with enough history (a year of `year_bars` plus warm-up).
/// Point in time: the label at a date uses bars up to it only.
#[must_use]
pub fn regimes(series: &[Series], rule: &RegimeRule, year_bars: usize) -> Vec<RegimePoint> {
    let market = market_returns(series);
    let closes: Vec<BTreeMap<DateTime<Utc>, f64>> = series
        .iter()
        .map(|s| s.bars.iter().map(|b| (b.ts, b.close)).collect())
        .collect();
    let dates: Vec<DateTime<Utc>> = market.iter().map(|m| m.0).collect();
    let returns: Vec<f64> = market.iter().map(|m| m.1).collect();
    let mut level = Vec::with_capacity(returns.len());
    let mut current = 1.0;
    for r in &returns {
        current *= 1.0 + r;
        level.push(current);
    }
    let warmup = year_bars.max(rule.trend_bars).max(rule.breadth_bars);
    let mut out = Vec::new();
    for k in warmup..dates.len() {
        let long = std_dev(&returns[k + 1 - year_bars..=k]);
        let short = std_dev(&returns[k + 1 - rule.vol_short_bars.min(year_bars)..=k]);
        let peak = level[k + 1 - year_bars..=k]
            .iter()
            .copied()
            .fold(f64::MIN, f64::max);
        let back = dates[k - rule.breadth_bars];
        let (up, counted) = closes.iter().fold((0_usize, 0_usize), |(up, n), c| {
            match (c.get(&dates[k]), c.get(&back)) {
                (Some(&now), Some(&then)) if then > 0.0 => (up + usize::from(now > then), n + 1),
                _ => (up, n),
            }
        });
        if counted == 0 || long <= 0.0 {
            continue;
        }
        let features = RegimeFeatures {
            vol_ratio: short / long,
            drawdown: 1.0 - level[k] / peak,
            breadth: up as f64 / counted as f64,
            trend: level[k] / level[k - rule.trend_bars] - 1.0,
        };
        out.push(RegimePoint {
            ts: dates[k],
            market_return: returns[k],
            features,
            regime: rule.classify(&features),
        });
    }
    out
}

#[cfg(test)]
mod tests;
