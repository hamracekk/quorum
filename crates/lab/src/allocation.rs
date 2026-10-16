//! Cross-market allocation study (DESIGN §11, P3 exit criterion): does a book across slots beat
//! each slot alone on Sharpe **and** drawdown?
//!
//! Inputs are each slot's walk-forward (out-of-sample) equity curve and its market's regime
//! exposure per day. Variants, all on the same days and rebalanced from trailing data only:
//! each slot alone, equal capital, risk parity (inverse trailing volatility, capped), and risk
//! parity tilted by each market's regime (what the regime holds back stays in cash). The days
//! are split into an earlier and a later part to show whether the answer holds over time.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use quorum_core::EquityPoint;
use quorum_core::metrics::Performance;
use serde::Serialize;

use crate::portfolio::{CombineConfig, Stream, Weighting, combine};

/// One slot's input.
#[derive(Debug, Clone)]
pub struct SlotStream {
    pub name: String,
    /// Walk-forward equity curve.
    pub curve: Vec<EquityPoint>,
    /// Day → exposure its market's regime allows (from `core::regime`, known at that day's
    /// close).
    pub regime_exposure: BTreeMap<NaiveDate, f64>,
}

#[derive(Debug, Clone)]
pub struct AllocationConfig {
    pub initial_cash: f64,
    /// Risk parity's cap on any slot's weight.
    pub max_weight: f64,
    pub vol_window_days: usize,
    pub rebalance_days: usize,
    /// Share of days held out as the later part.
    pub later_share: f64,
}

