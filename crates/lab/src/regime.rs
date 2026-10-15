//! Does the regime label help (DESIGN §7.3)? Two questions, in-sample vs out-of-sample:
//!
//! 1. **Does it separate risk?** After risk-off and crisis dates, the market's realised
//!    volatility over the next `horizon` bars should be higher than after trending dates —
//!    that is what budgets and brakes would use it for.
//! 2. **Does acting on it pay?** Hold the equal-weighted market at a fixed exposure per regime
//!    ([`RegimeStudyConfig::exposure`], set in advance, not fitted), one bar late — decided at a
//!    close, in place from the next close — paying costs on every change, against simply holding
//!    the market on the same dates.

use quorum_core::Series;
use quorum_core::regime::{Regime, RegimePoint, RegimeRule, regimes};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct RegimeStudyConfig {
    pub rule: RegimeRule,
    /// Bars ahead the forward volatility and return are measured over.
    pub horizon: usize,
    /// Round-trip cost as a fraction; an exposure change pays half of it per unit changed.
    pub cost: f64,
    pub out_of_sample: f64,
    /// Bars per year (from the slot's calendar): the one-year window and annualisation.
    pub periods_per_year: f64,
    /// Exposure held in trending, choppy, risk-off and crisis regimes.
    pub exposure: [f64; 4],
}

impl Default for RegimeStudyConfig {
    fn default() -> Self {
        Self {
            rule: RegimeRule::default(),
            horizon: 20,
            cost: 2.0 * (0.001 + 0.0002),
            out_of_sample: 0.3,
            periods_per_year: 365.0,
            exposure: [1.0, 0.6, 0.3, 0.0],
        }
    }
}

const fn slot(regime: Regime) -> usize {
    match regime {
        Regime::Trending => 0,
        Regime::Choppy => 1,
        Regime::RiskOff => 2,
        Regime::Crisis => 3,
    }
}

/// What followed one regime in one sample.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RegimeStats {
    pub regime: Regime,
    pub dates: usize,
    /// Share of the sample's dates in this regime.
    pub share: f64,
    /// Mean market return over the next `horizon` bars (windows overlap: no t-statistic).
    pub forward_return: f64,
    /// Mean annualised realised volatility over the next `horizon` bars.
    pub forward_vol: f64,
}

