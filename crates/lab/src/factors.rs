//! Cross-sectional factor study (DESIGN §7.2): does ranking instruments by a factor predict
//! their next returns, **after costs**, on dates the study has not seen?
//!
//! On rebalance dates every `horizon` bars (so forward windows never overlap) each instrument's
//! factor value at bar `t` is set against its forward return the way the engine would trade it:
//! enter at bar `t + 1`'s open, exit at bar `t + horizon`'s close. Per date:
//!
//! - **IC**: Spearman rank correlation between factor values and forward returns.
//! - **Long excess**: the top quantile's mean forward return over the whole universe's, minus
//!   the round-trip cost on the share of the top quantile that changed since the last date — what
//!   a long-only slot holding the factor's favourites would have gained.
//!
//! The first dates are in-sample, the last `out_of_sample` share out-of-sample. The factor's
//! direction (does high or low predict up?) is taken from the in-sample IC alone, then held
//! fixed out-of-sample. A factor is promising only with an in-sample |t| above
//! [`T_THRESHOLD`] (several factors are tested at once), the same sign out-of-sample with t ≥ 2,
//! and a positive net long excess out-of-sample.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use quorum_core::factors::{PriceFactor, ranks};
use quorum_core::{InstrumentId, Series};
use serde::Serialize;

/// In-sample |t| a factor needs: the multiple-testing bar for a family studied together
/// (the lab's evaluation threshold).
pub const T_THRESHOLD: f64 = 3.0;
/// Out-of-sample t a promising factor needs, in its in-sample direction.
pub const T_OUT_OF_SAMPLE: f64 = 2.0;
/// Fewer out-of-sample dates than this: too few to judge.
pub const MIN_OUT_OF_SAMPLE_DATES: usize = 20;

#[derive(Debug, Clone)]
pub struct FactorStudyConfig {
    /// Bars between rebalance dates, and the forward window.
    pub horizon: usize,
    /// Round-trip cost as a fraction (fees + slippage, both sides).
    pub cost: f64,
    /// Share of dates held out as the out-of-sample tail.
    pub out_of_sample: f64,
    /// Instruments a date needs to count.
    pub min_names: usize,
    /// Quantiles the cross-section is cut into (5 = quintiles).
    pub quantiles: usize,
    /// Bars per year, to annualise.
    pub periods_per_year: f64,
}

