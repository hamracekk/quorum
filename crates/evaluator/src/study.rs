//! The shadow study: what "always take" earns, and whether anything in the case files tells
//! winners from losers out of sample — the bar a meta model has to clear before it may size.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use quorum_core::factors::ranks;
use quorum_core::regime::Regime;
use quorum_lab::overfit::normal_quantile;
use serde::Serialize;

use crate::case::{Case, EXIT, HORIZONS};

#[derive(Debug, Clone, Copy)]
pub struct StudyConfig {
    /// The latest fraction of labelled cases (by time) held out.
    pub out_of_sample: f64,
    /// Family-wise error rate across all features tested.
    pub alpha: f64,
    /// Fewest winners and losers each side needs before a feature is judged.
    pub min_class: usize,
}

impl Default for StudyConfig {
    fn default() -> Self {
        Self {
            out_of_sample: 0.3,
            alpha: 0.05,
            min_class: 10,
        }
    }
}

/// One label's base rates: what taking every proposal earned.
#[derive(Debug, Clone, Serialize)]
pub struct HorizonRow {
    pub label: String,
    pub n: usize,
    /// Of `n`, how many were actually filled.
    pub taken: usize,
    pub win_rate: f64,
    pub mean_net: f64,
}

/// "Always take" on the strategy's own exit, in and out of sample: the baseline to beat.
#[derive(Debug, Clone, Serialize)]
pub struct Baseline {
    pub n_in: usize,
    pub n_out: usize,
    pub mean_in: f64,
    pub mean_out: f64,
    pub win_in: f64,
    pub win_out: f64,
    pub split: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Separates winners out of sample beyond the multiple-testing threshold.
    Separates,
    NoEdge,
    TooFew,
}

/// Whether one feature tells winners (net > 0 at the exit) from losers.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureRow {
    pub feature: String,
    pub n_in: usize,
    pub n_out: usize,
    /// Area under the ROC curve in sample, as measured (0.5 = no information).
    pub auc_in: f64,
    /// Out of sample, in the direction fixed in sample (> 0.5 = the in-sample reading held).
    pub auc_out: f64,
    /// Out-of-sample z of the rank-sum statistic under no information.
    pub z_out: f64,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegimeRow {
    pub regime: Option<Regime>,
    pub n: usize,
    pub win_rate: f64,
    pub mean_net: f64,
}

