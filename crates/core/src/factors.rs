//! Factors (DESIGN §7).
//!
//! A factor is a number per instrument and time that might say something about the instrument's
//! next returns. It proposes and trades nothing; it starts with zero weight and earns one only
//! through an out-of-sample study (`lab::factors`).
//!
//! **Point in time.** A factor's value at bar `t` is computed from bars up to and including `t`
//! (all closed): nothing later can reach it, and the study enters at bar `t + 1`.
//!
//! This module holds the price & trend family, computed from bars alone. Families from other
//! data (structure, news, macro) follow the same contract with their own inputs.

use std::fmt;

use crate::instrument::InstrumentId;
use crate::types::Bar;

/// A factor's name and version. A change to how it is computed bumps the version, and records
/// are kept per version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FactorId {
    pub name: &'static str,
    pub version: u32,
}

impl fmt::Display for FactorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// A factor computed from an instrument's bars.
pub trait PriceFactor: fmt::Debug + Send + Sync {
    fn id(&self) -> FactorId;

    /// What a higher value is meant to say, for the reader; the study measures the truth.
    fn meaning(&self) -> &'static str;

    /// Bars needed before the first value.
    fn warmup(&self) -> usize;

    /// The value at the last of `bars` (oldest first, all closed). `None` before warm-up or
    /// when the data cannot give a number (zero prices, no volume).
    fn value(&self, bars: &[Bar]) -> Option<f64>;
}

/// The last `n + 1` closes, or `None` with too few or a non-positive price among them.
fn closes(bars: &[Bar], n: usize) -> Option<Vec<f64>> {
    let window = bars.get(bars.len().checked_sub(n + 1)?..)?;
    let closes: Vec<f64> = window.iter().map(|b| b.close).collect();
    closes
        .iter()
        .all(|c| *c > 0.0 && c.is_finite())
        .then_some(closes)
}

fn log_returns(closes: &[f64]) -> Vec<f64> {
    closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect()
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Sample standard deviation; `None` for fewer than two values or no dispersion.
fn std_dev(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    let sd = var.sqrt();
    (sd > 0.0 && sd.is_finite()).then_some(sd)
}

/// Return over `lookback` bars: `close_t / close_(t−lookback) − 1`.
#[derive(Debug, Clone, Copy)]
pub struct Momentum {
    pub lookback: usize,
}

/// Return over `lookback` bars divided by the volatility over the same bars: a trend's strength
/// in standard deviations (a calm rise scores higher than a jumpy one of the same size).
#[derive(Debug, Clone, Copy)]
pub struct TrendStrength {
    pub lookback: usize,
}

/// Annualised volatility of log returns over `window` bars (the low-volatility effect, if any).
#[derive(Debug, Clone, Copy)]
pub struct RealizedVol {
    pub window: usize,
    pub periods_per_year: f64,
}

/// Distance of the close from its `window`-bar mean in standard deviations (stretched up or
/// down, i.e. a mean-reversion candidate).
#[derive(Debug, Clone, Copy)]
pub struct MeanReversionZ {
    pub window: usize,
}

/// Log of the last bar's volume over its `window`-bar mean before it (unusual activity).
#[derive(Debug, Clone, Copy)]
pub struct VolumeSurprise {
    pub window: usize,
}

const fn id(name: &'static str) -> FactorId {
    FactorId { name, version: 1 }
}

impl PriceFactor for Momentum {
    fn id(&self) -> FactorId {
        match self.lookback {
            5 => id("reversal_5"),
            20 => id("momentum_20"),
            60 => id("momentum_60"),
            120 => id("momentum_120"),
            _ => id("momentum"),
        }
    }

    fn meaning(&self) -> &'static str {
        "past return over the lookback"
    }

    fn warmup(&self) -> usize {
        self.lookback + 1
    }

    fn value(&self, bars: &[Bar]) -> Option<f64> {
        let c = closes(bars, self.lookback)?;
        Some(c.last()? / c.first()? - 1.0)
    }
}

impl PriceFactor for TrendStrength {
    fn id(&self) -> FactorId {
        id(if self.lookback == 60 {
            "trend_60"
        } else {
            "trend"
        })
    }

