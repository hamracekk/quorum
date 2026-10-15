//! Decision ensemble (M8): combine every source's calls into positions, weighted by evidence.
//!
//! Sources are the technical signals (detected here, bar by bar) and external calls (news
//! readings, prediction-market moves, followed traders) passed in at construction or live via
//! [`Strategy::inform`]. A source's **weight is earned, walk-forward**: once a day, using only
//! outcomes whose horizon had closed by then, each source's direction-signed returns over the
//! lookback are compared with the market's drift; a source weighs in only when its excess has
//! t ≥ `min_t` on ≥ `min_outcomes` outcomes and it is net-positive after costs. Weak sources
//! decay to zero as their record leaves the window. No record, no weight: with no weighted
//! source the ensemble stays flat.
//!
//! Sizing: conviction (weighted, signed agreement of the active calls on an instrument, in
//! −1..1) times `target_vol / realised volatility`, capped per instrument; then all instruments
//! are scaled together so the portfolio's volatility (from the covariance of recent returns,
//! so correlated positions count as one bet) stays within `target_vol`. Every target comes with
//! an explanation: contributors, weights, volatility and scaling.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::instrument::InstrumentId;
use crate::patterns::Bias;
use crate::signals::{SignalConfig, SignalDetector};
use crate::strategy::{ExternalCall, Strategy};
use crate::types::Bar;

#[derive(Debug, Clone)]
pub struct EnsembleConfig {
    /// Bar interval.
    pub step: Duration,
    /// Horizon a call's outcome is measured over (evidence).
    pub horizon: Duration,
    /// How long a call stays active after it is known.
    pub hold: Duration,
    /// Evidence window.
    pub lookback: Duration,
    pub min_outcomes: usize,
    /// Smallest t-statistic of the excess over the drift for a source to weigh in.
    pub min_t: f64,
    /// Round-trip cost; a source must be net-positive after it.
    pub cost: f64,
    /// Annualised portfolio volatility aimed at.
    pub target_vol: f64,
    /// Bars of returns behind volatility and correlation estimates.
    pub vol_bars: usize,
    /// Largest exposure per instrument.
    pub max_weight: f64,
    /// Smallest target change worth trading (avoids churn).
    pub band: f64,
    pub allow_short: bool,
    /// Include the candlestick and indicator signals as sources.
    pub technical: bool,
    pub signals: SignalConfig,
}

impl Default for EnsembleConfig {
    fn default() -> Self {
        Self {
            step: Duration::hours(1),
            horizon: Duration::hours(24),
            hold: Duration::hours(24),
            lookback: Duration::days(90),
            min_outcomes: 30,
            min_t: 2.0,
            cost: 0.0024,
            target_vol: 0.30,
            vol_bars: 720,
            max_weight: 1.0,
            band: 0.05,
            allow_short: false,
            technical: true,
            signals: SignalConfig::default(),
        }
    }
}

/// A source's record at the last weight update.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceWeight {
    pub source: String,
    pub weight: f64,
    pub outcomes: usize,
    pub mean: f64,
    pub excess: f64,
    pub t: f64,
}

#[derive(Debug)]
pub struct Ensemble {
    cfg: EnsembleConfig,
    bars: BTreeMap<InstrumentId, Vec<Bar>>,
    detectors: BTreeMap<InstrumentId, SignalDetector>,
    /// Every call, oldest first.
    calls: Vec<ExternalCall>,
    weights: BTreeMap<String, SourceWeight>,
    weights_day: Option<NaiveDate>,
    raw: BTreeMap<InstrumentId, f64>,
    last: BTreeMap<InstrumentId, f64>,
    /// The t a source needed at the last update (raised for the number of sources compared).
    required_t: f64,
    explained: BTreeMap<InstrumentId, Value>,
}

/// Raw return of a call known at `ts` over `horizon`, only if its window had closed by `now`.
fn forward(
    bars: &[Bar],
    ts: DateTime<Utc>,
    horizon: Duration,
    step: Duration,
    now: DateTime<Utc>,
) -> Option<f64> {
    let entry_index = bars.partition_point(|b| b.ts < ts);
    let entry = bars.get(entry_index)?;
    let end = entry.ts + horizon;
    if end > now || entry.open <= 0.0 {
        return None;
    }
    let exit_index = bars
        .partition_point(|b| b.ts < end)
        .checked_sub(1)
        .filter(|i| *i >= entry_index)?;
    let exit = bars.get(exit_index)?;
    (exit.ts + step <= now).then(|| exit.close / entry.open - 1.0)
}

