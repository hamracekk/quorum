//! Ports: the traits every external boundary is reached through (ADR 0001).
//!
//! Adapters (Binance, Parquet, …) live in other crates and implement these traits; the
//! engine, lab and CLI only ever see the traits. Ports are added in the milestone that
//! first needs them: `LlmProvider` and `NewsSource` (M5), `TraderSource` (M7),
//! `Notifier` (integrations). The order-execution port, [`Broker`](crate::broker::Broker),
//! is synchronous and lives with the engine in [`crate::broker`].

use std::error::Error as StdError;
use std::fmt;

use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_core::stream::BoxStream;
use serde::Serialize;

use crate::events::{Envelope, Event};
use crate::instrument::{InstrumentId, VenueId};
use crate::order::Order;
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

    /// Audit one agent tool call.
    async fn record_agent_call(&self, call: &AgentCall) -> Result<(), PortError>;

    /// The most recent agent tool calls, newest first.
    async fn agent_calls(&self, limit: usize) -> Result<Vec<AgentCall>, PortError>;
}
