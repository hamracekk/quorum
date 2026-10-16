//! Paper-trades one slot end to end on the shared engine (DESIGN §5.2, P0).
//!
//! A run: check the slot against the envelope and its ladder state, bring stored history up to
//! date, **rebuild by replay** (a fresh engine fed every stored bar; only bars past the
//! session's journal watermark have their decisions journaled), then trade live closed bars,
//! backfilling gaps and catching up on what the stream missed. Fills are simulated: the runner
//! has no order path to a venue.
//!
//! Files per slot live in `<data>/slots/<id>/` (see [`SlotFiles`]); bars, journal and event log
//! go through the ports in [`Ports`], so tests run on fakes.

mod build;
mod paper;
mod session;
pub mod watchdog;

use std::sync::Arc;

use quorum_core::{BarStore, EventLog, Journal, MarketData, MarketStream, Notifier};

pub use build::{backtest_config, interval_duration, strategy};
pub use paper::{Outcome, RunOptions, Status, run};
pub use session::{Session, SlotFiles, StateFile, request_stop};

/// The adapters a run talks to. The composition root (the CLI) picks concrete ones.
#[derive(Debug, Clone)]
pub struct Ports {
    pub market: Arc<dyn MarketData>,
    pub stream: Arc<dyn MarketStream>,
    pub bars: Arc<dyn BarStore>,
    pub journal: Arc<dyn Journal>,
    pub log: Arc<dyn EventLog>,
    /// Alerts and daily summaries to the operator (best effort: never fails a run).
    pub notifier: Arc<dyn Notifier>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The slot may not paper-trade now (envelope, ladder, venue, changed config).
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Slot(#[from] quorum_slots::SlotConfigError),
    #[error(transparent)]
    Port(#[from] quorum_core::PortError),
    #[error(transparent)]
    Engine(#[from] quorum_core::EngineError),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: String,
        source: serde_json::Error,
    },
}

pub type Result<T> = std::result::Result<T, RunnerError>;