impl Default for FactorStudyConfig {
    fn default() -> Self {
        Self {
            horizon: 7,
            cost: 2.0 * (0.001 + 0.0002),
            out_of_sample: 0.3,
            min_names: 10,
            quantiles: 5,
            periods_per_year: 365.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Promising,
    /// Right sign out-of-sample, not strong enough to trust.
    Weak,
    NoEdge,
    TooFew,
}

/// One sample's statistics.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct SampleStats {
    pub dates: usize,
    /// Mean IC, signed by the factor's direction.
    pub ic: f64,
    pub ic_t: f64,
    /// Mean long excess per period, net of turnover costs.
    pub long_excess_net: f64,
    /// The same, annualised.
    pub long_excess_net_annual: f64,
    /// Mean share of the top quantile replaced per date.
    pub turnover: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FactorRow {
    pub factor: String,
    pub meaning: &'static str,
    /// `+1`: high values predicted higher returns in-sample; `−1`: lower.
    pub direction: i8,
    pub mean_names: f64,
    pub in_sample: SampleStats,
    pub out_of_sample: SampleStats,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize)]
pub struct FactorStudy {
    pub horizon: usize,
    pub cost: f64,
    pub out_of_sample: f64,
    pub instruments: usize,
    pub first_date: Option<DateTime<Utc>>,
    pub last_date: Option<DateTime<Utc>>,
    /// How many factors were tested together: the multiple-testing context of any winner.
    pub factors_tested: usize,
    pub t_threshold: f64,
    /// Best out-of-sample long excess first.
    pub rows: Vec<FactorRow>,
}

/// One rebalance date: `(instrument, factor value, forward return)`.
type Cross = Vec<(InstrumentId, f64, f64)>;

/// Studies every factor over the same instruments and dates.
#[must_use]
pub fn study(
    factors: &[Box<dyn PriceFactor>],
    series: &[Series],
    cfg: &FactorStudyConfig,
) -> FactorStudy {
    let calendar = calendar(series);
    let dates: Vec<usize> = (0..calendar.len()).step_by(cfg.horizon.max(1)).collect();
    let mut rows: Vec<FactorRow> = factors
        .iter()
        .map(|f| study_one(f.as_ref(), series, &calendar, &dates, cfg))
        .collect();
    rows.sort_by(|a, b| {
        b.out_of_sample
            .long_excess_net
            .total_cmp(&a.out_of_sample.long_excess_net)
    });
    FactorStudy {
        horizon: cfg.horizon,
        cost: cfg.cost,
        out_of_sample: cfg.out_of_sample,
        instruments: series.len(),
        first_date: calendar.first().copied(),
        last_date: calendar.last().copied(),
        factors_tested: factors.len(),
        t_threshold: T_THRESHOLD,
        rows,
    }
}

/// Every bar timestamp across the series, sorted.
fn calendar(series: &[Series]) -> Vec<DateTime<Utc>> {
    let all: BTreeSet<DateTime<Utc>> = series
        .iter()
        .flat_map(|s| s.bars.iter().map(|b| b.ts))
        .collect();
    all.into_iter().collect()
}

/// The cross-section of `factor` on each rebalance date with enough names.
fn crosses(
    factor: &dyn PriceFactor,
    series: &[Series],
    calendar: &[DateTime<Utc>],
    dates: &[usize],
    cfg: &FactorStudyConfig,
) -> Vec<Cross> {
    let index: Vec<BTreeMap<DateTime<Utc>, usize>> = series
        .iter()
        .map(|s| s.bars.iter().enumerate().map(|(i, b)| (b.ts, i)).collect())
        .collect();
    let mut out = Vec::new();
    for &k in dates {
        let (Some(&now), Some(&entry_ts), Some(&exit_ts)) = (
            calendar.get(k),
            calendar.get(k + 1),
            calendar.get(k + cfg.horizon),
        ) else {
            continue;
        };
        let mut cross = Cross::new();
        for (s, idx) in series.iter().zip(&index) {
            let (Some(&i), Some(&e), Some(&x)) =
                (idx.get(&now), idx.get(&entry_ts), idx.get(&exit_ts))
            else {
                continue;
            };
            let (Some(value), entry, exit) =
                (factor.value(&s.bars[..=i]), s.bars[e].open, s.bars[x].close)
            else {
                continue;
            };
            if entry > 0.0 && value.is_finite() {
                cross.push((s.instrument.clone(), value, exit / entry - 1.0));
            }
        }
        if cross.len() >= cfg.min_names {
            out.push(cross);
        }
    }
    out
}

fn pearson(x: &[f64], y: &[f64]) -> Option<f64> {
    let n = x.len() as f64;
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in x.iter().zip(y) {
        sxy = (a - mx).mul_add(b - my, sxy);
        sxx += (a - mx).powi(2);
        syy += (b - my).powi(2);
    }
    let denominator = (sxx * syy).sqrt();
    (denominator > 0.0).then(|| sxy / denominator)
}

fn spearman(cross: &Cross) -> Option<f64> {
    let values: Vec<f64> = cross.iter().map(|c| c.1).collect();
    let returns: Vec<f64> = cross.iter().map(|c| c.2).collect();
    pearson(&ranks(&values), &ranks(&returns))
}

fn mean_t(xs: &[f64]) -> (f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    if xs.len() < 2 {
        return (mean, 0.0);
    }
    let sd = (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
    let t = if sd > 0.0 {
        mean / sd * n.sqrt()
    } else if mean == 0.0 {
        0.0
    } else {
        // Every value the same and non-zero: as strong as evidence gets, not "none". Kept
        // finite so reports stay valid JSON.
        f64::MAX.copysign(mean)
    };
    (mean, t)
}

/// Per date: top-quantile names by `direction × value`, their mean forward return over the
/// cross-section's.
fn top_excess(cross: &Cross, direction: f64, quantiles: usize) -> (BTreeSet<InstrumentId>, f64) {
    let mut sorted: Vec<&(InstrumentId, f64, f64)> = cross.iter().collect();
    sorted.sort_by(|a, b| (direction * b.1).total_cmp(&(direction * a.1)));
    let size = (cross.len() / quantiles.max(1)).max(1);
    let top = &sorted[..size];
    let universe = cross.iter().map(|c| c.2).sum::<f64>() / cross.len() as f64;
    let mean_top = top.iter().map(|c| c.2).sum::<f64>() / size as f64;
    (
        top.iter().map(|c| c.0.clone()).collect(),
        mean_top - universe,
    )
}

fn sample(crosses: &[Cross], ics: &[f64], direction: f64, cfg: &FactorStudyConfig) -> SampleStats {
    let signed: Vec<f64> = ics.iter().map(|ic| direction * ic).collect();
    let (ic, ic_t) = mean_t(&signed);
    let mut previous: Option<BTreeSet<InstrumentId>> = None;
    let (mut nets, mut turnovers) = (Vec::new(), Vec::new());
    for cross in crosses {
        let (top, excess) = top_excess(cross, direction, cfg.quantiles);
        // The first date buys the whole top quantile.
        let turnover = previous.as_ref().map_or(1.0, |prev| {
            top.difference(prev).count() as f64 / top.len().max(1) as f64
        });
        nets.push(cfg.cost.mul_add(-turnover, excess));
        turnovers.push(turnover);
        previous = Some(top);
    }
    let (long_excess_net, _) = mean_t(&nets);
    SampleStats {
        dates: crosses.len(),
        ic,
        ic_t,
        long_excess_net,
        long_excess_net_annual: long_excess_net * cfg.periods_per_year / cfg.horizon as f64,
        turnover: mean_t(&turnovers).0,
    }
}

fn study_one(
    factor: &dyn PriceFactor,
    series: &[Series],
    calendar: &[DateTime<Utc>],
    dates: &[usize],
    cfg: &FactorStudyConfig,
) -> FactorRow {
    let crosses = crosses(factor, series, calendar, dates, cfg);
    // A date whose IC is undefined (no dispersion) still counts for the long excess.
    let ics: Vec<f64> = crosses.iter().map(|c| spearman(c).unwrap_or(0.0)).collect();
    let split = crate::signals::split_index(crosses.len(), cfg.out_of_sample);
    let (in_ic, _) = mean_t(&ics[..split]);
    let direction = if in_ic < 0.0 { -1.0 } else { 1.0 };
    let in_sample = sample(&crosses[..split], &ics[..split], direction, cfg);
    let out_of_sample = sample(&crosses[split..], &ics[split..], direction, cfg);
    let verdict = if out_of_sample.dates < MIN_OUT_OF_SAMPLE_DATES {
        Verdict::TooFew
    } else if in_sample.ic_t >= T_THRESHOLD
        && out_of_sample.ic_t >= T_OUT_OF_SAMPLE
        && out_of_sample.long_excess_net > 0.0
    {
        Verdict::Promising
    } else if out_of_sample.ic > 0.0 && out_of_sample.long_excess_net > 0.0 {
        Verdict::Weak
    } else {
        Verdict::NoEdge
    };
    let names = crosses.iter().map(Vec::len).sum::<usize>() as f64 / crosses.len().max(1) as f64;
    FactorRow {
        factor: factor.id().to_string(),
        meaning: factor.meaning(),
        direction: if direction < 0.0 { -1 } else { 1 },
        mean_names: names,
        in_sample,
        out_of_sample,
        verdict,
    }
}

#[cfg(test)]
mod tests;
