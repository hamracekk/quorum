//! Scoreboard (M6): does a signal source predict anything, **after costs**, and better than
//! baselines that know nothing?
//!
//! A source is a stream of directional occurrences: a technical signal, a model's news
//! readings, or a baseline. Each occurrence is scored the way the engine would trade it: enter
//! at the open of the first bar that starts at or after the moment the signal was *known*, exit
//! at the close of the last bar inside the horizon, and sign the return by the direction. An
//! occurrence whose window has not closed yet is *pending*, not dropped, so a live source shows
//! how much of its record is still open.
//!
//! Per source and horizon: count, mean signed return, hit rate, net of the round-trip cost,
//! t-statistic, and the information coefficient (Spearman rank correlation of signed strength
//! with the raw return; only defined when directions or strengths vary). Decay is visible two
//! ways: across horizons (1h → 1d → 1w), and as the primary horizon over the most recent weeks
//! against the whole window.
//!
//! Baselines: *always long* (every bar, bullish) and *random* (every bar, a direction from a
//! deterministic hash). Timing is what a source must add: its *excess* is its mean minus what
//! its own mix of long and short calls would have earned at random times (`bias × always-long
//! mean`), so a bullish signal in a rising market gets no credit for the trend. A source only
//! earns [`Verdict::Edge`] when it is net-positive after costs and its excess has t > 2 at the
//! primary horizon, on enough outcomes spanning enough weeks. Caveats: overlapping windows
//! inflate t (the random baseline shows by how much), and many sources make some look good by
//! chance.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use trader_core::patterns::Bias;
use trader_core::signals::{Signal, SignalConfig, occurrences};
use trader_core::{Bar, InstrumentId, Series};

/// Id of the always-long baseline, the market's drift every excess is measured against.
pub const ALWAYS_LONG: &str = "always_long";

/// Returns closer than this are equal (rounding noise, not edge).
const TOLERANCE: f64 = 1e-9;

/// One directional call, known at `ts`.
#[derive(Debug, Clone, PartialEq)]
pub struct Occurrence {
    pub instrument: InstrumentId,
    /// When the signal was known (and could first be traded).
    pub ts: DateTime<Utc>,
    /// `1.0` bullish, `-1.0` bearish.
    pub direction: f64,
    /// Confidence in `0..=1` (`1.0` for signals without one).
    pub strength: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    News,
    /// Prediction-market probability moves (SRC-2).
    Prediction,
    /// Followed traders' opening fills (SRC-3).
    Trader,
    Technical,
    Baseline,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub kind: SourceKind,
    pub occurrences: Vec<Occurrence>,
}

#[derive(Debug, Clone)]
pub struct HorizonSpec {
    pub label: String,
    pub length: Duration,
}

#[derive(Debug, Clone)]
pub struct ScoreConfig {
    pub horizons: Vec<HorizonSpec>,
    /// Label of the horizon verdicts use (one of `horizons`).
    pub primary: String,
    /// Round-trip trading cost as a fraction.
    pub cost: f64,
    /// The "recent" window compared with the whole record (decay).
    pub recent: Duration,
    /// Outcomes at the primary horizon needed for a verdict.
    pub min_outcomes: usize,
    /// Weeks the outcomes must span for a verdict.
    pub min_weeks: f64,
    /// The scoreboard's "now" (the newest bar), so results are reproducible.
    pub now: DateTime<Utc>,
}

