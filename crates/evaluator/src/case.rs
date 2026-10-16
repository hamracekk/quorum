//! Case files: every entry a strategy proposes, with what was known when it was proposed and,
//! later, how it turned out — whether or not it was taken.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use quorum_core::factors::{PriceFactor, price_family, rank_scores};
use quorum_core::regime::{Regime, RegimeRule, regimes};
use quorum_core::{Bar, Event, InstrumentId, Series};
use serde::{Deserialize, Serialize};

/// Fixed horizons, in bars after the entry fill, at which every case is labelled besides the
/// strategy's own exit.
pub const HORIZONS: [usize; 3] = [1, 5, 20];

/// The label key of the strategy's own horizon: held until it exits.
pub const EXIT: &str = "exit";

/// Targets smaller than this count as flat.
const FLAT: f64 = 1e-9;

/// One proposed entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    /// The bar whose close produced the proposal; the entry fills at the next bar's open.
    pub ts: DateTime<Utc>,
    pub instrument: InstrumentId,
    /// +1 long, −1 short.
    pub direction: f64,
    /// The risk-adjusted target weight proposed.
    pub target: f64,
    /// Whether the entry was filled (false: refused by risk, halted, or never reached a venue).
    pub taken: bool,
    /// The market's regime at `ts`, when enough history exists to label it.
    pub regime: Option<Regime>,
    /// Point-in-time features at `ts`: `f:<factor>` values, `rank:<factor>` cross-sectional
    /// ranks in `[−1, 1]`, `fit:held` (positions already held), `fit:corr` (mean return
    /// correlation to them).
    pub features: BTreeMap<String, f64>,
    /// Net returns after the round-trip cost, signed by direction: `h<n>` at each fixed horizon,
    /// [`EXIT`] at the strategy's own exit. Missing until the future bars exist.
    pub outcomes: BTreeMap<String, f64>,
    /// Bars held until the strategy's exit, once it has exited.
    pub held_bars: Option<usize>,
    /// When the exit filled: the label is known from then on (a model may only learn from cases
    /// whose exit came before the cases it predicts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_ts: Option<DateTime<Utc>>,
}

/// How cases are built.
#[derive(Debug, Clone, Copy)]
pub struct CaseConfig {
    /// Round-trip cost as a fraction (fees and slippage, both legs).
    pub cost: f64,
    /// Bars in a year on the slot's calendar.
    pub periods_per_year: f64,
    /// Bars behind the return correlation to positions already held.
    pub corr_bars: usize,
    pub regime_rule: RegimeRule,
}

impl CaseConfig {
    #[must_use]
    pub fn new(cost: f64, periods_per_year: f64) -> Self {
        Self {
            cost,
            periods_per_year,
            corr_bars: 60,
            regime_rule: RegimeRule::default(),
        }
    }
}

/// One series with its bars indexed by time.
struct Indexed<'a> {
    bars: &'a [Bar],
    at: BTreeMap<DateTime<Utc>, usize>,
}

