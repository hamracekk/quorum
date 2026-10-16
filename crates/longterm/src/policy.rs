//! The operator's long-term policy and the strategy that follows it.
//!
//! A policy is target weights, a monthly contribution and a rebalancing band. The strategy never
//! predicts anything: new money goes to the holdings furthest below their targets (no selling),
//! and only when a holding drifts further than the band from its target is the whole book
//! rebalanced. Selling is rare, which keeps tax lots old enough to be tax-free.

use std::collections::BTreeMap;
use std::path::Path;

use quorum_core::strategy::BookView;
use quorum_core::{Bar, InstrumentId, Strategy};
use serde::Deserialize;

use crate::LongTermError;

/// One holding of a policy.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Holding {
    pub instrument: InstrumentId,
    pub weight: f64,
}

/// A long-term policy.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub name: String,
    /// Invested at the start, in CZK.
    pub start_czk: f64,
    /// Added at the start of every month, in CZK.
    pub monthly_czk: f64,
    /// Rebalance the whole book when a holding's weight is further than this from its target
    /// (absolute, 0.05 = 5 points).
    pub band: f64,
    /// Share of the book kept in cash for costs.
    #[serde(default = "default_buffer")]
    pub cash_buffer: f64,
    pub holdings: Vec<Holding>,
}

const fn default_buffer() -> f64 {
    0.005
}

/// `config/longterm.toml`: how policies are priced and run, and the policies.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongTermConfig {
    /// The research slot whose stored bars price the holdings.
    pub prices_from: String,
    /// One currency conversion's cost (in when contributing, out when valuing at the end).
    pub fx_cost: f64,
    /// Commission per trade as a fraction of notional.
    pub fee_rate: f64,
    pub slippage_bps: f64,
    /// The first is the operator's policy; the others are alternatives it is compared with.
    pub policies: Vec<Policy>,
}

impl LongTermConfig {
    pub fn load(path: &Path) -> Result<Self, LongTermError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| LongTermError::Config(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, LongTermError> {
        let cfg: Self = toml::from_str(text).map_err(|e| LongTermError::Config(e.to_string()))?;
        let invalid = |m: String| Err(LongTermError::Config(m));
        if cfg.policies.is_empty() {
            return invalid("no policies".to_owned());
        }
        let cost = |x: f64| x.is_finite() && (0.0..0.05).contains(&x);
        if !cost(cfg.fx_cost) || !cost(cfg.fee_rate) || !cost(cfg.slippage_bps / 10_000.0) {
            return invalid("fx_cost, fee_rate and slippage must be small fractions".to_owned());
        }
        for p in &cfg.policies {
            let sum: f64 = p.holdings.iter().map(|h| h.weight).sum();
            if p.holdings.is_empty()
                || p.holdings
                    .iter()
                    .any(|h| !(h.weight.is_finite() && h.weight > 0.0))
                || (sum - 1.0).abs() > 1e-9
            {
                return invalid(format!("{}: weights must be positive and sum to 1", p.name));
            }
            if !(p.start_czk >= 0.0 && p.monthly_czk >= 0.0 && p.start_czk + p.monthly_czk > 0.0) {
                return invalid(format!("{}: start_czk or monthly_czk must be > 0", p.name));
            }
            if !(p.band > 0.0 && p.band < 0.5 && (0.0..0.2).contains(&p.cash_buffer)) {
                return invalid(format!(
                    "{}: band in (0, 0.5), cash_buffer in [0, 0.2)",
                    p.name
                ));
            }
        }
        Ok(cfg)
    }
}

/// Targets for a book, or `None` when nothing needs to change.
///
/// At the start every holding gets its target. When `new_money` arrived (a contribution), it
/// goes to the holdings furthest below target and only those are named — nothing is sold.
/// Otherwise the whole book is rebalanced only when a holding is further than the band from its
/// target.
#[must_use]
pub fn plan(
    policy: &Policy,
    book: &BookView,
    first: bool,
    new_money: bool,
) -> Option<BTreeMap<InstrumentId, f64>> {
    if book.equity <= 0.0 {
        return None;
    }
    let invest = 1.0 - policy.cash_buffer;
    let weight = |h: &Holding| book.values.get(&h.instrument).copied().unwrap_or(0.0) / book.equity;
    let full = || {
        Some(
            policy
                .holdings
                .iter()
                .map(|h| (h.instrument.clone(), h.weight * invest))
                .collect(),
        )
    };
    if first {
        return full();
    }
    if new_money {
        let spare = policy.cash_buffer.mul_add(-book.equity, book.cash);
        if spare <= 0.0 {
            return None;
        }
        // Water-fill: close each holding's gap to target with new money; what is left follows
        // the weights.
        let gaps: Vec<f64> = policy
            .holdings
            .iter()
            .map(|h| h.weight.mul_add(invest, -weight(h)).max(0.0) * book.equity)
            .collect();
        let total: f64 = gaps.iter().sum();
        let buys: BTreeMap<InstrumentId, f64> = policy
            .holdings
            .iter()
            .zip(&gaps)
            .filter_map(|(h, gap)| {
                let buy = if total >= spare {
                    gap * spare / total
                } else {
                    (spare - total).mul_add(h.weight, *gap)
                };
                (buy > 0.0).then(|| (h.instrument.clone(), weight(h) + buy / book.equity))
            })
            .collect();
        return (!buys.is_empty()).then_some(buys);
    }
    let breached = policy
        .holdings
        .iter()
        .any(|h| h.weight.mul_add(-invest, weight(h)).abs() > policy.band);
    if breached { full() } else { None }
}

/// The policy as a strategy: it ignores bars and sets targets from the book at each close.
#[derive(Debug)]
pub struct PolicyStrategy {
    policy: Policy,
    started: bool,
    /// Cash at the previous close: more now means a contribution arrived.
    cash: f64,
}

impl PolicyStrategy {
    #[must_use]
    pub const fn new(policy: Policy) -> Self {
        Self {
            policy,
            started: false,
            cash: 0.0,
        }
    }
}

impl Strategy for PolicyStrategy {
    fn name(&self) -> &str {
        &self.policy.name
    }

    fn on_bar(&mut self, _instrument: &InstrumentId, _bar: &Bar) -> Option<f64> {
        None
    }

    fn on_book(&mut self, book: &BookView) -> Option<BTreeMap<InstrumentId, f64>> {
        // A rise of more than a basis point of the book is new money, not rounding.
        let new_money = book.cash > 1e-4f64.mul_add(book.equity, self.cash);
        self.cash = book.cash;
        let targets = plan(&self.policy, book, !self.started, new_money);
        if targets.is_some() {
            self.started = true;
        }
        targets
    }
}