/// One source at one horizon.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HorizonScore {
    pub horizon: String,
    /// Occurrences with a closed window.
    pub count: usize,
    /// Occurrences whose window has not closed yet (or that have no price data).
    pub pending: usize,
    /// Mean direction-signed return, before costs.
    pub mean_return: f64,
    pub hit_rate: f64,
    /// `mean_return − cost`: what one trade per occurrence earned on average.
    pub net: f64,
    /// t-statistic of the mean signed return (inflated when windows overlap).
    pub t_stat: f64,
    /// Standard error of the mean signed return.
    pub std_error: f64,
    /// Mean direction of the scored calls: `1` all long, `−1` all short.
    pub bias: f64,
    /// `mean_return − bias × always-long mean`: what the timing added over the drift.
    pub excess: f64,
    /// t-statistic of `excess`.
    pub t_excess: f64,
    /// Spearman correlation of signed strength with the raw return; `None` when undefined.
    pub ic: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Net-positive after costs, with an excess over the drift of t > 2 at the primary horizon.
    Edge,
    NoEdge,
    /// Enough outcomes, not yet spanning `min_weeks`.
    Collecting,
    /// Fewer than `min_outcomes` closed windows at the primary horizon.
    TooFew,
    /// A baseline row (the bar to beat, not a candidate).
    Baseline,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceScore {
    pub id: String,
    pub kind: SourceKind,
    pub occurrences: usize,
    pub first: Option<DateTime<Utc>>,
    pub last: Option<DateTime<Utc>>,
    /// Weeks between the first and last scored occurrence at the primary horizon.
    pub weeks: f64,
    pub horizons: Vec<HorizonScore>,
    /// Primary horizon over occurrences in the recent window only.
    pub recent: HorizonScore,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scoreboard {
    pub horizons: Vec<String>,
    pub primary: String,
    pub cost: f64,
    pub recent_days: i64,
    pub min_outcomes: usize,
    pub min_weeks: f64,
    pub now: DateTime<Utc>,
    /// News first, then technical signals, then baselines; strongest excess evidence (t at the
    /// primary horizon) first within each.
    pub rows: Vec<SourceScore>,
}

/// Every non-neutral technical signal as a source, for occurrences known at or after `start`.
/// A signal on bar `i` is known when the bar closes, i.e. at the next bar's open.
pub fn technical_sources(
    series: &[Series],
    start: DateTime<Utc>,
    cfg: SignalConfig,
) -> Vec<Source> {
    let mut by_signal: BTreeMap<Signal, Vec<Occurrence>> = BTreeMap::new();
    for s in series {
        for (i, signal) in occurrences(&s.bars, cfg) {
            if signal.bias() == Bias::Neutral {
                continue;
            }
            let Some(bar) = s.bars.get(i) else { continue };
            // The last bar's signal is known only after it closes: pending until a bar follows.
            let known = s
                .bars
                .get(i + 1)
                .map_or(bar.ts + Duration::seconds(1), |next| next.ts);
            if known >= start {
                by_signal.entry(signal).or_default().push(Occurrence {
                    instrument: s.instrument.clone(),
                    ts: known,
                    direction: signal.bias().sign(),
                    strength: 1.0,
                });
            }
        }
    }
    Signal::all()
        .into_iter()
        .filter(|signal| signal.bias() != Bias::Neutral)
        .map(|signal| Source {
            id: signal.to_string(),
            kind: SourceKind::Technical,
            occurrences: by_signal.remove(&signal).unwrap_or_default(),
        })
        .collect()
}

/// *Always long* and *random* at every bar starting at or after `start`.
pub fn baselines(series: &[Series], start: DateTime<Utc>) -> Vec<Source> {
    let mut long = Vec::new();
    let mut random = Vec::new();
    for s in series {
        let salt = fnv(s.instrument.to_string().as_bytes());
        for bar in s.bars.iter().filter(|b| b.ts >= start) {
            let occurrence = |direction| Occurrence {
                instrument: s.instrument.clone(),
                ts: bar.ts,
                direction,
                strength: 1.0,
            };
            long.push(occurrence(1.0));
            let coin = splitmix(salt ^ bar.ts.timestamp().cast_unsigned());
            random.push(occurrence(if coin & 1 == 0 { 1.0 } else { -1.0 }));
        }
    }
    vec![
        Source {
            id: ALWAYS_LONG.into(),
            kind: SourceKind::Baseline,
            occurrences: long,
        },
        Source {
            id: "random".into(),
            kind: SourceKind::Baseline,
            occurrences: random,
        },
    ]
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

const fn splitmix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Raw return of trading a call known at `ts` over `length`, or `None` while the window is
/// open (or without price data).
fn forward(bars: &[Bar], ts: DateTime<Utc>, length: Duration) -> Option<f64> {
    let entry_index = bars.partition_point(|b| b.ts < ts);
    let entry = bars.get(entry_index)?;
    if entry.open <= 0.0 || entry.ts - ts > length {
        return None;
    }
    // The first bar starting at or after the window's end must exist: the window is closed.
    let end = bars.partition_point(|b| b.ts < entry.ts + length);
    if end >= bars.len() {
        return None;
    }
    let exit_index = end.checked_sub(1).filter(|i| *i >= entry_index)?;
    Some(bars.get(exit_index)?.close / entry.open - 1.0)
}

/// (ts, signed strength, signed return) of one source's closed windows at one horizon.
type Outcomes = Vec<(DateTime<Utc>, f64, f64)>;

fn outcomes(source: &Source, bars: &BTreeMap<&InstrumentId, &[Bar]>, length: Duration) -> Outcomes {
    source
        .occurrences
        .iter()
        .filter_map(|o| {
            let r = forward(bars.get(&o.instrument)?, o.ts, length)?;
            Some((o.ts, o.direction * o.strength, o.direction.signum() * r))
        })
        .collect()
}

fn source_score(
    source: &Source,
    bars: &BTreeMap<&InstrumentId, &[Bar]>,
    cfg: &ScoreConfig,
) -> SourceScore {
    let per_horizon: Vec<Outcomes> = cfg
        .horizons
        .iter()
        .map(|h| outcomes(source, bars, h.length))
        .collect();
    let total = source.occurrences.len();
    let horizons = cfg
        .horizons
        .iter()
        .zip(&per_horizon)
        .map(|(h, values)| horizon_score(&h.label, values, total, cfg.cost))
        .collect();
    let primary = cfg
        .horizons
        .iter()
        .position(|h| h.label == cfg.primary)
        .and_then(|i| per_horizon.get(i))
        .map_or(&[][..], Vec::as_slice);
    let recent_start = cfg.now - cfg.recent;
    let recent_total = source
        .occurrences
        .iter()
        .filter(|o| o.ts >= recent_start)
        .count();
    let recent: Outcomes = primary
        .iter()
        .filter(|(ts, ..)| *ts >= recent_start)
        .copied()
        .collect();
    let span = match (primary.first(), primary.last()) {
        (Some(first), Some(last)) => last.0 - first.0,
        _ => Duration::zero(),
    };
    SourceScore {
        id: source.id.clone(),
        kind: source.kind,
        occurrences: total,
        first: source.occurrences.iter().map(|o| o.ts).min(),
        last: source.occurrences.iter().map(|o| o.ts).max(),
        weeks: span.num_seconds() as f64 / (7.0 * 86_400.0),
        horizons,
        recent: horizon_score(&cfg.primary, &recent, recent_total, cfg.cost),
        verdict: Verdict::TooFew,
    }
}

/// Score every source against `series` (pooled across instruments).
pub fn score(sources: &[Source], series: &[Series], cfg: &ScoreConfig) -> Scoreboard {
    let bars: BTreeMap<&InstrumentId, &[Bar]> = series
        .iter()
        .map(|s| (&s.instrument, s.bars.as_slice()))
        .collect();
    let mut rows: Vec<SourceScore> = sources
        .iter()
        .map(|source| source_score(source, &bars, cfg))
        .collect();

    // The drift per horizon: always long's mean signed return.
    let drift: BTreeMap<String, f64> = rows
        .iter()
        .filter(|r| r.id == ALWAYS_LONG)
        .flat_map(|r| r.horizons.iter())
        .filter(|h| h.count > 0)
        .map(|h| (h.horizon.clone(), h.mean_return))
        .collect();
    for row in &mut rows {
        for h in row
            .horizons
            .iter_mut()
            .chain(std::iter::once(&mut row.recent))
        {
            let drift = drift.get(&h.horizon).copied().unwrap_or(0.0);
            h.excess = h.bias.mul_add(-drift, h.mean_return);
            if h.std_error > TOLERANCE && h.excess.abs() > TOLERANCE {
                h.t_excess = h.excess / h.std_error;
            }
        }
    }
    let at_primary = |row: &SourceScore| -> HorizonScore {
        row.horizons
            .iter()
            .find(|h| h.horizon == cfg.primary)
            .cloned()
            .unwrap_or_default()
    };
    for row in &mut rows {
        let primary = at_primary(row);
        row.verdict = if row.kind == SourceKind::Baseline {
            Verdict::Baseline
        } else if primary.count < cfg.min_outcomes {
            Verdict::TooFew
        } else if row.weeks < cfg.min_weeks {
            Verdict::Collecting
        } else if primary.net > 0.0 && primary.t_excess > 2.0 {
            Verdict::Edge
        } else {
            Verdict::NoEdge
        };
    }
    let evidence = |row: &SourceScore| {
        let primary = at_primary(row);
        if primary.count > 0 {
            primary.t_excess
        } else {
            f64::NEG_INFINITY
        }
    };
    rows.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| evidence(b).total_cmp(&evidence(a)))
    });

    Scoreboard {
        horizons: cfg.horizons.iter().map(|h| h.label.clone()).collect(),
        primary: cfg.primary.clone(),
        cost: cfg.cost,
        recent_days: cfg.recent.num_days(),
        min_outcomes: cfg.min_outcomes,
        min_weeks: cfg.min_weeks,
        now: cfg.now,
        rows,
    }
}