/// A daily return stream's summary.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Performance {
    pub annual_return: f64,
    pub annual_vol: f64,
    pub sharpe: f64,
    pub max_drawdown: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub regimes: Vec<RegimeStats>,
    /// Risk-off and crisis are followed by higher volatility than trending.
    pub separates_risk: bool,
    pub regime_exposure: Performance,
    pub hold: Performance,
    /// Higher Sharpe and smaller drawdown than holding.
    pub exposure_helps: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegimeStudy {
    pub instruments: usize,
    pub horizon: usize,
    pub exposure: [f64; 4],
    pub current: Option<RegimePoint>,
    pub in_sample: Sample,
    pub out_of_sample: Sample,
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn std_dev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

fn performance(returns: &[f64], periods_per_year: f64) -> Performance {
    let (m, sd) = (mean(returns), std_dev(returns));
    let (mut level, mut peak, mut max_drawdown) = (1.0_f64, 1.0_f64, 0.0_f64);
    for r in returns {
        level *= 1.0 + r;
        peak = peak.max(level);
        max_drawdown = max_drawdown.max(1.0 - level / peak);
    }
    Performance {
        annual_return: m * periods_per_year,
        annual_vol: sd * periods_per_year.sqrt(),
        sharpe: if sd > 0.0 {
            m / sd * periods_per_year.sqrt()
        } else {
            0.0
        },
        max_drawdown,
    }
}

fn sample(points: &[RegimePoint], cfg: &RegimeStudyConfig) -> Sample {
    let h = cfg.horizon.max(1);
    let mut stats = Regime::ALL.map(|regime| (regime, Vec::new(), Vec::new()));
    for (k, p) in points.iter().enumerate() {
        let Some(ahead) = points.get(k + 1..=k + h) else {
            continue;
        };
        let returns: Vec<f64> = ahead.iter().map(|q| q.market_return).collect();
        let entry = &mut stats[slot(p.regime)];
        entry
            .1
            .push(returns.iter().fold(1.0, |acc, r| acc * (1.0 + r)) - 1.0);
        entry
            .2
            .push(std_dev(&returns) * cfg.periods_per_year.sqrt());
    }
    let counted: usize = stats.iter().map(|s| s.1.len()).sum();
    let regimes: Vec<RegimeStats> = stats
        .iter()
        .map(|(regime, fwd, vol)| RegimeStats {
            regime: *regime,
            dates: fwd.len(),
            share: fwd.len() as f64 / counted.max(1) as f64,
            forward_return: mean(fwd),
            forward_vol: mean(vol),
        })
        .collect();
    let vol_of = |r: Regime| {
        regimes
            .iter()
            .find(|s| s.regime == r)
            .filter(|s| s.dates > 0)
    };
    let separates_risk = vol_of(Regime::Trending).is_some_and(|t| {
        [Regime::RiskOff, Regime::Crisis]
            .iter()
            .filter_map(|&r| vol_of(r))
            .all(|s| s.forward_vol > t.forward_vol)
            && (vol_of(Regime::RiskOff).is_some() || vol_of(Regime::Crisis).is_some())
    });

    // Decided at the close of k, in place from the close of k + 1: earns the return of k + 2.
    let (mut strategy, mut hold) = (Vec::new(), Vec::new());
    let mut exposure = 0.0;
    for k in 0..points.len().saturating_sub(2) {
        let target = cfg.exposure[slot(points[k].regime)];
        let change = (target - exposure).abs();
        exposure = target;
        let r = points[k + 2].market_return;
        strategy.push(exposure.mul_add(r, -change * cfg.cost / 2.0));
        hold.push(r);
    }
    let regime_exposure = performance(&strategy, cfg.periods_per_year);
    let hold = performance(&hold, cfg.periods_per_year);
    Sample {
        from: points.first().map(|p| p.ts),
        to: points.last().map(|p| p.ts),
        regimes,
        separates_risk,
        exposure_helps: regime_exposure.sharpe > hold.sharpe
            && regime_exposure.max_drawdown < hold.max_drawdown,
        regime_exposure,
        hold,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a positive bar count, rounded"
)]
const fn year_bars(periods_per_year: f64) -> usize {
    periods_per_year.round().max(2.0) as usize
}

/// Labels the market of `series` and studies the label in-sample vs out-of-sample.
#[must_use]
pub fn study(series: &[Series], cfg: &RegimeStudyConfig) -> RegimeStudy {
    let points = regimes(series, &cfg.rule, year_bars(cfg.periods_per_year));
    let split = crate::signals::split_index(points.len(), cfg.out_of_sample);
    RegimeStudy {
        instruments: series.len(),
        horizon: cfg.horizon,
        exposure: cfg.exposure,
        current: points.last().copied(),
        in_sample: sample(&points[..split], cfg),
        out_of_sample: sample(&points[split..], cfg),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use quorum_core::{Bar, InstrumentId};

    use super::*;

    /// Calm rises interrupted by violent falls, repeated: regimes should see the falls coming
    /// in volatility, and the exposure map should cut the drawdowns.
    fn cycles(name: &str) -> Series {
        let t0 = Utc
            .with_ymd_and_hms(2015, 1, 1, 0, 0, 0)
            .single()
            .expect("valid");
        let mut c = 100.0;
        let mut bars = Vec::new();
        for d in 0..3_000_i64 {
            let phase = d % 400;
            let r = if phase < 300 {
                if d % 2 == 0 { 0.006 } else { -0.002 }
            } else if d % 2 == 0 {
                -0.06
            } else {
                0.04
            };
            c *= 1.0 + r;
            bars.push(Bar {
                ts: t0 + Duration::days(d),
                open: c,
                high: c,
                low: c,
                close: c,
                volume: 1.0,
            });
        }
        Series::new(InstrumentId::spot("test", name).expect("valid"), bars)
    }

    #[test]
    fn violent_regimes_are_followed_by_higher_volatility() {
        let series: Vec<Series> = (0..4).map(|i| cycles(&format!("A{i}"))).collect();
        let cfg = RegimeStudyConfig {
            horizon: 10,
            ..RegimeStudyConfig::default()
        };
        let report = study(&series, &cfg);
        assert!(
            report.in_sample.separates_risk,
            "{:?}",
            report.in_sample.regimes
        );
        assert!(report.out_of_sample.separates_risk);
        assert!(report.current.is_some());
    }

    #[test]
    fn the_exposure_map_cuts_drawdowns_in_falling_regimes() {
        let series: Vec<Series> = (0..4).map(|i| cycles(&format!("A{i}"))).collect();
        let report = study(
            &series,
            &RegimeStudyConfig {
                horizon: 10,
                ..RegimeStudyConfig::default()
            },
        );
        let out = &report.out_of_sample;
        assert!(
            out.regime_exposure.max_drawdown < out.hold.max_drawdown,
            "{out:?}"
        );
    }

    #[test]
    fn performance_handles_flat_and_falling_streams() {
        let flat = performance(&[0.0; 10], 365.0);
        assert!(flat.sharpe.abs() < f64::EPSILON && flat.max_drawdown.abs() < f64::EPSILON);
        let falling = performance(&[-0.5, -0.5], 365.0);
        assert!((falling.max_drawdown - 0.75).abs() < 1e-12);
    }
}
