//! The autonomy ladder (DESIGN §5.3): promotion one rung at a time on measured evidence,
//! automatic demotion, and the envelope's `max_mode` as a ceiling nothing can pass.
//!
//! The ladder only decides. Evidence is measured elsewhere (lab, journal, execution) and handed
//! in as [`Evidence`]; the caller journals each [`Transition`] and acts on it.

use chrono::{DateTime, Utc};
use quorum_envelope::{Envelope, Mode};
use serde::{Deserialize, Serialize};

use crate::config::{SlotConfig, SlotId};

/// Promotion and demotion thresholds. Defaults are DESIGN §5.3's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gates {
    /// Research → Paper: deflated Sharpe over every variant tried.
    pub min_deflated_sharpe: f64,
    /// Research → Paper: probability of backtest overfitting.
    pub max_pbo: f64,
    /// Paper → `LiveMicro`: weeks in paper.
    pub paper_weeks: f64,
    /// `LiveMicro` → Live: weeks in live-micro.
    pub live_micro_weeks: f64,
    /// `LiveMicro` → Live: realised slippage over the cost model.
    pub max_slippage_ratio: f64,
    /// Demotion to Paper: realised costs over the cost model, trailing two weeks.
    pub demote_cost_ratio: f64,
}

impl Default for Gates {
    fn default() -> Self {
        Self {
            min_deflated_sharpe: 0.9,
            max_pbo: 0.3,
            paper_weeks: 8.0,
            live_micro_weeks: 12.0,
            max_slippage_ratio: 1.5,
            demote_cost_ratio: 2.0,
        }
    }
}

/// What has been measured about the slot, grouped by the gate that reads it. `Default` is
/// "nothing proven yet".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Evidence {
    pub research: ResearchRecord,
    pub paper: PaperRecord,
    pub live: LiveRecord,
    pub alarms: Alarms,
}

/// Research → Paper evidence from the lab.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResearchRecord {
    /// Walk-forward beats the slot's benchmark net of costs.
    pub beats_benchmark: bool,
    pub deflated_sharpe: Option<f64>,
    pub pbo: Option<f64>,
}

/// Paper → `LiveMicro` evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaperRecord {
    /// Paper record consistent with the backtest (tracking error inside its band).
    pub tracking_ok: bool,
    /// Every stress scenario stayed within the loss limits.
    pub stress_passed: bool,
}

/// Evidence from real orders.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveRecord {
    /// Realised slippage over the cost model.
    pub slippage_ratio: Option<f64>,
    /// Realised costs over the cost model, trailing two weeks.
    pub cost_ratio: Option<f64>,
    /// Venue vs journal mismatches since the last review.
    pub reconciliation_breaks: u32,
}

/// Conditions that demote a slot one rung.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Alarms {
    pub drawdown_over_budget: bool,
    pub drift: bool,
}

/// Why a slot was demoted or halted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    ReconciliationBreak,
    /// The envelope's `max_mode` is now below the slot's mode.
    EnvelopeLowered,
    CostsOverModel,
    DrawdownOverBudget,
    Drift,
}

/// Why a slot was not promoted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Blocker {
    Halted,
    TopRung,
    /// The envelope caps the slot at this mode.
    EnvelopeCap(Mode),
    NoBenchmarkEdge,
    DeflatedSharpe(Option<f64>),
    Pbo(Option<f64>),
    TooSoon {
        weeks: f64,
        needed: f64,
    },
    Tracking,
    StressTests,
    Slippage(Option<f64>),
}

/// The outcome of a review.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    Promoted {
        from: Mode,
        to: Mode,
    },
    Demoted {
        from: Mode,
        to: Mode,
        trigger: Trigger,
    },
    /// Stopped until the operator calls [`SlotState::resume`].
    Halted {
        trigger: Trigger,
    },
    Held {
        blockers: Vec<Blocker>,
    },
}

/// Where a slot stands. Serializable so it can be journaled and restored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotState {
    id: SlotId,
    mode: Mode,
    /// When the slot entered its current mode.
    since: DateTime<Utc>,
    halted: bool,
}

impl SlotState {
    /// A new slot at its configured start mode, capped by the envelope.
    #[must_use]
    pub fn new(config: &SlotConfig, envelope: &Envelope, now: DateTime<Utc>) -> Self {
        let cap = envelope.max_mode(config.id.as_str());
        Self {
            id: config.id.clone(),
            mode: config.start_mode.min(cap),
            since: now,
            halted: false,
        }
    }

    #[must_use]
    pub const fn id(&self) -> &SlotId {
        &self.id
    }

    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub const fn since(&self) -> DateTime<Utc> {
        self.since
    }

