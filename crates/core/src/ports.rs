//! Ports: the traits every external boundary is reached through (ADR 0001).
//!
//! Adapters (Binance, Parquet, …) live in other crates and implement these traits; the
//! engine, lab and CLI only ever see the traits. Ports are added in the milestone that
//! first needs them: `TraderSource` (M7). The order-execution port, [`Broker`](crate::broker::Broker),
//! is synchronous and lives with the engine in [`crate::broker`].

use std::error::Error as StdError;
use std::fmt;

use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_core::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::agents::{
    AgentControl, AgentMessage, AgentReport, AgentRun, Finding, FindingQuery, MessageQuery,
    NewFinding, NewMessage, NewReport, Verdict,
};
use crate::chat::{ChatReply, ChatRequest};
use crate::events::{Envelope, Event};
use crate::instrument::{InstrumentId, VenueId};
use crate::news::{Extraction, NewsItem, NewsQuery, NewsSourceStats, StoredNews, TokenUsage};
use crate::order::Order;
use crate::predictions::{PredictionMarket, ProbabilityPoint};
use crate::traders::{LeaderboardEntry, TraderAccount, TraderFill};
use crate::types::{Bar, Fill, Series};

/// Error crossing a port boundary. Adapters keep their own detailed errors and wrap them.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("{0} is not supported by this adapter")]
    Unsupported(String),
    #[error("invalid data: {0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Adapter {
        context: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl PortError {
    pub fn adapter(
        context: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Adapter {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

/// Historical market data from one venue.
#[async_trait]
pub trait MarketData: Send + Sync + fmt::Debug {
    fn venue(&self) -> &VenueId;

    /// Cheap reachability check (e.g. an exchange ping) for status displays.
    /// Adapters without a remote dependency keep the default: always reachable.
    async fn check(&self) -> Result<(), PortError> {
        Ok(())
    }

    /// Closed bars with `start <= ts < end`, sorted by time without duplicates.
    /// Bars still forming at call time are never returned.
    async fn bars(
        &self,
        instrument: &InstrumentId,
        interval: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Bar>, PortError>;
}

/// A closed bar from a live stream.
pub type StreamedBar = Result<(InstrumentId, Bar), PortError>;

/// Live market data from one venue: closed bars as they complete.
#[async_trait]
pub trait MarketStream: Send + Sync + fmt::Debug {
    fn venue(&self) -> &VenueId;

    /// Closed bars of `instruments` in arrival order. Implementations reconnect on their own
    /// (bars missed while disconnected are the consumer's to backfill); the stream ends when
    /// dropped or on a fatal error. Bars still forming are never yielded.
    async fn subscribe(
        &self,
        instruments: &[InstrumentId],
        interval: &str,
    ) -> Result<BoxStream<'static, StreamedBar>, PortError>;
}

/// Durable storage of bar series, one series per instrument and interval.
pub trait BarStore: Send + Sync + fmt::Debug {
    /// `None` when nothing has been stored for this instrument and interval yet.
    fn load(&self, instrument: &InstrumentId, interval: &str) -> Result<Option<Series>, PortError>;

    /// Replace the stored series. Bars must be sorted by time without duplicates.
    fn save(&self, series: &Series, interval: &str) -> Result<(), PortError>;
}

/// Append-only, replayable record of every event (ADR 0002).
pub trait EventLog: Send + Sync + fmt::Debug {
    /// Append `event`, assigning the next sequence number (strictly increasing, never reused).
    fn append(&self, event: &Event) -> Result<Envelope, PortError>;

    /// Events with `start <= ts < end`, in sequence order.
    fn read(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Envelope>, PortError>;
}

/// A news feed (RSS/Atom, …). Content is untrusted: adapters return plain, bounded text and
/// http(s) links only, and never follow anything inside an item.
#[async_trait]
pub trait NewsSource: Send + Sync + fmt::Debug {
    /// Configured source name, recorded on every item.
    fn name(&self) -> &str;

    /// The feed's current items, sanitized, without instrument tags. `fetched_at` is now.
    async fn fetch(&self) -> Result<Vec<NewsItem>, PortError>;
}

/// A strategy decision as journaled: a changed target and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub ts: DateTime<Utc>,
    pub instrument: InstrumentId,
    pub strategy: String,
    pub target: f64,
    pub reason: Option<serde_json::Value>,
}

/// A prediction-market exchange, read only (SRC-2): market state and probability history.
/// Questions are untrusted text; nothing here can place an order.
#[async_trait]
pub trait PredictionSource: Send + Sync + fmt::Debug {
    /// Exchange id, e.g. `kalshi`.
    fn venue(&self) -> &str;

    async fn market(&self, ticker: &str) -> Result<PredictionMarket, PortError>;

    /// Hourly implied probabilities with `start <= ts <= end`, oldest first.
    async fn history(
        &self,
        ticker: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProbabilityPoint>, PortError>;
}

/// A venue with public accounts, read only (SRC-3): positions and fills of an address.
/// Nothing here can place an order or sign anything.
#[async_trait]
pub trait TraderSource: Send + Sync + fmt::Debug {
    /// Venue id, e.g. `hyperliquid`.
    fn venue(&self) -> &str;

    async fn account(&self, address: &str) -> Result<TraderAccount, PortError>;

    /// Fills with `start <= ts <= end`, oldest first (venues may only keep the latest few
    /// thousand).
    async fn fills(
        &self,
        address: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<TraderFill>, PortError>;

    /// The venue's public leaderboard, if it has one.
    async fn leaderboard(&self) -> Result<Vec<LeaderboardEntry>, PortError> {
        Err(PortError::Invalid(format!(
            "{} has no leaderboard",
            self.venue()
        )))
    }
}

/// A structured-output request to a language model (M5b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    /// Fixed instructions (kept byte-identical across calls so providers can cache them).
    pub system: String,
    /// The untrusted material, already wrapped and labelled as data by the caller.
    pub user: String,
    /// JSON Schema the answer must follow.
    pub schema: serde_json::Value,
    pub max_tokens: u32,
}

/// A model's answer: the raw JSON text (validated by the caller) and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A language model (Claude API, Ollama). Output is data to validate, never instructions:
/// callers parse it against their schema and discard anything that does not conform.
#[async_trait]
pub trait LlmProvider: Send + Sync + fmt::Debug {
    /// `claude`, `ollama`, …
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    /// One structured-output completion. A refusal or truncated answer is an error.
    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, PortError>;
}

/// A model that can hold a tool-using conversation (AG-3b): one step per call.
#[async_trait]
pub trait ChatModel: Send + Sync + fmt::Debug {
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    /// The next assistant turn. A refusal is an error.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatReply, PortError>;
}

/// Durable news items (M5a), kept with the journal.
#[async_trait]
pub trait NewsStore: Send + Sync + fmt::Debug {
    /// Store `items`, skipping duplicates (same canonical URL, or the same headline within
    /// `news::DUPLICATE_WINDOW_HOURS`).
    async fn store_news(&self, items: &[NewsItem]) -> Result<StoredNews, PortError>;

    /// Items matching `query`, newest first.
    async fn news(&self, query: &NewsQuery) -> Result<Vec<NewsItem>, PortError>;

    /// Item count and newest item per source.
    async fn news_stats(&self) -> Result<Vec<NewsSourceStats>, PortError>;

    /// Newest items `tier` has not read yet under `prompt_version` (a failed attempt counts as
    /// not read, so it is retried; a rejected answer counts as read), newest first.
    async fn unextracted(
        &self,
        prompt_version: &str,
        tier: &str,
        limit: usize,
    ) -> Result<Vec<NewsItem>, PortError>;

    /// Record one extraction attempt (replacing an earlier one of the same item, prompt and tier).
    async fn record_extraction(&self, extraction: &Extraction) -> Result<(), PortError>;

    /// Every attempt for these items under `prompt_version`.
    async fn extractions(
        &self,
        urls: &[String],
        prompt_version: &str,
    ) -> Result<Vec<Extraction>, PortError>;

    /// Tokens spent per tier since `since`.
    async fn token_usage(&self, since: DateTime<Utc>) -> Result<TokenUsage, PortError>;

    /// Valid readings (`ok`, relevant) recorded since `since`, every prompt version and tier,
    /// oldest first: what the scoreboard scores.
    async fn readings(&self, since: DateTime<Utc>) -> Result<Vec<Extraction>, PortError>;
}

/// Agents' shared knowledge and messages (ADR 0003), kept with the journal.
///
/// Callers validate shapes first (`NewFinding::validate`, `NewMessage::validate`); the store
/// enforces references: a superseded finding and a reply's thread must exist.
#[async_trait]
pub trait AgentStore: Send + Sync + fmt::Debug {
    /// Store a finding by `agent` (in `role`) with status `hypothesis`.
    async fn post_finding(
        &self,
        agent: &str,
        role: &str,
        finding: &NewFinding,
        ts: DateTime<Utc>,
    ) -> Result<Finding, PortError>;

    /// Findings matching `query`, newest first.
    async fn findings(&self, query: &FindingQuery) -> Result<Vec<Finding>, PortError>;

    async fn finding(&self, id: i64) -> Result<Option<Finding>, PortError>;

    /// Store `verdict` as the finding's latest (and in its history) and move the finding to
    /// `verdict.outcome`. A retired finding only changes again by the operator's decision.
    async fn record_verdict(&self, id: i64, verdict: &Verdict) -> Result<Finding, PortError>;

    async fn send_message(
        &self,
        sender: &str,
        message: &NewMessage,
        ts: DateTime<Utc>,
    ) -> Result<AgentMessage, PortError>;

    /// Messages matching `query`: the newest `limit`, returned oldest first.
    async fn messages(&self, query: &MessageQuery) -> Result<Vec<AgentMessage>, PortError>;

    /// Messages to `reader` (or to everyone) it has not read yet, oldest first, and mark
    /// them read. Its own messages are never unread.
    async fn take_unread(&self, reader: &str, limit: usize)
    -> Result<Vec<AgentMessage>, PortError>;

    /// How many messages `reader` has not read yet.
    async fn unread_count(&self, reader: &str) -> Result<usize, PortError>;

    /// Set the runtime switch of one agent or of `*` (all agents).
    async fn set_control(&self, control: &AgentControl) -> Result<(), PortError>;

    async fn controls(&self) -> Result<Vec<AgentControl>, PortError>;

    /// Start recording a run of an internal agent (status `running`).
    async fn start_agent_run(
        &self,
        agent: &str,
        task: &str,
        trigger: &str,
        ts: DateTime<Utc>,
    ) -> Result<AgentRun, PortError>;

    /// Close a run with its outcome.
    async fn finish_agent_run(&self, run: &AgentRun) -> Result<(), PortError>;

    /// The latest runs, newest first, optionally of one agent.
    async fn agent_runs(
        &self,
        agent: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentRun>, PortError>;

    /// Tokens (input + output) an agent's runs used since `since` (for its daily budget).
    async fn agent_tokens(&self, agent: &str, since: DateTime<Utc>) -> Result<u64, PortError>;

    /// Store a report by `agent`.
    async fn post_report(
        &self,
        agent: &str,
        report: &NewReport,
        ts: DateTime<Utc>,
    ) -> Result<AgentReport, PortError>;

    /// The latest reports, newest first, optionally of one agent.
    async fn reports(
        &self,
        agent: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentReport>, PortError>;

    /// Store a position suggestion by `author` (status `pending`).
    async fn post_suggestion(
        &self,
        author: &str,
        suggestion: &crate::suggestions::NewSuggestion,
        price: Option<f64>,
        ts: DateTime<Utc>,
    ) -> Result<crate::suggestions::Suggestion, PortError>;

    async fn suggestion(
        &self,
        id: i64,
    ) -> Result<Option<crate::suggestions::Suggestion>, PortError>;

    /// The latest suggestions, newest first, optionally with one status.
    async fn suggestions(
        &self,
        status: Option<crate::suggestions::Status>,
        limit: usize,
    ) -> Result<Vec<crate::suggestions::Suggestion>, PortError>;

    /// Suggestions a paper session applied, in order (for replay).
    async fn session_suggestions(
        &self,
        session: &str,
    ) -> Result<Vec<crate::suggestions::Suggestion>, PortError>;

    /// Save a suggestion's status, decision and execution fields.
    async fn update_suggestion(
        &self,
        suggestion: &crate::suggestions::Suggestion,
    ) -> Result<(), PortError>;

    /// Store a risk-reducing position control by `author` (status `pending`).
    async fn post_control(
        &self,
        author: &str,
        instrument: &InstrumentId,
        action: crate::suggestions::ControlAction,
        reason: &str,
        ts: DateTime<Utc>,
    ) -> Result<crate::suggestions::PositionControl, PortError>;

    /// The latest position controls, newest first, optionally with one status.
    async fn position_controls(
        &self,
        status: Option<crate::suggestions::ControlStatus>,
        limit: usize,
    ) -> Result<Vec<crate::suggestions::PositionControl>, PortError>;

    /// Controls a paper session applied, in order (for replay).
    async fn session_controls(
        &self,
        session: &str,
    ) -> Result<Vec<crate::suggestions::PositionControl>, PortError>;

    async fn update_control(
        &self,
        control: &crate::suggestions::PositionControl,
    ) -> Result<(), PortError>;
}

/// How urgent a notification is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Routine: daily summaries, session started.
    Info,
    /// Needs a look soon: risk halt, rejected order, live stream ended.
    Warning,
    /// Needs a look now: kill switch, the engine stopped on an error.
    Critical,
}

/// A human-readable message for the operator. Plain text only; never contains secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub ts: DateTime<Utc>,
    pub severity: Severity,
    /// One line, e.g. `paper · daily summary 2026-09-30`.
    pub title: String,
    pub body: String,
}

/// Outbound messages to the operator: log, desktop notification, later webhooks/n8n (INT).
///
/// Delivery is best effort. Callers treat an error as "not delivered" and carry on; a
/// notification failure must never stop trading.
#[async_trait]
pub trait Notifier: Send + Sync + fmt::Debug {
    /// Short adapter name for status displays, e.g. `log` or `desktop`.
    fn name(&self) -> &str;

    async fn notify(&self, notification: &Notification) -> Result<(), PortError>;
}

/// What produced a journaled run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Backtest,
    Replay,
    Paper,
    Live,
}

impl RunKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backtest => "backtest",
            Self::Replay => "replay",
            Self::Paper => "paper",
            Self::Live => "live",
        }
    }
}

impl FromStr for RunKind {
    type Err = PortError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "backtest" => Ok(Self::Backtest),
            "replay" => Ok(Self::Replay),
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            other => Err(PortError::Invalid(format!("unknown run kind `{other}`"))),
        }
    }
}

