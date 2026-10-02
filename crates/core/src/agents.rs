//! Agents' shared knowledge and communication (ADR 0003): findings, messages, controls.
//!
//! Agents are analysts, never traders. What they produce is data with an author:
//!
//! - a [`Finding`] states a claim with evidence; testable kinds carry a typed [`Hypothesis`]
//!   the lab can evaluate (AG-2). Only evaluation may confirm or reject a finding; agents
//!   create findings and supersede them with new ones, they never edit or delete them.
//! - an [`AgentMessage`] is a line in a thread between agents and the operator. Messages are
//!   untrusted data for whoever reads them: they never widen an agent's tools or scope.
//! - an [`AgentControl`] enables or disables one agent, or all of them (`*`), at runtime.
//!
//! Everything here is validated before it is stored; storage goes through [`AgentStore`]
//! (`crate::ports`).
//!
//! [`AgentStore`]: crate::ports::AgentStore

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;
use crate::signals::Signal;

/// The human running trader; a message sender and recipient, never an agent id.
pub const OPERATOR: &str = "operator";
/// Broadcast recipient: every agent (and the operator) can read it.
pub const EVERYONE: &str = "all";
/// Identity of MCP clients that did not declare a registered agent.
pub const GUEST: &str = "guest";
/// Control target meaning "every agent".
pub const ALL_AGENTS: &str = "*";

const MAX_ID: usize = 32;
const MIN_CLAIM: usize = 10;
const MAX_CLAIM: usize = 2_000;
const MAX_BODY: usize = 4_000;
const MAX_LIST: usize = 20;
const MAX_TEXT: usize = 500;
const MAX_HORIZON: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct AgentError(pub String);

fn err<T>(message: impl Into<String>) -> Result<T, AgentError> {
    Err(AgentError(message.into()))
}

/// Agent ids are lower-case slugs (`researcher`, `risk-reviewer`) and never a reserved name.
pub fn validate_agent_id(id: &str) -> Result<(), AgentError> {
    let mut chars = id.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !(starts_with_letter && rest_ok && id.len() <= MAX_ID) {
        return err(format!(
            "agent id `{id}` must be a lower-case slug (a-z, 0-9, -) of at most {MAX_ID} characters starting with a letter"
        ));
    }
    if [OPERATOR, EVERYONE, GUEST].contains(&id) {
        return err(format!("agent id `{id}` is reserved"));
    }
    Ok(())
}

fn check_len(name: &str, text: &str, min: usize, max: usize) -> Result<(), AgentError> {
    let len = text.trim().chars().count();
    if (min..=max).contains(&len) {
        Ok(())
    } else {
        err(format!("{name} must be {min}–{max} characters, got {len}"))
    }
}

/// What a finding is about. Testable kinds require the matching [`Hypothesis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FindingKind {
    /// A signal predicts forward returns after costs.
    SignalEdge,
    /// A strategy with these parameters beats its benchmark out-of-sample.
    StrategyParams,
    /// A note: context, a caveat, a review comment. Never confirmable.
    Observation,
}

impl FindingKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignalEdge => "signal_edge",
            Self::StrategyParams => "strategy_params",
            Self::Observation => "observation",
        }
    }
}

impl FromStr for FindingKind {
    type Err = AgentError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [Self::SignalEdge, Self::StrategyParams, Self::Observation]
            .into_iter()
            .find(|k| k.as_str() == text)
            .ok_or_else(|| {
                AgentError(format!(
                    "unknown finding kind `{text}` (signal_edge, strategy_params, observation)"
                ))
            })
    }
}

/// Where a finding stands. Agents only ever create `hypothesis`; evaluation (AG-2) and the
/// operator move it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Hypothesis,
    Testing,
    Confirmed,
    Rejected,
    Retired,
}

impl FindingStatus {
    pub const ALL: [Self; 5] = [
        Self::Hypothesis,
        Self::Testing,
        Self::Confirmed,
        Self::Rejected,
        Self::Retired,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hypothesis => "hypothesis",
            Self::Testing => "testing",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Retired => "retired",
        }
    }
}

impl fmt::Display for FindingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FindingStatus {
    type Err = AgentError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
            .ok_or_else(|| AgentError(format!("unknown finding status `{text}`")))
    }
}

/// A claim the lab can test deterministically (AG-2), instead of prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Hypothesis {
    /// `signal` on `instruments` predicts the bias-signed return `horizon` bars ahead.
    SignalEdge {
        signal: Signal,
        instruments: Vec<InstrumentId>,
        interval: String,
        horizon: usize,
    },
    /// `strategy` with `params` beats buy & hold out-of-sample on `instruments`.
    StrategyParams {
        strategy: String,
        params: BTreeMap<String, f64>,
        instruments: Vec<InstrumentId>,
        interval: String,
    },
}

