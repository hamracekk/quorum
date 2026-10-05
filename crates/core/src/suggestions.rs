//! Position suggestions (T-3, ADR 0006): an agent proposes, the operator decides.
//!
//! A suggestion is a proposal, never an order: an instrument, a side (go long, go short, or
//! close), a size as a fraction of equity, a stop and optional take-profit (both as fractions of
//! the price at entry), a Markdown rationale with evidence, a confidence and an expiry. Only the
//! operator accepts or rejects it; an accepted suggestion is applied by the running paper
//! session as a manual trade on that instrument, with its stops. The record of what happened
//! after each suggestion becomes the author's track record.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;

/// Longest rationale (Markdown).
pub const MAX_RATIONALE: usize = 4_000;
const MIN_RATIONALE: usize = 20;
/// Most evidence references.
pub const MAX_EVIDENCE: usize = 10;
/// Stop distance bounds (fractions of the price).
pub const MIN_STOP: f64 = 0.002;
pub const MAX_STOP: f64 = 0.5;
/// `decided_by` of a decision the operator made.
pub const OPERATOR_DECISION: &str = "operator";
/// `decided_by` of a decision the operator's auto-accept policy made (T-6).
pub const POLICY_DECISION: &str = "policy";
/// Longest a suggestion stays open for a decision.
pub const MAX_EXPIRY_HOURS: i64 = 72;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Go (or stay) long.
    Long,
    /// Go short (only where shorting is allowed).
    Short,
    /// Close the position.
    Close,
}

impl Side {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
            Self::Close => "close",
        }
    }

    /// Sign of the target: `1`, `-1`, or `0` for a close.
    pub const fn sign(self) -> f64 {
        match self {
            Self::Long => 1.0,
            Self::Short => -1.0,
            Self::Close => 0.0,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Side {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "long" => Ok(Self::Long),
            "short" => Ok(Self::Short),
            "close" => Ok(Self::Close),
            other => Err(format!("side `{other}`: long, short or close")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting for the operator.
    Pending,
    /// Accepted; waits for the paper session to apply it.
    Accepted,
    Rejected,
    /// Not decided (or not applied) in time.
    Expired,
    /// Applied by the paper session.
    Executed,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Executed => "executed",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            "expired" => Ok(Self::Expired),
            "executed" => Ok(Self::Executed),
            other => Err(format!("unknown suggestion status `{other}`")),
        }
    }
}

/// A suggestion as an agent submits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSuggestion {
    pub instrument: InstrumentId,
    pub side: Side,
    /// Fraction of equity (ignored for a close).
    pub size: f64,
    /// Stop distance as a fraction of the entry price (required to open).
    pub stop_pct: Option<f64>,
    /// Take-profit distance as a fraction of the entry price.
    pub take_profit_pct: Option<f64>,
    /// Why, in Markdown.
    pub rationale: String,
    /// References: `finding:12`, `run:<id>`, a source id, a URL.
    pub evidence: Vec<String>,
    /// The author's probability that it works out, 0–1.
    pub confidence: Option<f64>,
    pub expires_in: Duration,
}

impl NewSuggestion {
    /// Shape checks that need no configuration (`max_size`: the largest exposure allowed).
    pub fn validate(&self, max_size: f64) -> Result<(), String> {
        let text = self.rationale.trim().chars().count();
        if !(MIN_RATIONALE..=MAX_RATIONALE).contains(&text) {
            return Err(format!(
                "rationale: {MIN_RATIONALE}–{MAX_RATIONALE} characters"
            ));
        }
        if self.evidence.len() > MAX_EVIDENCE || self.evidence.iter().any(|e| e.len() > 300) {
            return Err(format!(
                "evidence: at most {MAX_EVIDENCE} references of 300 characters"
            ));
        }
        if self.confidence.is_some_and(|c| !(0.0..=1.0).contains(&c)) {
            return Err("confidence must be between 0 and 1".into());
        }
        let hours = self.expires_in.num_hours();
        if !(1..=MAX_EXPIRY_HOURS).contains(&hours) {
            return Err(format!("expiry: 1–{MAX_EXPIRY_HOURS} hours"));
        }
        if self.side == Side::Close {
            return Ok(());
        }
        if !(self.size > 0.0 && self.size <= max_size) {
            return Err(format!(
                "size must be above 0 and at most {max_size} of equity"
            ));
        }
        match self.stop_pct {
            Some(stop) if (MIN_STOP..=MAX_STOP).contains(&stop) => {}
            _ => {
                return Err(format!(
                    "a stop is required to open: stop_pct between {MIN_STOP} and {MAX_STOP}"
                ));
            }
        }
        if self
            .take_profit_pct
            .is_some_and(|tp| !(MIN_STOP..=5.0).contains(&tp))
        {
            return Err("take_profit_pct must be between 0.002 and 5".into());
        }
        Ok(())
    }
}