/// Builds a case for every entry in `events` (a target going from flat to a position, or
/// flipping side) and labels it from the bars after it.
///
/// `series` are the bars the decisions were made on; `fills` the `(instrument, fill time)` of
/// every fill that happened, which decides `taken`. Features use bars up to the case's bar only.
///
/// With `measure_from` (a paper session's first traded bar), targets from the warm-up bars before
/// it are not entries: what the warm-up left is bought at the first traded bar, so each such
/// position becomes one case decided on the bar before it.
#[must_use]
pub fn cases(
    series: &[Series],
    events: &[Event],
    fills: &BTreeSet<(InstrumentId, DateTime<Utc>)>,
    measure_from: Option<DateTime<Utc>>,
    cfg: &CaseConfig,
) -> Vec<Case> {
    let events = measure_from.map_or_else(|| events.to_vec(), |m| measured(series, events, m));
    let indexed: BTreeMap<&InstrumentId, Indexed<'_>> = series
        .iter()
        .map(|s| {
            let at = s.bars.iter().enumerate().map(|(i, b)| (b.ts, i)).collect();
            (&s.instrument, Indexed { bars: &s.bars, at })
        })
        .collect();
    let regime: BTreeMap<DateTime<Utc>, Regime> =
        regimes(series, &cfg.regime_rule, year_bars(cfg.periods_per_year))
            .into_iter()
            .map(|p| (p.ts, p.regime))
            .collect();
    let family = price_family(cfg.periods_per_year);

    let mut out: Vec<Case> = Vec::new();
    let mut last: BTreeMap<InstrumentId, f64> = BTreeMap::new();
    let mut open: BTreeMap<InstrumentId, usize> = BTreeMap::new();
    for event in &events {
        let Event::Target {
            ts,
            instrument,
            target,
        } = event
        else {
            continue;
        };
        let Some(own) = indexed.get(instrument) else {
            continue;
        };
        let prev = last.get(instrument).copied().unwrap_or(0.0);
        let was_open = prev.abs() >= FLAT;
        let is_open = target.abs() >= FLAT;
        let flipped = was_open && is_open && prev.signum() != target.signum();
        if was_open
            && (!is_open || flipped)
            && let Some(case) = open.remove(instrument)
        {
            label_exit(&mut out[case], own, *ts, cfg.cost);
        }
        if is_open
            && (!was_open || flipped)
            && let Some(&i) = own.at.get(ts)
        {
            // Held: positions opened on an earlier bar (entries on this bar are simultaneous).
            let held: Vec<&InstrumentId> = open
                .iter()
                .filter(|(id, c)| *id != instrument && out[**c].ts < *ts)
                .map(|(id, _)| id)
                .collect();
            let mut case = Case {
                ts: *ts,
                instrument: instrument.clone(),
                direction: target.signum(),
                target: *target,
                taken: own
                    .bars
                    .get(i + 1)
                    .is_some_and(|b| fills.contains(&(instrument.clone(), b.ts))),
                regime: regime.get(ts).copied(),
                features: features(&family, &indexed, instrument, *ts, &held, cfg),
                outcomes: BTreeMap::new(),
                held_bars: None,
                exit_ts: None,
            };
            label_horizons(&mut case, own, i, cfg.cost);
            open.insert(instrument.clone(), out.len());
            out.push(case);
        }
        last.insert(instrument.clone(), *target);
    }
    out
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a positive bar count, rounded"
)]
const fn year_bars(periods_per_year: f64) -> usize {
    periods_per_year.round().max(2.0) as usize
}

/// `events` with the warm-up's targets replaced by one target per position it left, on each
/// instrument's last bar before `measure_from`.
fn measured(series: &[Series], events: &[Event], measure_from: DateTime<Utc>) -> Vec<Event> {
    let mut carried: BTreeMap<InstrumentId, f64> = BTreeMap::new();
    let mut after = Vec::new();
    for event in events {
        match event {
            Event::Target {
                ts,
                instrument,
                target,
            } if *ts < measure_from => {
                carried.insert(instrument.clone(), *target);
            }
            Event::Target { .. } => after.push(event.clone()),
            _ => {}
        }
    }
    let mut out: Vec<Event> = series
        .iter()
        .filter_map(|s| {
            let target = *carried.get(&s.instrument)?;
            let ts = s.bars.iter().rev().find(|b| b.ts < measure_from)?.ts;
            Some(Event::Target {
                ts,
                instrument: s.instrument.clone(),
                target,
            })
        })
        .collect();
    out.sort_by_key(Event::ts);
    // Targets decided on the bar before `measure_from` are already in `carried`.
    out.extend(after);
    out
}

/// Bars of `s` up to and including `ts`.
fn upto<'a>(s: &Indexed<'a>, ts: DateTime<Utc>) -> Option<&'a [Bar]> {
    s.at.get(&ts).map(|&i| &s.bars[..=i])
}