impl Hypothesis {
    pub const fn kind(&self) -> FindingKind {
        match self {
            Self::SignalEdge { .. } => FindingKind::SignalEdge,
            Self::StrategyParams { .. } => FindingKind::StrategyParams,
        }
    }

    pub fn instruments(&self) -> &[InstrumentId] {
        match self {
            Self::SignalEdge { instruments, .. } | Self::StrategyParams { instruments, .. } => {
                instruments
            }
        }
    }

    fn validate(&self) -> Result<(), AgentError> {
        let (instruments, interval) = match self {
            Self::SignalEdge {
                instruments,
                interval,
                horizon,
                ..
            } => {
                if !(1..=MAX_HORIZON).contains(horizon) {
                    return err(format!("horizon must be 1–{MAX_HORIZON} bars"));
                }
                (instruments, interval)
            }
            Self::StrategyParams {
                strategy,
                params,
                instruments,
                interval,
            } => {
                check_len("strategy", strategy, 1, 64)?;
                if params.len() > MAX_LIST || params.values().any(|v| !v.is_finite()) {
                    return err(format!("params: at most {MAX_LIST} finite numbers"));
                }
                (instruments, interval)
            }
        };
        if instruments.is_empty() || instruments.len() > MAX_LIST {
            return err(format!("a hypothesis names 1–{MAX_LIST} instruments"));
        }
        check_len("interval", interval, 1, 8)
    }
}

/// What backs a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Evidence {
    /// A journaled run (`list_runs`).
    Run { id: String },
    /// Another finding.
    Finding { id: i64 },
    /// An external source; untrusted, kept for reference.
    Url { url: String },
    /// A short free-text pointer, e.g. a signal-study result.
    Note { text: String },
}

impl Evidence {
    fn validate(&self) -> Result<(), AgentError> {
        match self {
            Self::Run { id } => check_len("evidence run id", id, 1, 64),
            Self::Finding { id } => {
                if *id > 0 {
                    Ok(())
                } else {
                    err("evidence finding id must be positive")
                }
            }
            Self::Url { url } => {
                check_len("evidence url", url, 1, MAX_TEXT)?;
                if url.starts_with("https://") || url.starts_with("http://") {
                    Ok(())
                } else {
                    err("evidence url must start with http:// or https://")
                }
            }
            Self::Note { text } => check_len("evidence note", text, 1, MAX_TEXT),
        }
    }
}

/// A finding as an agent submits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewFinding {
    pub kind: FindingKind,
    /// Instruments the claim is about (for observations: optional context).
    #[serde(default)]
    pub instruments: Vec<InstrumentId>,
    pub claim: String,
    /// Required for testable kinds, absent for observations.
    #[serde(default)]
    pub hypothesis: Option<Hypothesis>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// Earlier finding this one replaces (the earlier one is kept, marked superseded).
    #[serde(default)]
    pub supersedes: Option<i64>,
    /// How likely the author thinks evaluation will confirm the claim (0–1). Scored against
    /// outcomes as a Brier score in the author's track record.
    #[serde(default)]
    pub confidence: Option<f64>,
}

impl NewFinding {
    /// Shape checks that need no storage: lengths, kind/hypothesis agreement, limits.
    pub fn validate(&self) -> Result<(), AgentError> {
        check_len("claim", &self.claim, MIN_CLAIM, MAX_CLAIM)?;
        match (&self.kind, &self.hypothesis) {
            (FindingKind::Observation, Some(_)) => {
                return err("an observation carries no hypothesis");
            }
            (FindingKind::Observation, None) => {}
            (kind, None) => {
                return err(format!(
                    "a {} finding needs a matching hypothesis",
                    kind.as_str()
                ));
            }
            (kind, Some(h)) if h.kind() != *kind => {
                return err(format!(
                    "the hypothesis is a {} but the finding is a {}",
                    h.kind().as_str(),
                    kind.as_str()
                ));
            }
            (_, Some(h)) => h.validate()?,
        }
        if self.instruments.len() > MAX_LIST {
            return err(format!("at most {MAX_LIST} instruments"));
        }
        if self.evidence.len() > MAX_LIST {
            return err(format!("at most {MAX_LIST} pieces of evidence"));
        }
        for evidence in &self.evidence {
            evidence.validate()?;
        }
        if self.supersedes.is_some_and(|id| id <= 0) {
            return err("supersedes must be a finding id");
        }
        if self
            .confidence
            .is_some_and(|c| !(c.is_finite() && (0.0..=1.0).contains(&c)))
        {
            return err("confidence must be between 0 and 1");
        }
        Ok(())
    }

