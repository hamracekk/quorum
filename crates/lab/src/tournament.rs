//! Tournament rules (M9, PLAN section 5): when a challenger replaces the champion, and when a
//! contender is retired.
//!
//! Every contender is judged on the same bars from the day it entered (paper is deterministic,
//! so its shadow record is a backtest over those bars with the one engine). A challenger is
//! promoted when, over its own window, it beat the champion's Sharpe without a worse drawdown,
//! on enough days and trades; the best such challenger wins. A challenger that trails buy & hold
//! (Sharpe) at two consecutive reviews is retired: killed faster than promoted. A champion that
//! trails buy & hold is flagged, not removed — only a better challenger replaces it.
//!
//! E-4: a promotion also needs the challenger's deflated Sharpe — the probability its Sharpe is
//! real after every contender ever tried — to reach `min_deflated`; a few weeks of luck from one
//! of many challengers is not enough.

use serde::Serialize;
use trader_core::Report;

use crate::overfit::deflated_sharpe_trials;

#[derive(Debug, Clone, Copy)]
pub struct Rules {
    /// Days a challenger must have run before it can be promoted.
    pub min_days: f64,
    /// Trades it must have made.
    pub min_trades: usize,
    /// Consecutive reviews below buy & hold before a challenger is retired.
    pub retire_after: u32,
    /// Days before a review counts against a contender at all.
    pub grace_days: f64,
    /// Least deflated Sharpe for a promotion (`0` = off).
    pub min_deflated: f64,
    /// Contenders ever tried, champion and retired ones included.
    pub trials: usize,
    /// Bars per year of the records (365 for daily bars).
    pub periods_per_year: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            min_days: 28.0,
            min_trades: 10,
            retire_after: 2,
            grace_days: 7.0,
            min_deflated: 0.9,
            trials: 0,
            periods_per_year: 365.0,
        }
    }
}

/// A challenger at a review: its record and the champion's over the same window.
#[derive(Debug, Clone)]
pub struct Standing {
    pub id: String,
    pub days: f64,
    pub record: Report,
    /// The champion over the challenger's window.
    pub champion: Report,
    /// Consecutive earlier reviews it trailed buy & hold.
    pub losses: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Promote {
        id: String,
        reason: String,
    },
    Retire {
        id: String,
        reason: String,
    },
    /// Still running; `losses` is its new count of consecutive reviews below buy & hold.
    Keep {
        id: String,
        losses: u32,
        reason: String,
    },
}

/// What happens to the challengers at a review (at most one promotion).
pub fn review(standings: &[Standing], rules: Rules) -> Vec<Action> {
    let sharpes: Vec<f64> = standings
        .iter()
        .map(|s| s.record.strategy.sharpe)
        .chain(standings.first().map(|s| s.champion.strategy.sharpe))
        .collect();
    let deflated = |s: &Standing| {
        deflated_sharpe_trials(
            s.record.strategy.sharpe,
            &sharpes,
            rules.trials,
            s.days / 365.0,
            rules.periods_per_year,
        )
    };
    let real = |s: &Standing| rules.min_deflated <= 0.0 || deflated(s) >= rules.min_deflated;
    let eligible = |s: &Standing| {
        s.days >= rules.min_days
            && s.record.trades >= rules.min_trades
            && s.record.strategy.sharpe > s.champion.strategy.sharpe
            && s.record.strategy.max_drawdown <= s.champion.strategy.max_drawdown + 1e-9
            && real(s)
    };
    let winner = standings
        .iter()
        .filter(|s| eligible(s))
        .max_by(|a, b| {
            a.record
                .strategy
                .sharpe
                .total_cmp(&b.record.strategy.sharpe)
        })
        .map(|s| s.id.clone());
    standings
        .iter()
        .map(|s| {
            let sharpe = s.record.strategy.sharpe;
            let champion = s.champion.strategy.sharpe;
            if winner.as_deref() == Some(s.id.as_str()) {
                return Action::Promote {
                    id: s.id.clone(),
                    reason: format!(
                        "Sharpe {sharpe:.2} vs champion {champion:.2}, drawdown {:.1}% vs {:.1}%, \
                         {} trades over {:.0} days",
                        s.record.strategy.max_drawdown * 100.0,
                        s.champion.strategy.max_drawdown * 100.0,
                        s.record.trades,
                        s.days
                    ),
                };
            }
            let baseline = s.record.benchmark.sharpe;
            let counted = s.days >= rules.grace_days;
            let trailing = counted && sharpe < baseline;
            let losses = if trailing { s.losses + 1 } else { 0 };
            if losses >= rules.retire_after {
                Action::Retire {
                    id: s.id.clone(),
                    reason: format!(
                        "Sharpe {sharpe:.2} below buy & hold {baseline:.2} at {losses} reviews in a row"
                    ),
                }
            } else {
                let why = if s.days < rules.min_days {
                    format!("{:.0} of {:.0} days", s.days, rules.min_days)
                } else if s.record.trades < rules.min_trades {
                    format!("{} of {} trades", s.record.trades, rules.min_trades)
                } else if sharpe <= champion {
                    format!("Sharpe {sharpe:.2} not above champion {champion:.2}")
                } else if s.record.strategy.max_drawdown
                    > s.champion.strategy.max_drawdown + 1e-9
                {
                    "drawdown worse than the champion's".to_owned()
                } else {
                    format!(
                        "deflated Sharpe {:.0}% below {:.0}% ({} contenders tried): could be luck",
                        deflated(s) * 100.0,
                        rules.min_deflated * 100.0,
                        rules.trials.max(sharpes.len())
                    )
                };
                Action::Keep {
                    id: s.id.clone(),
                    losses,
                    reason: why,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