/// One source's outcomes at a weight update.
#[derive(Debug, Default)]
struct Evidence {
    values: Vec<f64>,
    signs: f64,
    /// Outcomes whose windows do not overlap an earlier counted one on the same instrument.
    independent: usize,
    last: BTreeMap<InstrumentId, DateTime<Utc>>,
}

/// Standard normal quantile (Acklam's rational approximation, |error| < 1.2e-9).
fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let poly = |coefficients: &[f64], x: f64| {
        coefficients
            .iter()
            .fold(0.0_f64, |acc, c| acc.mul_add(x, *c))
    };
    let p = p.clamp(1e-300, 1.0 - 1e-16);
    if p < 0.024_25 {
        let q = (-2.0 * p.ln()).sqrt();
        poly(&C, q) / poly(&D, q).mul_add(q, 1.0)
    } else if p > 1.0 - 0.024_25 {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -poly(&C, q) / poly(&D, q).mul_add(q, 1.0)
    } else {
        let q = p - 0.5;
        let r = q * q;
        poly(&A, r) * q / poly(&B, r).mul_add(r, 1.0)
    }
}

/// The t a source needs when `sources` are compared at once: `min_t`'s one-sided level split
/// across them (Bonferroni), never below `min_t`.
fn bonferroni(min_t: f64, sources: usize) -> f64 {
    if sources <= 1 {
        return min_t;
    }
    let tail = 1.0 - normal_quantile_cdf(min_t);
    min_t.max(normal_quantile(1.0 - tail / sources as f64))
}

/// Standard normal CDF (Abramowitz–Stegun 7.1.26 via erf, |error| < 1.5e-7).
fn normal_quantile_cdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / 0.327_591_1_f64.mul_add(z, 1.0);
    let poly = [
        1.061_405_429,
        -1.453_152_027,
        1.421_413_741,
        -0.284_496_736,
        0.254_829_592,
    ]
    .iter()
    .fold(0.0_f64, |acc, c| acc.mul_add(t, *c));
    let erf = (-poly * t).mul_add((-z * z).exp(), 1.0);
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

fn mean_and_error(values: &[f64]) -> Option<(f64, f64)> {
    let n = values.len() as f64;
    if values.len() < 2 {
        return None;
    }
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    Some((mean, (variance / n).sqrt()))
}

impl Ensemble {
    pub fn new(cfg: EnsembleConfig, mut calls: Vec<ExternalCall>) -> Self {
        calls.sort_by_key(|c| c.ts);
        Self {
            cfg,
            bars: BTreeMap::new(),
            detectors: BTreeMap::new(),
            calls,
            weights: BTreeMap::new(),
            weights_day: None,
            raw: BTreeMap::new(),
            last: BTreeMap::new(),
            required_t: 0.0,
            explained: BTreeMap::new(),
        }
    }

    /// Weights at the last daily update (every evaluated source, weighted or not).
    pub fn weights(&self) -> Vec<SourceWeight> {
        self.weights.values().cloned().collect()
    }

    fn insert(&mut self, call: ExternalCall) {
        let at = self.calls.partition_point(|c| c.ts <= call.ts);
        let duplicate = self
            .calls
            .get(..at)
            .unwrap_or_default()
            .iter()
            .rev()
            .take_while(|c| c.ts == call.ts)
            .any(|c| c.source == call.source && c.instrument == call.instrument);
        if !duplicate {
            self.calls.insert(at, call);
        }
    }

    fn bars_per_year(&self) -> f64 {
        let seconds = self.cfg.step.num_seconds().max(1) as f64;
        365.25 * 86_400.0 / seconds
    }

    /// The market's drift: mean `horizon` return of every bar in the window (always long).
    fn drift(&self, start: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
        let mut values = Vec::new();
        for bars in self.bars.values() {
            let from = bars.partition_point(|b| b.ts < start);
            for bar in bars.get(from..).unwrap_or_default() {
                if let Some(r) = forward(bars, bar.ts, self.cfg.horizon, self.cfg.step, now) {
                    values.push(r);
                }
            }
        }
        if values.is_empty() {
            0.0
        } else {
            values.iter().sum::<f64>() / values.len() as f64
        }
    }