    /// Every instrument mentioned, in the finding and its hypothesis.
    pub fn all_instruments(&self) -> impl Iterator<Item = &InstrumentId> {
        self.instruments
            .iter()
            .chain(self.hypothesis.iter().flat_map(|h| h.instruments().iter()))
    }
}

/// A stored finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: i64,
    pub agent: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub kind: FindingKind,
    pub instruments: Vec<InstrumentId>,
    pub claim: String,
    pub hypothesis: Option<Hypothesis>,
    pub evidence: Vec<Evidence>,
    pub status: FindingStatus,
    pub supersedes: Option<i64>,
    /// Newer finding that replaced this one, if any.
    pub superseded_by: Option<i64>,
    pub confidence: Option<f64>,
    /// The latest evaluation or operator decision, if any.
    pub verdict: Option<Verdict>,
}

/// The outcome of evaluating a finding (or an operator's decision about it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub evaluated_at: DateTime<Utc>,
    /// Who triggered it: an agent id or `operator`. The result never depends on who asked.
    pub by: String,
    /// `signal_study_holdout`, `strategy_holdout`, or `operator`.
    pub method: String,
    /// The status the finding moved to: confirmed, rejected, testing (inconclusive), or any
    /// status an operator chose.
    pub outcome: FindingStatus,
    /// One readable line: the numbers behind the outcome.
    pub summary: String,
    /// Named numbers the evaluation used (counts, returns, t-statistics, thresholds).
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    /// Data the evaluation looked at.
    #[serde(default)]
    pub window: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// How an agent's findings fared under evaluation.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TrackRecord {
    pub findings: usize,
    pub confirmed: usize,
    pub rejected: usize,
    /// Evaluated but inconclusive (`testing`).
    pub inconclusive: usize,
    /// Not evaluated yet (`hypothesis`).
    pub pending: usize,
    pub retired: usize,
    /// confirmed / (confirmed + rejected); `None` before any decisive evaluation.
    pub hit_rate: Option<f64>,
    /// Mean squared error of stated confidence against decisive outcomes (0 is perfect,
    /// 0.25 is a coin flip); `None` when no decided finding carried a confidence.
    pub brier: Option<f64>,
    /// Decided findings with a confidence, i.e. how many the Brier score averages.
    pub brier_n: usize,
}

impl TrackRecord {
    pub fn of<'a>(findings: impl IntoIterator<Item = &'a Finding>) -> Self {
        let mut record = Self::default();
        let mut squared = 0.0;
        for finding in findings {
            record.findings += 1;
            let decided = match finding.status {
                FindingStatus::Confirmed => {
                    record.confirmed += 1;
                    Some(1.0)
                }
                FindingStatus::Rejected => {
                    record.rejected += 1;
                    Some(0.0)
                }
                FindingStatus::Testing => {
                    record.inconclusive += 1;
                    None
                }
                FindingStatus::Hypothesis => {
                    record.pending += 1;
                    None
                }
                FindingStatus::Retired => {
                    record.retired += 1;
                    None
                }
            };
            if let (Some(outcome), Some(confidence)) = (decided, finding.confidence) {
                let error = confidence - outcome;
                squared = error.mul_add(error, squared);
                record.brier_n += 1;
            }
        }
        let decided = record.confirmed + record.rejected;
        if decided > 0 {
            record.hit_rate = Some(record.confirmed as f64 / decided as f64);
        }
        if record.brier_n > 0 {
            record.brier = Some(squared / record.brier_n as f64);
        }
        record
    }
}

/// Which findings to list. Newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindingQuery {
    pub agent: Option<String>,
    pub status: Option<FindingStatus>,
    /// Full-text search over claims (plain words; operators are not interpreted).
    pub text: Option<String>,
    pub limit: usize,
}

/// A message as a sender submits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMessage {
    /// An agent id, `operator`, or `all`.
    pub recipient: String,
    pub body: String,
    /// Thread to reply in (any message id of it); `None` starts a new thread.
    #[serde(default)]
    pub thread: Option<i64>,
    /// Ids this message is about, e.g. `finding:12`, `run:<id>`, `order:o-3`.
    #[serde(default)]
    pub refs: Vec<String>,
}