/// One journaled run: a backtest, a replay, or a paper/live session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunInfo {
    pub id: String,
    pub kind: RunKind,
    /// Human-readable summary, e.g. strategy, parameters and instruments.
    pub label: String,
    pub started_at: DateTime<Utc>,
}

/// One tool call made by an AI agent through the MCP server, as audited in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentCall {
    pub ts: DateTime<Utc>,
    /// Client name reported by the agent at MCP `initialize` (e.g. `claude-code`).
    pub agent: String,
    pub tool: String,
    /// The call's arguments as JSON text.
    pub arguments: String,
    pub ok: bool,
    /// One-line result summary, or the error message.
    pub summary: String,
    pub duration_ms: u64,
}

/// Queryable record of decisions and executions, grouped by run (ADR 0002).
#[async_trait]
pub trait Journal: Send + Sync + fmt::Debug {
    async fn start_run(&self, run: &RunInfo) -> Result<(), PortError>;

    /// Record the decision events of `run_id` atomically (all or nothing). Market bars are
    /// not journaled — they live in the bar store and event log. Returns rows written.
    async fn record(&self, run_id: &str, events: &[Event]) -> Result<usize, PortError>;

    /// All runs, newest first.
    async fn runs(&self) -> Result<Vec<RunInfo>, PortError>;

    /// Fills of `run_id` in execution order.
    async fn fills(&self, run_id: &str) -> Result<Vec<Fill>, PortError>;

    /// Orders of `run_id` in their latest state, by id.
    async fn orders(&self, run_id: &str) -> Result<Vec<Order>, PortError>;

    /// The latest `limit` strategy decisions (signals) of `run_id`, newest first, with the
    /// reasons strategies gave (M8).
    async fn decisions(&self, run_id: &str, limit: usize) -> Result<Vec<Decision>, PortError>;

    /// Audit one agent tool call.
    async fn record_agent_call(&self, call: &AgentCall) -> Result<(), PortError>;

    /// The most recent agent tool calls, newest first.
    async fn agent_calls(&self, limit: usize) -> Result<Vec<AgentCall>, PortError>;
}