/// The rule "skip entries in this regime", chosen in sample and judged out of sample.
#[derive(Debug, Clone, Serialize)]
pub struct RegimeSkip {
    pub regime: Option<Regime>,
    pub n_in: usize,
    pub mean_in: f64,
    pub n_out: usize,
    /// Mean net of the out-of-sample entries the rule would have skipped.
    pub mean_out: f64,
    /// "Always take" out of sample, and with the rule applied.
    pub always_out: f64,
    pub skipping_out: f64,
    /// t of the skipped entries' out-of-sample mean (negative: they lost).
    pub t_out: f64,
    pub verdict: SkipVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipVerdict {
    /// The regime's entries lost in sample and lost out of sample beyond the threshold.
    Holds,
    /// Chosen in sample, but out of sample the loss is not significant (or there was none).
    NotSignificant,
    /// The regime's entries did not lose in sample: no rule to test.
    NotChosen,
    TooFew,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluatorStudy {
    pub cases: usize,
    /// Cases with an exit label (the strategy has closed them).
    pub labelled: usize,
    pub horizons: Vec<HorizonRow>,
    pub baseline: Baseline,
    /// Strongest out-of-sample separation first.
    pub features: Vec<FeatureRow>,
    pub regimes: Vec<RegimeRow>,
    /// One per regime seen; the threshold is corrected for how many were tested.
    pub regime_skips: Vec<RegimeSkip>,
    pub features_tested: usize,
    /// One-sided z a feature needs out of sample (Bonferroni over `features_tested`).
    pub z_threshold: f64,
}

/// Studies `cases` (any order).
#[must_use]
pub fn study(cases: &[Case], cfg: &StudyConfig) -> EvaluatorStudy {
    let labels: Vec<String> = HORIZONS
        .iter()
        .map(|h| format!("h{h}"))
        .chain(std::iter::once(EXIT.to_owned()))
        .collect();
    let horizons = labels
        .iter()
        .map(|label| {
            let with: Vec<(&Case, f64)> = cases
                .iter()
                .filter_map(|c| c.outcomes.get(label).map(|r| (c, *r)))
                .collect();
            let nets: Vec<f64> = with.iter().map(|(_, r)| *r).collect();
            HorizonRow {
                label: label.clone(),
                n: with.len(),
                taken: with.iter().filter(|(c, _)| c.taken).count(),
                win_rate: win_rate(&nets),
                mean_net: mean(&nets),
            }
        })
        .collect();

    let mut labelled: Vec<(&Case, f64)> = cases
        .iter()
        .filter_map(|c| c.outcomes.get(EXIT).map(|r| (c, *r)))
        .collect();
    labelled.sort_by_key(|(c, _)| c.ts);
    let (in_sample, out_sample) = labelled.split_at(split(labelled.len(), cfg.out_of_sample));
    let nets = |xs: &[(&Case, f64)]| xs.iter().map(|(_, r)| *r).collect::<Vec<_>>();
    let baseline = Baseline {
        n_in: in_sample.len(),
        n_out: out_sample.len(),
        mean_in: mean(&nets(in_sample)),
        mean_out: mean(&nets(out_sample)),
        win_in: win_rate(&nets(in_sample)),
        win_out: win_rate(&nets(out_sample)),
        split: out_sample.first().map(|(c, _)| c.ts),
    };

    let names: Vec<&String> = labelled
        .iter()
        .flat_map(|(c, _)| c.features.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let z_threshold = normal_quantile(1.0 - cfg.alpha / names.len().max(1) as f64);
    let mut features: Vec<FeatureRow> = names
        .iter()
        .map(|name| feature_row(name, in_sample, out_sample, z_threshold, cfg))
        .collect();
    features.sort_by(|a, b| b.z_out.total_cmp(&a.z_out));

    let mut by_regime: BTreeMap<Option<Regime>, Vec<f64>> = BTreeMap::new();
    for (c, r) in &labelled {
        by_regime.entry(c.regime).or_default().push(*r);
    }
    let regimes = by_regime
        .into_iter()
        .map(|(regime, nets)| RegimeRow {
            regime,
            n: nets.len(),
            win_rate: win_rate(&nets),
            mean_net: mean(&nets),
        })
        .collect();

    let regime_skips = regime_skips(in_sample, out_sample, cfg);

    EvaluatorStudy {
        cases: cases.len(),
        labelled: labelled.len(),
        horizons,
        baseline,
        features,
        regimes,
        regime_skips,
        features_tested: names.len(),
        z_threshold,
    }
}

/// For each regime: skip its entries if they lost in sample, then judge that out of sample.
fn regime_skips(
    in_sample: &[(&Case, f64)],
    out_sample: &[(&Case, f64)],
    cfg: &StudyConfig,
) -> Vec<RegimeSkip> {
    let regimes: std::collections::BTreeSet<Option<Regime>> = in_sample
        .iter()
        .chain(out_sample)
        .map(|(c, _)| c.regime)
        .collect();
    let threshold = normal_quantile(1.0 - cfg.alpha / regimes.len().max(1) as f64);
    let always_out = mean(&out_sample.iter().map(|(_, r)| *r).collect::<Vec<_>>());
    regimes
        .into_iter()
        .map(|regime| {
            let of = |xs: &[(&Case, f64)]| -> Vec<f64> {
                xs.iter()
                    .filter(|(c, _)| c.regime == regime)
                    .map(|(_, r)| *r)
                    .collect()
            };
            let (a, b) = (of(in_sample), of(out_sample));
            let kept: Vec<f64> = out_sample
                .iter()
                .filter(|(c, _)| c.regime != regime)
                .map(|(_, r)| *r)
                .collect();
            let t_out = t_stat(&b);
            let verdict = if a.len() < cfg.min_class || b.len() < cfg.min_class {
                SkipVerdict::TooFew
            } else if mean(&a) >= 0.0 {
                SkipVerdict::NotChosen
            } else if t_out <= -threshold {
                SkipVerdict::Holds
            } else {
                SkipVerdict::NotSignificant
            };
            RegimeSkip {
                regime,
                n_in: a.len(),
                mean_in: mean(&a),
                n_out: b.len(),
                mean_out: mean(&b),
                always_out,
                skipping_out: mean(&kept),
                t_out,
                verdict,
            }
        })
        .collect()
}

/// t of the mean of `xs` against zero; 0 with fewer than two values or no dispersion.
fn t_stat(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let n = xs.len() as f64;
    let sd = (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
    if sd > 0.0 { m / sd * n.sqrt() } else { 0.0 }
}

/// How many of `n` time-ordered cases are in sample.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a count between 0 and n, rounded"
)]
fn split(n: usize, out_of_sample: f64) -> usize {
    ((n as f64) * (1.0 - out_of_sample.clamp(0.0, 1.0))).round() as usize
}

fn feature_row(
    name: &str,
    in_sample: &[(&Case, f64)],
    out_sample: &[(&Case, f64)],
    z_threshold: f64,
    cfg: &StudyConfig,
) -> FeatureRow {
    let pick = |xs: &[(&Case, f64)]| -> Vec<(f64, bool)> {
        xs.iter()
            .filter_map(|(c, r)| c.features.get(name).map(|v| (*v, *r > 0.0)))
            .collect()
    };
    let (a, b) = (pick(in_sample), pick(out_sample));
    let (auc_in, _) = auc(&a);
    let (auc_raw, z_raw) = auc(&b);
    // The direction is fixed in sample; out of sample is read in that direction.
    let (auc_out, z_out) = if auc_in >= 0.5 {
        (auc_raw, z_raw)
    } else {
        (1.0 - auc_raw, -z_raw)
    };
    let enough = |xs: &[(f64, bool)]| {
        let wins = xs.iter().filter(|(_, w)| *w).count();
        wins >= cfg.min_class && xs.len() - wins >= cfg.min_class
    };
    let verdict = if !enough(&a) || !enough(&b) {
        Verdict::TooFew
    } else if z_out >= z_threshold {
        Verdict::Separates
    } else {
        Verdict::NoEdge
    };
    FeatureRow {
        feature: name.to_owned(),
        n_in: a.len(),
        n_out: b.len(),
        auc_in,
        auc_out,
        z_out,
        verdict,
    }
}

/// AUC of `value` for `win` (ties averaged) and the rank-sum z under no information; `(0.5, 0)`
/// without both classes.
#[must_use]
pub fn auc(xs: &[(f64, bool)]) -> (f64, f64) {
    let wins = xs.iter().filter(|(_, w)| *w).count() as f64;
    let losses = xs.len() as f64 - wins;
    if wins == 0.0 || losses == 0.0 {
        return (0.5, 0.0);
    }
    let values: Vec<f64> = xs.iter().map(|(v, _)| *v).collect();
    let rank_sum: f64 = ranks(&values)
        .iter()
        .zip(xs)
        .filter(|(_, (_, w))| *w)
        .map(|(r, _)| r + 1.0)
        .sum();
    let u = rank_sum - wins * (wins + 1.0) / 2.0;
    let sd = (wins * losses * (wins + losses + 1.0) / 12.0).sqrt();
    (u / (wins * losses), (u - wins * losses / 2.0) / sd)
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn win_rate(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().filter(|r| **r > 0.0).count() as f64 / xs.len() as f64
    }
}