impl NewMessage {
    pub fn validate(&self, sender: &str) -> Result<(), AgentError> {
        check_len("body", &self.body, 1, MAX_BODY)?;
        if self.recipient != OPERATOR && self.recipient != EVERYONE {
            validate_agent_id(&self.recipient)?;
        }
        if self.recipient == sender {
            return err("a message needs a recipient other than its sender");
        }
        if self.refs.len() > MAX_LIST {
            return err(format!("at most {MAX_LIST} refs"));
        }
        for reference in &self.refs {
            check_len("ref", reference, 1, 128)?;
        }
        if self.thread.is_some_and(|t| t <= 0) {
            return err("thread must be a message id");
        }
        Ok(())
    }
}

/// A stored message. `thread` is the id of the thread's first message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub thread: i64,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub refs: Vec<String>,
}

/// Which messages to list. Oldest first within the window of the newest `limit`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageQuery {
    /// Any message id of the thread.
    pub thread: Option<i64>,
    /// Messages sent by or addressed to this participant (or to everyone).
    pub participant: Option<String>,
    pub limit: usize,
}

/// Runtime switch for one agent, or every agent (`*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentControl {
    pub agent: String,
    pub enabled: bool,
    pub reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// One run of an internal agent (AG-3b): what it was asked, what it cost, how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: i64,
    pub agent: String,
    pub task: String,
    /// `manual` (operator), `schedule`, or the id of the agent that asked (orchestrator).
    pub trigger: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// `running`, `done`, `stopped` (budget or step limit), `failed`.
    pub status: String,
    pub steps: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The agent's closing text.
    pub summary: Option<String>,
    pub error: Option<String>,
}

/// A report to the operator (AG-3c): the orchestrator's weekly post-mortem.
///
/// `facts` are computed by trader for the period (paper session, tournament, scoreboard, agents), so the numbers in a
/// report never depend on what the agent wrote; `body` is the agent's reading of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    pub id: i64,
    pub agent: String,
    pub created_at: DateTime<Utc>,
    pub period_from: DateTime<Utc>,
    pub period_to: DateTime<Utc>,
    pub title: String,
    /// Markdown by the agent.
    pub body: String,
    pub facts: serde_json::Value,
}

