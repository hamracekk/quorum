//! Event study of trading signals: does a candlestick pattern or indicator event predict the
//! next bars, **after costs**, on data it has not seen?
//!
//! For every occurrence the forward return is measured the way the engine would trade it —
//! enter at the **next bar's open**, exit at the close `h` bars after the signal — and signed by
//! the signal's bias (bearish signals profit from falls). It is compared with the same measure
//! over every bar (the baseline) and with the round-trip trading cost. History is split: the
//! first part is in-sample, the last part (default 30%) out-of-sample, and only a signal that
//! also holds up out-of-sample can be called promising.
//!
//! Caveats, stated in the output too: forward windows longer than one bar overlap, which
//! inflates t-statistics; and testing many signals makes some look significant by chance.

use std::collections::BTreeMap;

use serde::Serialize;
use trader_core::Series;
use trader_core::patterns::Bias;
use trader_core::signals::{Signal, SignalConfig, occurrences};

#[derive(Debug, Clone)]
pub struct StudyConfig {
    /// Forward horizons in bars, e.g. `[1, 4, 24]`.
    pub horizons: Vec<usize>,
    /// Horizon the verdict is based on (must be one of `horizons`).
    pub primary: usize,
    /// Round-trip trading cost as a fraction (fees + slippage, both sides).
    pub cost: f64,
    /// Share of each series held out as the out-of-sample tail.
    pub out_of_sample: f64,
    pub signals: SignalConfig,
}

impl Default for StudyConfig {
    fn default() -> Self {
        Self {
            horizons: vec![1, 4, 24],
            primary: 24,
            cost: 2.0 * (0.001 + 0.0002),
            out_of_sample: 0.3,
            signals: SignalConfig::default(),
        }
    }
}