    #[must_use]
    pub const fn is_halted(&self) -> bool {
        self.halted
    }

    /// Weeks in the current mode at `now`.
    #[must_use]
    pub fn weeks_in_mode(&self, now: DateTime<Utc>) -> f64 {
        (now - self.since).num_seconds() as f64 / (7.0 * 86_400.0)
    }

    /// Operator only: clears a halt. The slot resumes at Paper at most — real money again goes
    /// through the ladder, with the paper clock restarted.
    pub fn resume(&mut self, now: DateTime<Utc>) {
        if self.halted {
            self.halted = false;
            if self.mode > Mode::Paper {
                self.mode = Mode::Paper;
            }
            self.since = now;
        }
    }

    /// Reviews the slot: halt, then demotions, then at most one promotion.
    pub fn review(
        &mut self,
        evidence: &Evidence,
        gates: &Gates,
        envelope: &Envelope,
        now: DateTime<Utc>,
    ) -> Transition {
        if self.halted {
            return Transition::Held {
                blockers: vec![Blocker::Halted],
            };
        }
        if evidence.live.reconciliation_breaks > 0 {
            self.halted = true;
            return Transition::Halted {
                trigger: Trigger::ReconciliationBreak,
            };
        }
        let cap = envelope.max_mode(self.id.as_str());
        if let Some((to, trigger)) = self.demotion(evidence, gates, cap) {
            let from = self.mode;
            self.set(to, now);
            return Transition::Demoted { from, to, trigger };
        }
        let Some(to) = self.mode.next() else {
            return Transition::Held {
                blockers: vec![Blocker::TopRung],
            };
        };
        if to > cap {
            return Transition::Held {
                blockers: vec![Blocker::EnvelopeCap(cap)],
            };
        }
        let blockers = self.promotion_blockers(to, evidence, gates, now);
        if blockers.is_empty() {
            let from = self.mode;
            self.set(to, now);
            Transition::Promoted { from, to }
        } else {
            Transition::Held { blockers }
        }
    }

    fn demotion(&self, evidence: &Evidence, gates: &Gates, cap: Mode) -> Option<(Mode, Trigger)> {
        if self.mode > cap {
            return Some((cap, Trigger::EnvelopeLowered));
        }
        let costly = evidence
            .live
            .cost_ratio
            .is_some_and(|r| r > gates.demote_cost_ratio);
        if costly && self.mode.is_live() {
            return Some((Mode::Paper, Trigger::CostsOverModel));
        }
        let trigger = if evidence.alarms.drawdown_over_budget {
            Trigger::DrawdownOverBudget
        } else if evidence.alarms.drift {
            Trigger::Drift
        } else {
            return None;
        };
        self.mode.prev().map(|to| (to, trigger))
    }

    fn promotion_blockers(
        &self,
        to: Mode,
        evidence: &Evidence,
        gates: &Gates,
        now: DateTime<Utc>,
    ) -> Vec<Blocker> {
        let mut blockers = Vec::new();
        let weeks = self.weeks_in_mode(now);
        match to {
            Mode::Research => {}
            Mode::Paper => {
                if !evidence.research.beats_benchmark {
                    blockers.push(Blocker::NoBenchmarkEdge);
                }
                if !evidence
                    .research
                    .deflated_sharpe
                    .is_some_and(|s| s >= gates.min_deflated_sharpe)
                {
                    blockers.push(Blocker::DeflatedSharpe(evidence.research.deflated_sharpe));
                }
                if !evidence.research.pbo.is_some_and(|p| p <= gates.max_pbo) {
                    blockers.push(Blocker::Pbo(evidence.research.pbo));
                }
            }
            Mode::LiveMicro => {
                if weeks < gates.paper_weeks {
                    blockers.push(Blocker::TooSoon {
                        weeks,
                        needed: gates.paper_weeks,
                    });
                }
                if !evidence.paper.tracking_ok {
                    blockers.push(Blocker::Tracking);
                }
                if !evidence.paper.stress_passed {
                    blockers.push(Blocker::StressTests);
                }
            }
            Mode::Live => {
                if weeks < gates.live_micro_weeks {
                    blockers.push(Blocker::TooSoon {
                        weeks,
                        needed: gates.live_micro_weeks,
                    });
                }
                if !evidence
                    .live
                    .slippage_ratio
                    .is_some_and(|r| r <= gates.max_slippage_ratio)
                {
                    blockers.push(Blocker::Slippage(evidence.live.slippage_ratio));
                }
            }
        }
        blockers
    }

    const fn set(&mut self, mode: Mode, now: DateTime<Utc>) {
        self.mode = mode;
        self.since = now;
    }
}

#[cfg(test)]
mod tests;