/// A report before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReport {
    pub period_from: DateTime<Utc>,
    pub period_to: DateTime<Utc>,
    pub title: String,
    pub body: String,
    pub facts: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btc() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
    }

    fn edge() -> Hypothesis {
        Hypothesis::SignalEdge {
            signal: Signal::RsiOversold,
            instruments: vec![btc()],
            interval: "1h".into(),
            horizon: 24,
        }
    }

    fn finding(kind: FindingKind, hypothesis: Option<Hypothesis>) -> NewFinding {
        NewFinding {
            kind,
            instruments: vec![],
            claim: "RSI oversold bounces on BTC within a day".into(),
            hypothesis,
            evidence: vec![Evidence::Note {
                text: "signal_study 2y".into(),
            }],
            supersedes: None,
            confidence: Some(0.6),
        }
    }

    #[test]
    fn agent_ids_are_slugs_and_not_reserved() {
        for good in ["researcher", "risk-reviewer", "a1"] {
            assert!(validate_agent_id(good).is_ok(), "{good}");
        }
        let long = "x".repeat(33);
        for bad in [
            "",
            "Researcher",
            "1st",
            "risk_reviewer",
            "operator",
            "all",
            "guest",
            long.as_str(),
        ] {
            assert!(validate_agent_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn testable_kinds_need_a_matching_hypothesis() {
        assert!(
            finding(FindingKind::SignalEdge, Some(edge()))
                .validate()
                .is_ok()
        );
        assert!(finding(FindingKind::SignalEdge, None).validate().is_err());
        assert!(
            finding(FindingKind::StrategyParams, Some(edge()))
                .validate()
                .is_err()
        );
        assert!(
            finding(FindingKind::Observation, Some(edge()))
                .validate()
                .is_err()
        );
        assert!(finding(FindingKind::Observation, None).validate().is_ok());
    }

    #[test]
    fn claims_hypotheses_and_evidence_are_bounded() {
        let mut short = finding(FindingKind::Observation, None);
        short.claim = "too short".into();
        assert!(short.validate().is_err());

        let mut far = edge();
        if let Hypothesis::SignalEdge { horizon, .. } = &mut far {
            *horizon = 501;
        }
        assert!(
            finding(FindingKind::SignalEdge, Some(far))
                .validate()
                .is_err()
        );

        let mut bad_url = finding(FindingKind::Observation, None);
        bad_url.evidence = vec![Evidence::Url {
            url: "file:///etc/passwd".into(),
        }];
        assert!(bad_url.validate().is_err());

        let nan = Hypothesis::StrategyParams {
            strategy: "sma-cross".into(),
            params: BTreeMap::from([("fast".into(), f64::NAN)]),
            instruments: vec![btc()],
            interval: "1h".into(),
        };
        assert!(
            finding(FindingKind::StrategyParams, Some(nan))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn hypotheses_round_trip_through_json_with_typed_signals() {
        let json = serde_json::to_string(&edge()).expect("serialize");
        assert!(json.contains("\"type\":\"signal_edge\""), "{json}");
        assert!(json.contains("\"signal\":\"rsi_oversold\""), "{json}");
        let back: Hypothesis = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, edge());
        let unknown = json.replace("rsi_oversold", "moon_phase");
        assert!(serde_json::from_str::<Hypothesis>(&unknown).is_err());
    }

    #[test]
    fn all_instruments_covers_the_hypothesis() {
        let f = finding(FindingKind::SignalEdge, Some(edge()));
        assert_eq!(f.all_instruments().collect::<Vec<_>>(), vec![&btc()]);
    }

    #[test]
    fn messages_need_a_valid_other_recipient() {
        let message = |recipient: &str, body: &str| NewMessage {
            recipient: recipient.into(),
            body: body.into(),
            thread: None,
            refs: vec!["finding:1".into()],
        };
        assert!(
            message("operator", "please review")
                .validate("researcher")
                .is_ok()
        );
        assert!(message("all", "heads up").validate("researcher").is_ok());
        assert!(
            message("risk-reviewer", "see finding 1")
                .validate("researcher")
                .is_ok()
        );
        assert!(
            message("researcher", "self")
                .validate("researcher")
                .is_err()
        );
        assert!(message("Bad Id", "x").validate("researcher").is_err());
        assert!(message("operator", "").validate("researcher").is_err());
        assert!(
            message("operator", &"x".repeat(4_001))
                .validate("researcher")
                .is_err()
        );
    }

    #[test]
    fn confidence_must_be_a_probability() {
        for bad in [-0.1, 1.5, f64::NAN] {
            let mut f = finding(FindingKind::Observation, None);
            f.confidence = Some(bad);
            assert!(f.validate().is_err(), "{bad}");
        }
    }

    fn stored(status: FindingStatus, confidence: Option<f64>) -> Finding {
        Finding {
            id: 1,
            agent: "researcher".into(),
            role: "r".into(),
            created_at: DateTime::from_timestamp(0, 0).expect("ts"),
            updated_at: DateTime::from_timestamp(0, 0).expect("ts"),
            kind: FindingKind::Observation,
            instruments: vec![],
            claim: "claim text".into(),
            hypothesis: None,
            evidence: vec![],
            status,
            supersedes: None,
            superseded_by: None,
            confidence,
            verdict: None,
        }
    }

    #[test]
    fn track_records_count_outcomes_and_score_confidence() {
        let findings = [
            stored(FindingStatus::Confirmed, Some(0.8)),
            stored(FindingStatus::Rejected, Some(0.6)),
            stored(FindingStatus::Rejected, None),
            stored(FindingStatus::Testing, Some(0.9)),
            stored(FindingStatus::Hypothesis, None),
            stored(FindingStatus::Retired, None),
        ];
        let record = TrackRecord::of(&findings);
        assert_eq!(
            (record.findings, record.confirmed, record.rejected),
            (6, 1, 2)
        );
        assert_eq!(
            (record.inconclusive, record.pending, record.retired),
            (1, 1, 1)
        );
        assert!((record.hit_rate.expect("decided") - 1.0 / 3.0).abs() < 1e-12);
        // (0.8 − 1)² = 0.04 and (0.6 − 0)² = 0.36 → mean 0.2; undecided ones don't count.
        assert_eq!(record.brier_n, 2);
        assert!((record.brier.expect("scored") - 0.2).abs() < 1e-12);
        assert_eq!(TrackRecord::of(&[]).hit_rate, None);
    }

    #[test]
    fn statuses_and_kinds_parse_from_text() {
        for status in FindingStatus::ALL {
            assert_eq!(status.as_str().parse::<FindingStatus>(), Ok(status));
        }
        assert_eq!(
            "observation".parse::<FindingKind>(),
            Ok(FindingKind::Observation)
        );
        assert!("rumour".parse::<FindingKind>().is_err());
    }
}