fn features(
    family: &[Box<dyn PriceFactor>],
    indexed: &BTreeMap<&InstrumentId, Indexed<'_>>,
    instrument: &InstrumentId,
    ts: DateTime<Utc>,
    held: &[&InstrumentId],
    cfg: &CaseConfig,
) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for factor in family {
        let id = factor.id().to_string();
        let values: Vec<(InstrumentId, f64)> = indexed
            .iter()
            .filter_map(|(inst, s)| {
                let v = factor.value(upto(s, ts)?)?;
                v.is_finite().then(|| ((*inst).clone(), v))
            })
            .collect();
        if let Some((_, v)) = values.iter().find(|(inst, _)| inst == instrument) {
            out.insert(format!("f:{id}"), *v);
        }
        if let Some((_, r)) = rank_scores(&values)
            .into_iter()
            .find(|(inst, _)| inst == instrument)
        {
            out.insert(format!("rank:{id}"), r);
        }
    }
    out.insert("fit:held".to_owned(), held.len() as f64);
    let own = indexed
        .get(instrument)
        .and_then(|s| returns(s, ts, cfg.corr_bars));
    let corrs: Vec<f64> = held
        .iter()
        .filter_map(|h| {
            let other = returns(indexed.get(h)?, ts, cfg.corr_bars)?;
            correlation(own.as_ref()?, &other)
        })
        .collect();
    if !corrs.is_empty() {
        out.insert(
            "fit:corr".to_owned(),
            corrs.iter().sum::<f64>() / corrs.len() as f64,
        );
    }
    out
}

/// Log returns into each of the last `n` bars up to `ts`, keyed by bar time.
fn returns(s: &Indexed<'_>, ts: DateTime<Utc>, n: usize) -> Option<BTreeMap<DateTime<Utc>, f64>> {
    let bars = upto(s, ts)?;
    let window = bars.get(bars.len().checked_sub(n + 1)?..)?;
    Some(
        window
            .windows(2)
            .filter(|w| w[0].close > 0.0 && w[1].close > 0.0)
            .map(|w| (w[1].ts, (w[1].close / w[0].close).ln()))
            .collect(),
    )
}

/// Pearson correlation over the dates both have; `None` with fewer than 10 or no dispersion.
fn correlation(a: &BTreeMap<DateTime<Utc>, f64>, b: &BTreeMap<DateTime<Utc>, f64>) -> Option<f64> {
    let pairs: Vec<(f64, f64)> = a
        .iter()
        .filter_map(|(t, x)| b.get(t).map(|y| (*x, *y)))
        .collect();
    if pairs.len() < 10 {
        return None;
    }
    let n = pairs.len() as f64;
    let (mx, my) = pairs
        .iter()
        .fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x / n, sy + y / n));
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in &pairs {
        sxy = (x - mx).mul_add(y - my, sxy);
        sxx += (x - mx).powi(2);
        syy += (y - my).powi(2);
    }
    let r = sxy / (sxx * syy).sqrt();
    r.is_finite().then_some(r)
}

/// Fills at the open of bar `i + 1`; exits at the close of bar `i + h`.
fn label_horizons(case: &mut Case, s: &Indexed<'_>, i: usize, cost: f64) {
    let Some(entry) = s.bars.get(i + 1).map(|b| b.open).filter(|p| *p > 0.0) else {
        return;
    };
    for h in HORIZONS {
        if let Some(exit) = s.bars.get(i + h).map(|b| b.close) {
            case.outcomes.insert(
                format!("h{h}"),
                case.direction.mul_add(exit / entry - 1.0, -cost),
            );
        }
    }
}

/// The strategy's exit decided at `exit_ts` fills at the next bar's open.
fn label_exit(case: &mut Case, s: &Indexed<'_>, exit_ts: DateTime<Utc>, cost: f64) {
    let (Some(&i), Some(&j)) = (s.at.get(&case.ts), s.at.get(&exit_ts)) else {
        return;
    };
    let entry = s.bars.get(i + 1).map(|b| b.open).filter(|p| *p > 0.0);
    let exit_bar = s.bars.get(j + 1);
    let exit = exit_bar.map(|b| b.open);
    if let (Some(entry), Some(exit)) = (entry, exit) {
        case.exit_ts = exit_bar.map(|b| b.ts);
        case.outcomes.insert(
            EXIT.to_owned(),
            case.direction.mul_add(exit / entry - 1.0, -cost),
        );
        case.held_bars = Some(j - i);
    }
}