    /// Recompute every source's weight from outcomes closed by `now` (walk-forward).
    fn update_weights(&mut self, now: DateTime<Utc>) {
        let start = now - self.cfg.lookback;
        let drift = self.drift(start, now);
        let from = self.calls.partition_point(|c| c.ts < start);
        // Per source: signed returns, sum of directions, and per instrument the last entry
        // counted as independent (windows that overlap a counted one add no information).
        let mut by_source: BTreeMap<&str, Evidence> = BTreeMap::new();
        for call in self
            .calls
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.ts <= now)
        {
            let Some(bars) = self.bars.get(&call.instrument) else {
                continue;
            };
            let Some(r) = forward(bars, call.ts, self.cfg.horizon, self.cfg.step, now) else {
                continue;
            };
            let evidence = by_source.entry(call.source.as_str()).or_default();
            evidence.values.push(call.direction.signum() * r);
            evidence.signs += call.direction.signum();
            let last = evidence.last.get(&call.instrument);
            if last.is_none_or(|t| call.ts - *t >= self.cfg.horizon) {
                evidence.independent += 1;
                evidence.last.insert(call.instrument.clone(), call.ts);
            }
        }
        let candidates = by_source
            .values()
            .filter(|e| e.independent >= self.cfg.min_outcomes)
            .count();
        let required = bonferroni(self.cfg.min_t, candidates);
        let mut weights = BTreeMap::new();
        for (source, evidence) in by_source {
            let Some((mean, error)) = mean_and_error(&evidence.values) else {
                continue;
            };
            let n = evidence.values.len() as f64;
            let bias = evidence.signs / n;
            let excess = bias.mul_add(-drift, mean);
            // Standard error over independent windows, not over overlapping calls.
            let error = error * (n / evidence.independent.max(1) as f64).sqrt();
            let t = if error > 1e-12 { excess / error } else { 0.0 };
            let earned = evidence.independent >= self.cfg.min_outcomes
                && t >= required
                && mean - self.cfg.cost > 0.0;
            weights.insert(
                source.to_owned(),
                SourceWeight {
                    source: source.to_owned(),
                    weight: if earned { t } else { 0.0 },
                    outcomes: evidence.independent,
                    mean,
                    excess,
                    t,
                },
            );
        }
        self.required_t = required;
        self.weights = weights;
        self.weights_day = Some(now.date_naive());
    }

    /// Weighted agreement of the active calls on `instrument`, and who contributed.
    pub(super) fn conviction(
        &self,
        instrument: &InstrumentId,
        now: DateTime<Utc>,
    ) -> (f64, Vec<Value>) {
        let total: f64 = self.weights.values().map(|w| w.weight).sum();
        if total <= 0.0 {
            return (0.0, Vec::new());
        }
        let from = self.calls.partition_point(|c| c.ts <= now - self.cfg.hold);
        let mut latest: BTreeMap<&str, &ExternalCall> = BTreeMap::new();
        for call in self
            .calls
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.ts <= now)
        {
            if call.instrument == *instrument {
                latest.insert(call.source.as_str(), call);
            }
        }
        let mut sum = 0.0;
        let mut contributors = Vec::new();
        for (source, call) in latest {
            let weight = self.weights.get(source).map_or(0.0, |w| w.weight);
            if weight <= 0.0 {
                continue;
            }
            sum = (weight * call.direction).mul_add(call.strength, sum);
            contributors.push(json!({
                "source": source, "direction": call.direction, "strength": call.strength,
                "weight": weight, "known_at": call.ts,
            }));
        }
        ((sum / total).clamp(-1.0, 1.0), contributors)
    }

    fn returns(&self, instrument: &InstrumentId) -> Vec<(DateTime<Utc>, f64)> {
        let Some(bars) = self.bars.get(instrument) else {
            return Vec::new();
        };
        let from = bars.len().saturating_sub(self.cfg.vol_bars + 1);
        bars.get(from..)
            .unwrap_or_default()
            .windows(2)
            .filter_map(|pair| match pair {
                [a, b] if a.close > 0.0 => Some((b.ts, (b.close / a.close).ln())),
                _ => None,
            })
            .collect()
    }

    /// Annualised realised volatility of `instrument`; `None` while warming up.
    fn volatility(&self, instrument: &InstrumentId) -> Option<f64> {
        let returns: Vec<f64> = self
            .returns(instrument)
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        if returns.len() < self.cfg.vol_bars / 2 {
            return None;
        }
        let (_, error) = mean_and_error(&returns)?;
        let std = error * (returns.len() as f64).sqrt();
        (std > 0.0).then(|| std * self.bars_per_year().sqrt())
    }

    /// Factor that keeps the whole book within `target_vol`, from the covariance of recent
    /// returns (correlated positions add up), and within a gross exposure of 1.
    fn portfolio_scale(&self) -> f64 {
        let held: Vec<(&InstrumentId, f64)> = self
            .raw
            .iter()
            .filter(|(_, w)| w.abs() > 1e-9)
            .map(|(i, w)| (i, *w))
            .collect();
        if held.is_empty() {
            return 1.0;
        }
        let series: Vec<BTreeMap<DateTime<Utc>, f64>> = held
            .iter()
            .map(|(i, _)| self.returns(i).into_iter().collect())
            .collect();
        let common: Vec<DateTime<Utc>> = series.first().map_or_else(Vec::new, |first| {
            first
                .keys()
                .filter(|ts| series.iter().all(|s| s.contains_key(*ts)))
                .copied()
                .collect()
        });
        let mut variance = 0.0;
        if common.len() > 1 {
            let n = common.len() as f64;
            let means: Vec<f64> = series
                .iter()
                .map(|s| common.iter().filter_map(|t| s.get(t)).sum::<f64>() / n)
                .collect();
            for (a, (sa, (_, wa))) in series.iter().zip(&held).enumerate() {
                for (b, (sb, (_, wb))) in series.iter().zip(&held).enumerate() {
                    let (ma, mb) = (
                        means.get(a).copied().unwrap_or(0.0),
                        means.get(b).copied().unwrap_or(0.0),
                    );
                    let cov = common
                        .iter()
                        .filter_map(|t| Some((sa.get(t)? - ma) * (sb.get(t)? - mb)))
                        .sum::<f64>()
                        / (n - 1.0);
                    variance = (wa * wb).mul_add(cov, variance);
                }
            }
        }
        let vol = (variance.max(0.0) * self.bars_per_year()).sqrt();
        let gross: f64 = held.iter().map(|(_, w)| w.abs()).sum();
        let by_vol = if vol > self.cfg.target_vol {
            self.cfg.target_vol / vol
        } else {
            1.0
        };
        let by_gross = if gross > 1.0 { 1.0 / gross } else { 1.0 };
        by_vol.min(by_gross)
    }

    fn observe(&mut self, instrument: &InstrumentId, bar: &Bar) {
        let bars = self.bars.entry(instrument.clone()).or_default();
        if bars.last().is_some_and(|last| last.ts >= bar.ts) {
            return;
        }
        bars.push(*bar);
        if self.cfg.technical {
            let signals = self
                .detectors
                .entry(instrument.clone())
                .or_insert_with(|| SignalDetector::new(self.cfg.signals))
                .push(bar);
            for signal in signals.into_iter().filter(|s| s.bias() != Bias::Neutral) {
                self.insert(ExternalCall {
                    source: signal.to_string(),
                    instrument: instrument.clone(),
                    ts: bar.ts + self.cfg.step,
                    direction: signal.bias().sign(),
                    strength: 1.0,
                });
            }
        }
    }
}