/// `values`: closed windows of `total` occurrences.
fn horizon_score(
    label: &str,
    values: &[(DateTime<Utc>, f64, f64)],
    total: usize,
    cost: f64,
) -> HorizonScore {
    let count = values.len();
    let mut score = HorizonScore {
        horizon: label.to_owned(),
        count,
        pending: total.saturating_sub(count),
        ..HorizonScore::default()
    };
    if count == 0 {
        return score;
    }
    let n = count as f64;
    let mean = values.iter().map(|v| v.2).sum::<f64>() / n;
    score.mean_return = mean;
    score.hit_rate = values.iter().filter(|v| v.2 > 0.0).count() as f64 / n;
    score.net = mean - cost;
    score.bias = values.iter().map(|v| v.1.signum()).sum::<f64>() / n;
    if count > 1 {
        let variance = values.iter().map(|v| (v.2 - mean).powi(2)).sum::<f64>() / (n - 1.0);
        score.std_error = (variance / n).sqrt();
        if score.std_error > TOLERANCE {
            score.t_stat = mean / score.std_error;
        }
    }
    // Raw return = signed return × direction (strength > 0, so its sign is the direction's).
    let strengths: Vec<f64> = values.iter().map(|v| v.1).collect();
    let raw: Vec<f64> = values.iter().map(|v| v.2 * v.1.signum()).collect();
    score.ic = spearman(&strengths, &raw);
    score
}