/// Forward-return statistics for one signal, horizon and sample.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HorizonStats {
    pub horizon: usize,
    pub count: usize,
    /// Mean bias-signed forward return, before costs.
    pub mean_return: f64,
    /// Share of occurrences whose bias-signed return was positive.
    pub hit_rate: f64,
    /// Bias-signed mean forward return over *every* bar of the same sample.
    pub baseline: f64,
    /// `mean_return − baseline`: what the signal adds over an ordinary bar.
    pub edge: f64,
    /// `mean_return − cost`: what one trade on the signal would have earned.
    pub net: f64,
    /// t-statistic of the excess over baseline (inflated when windows overlap).
    pub t_stat: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Net-positive after costs with t > 2 out-of-sample, and net-positive in-sample.
    Promising,
    /// Some positive evidence, not enough to trust.
    Weak,
    NoEdge,
    /// Fewer than 30 out-of-sample occurrences.
    TooFew,
    /// Neutral signals (doji) have no direction to score.
    Neutral,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalStats {
    pub signal: Signal,
    pub family: &'static str,
    pub bias: Bias,
    pub occurrences: usize,
    pub in_sample: Vec<HorizonStats>,
    pub out_of_sample: Vec<HorizonStats>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalStudy {
    pub horizons: Vec<usize>,
    pub primary: usize,
    pub cost: f64,
    pub out_of_sample: f64,
    pub bars: usize,
    /// How many signals were scored — the multiple-testing context for any "winner".
    pub signals_tested: usize,
    /// Best out-of-sample net return at the primary horizon first.
    pub rows: Vec<SignalStats>,
}

const MIN_OUT_OF_SAMPLE: usize = 30;

/// Sample (in/out) → horizon → bias-signed returns for one signal.
type Samples = [BTreeMap<usize, Vec<f64>>; 2];

/// Score every signal across `series` (pooled).
pub fn study(series: &[Series], cfg: &StudyConfig) -> SignalStudy {
    let mut returns: BTreeMap<Signal, Samples> = BTreeMap::new();
    let mut counts: BTreeMap<Signal, usize> = BTreeMap::new();
    // Sample → horizon → (sum, count) of unsigned forward returns over every bar.
    let mut baseline: [BTreeMap<usize, (f64, usize)>; 2] = Default::default();
    let mut bars_total = 0;

    for s in series {
        let bars = &s.bars;
        bars_total += bars.len();
        let split = split_index(bars.len(), cfg.out_of_sample);
        let forward = |i: usize, h: usize| -> Option<f64> {
            let entry = bars.get(i + 1)?.open;
            let exit = bars.get(i + h)?.close;
            (entry > 0.0).then(|| exit / entry - 1.0)
        };
        for i in 0..bars.len() {
            let sample = usize::from(i >= split);
            for &h in &cfg.horizons {
                if let Some(r) = forward(i, h) {
                    let entry = baseline[sample].entry(h).or_insert((0.0, 0));
                    entry.0 += r;
                    entry.1 += 1;
                }
            }
        }
        for (i, signal) in occurrences(bars, cfg.signals) {
            *counts.entry(signal).or_default() += 1;
            let sign = signal.bias().sign();
            let sample = usize::from(i >= split);
            let slot = returns.entry(signal).or_default();
            for &h in &cfg.horizons {
                if let Some(r) = forward(i, h) {
                    slot[sample].entry(h).or_default().push(sign * r);
                }
            }
        }
    }

    let base = |sample: usize, h: usize| -> f64 {
        baseline[sample]
            .get(&h)
            .filter(|(_, n)| *n > 0)
            .map_or(0.0, |(sum, n)| sum / *n as f64)
    };

    let mut rows: Vec<SignalStats> = Signal::all()
        .into_iter()
        .map(|signal| {
            let sign = signal.bias().sign();
            let empty = Samples::default();
            let samples = returns.get(&signal).unwrap_or(&empty);
            let stats = |sample: usize| -> Vec<HorizonStats> {
                cfg.horizons
                    .iter()
                    .map(|&h| {
                        let values = samples[sample].get(&h).map_or(&[][..], Vec::as_slice);
                        horizon_stats(h, values, sign * base(sample, h), cfg.cost)
                    })
                    .collect()
            };
            let (in_sample, out_of_sample) = (stats(0), stats(1));
            let verdict = verdict(signal, &in_sample, &out_of_sample, cfg.primary);
            SignalStats {
                signal,
                family: signal.family(),
                bias: signal.bias(),
                occurrences: counts.get(&signal).copied().unwrap_or(0),
                in_sample,
                out_of_sample,
                verdict,
            }
        })
        .collect();

    let primary_net = |row: &SignalStats| {
        row.out_of_sample
            .iter()
            .find(|h| h.horizon == cfg.primary && h.count > 0)
            .map_or(f64::NEG_INFINITY, |h| h.net)
    };
    rows.sort_by(|a, b| primary_net(b).total_cmp(&primary_net(a)));

    SignalStudy {
        horizons: cfg.horizons.clone(),
        primary: cfg.primary,
        cost: cfg.cost,
        out_of_sample: cfg.out_of_sample,
        bars: bars_total,
        signals_tested: rows
            .iter()
            .filter(|r| r.verdict != Verdict::Neutral)
            .count(),
        rows,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the rounded value is clamped to [0, len] before the cast"
)]
fn split_index(len: usize, out_of_sample: f64) -> usize {
    let held_out = (len as f64 * out_of_sample.clamp(0.0, 1.0)).round();
    len - held_out.clamp(0.0, len as f64) as usize
}

fn horizon_stats(horizon: usize, values: &[f64], baseline: f64, cost: f64) -> HorizonStats {
    let count = values.len();
    if count == 0 {
        return HorizonStats {
            horizon,
            baseline,
            ..HorizonStats::default()
        };
    }
    let n = count as f64;
    let mean = values.iter().sum::<f64>() / n;
    let hits = values.iter().filter(|r| **r > 0.0).count() as f64;
    let t_stat = if count > 1 {
        let variance = values.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        let se = (variance / n).sqrt();
        if se > 0.0 {
            (mean - baseline) / se
        } else {
            0.0
        }
    } else {
        0.0
    };
    HorizonStats {
        horizon,
        count,
        mean_return: mean,
        hit_rate: hits / n,
        baseline,
        edge: mean - baseline,
        net: mean - cost,
        t_stat,
    }
}

fn verdict(
    signal: Signal,
    in_sample: &[HorizonStats],
    out_of_sample: &[HorizonStats],
    primary: usize,
) -> Verdict {
    if signal.bias() == Bias::Neutral {
        return Verdict::Neutral;
    }
    let at = |stats: &[HorizonStats]| {
        stats
            .iter()
            .find(|h| h.horizon == primary)
            .cloned()
            .unwrap_or_default()
    };
    let (ins, oos) = (at(in_sample), at(out_of_sample));
    if oos.count < MIN_OUT_OF_SAMPLE {
        Verdict::TooFew
    } else if oos.net > 0.0 && oos.t_stat > 2.0 && ins.net > 0.0 {
        Verdict::Promising
    } else if oos.net > 0.0 || (ins.net > 0.0 && ins.t_stat > 2.0) {
        Verdict::Weak
    } else {
        Verdict::NoEdge
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use trader_core::{Bar, InstrumentId};

    use super::*;

    fn series(closes: &[f64]) -> Series {
        let bars = closes
            .iter()
            .zip(0_i64..)
            .map(|(&close, i)| Bar {
                ts: DateTime::from_timestamp(i * 3_600, 0).expect("valid timestamp"),
                open: close,
                high: close,
                low: close,
                close,
                volume: 1.0,
            })
            .collect();
        Series::new(
            InstrumentId::spot("test", "A").expect("valid instrument"),
            bars,
        )
    }

    fn cfg() -> StudyConfig {
        StudyConfig {
            horizons: vec![1, 3],
            primary: 3,
            cost: 0.0,
            out_of_sample: 0.3,
            signals: SignalConfig {
                sma_fast: 2,
                sma_slow: 4,
                rsi_period: 3,
                bollinger_period: 10,
                ..SignalConfig::default()
            },
        }
    }

    #[test]
    fn stats_measure_mean_hit_rate_edge_net_and_t() {
        let s = horizon_stats(4, &[0.02, 0.04, -0.01, 0.03], 0.005, 0.01);
        assert_eq!(s.count, 4);
        assert!((s.mean_return - 0.02).abs() < 1e-12);
        assert!((s.hit_rate - 0.75).abs() < 1e-12);
        assert!((s.edge - 0.015).abs() < 1e-12);
        assert!((s.net - 0.01).abs() < 1e-12);
        assert!(s.t_stat > 0.0);
        assert_eq!(horizon_stats(1, &[], 0.0, 0.0).count, 0);
    }

    #[test]
    fn split_keeps_the_tail_out_of_sample() {
        assert_eq!(split_index(100, 0.3), 70);
        assert_eq!(split_index(10, 0.0), 10);
        assert_eq!(split_index(10, 1.0), 0);
    }

    #[test]
    fn verdicts_require_out_of_sample_evidence_after_costs() {
        let h = |count, net, t_stat| HorizonStats {
            horizon: 24,
            count,
            net,
            t_stat,
            ..HorizonStats::default()
        };
        let bull = Signal::RsiOversold;
        assert_eq!(
            verdict(bull, &[h(100, 0.01, 3.0)], &[h(50, 0.01, 2.5)], 24),
            Verdict::Promising
        );
        assert_eq!(
            verdict(bull, &[h(100, 0.01, 3.0)], &[h(50, -0.01, -1.0)], 24),
            Verdict::Weak
        );
        assert_eq!(
            verdict(bull, &[h(100, -0.01, 0.0)], &[h(50, -0.01, 0.0)], 24),
            Verdict::NoEdge
        );
        assert_eq!(
            verdict(bull, &[h(100, 0.05, 9.0)], &[h(10, 0.05, 9.0)], 24),
            Verdict::TooFew
        );
        let doji = Signal::Pattern(trader_core::patterns::Pattern::Doji);
        assert_eq!(verdict(doji, &[], &[], 24), Verdict::Neutral);
    }

    #[test]
    fn bearish_signals_profit_from_falls_and_entries_use_the_next_open() {
        // Rise then fall: the death cross fires near the top; prices keep falling afterwards.
        let mut closes: Vec<f64> = (0..20).map(|i| 100.0 + f64::from(i)).collect();
        closes.extend((0..20).map(|i| 2.0_f64.mul_add(-f64::from(i), 119.0)));
        let result = study(&[series(&closes)], &cfg());
        let death = result
            .rows
            .iter()
            .find(|r| r.signal == Signal::DeathCross)
            .expect("death cross row");
        assert_eq!(death.occurrences, 1);
        let all: Vec<&HorizonStats> = death.in_sample.iter().chain(&death.out_of_sample).collect();
        assert!(
            all.iter().any(|h| h.count == 1 && h.mean_return > 0.0),
            "{all:?}"
        );
    }

    #[test]
    fn study_covers_every_signal_and_reports_what_was_tested() {
        let closes: Vec<f64> = (0..200)
            .map(|i| (f64::from(i) / 3.0).sin().mul_add(5.0, 100.0))
            .collect();
        let result = study(&[series(&closes)], &cfg());
        assert_eq!(result.rows.len(), Signal::all().len());
        assert_eq!(
            result.signals_tested,
            Signal::all().len() - 1,
            "doji is not scored"
        );
        assert_eq!(result.bars, 200);
    }
}
