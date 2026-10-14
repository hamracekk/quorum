//! Position sizing across markets: how much each trade gets, how many trades may run at once,
//! and how much of the account each market may use.
//!
//! The strategy says *what* to hold (a target per instrument); [`SizingRules`] can then
//! override *how much*, applied by the engine to every timestamp's targets before the risk
//! limits:
//!
//! - `mode`: keep the strategy's sizes (`strategy`), give every holding the same share
//!   (`equal`), or start every trade with the same amount of money (`fixed`, `fixed_notional`);
//! - `max_notional`: no trade larger than this amount of money;
//! - `max_positions`: at most this many trades at once — holdings already open keep their
//!   place, then the largest targets;
//! - `markets`: a budget per market (`binance:spot`, later `ibkr:equity` …) — at most a share
//!   of equity in it and at most a number of positions — so a portfolio can span crypto,
//!   equities and other markets with each held to its limits.
//!
//! The defaults change nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;

/// How each trade is sized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SizingMode {
    /// The strategy's own sizes (e.g. volatility-targeted).
    #[default]
    Strategy,
    /// The same share for every holding: `1 / max_positions`, or `1 / holdings wanted`.
    Equal,
    /// The same amount of money for every new trade (`fixed_notional`); open trades keep it.
    Fixed,
}

/// Limits for one market.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketBudget {
    /// `venue` (every kind) or `venue:kind` (e.g. `binance:spot`, `ibkr:equity`).
    pub market: String,
    /// Most of equity held in this market, gross (`0` = no cap).
    #[serde(default)]
    pub budget: f64,
    /// Most positions at once in this market (`0` = no cap).
    #[serde(default)]
    pub max_positions: usize,
}

/// How targets are sized (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SizingRules {
    pub mode: SizingMode,
    /// Money every trade starts with in `fixed` mode.
    pub fixed_notional: f64,
    /// Largest trade in money (`0` = no cap).
    pub max_notional: f64,
    /// Most trades at once across all markets (`0` = no cap).
    pub max_positions: usize,
    pub markets: Vec<MarketBudget>,
}

/// The market key of an instrument: `venue:kind`.
pub fn market_of(instrument: &InstrumentId) -> String {
    format!("{}:{}", instrument.venue(), instrument.kind().code())
}

fn matches(budget: &MarketBudget, instrument: &InstrumentId) -> bool {
    let key = market_of(instrument);
    budget.market == key || key.split(':').next() == Some(budget.market.as_str())
}

impl SizingRules {
    /// Nothing to do: the strategy's targets pass untouched.
    pub fn is_passthrough(&self) -> bool {
        self.mode == SizingMode::Strategy
            && self.max_notional <= 0.0
            && self.max_positions == 0
            && self.markets.is_empty()
    }

    /// Size `wanted` (target per instrument, fractions of equity) at `equity`; `held` holds the
    /// targets currently in force (so open trades keep their place under a position cap).
    pub fn apply(
        &self,
        wanted: &BTreeMap<InstrumentId, f64>,
        held: &BTreeMap<InstrumentId, f64>,
        equity: f64,
    ) -> BTreeMap<InstrumentId, f64> {
        if self.is_passthrough() || equity <= 0.0 {
            return wanted.clone();
        }
        let live = wanted.values().filter(|t| t.abs() > f64::EPSILON).count();
        let slots = if self.max_positions > 0 {
            self.max_positions
        } else {
            live.max(1)
        };
        let mut out: BTreeMap<InstrumentId, f64> = wanted
            .iter()
            .map(|(id, t)| {
                let sized = if t.abs() <= f64::EPSILON {
                    0.0
                } else {
                    match self.mode {
                        SizingMode::Strategy => *t,
                        SizingMode::Equal => t.signum() / slots as f64,
                        // An open trade keeps the size it started with; only new ones are
                        // sized at today's equity (no daily resizing as equity moves).
                        SizingMode::Fixed => held
                            .get(id)
                            .copied()
                            .filter(|h| h.signum() == t.signum() && h.abs() > f64::EPSILON)
                            .unwrap_or_else(|| t.signum() * self.fixed_notional.max(0.0) / equity),
                    }
                };
                let capped = if self.max_notional > 0.0 {
                    sized.clamp(-self.max_notional / equity, self.max_notional / equity)
                } else {
                    sized
                };
                (id.clone(), capped)
            })
            .collect();
        if self.max_positions > 0 {
            keep_at_most(&mut out, held, |_| true, self.max_positions);
        }
        for budget in &self.markets {
            if budget.max_positions > 0 {
                keep_at_most(
                    &mut out,
                    held,
                    |id| matches(budget, id),
                    budget.max_positions,
                );
            }
            if budget.budget > 0.0 {
                let gross: f64 = out
                    .iter()
                    .filter(|(id, _)| matches(budget, id))
                    .map(|(_, t)| t.abs())
                    .sum();
                if gross > budget.budget {
                    let scale = budget.budget / gross;
                    for (id, t) in &mut out {
                        if matches(budget, id) {
                            *t *= scale;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Zero all but `n` of the non-zero targets among those `selected`: open holdings first, then
/// the largest, ties by instrument.
fn keep_at_most(
    targets: &mut BTreeMap<InstrumentId, f64>,
    held: &BTreeMap<InstrumentId, f64>,
    selected: impl Fn(&InstrumentId) -> bool,
    n: usize,
) {
    let mut live: Vec<(InstrumentId, bool, f64)> = targets
        .iter()
        .filter(|(id, t)| t.abs() > f64::EPSILON && selected(id))
        .map(|(id, t)| {
            let open = held.get(id).is_some_and(|h| h.abs() > f64::EPSILON);
            (id.clone(), open, t.abs())
        })
        .collect();
    if live.len() <= n {
        return;
    }
    live.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.total_cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    for (id, _, _) in live.into_iter().skip(n) {
        targets.insert(id, 0.0);
    }
}

#[cfg(test)]
mod tests;