    fn meaning(&self) -> &'static str {
        "return over the lookback in volatility units"
    }

    fn warmup(&self) -> usize {
        self.lookback + 1
    }

    fn value(&self, bars: &[Bar]) -> Option<f64> {
        let r = log_returns(&closes(bars, self.lookback)?);
        let sd = std_dev(&r)?;
        Some(r.iter().sum::<f64>() / (sd * (r.len() as f64).sqrt()))
    }
}

impl PriceFactor for RealizedVol {
    fn id(&self) -> FactorId {
        id(if self.window == 30 { "vol_30" } else { "vol" })
    }

    fn meaning(&self) -> &'static str {
        "annualised realised volatility"
    }

    fn warmup(&self) -> usize {
        self.window + 1
    }

    fn value(&self, bars: &[Bar]) -> Option<f64> {
        let r = log_returns(&closes(bars, self.window)?);
        Some(std_dev(&r)? * self.periods_per_year.sqrt())
    }
}

impl PriceFactor for MeanReversionZ {
    fn id(&self) -> FactorId {
        id(if self.window == 20 {
            "stretch_20"
        } else {
            "stretch"
        })
    }

    fn meaning(&self) -> &'static str {
        "close above its mean, in standard deviations"
    }

    fn warmup(&self) -> usize {
        self.window
    }

    fn value(&self, bars: &[Bar]) -> Option<f64> {
        let c = closes(bars, self.window.checked_sub(1)?)?;
        Some((c.last()? - mean(&c)) / std_dev(&c)?)
    }
}

impl PriceFactor for VolumeSurprise {
    fn id(&self) -> FactorId {
        id(if self.window == 30 {
            "volume_surprise_30"
        } else {
            "volume_surprise"
        })
    }

    fn meaning(&self) -> &'static str {
        "volume far above its recent mean"
    }

    fn warmup(&self) -> usize {
        self.window + 1
    }

    fn value(&self, bars: &[Bar]) -> Option<f64> {
        let (last, before) = bars.split_last()?;
        let window = before.get(before.len().checked_sub(self.window)?..)?;
        let avg = window.iter().map(|b| b.volume).sum::<f64>() / self.window as f64;
        (avg > 0.0 && last.volume > 0.0).then(|| (last.volume / avg).ln())
    }
}

/// The price & trend family, for `periods_per_year` bars a year (from the slot's calendar: 365
/// daily bars for crypto, about 252 for exchange-traded markets).
#[must_use]
pub fn price_family(periods_per_year: f64) -> Vec<Box<dyn PriceFactor>> {
    vec![
        Box::new(Momentum { lookback: 5 }),
        Box::new(Momentum { lookback: 20 }),
        Box::new(Momentum { lookback: 60 }),
        Box::new(Momentum { lookback: 120 }),
        Box::new(TrendStrength { lookback: 60 }),
        Box::new(RealizedVol {
            window: 30,
            periods_per_year,
        }),
        Box::new(MeanReversionZ { window: 20 }),
        Box::new(VolumeSurprise { window: 30 }),
    ]
}

/// Cross-sectional ranks scaled to `[−1, 1]` (ties share their average rank): comparable across
/// dates and robust to outliers. Fewer than two values give no ranks.
#[must_use]
pub fn rank_scores(values: &[(InstrumentId, f64)]) -> Vec<(InstrumentId, f64)> {
    let n = values.len();
    if n < 2 {
        return Vec::new();
    }
    let ranks = ranks(&values.iter().map(|(_, v)| *v).collect::<Vec<_>>());
    let top = (n - 1) as f64;
    values
        .iter()
        .zip(ranks)
        .map(|((id, _), r)| (id.clone(), 2.0 * r / top - 1.0))
        .collect()
}

/// 0-based ranks with ties averaged.
#[must_use]
pub fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut ranks = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i;
        // Ties are exact equality on purpose: equal values share a rank.
        while j + 1 < order.len() && values[order[j + 1]].total_cmp(&values[order[i]]).is_eq() {
            j += 1;
        }
        let shared = (i + j) as f64 / 2.0;
        for &k in &order[i..=j] {
            ranks[k] = shared;
        }
        i = j + 1;
    }
    ranks
}

#[cfg(test)]
mod tests;