/// Spearman rank correlation (average ranks for ties); `None` below 3 points or when either
/// side is constant.
pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 3 {
        return None;
    }
    let (rx, ry) = (ranks(xs), ranks(ys));
    let n = rx.len() as f64;
    let (mx, my) = (rx.iter().sum::<f64>() / n, ry.iter().sum::<f64>() / n);
    let (mut cov, mut vx, mut vy) = (0.0, 0.0, 0.0);
    for (x, y) in rx.iter().zip(&ry) {
        cov = (x - mx).mul_add(y - my, cov);
        vx += (x - mx).powi(2);
        vy += (y - my).powi(2);
    }
    (vx > 0.0 && vy > 0.0).then(|| cov / (vx * vy).sqrt())
}

fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    order.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut ranks = vec![0.0; values.len()];
    let mut start = 0;
    while start < order.len() {
        let value = order.get(start).map(|o| o.1);
        let end = start
            + order
                .iter()
                .skip(start)
                .take_while(|o| Some(o.1) == value)
                .count();
        // Positions start..end share the average 1-based rank.
        let rank = (start + end + 1) as f64 / 2.0;
        for (index, _) in order.iter().skip(start).take(end - start) {
            if let Some(slot) = ranks.get_mut(*index) {
                *slot = rank;
            }
        }
        start = end;
    }
    ranks
}

#[cfg(test)]
mod tests;