/// A stored suggestion and what became of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: i64,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub instrument: InstrumentId,
    pub side: Side,
    pub size: f64,
    pub stop_pct: Option<f64>,
    pub take_profit_pct: Option<f64>,
    /// Latest price when it was suggested.
    pub price: Option<f64>,
    pub rationale: String,
    pub evidence: Vec<String>,
    pub confidence: Option<f64>,
    pub status: Status,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_reason: Option<String>,
    /// Who decided: [`OPERATOR_DECISION`] or [`POLICY_DECISION`] (the auto-accept policy).
    pub decided_by: Option<String>,
    /// The paper session that applied it, and the bar time it took effect after.
    pub session_id: Option<String>,
    pub applied_after: Option<DateTime<Utc>>,
    pub executed_at: Option<DateTime<Utc>>,
}

impl Suggestion {
    /// The paper target it asks for (fraction of equity).
    pub fn target(&self) -> f64 {
        self.side.sign() * self.size
    }

    /// Label in decisions and positions, e.g. `suggestion #3 by scout`.
    pub fn label(&self) -> String {
        format!("suggestion #{} by {}", self.id, self.author)
    }
}

/// A risk-reducing action on one open paper position (T-5), by a watcher agent.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum ControlAction {
    /// Move the stop to this price (closer to the market only).
    TightenStop { stop: f64 },
    /// Close the position.
    Close,
}

impl ControlAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TightenStop { .. } => "tighten_stop",
            Self::Close => "close",
        }
    }
}

/// What became of a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlStatus {
    /// Waiting for the paper session.
    Pending,
    Applied,
    /// The session could not apply it (no position, not tighter, …): see the note.
    Refused,
}

impl ControlStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Refused => "refused",
        }
    }
}

impl FromStr for ControlStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "applied" => Ok(Self::Applied),
            "refused" => Ok(Self::Refused),
            other => Err(format!("unknown control status `{other}`")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionControl {
    pub id: i64,
    pub author: String,
    pub created_at: DateTime<Utc>,
    pub instrument: InstrumentId,
    #[serde(flatten)]
    pub action: ControlAction,
    pub reason: String,
    pub status: ControlStatus,
    pub note: Option<String>,
    pub session_id: Option<String>,
    pub applied_after: Option<DateTime<Utc>>,
    pub applied_at: Option<DateTime<Utc>>,
}

impl PositionControl {
    pub fn label(&self) -> String {
        format!("control #{} by {}", self.id, self.author)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long() -> NewSuggestion {
        NewSuggestion {
            instrument: InstrumentId::spot("binance", "BTCUSDT").expect("id"),
            side: Side::Long,
            size: 0.2,
            stop_pct: Some(0.05),
            take_profit_pct: Some(0.15),
            rationale: "Two followed traders opened longs; news is bullish.".into(),
            evidence: vec!["finding:3".into()],
            confidence: Some(0.55),
            expires_in: Duration::hours(24),
        }
    }

    #[test]
    fn a_sound_suggestion_passes() {
        assert_eq!(long().validate(1.0), Ok(()));
    }

    #[test]
    fn opening_needs_a_stop_and_a_size_within_limits() {
        let no_stop = NewSuggestion {
            stop_pct: None,
            ..long()
        };
        assert!(
            no_stop
                .validate(1.0)
                .is_err_and(|e| e.contains("stop is required"))
        );
        let too_big = NewSuggestion {
            size: 0.8,
            ..long()
        };
        assert!(too_big.validate(0.5).is_err());
        let close = NewSuggestion {
            side: Side::Close,
            size: 0.0,
            stop_pct: None,
            ..long()
        };
        assert_eq!(
            close.validate(0.5),
            Ok(()),
            "a close needs neither size nor stop"
        );
    }

    #[test]
    fn text_confidence_and_expiry_are_bounded() {
        let short_text = NewSuggestion {
            rationale: "buy".into(),
            ..long()
        };
        assert!(short_text.validate(1.0).is_err());
        let sure = NewSuggestion {
            confidence: Some(1.5),
            ..long()
        };
        assert!(sure.validate(1.0).is_err());
        let forever = NewSuggestion {
            expires_in: Duration::days(30),
            ..long()
        };
        assert!(forever.validate(1.0).is_err());
    }

    #[test]
    fn sides_and_statuses_round_trip() {
        for side in [Side::Long, Side::Short, Side::Close] {
            assert_eq!(side.as_str().parse::<Side>(), Ok(side));
        }
        assert_eq!("executed".parse::<Status>(), Ok(Status::Executed));
        assert!("filled".parse::<Status>().is_err());
    }
}