impl Default for AllocationConfig {
    fn default() -> Self {
        Self {
            initial_cash: 10_000.0,
            max_weight: 0.8,
            vol_window_days: 60,
            rebalance_days: 7,
            later_share: 0.3,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Variant {
    pub name: String,
    pub whole: Performance,
    pub earlier: Performance,
    pub later: Performance,
    /// Average weight per slot (cash is the rest).
    pub avg_weights: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AllocationStudy {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub later_from: Option<NaiveDate>,
    /// Correlation of the slots' daily returns, in input order.
    pub correlation: Vec<Vec<f64>>,
    pub variants: Vec<Variant>,
    /// The regime-tilted risk-parity book beats every slot alone on Sharpe and drawdown, in
    /// both parts.
    pub beats_each_alone: bool,
}

fn performance(curve: &[EquityPoint]) -> Performance {
    curve.first().map_or_else(Performance::default, |first| {
        Performance::from_curve(first.equity, curve)
    })
}

fn variant(
    name: &str,
    streams: &[Stream],
    cfg: &CombineConfig,
    later_share: f64,
) -> (Variant, Vec<Vec<f64>>) {
    let combined = combine(streams, cfg);
    let split = crate::signals::split_index(combined.curve.len(), later_share);
    let variant = Variant {
        name: name.to_owned(),
        whole: performance(&combined.curve),
        earlier: performance(&combined.curve[..split]),
        later: performance(&combined.curve[split.saturating_sub(1)..]),
        avg_weights: combined
            .streams
            .iter()
            .map(|s| (s.name.clone(), s.avg_weight))
            .collect(),
    };
    (variant, combined.correlation)
}

/// Studies allocating across `slots`.
#[must_use]
pub fn study(slots: &[SlotStream], cfg: &AllocationConfig) -> AllocationStudy {
    let streams: Vec<Stream> = slots
        .iter()
        .map(|s| Stream {
            name: s.name.clone(),
            curve: s.curve.clone(),
        })
        .collect();
    let n = slots.len();
    let base = CombineConfig {
        initial_cash: cfg.initial_cash,
        weighting: Weighting::Fixed(vec![1.0; n]),
        vol_window_days: cfg.vol_window_days,
        rebalance_days: cfg.rebalance_days,
        tilts: Vec::new(),
    };
    let mut variants = Vec::new();
    for (i, slot) in slots.iter().enumerate() {
        let mut one = vec![0.0; n];
        one[i] = 1.0;
        let alone = CombineConfig {
            weighting: Weighting::Fixed(one),
            ..base.clone()
        };
        variants.push(
            variant(
                &format!("{} alone", slot.name),
                &streams,
                &alone,
                cfg.later_share,
            )
            .0,
        );
    }
    variants.push(variant("equal capital", &streams, &base, cfg.later_share).0);
    let parity = CombineConfig {
        weighting: Weighting::InverseVol {
            max_weight: cfg.max_weight,
        },
        ..base.clone()
    };
    let (rp, correlation) = variant("risk parity", &streams, &parity, cfg.later_share);
    variants.push(rp);
    let tilted = CombineConfig {
        tilts: slots.iter().map(|s| s.regime_exposure.clone()).collect(),
        ..parity
    };
    let tilt = variant("risk parity + regime", &streams, &tilted, cfg.later_share).0;
    let alone = &variants[..n];
    let better =
        |a: &Performance, b: &Performance| a.sharpe > b.sharpe && a.max_drawdown < b.max_drawdown;
    let beats_each_alone = alone
        .iter()
        .all(|s| better(&tilt.earlier, &s.earlier) && better(&tilt.later, &s.later));
    variants.push(tilt);
    let combined = combine(&streams, &base);
    let split = crate::signals::split_index(combined.curve.len(), cfg.later_share);
    AllocationStudy {
        from: combined.curve.first().map(|p| p.ts.date_naive()),
        to: combined.curve.last().map(|p| p.ts.date_naive()),
        later_from: combined.curve.get(split).map(|p| p.ts.date_naive()),
        correlation,
        variants,
        beats_each_alone,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::*;

    /// Two streams with the same drift and uncorrelated wiggles: combining them halves the noise.
    fn stream(name: &str, phase: i64) -> SlotStream {
        let t0 = Utc
            .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
            .single()
            .expect("valid");
        let mut e = 10_000.0;
        let curve = (0..1_000)
            .map(|d| {
                let wiggle = if (d + phase) % 4 < 2 { 0.02 } else { -0.018 };
                e *= 1.0 + wiggle;
                EquityPoint {
                    ts: t0 + Duration::days(d),
                    equity: e,
                }
            })
            .collect();
        SlotStream {
            name: name.into(),
            curve,
            regime_exposure: BTreeMap::new(),
        }
    }

    #[test]
    fn uncorrelated_slots_combine_into_a_smoother_book() {
        let slots = [stream("a", 0), stream("b", 1)];
        let report = study(&slots, &AllocationConfig::default());
        let get = |name: &str| {
            report
                .variants
                .iter()
                .find(|v| v.name == name)
                .expect("variant")
        };
        let (a, rp) = (get("a alone"), get("risk parity"));
        assert!(
            rp.whole.max_drawdown < a.whole.max_drawdown,
            "{rp:?} vs {a:?}"
        );
        assert!(rp.whole.sharpe > a.whole.sharpe);
        assert!(report.correlation[0][1] < 0.5);
        assert_eq!(
            report.variants.len(),
            5,
            "two alone, equal, parity, parity + regime"
        );
    }

    #[test]
    fn a_regime_at_zero_exposure_holds_that_slot_in_cash() {
        let mut slots = [stream("a", 0), stream("b", 1)];
        let first = slots[1].curve[0].ts.date_naive();
        slots[1].regime_exposure = [(first, 0.0)].into();
        let report = study(&slots, &AllocationConfig::default());
        let tilted = report
            .variants
            .iter()
            .find(|v| v.name == "risk parity + regime")
            .expect("variant");
        let b = tilted
            .avg_weights
            .iter()
            .find(|(n, _)| n == "b")
            .expect("b")
            .1;
        assert!(b < 0.05, "b held in cash almost throughout: {b}");
    }
}