impl Strategy for Ensemble {
    fn name(&self) -> &'static str {
        "ensemble"
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        self.observe(instrument, bar);
        let now = bar.ts + self.cfg.step;
        if self.weights_day != Some(now.date_naive()) {
            self.update_weights(now);
        }
        let (conviction, contributors) = self.conviction(instrument, now);
        let volatility = self.volatility(instrument);
        let floor = if self.cfg.allow_short {
            -self.cfg.max_weight
        } else {
            0.0
        };
        let raw = volatility.map_or(0.0, |vol| {
            (conviction * self.cfg.target_vol / vol).clamp(floor, self.cfg.max_weight)
        });
        self.raw.insert(instrument.clone(), raw);
        let scale = self.portfolio_scale();
        let target = raw * scale;
        let weighted = self.weights.values().filter(|w| w.weight > 0.0).count();
        let note = if weighted == 0 {
            "no source has earned weight yet: flat"
        } else if volatility.is_none() {
            "warming up volatility: flat"
        } else if contributors.is_empty() {
            "no active call from a weighted source: flat"
        } else {
            ""
        };
        let mut top: Vec<&SourceWeight> =
            self.weights.values().filter(|w| w.weight > 0.0).collect();
        top.sort_by(|a, b| b.weight.total_cmp(&a.weight));
        self.explained.insert(
            instrument.clone(),
            json!({
                "conviction": conviction, "volatility": volatility, "raw_target": raw,
                "portfolio_scale": scale, "target": target, "contributors": contributors,
                "required_t": self.required_t,
                "weighted_sources": top.iter().take(5).map(|w| json!({
                    "source": w.source, "weight": w.weight, "t": w.t, "outcomes": w.outcomes,
                })).collect::<Vec<_>>(),
                "note": note,
            }),
        );
        let previous = self.last.get(instrument).copied();
        let changed = previous
            .is_none_or(|p| (target - p).abs() >= self.cfg.band || (target == 0.0 && p != 0.0));
        if changed {
            self.last.insert(instrument.clone(), target);
            Some(target)
        } else {
            None
        }
    }

    fn explain(&self, instrument: &InstrumentId) -> Option<Value> {
        self.explained.get(instrument).cloned()
    }

    fn inform(&mut self, calls: &[ExternalCall]) {
        for call in calls {
            self.insert(call.clone());
        }
    }
}

#[cfg(test)]
mod tests;
