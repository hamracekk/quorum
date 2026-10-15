//! The forward recorder (DESIGN §6, P1): records what has little or no public history —
//! liquidations, order-book depth, derivatives at 5-minute resolution — so it becomes history.
//!
//! Records go to append-only daily JSONL files per feed (see [`RecordStore`]), each with
//! `known_time` and `event_time`. Markers record starts, stops, stream drops and failed polls,
//! so research can tell "nothing happened" from "we were not listening".
//!
//! The recorder is its own process: a failure here never touches trading, and the reverse.

mod config;
mod run;
mod store;

pub use config::{RecorderConfig, RecorderConfigError};
pub use run::{Feeds, Outcome, Status, record};
pub use store::RecordStore;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Config(#[from] RecorderConfigError),
    #[error(transparent)]
    Port(#[from] quorum_core::PortError),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("record: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, RecorderError>;
