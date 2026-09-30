//! Ports: the traits every external boundary is reached through (ADR 0001).
//!
//! Adapters (Binance, Parquet, …) live in other crates and implement these traits; the
//! engine, lab and CLI only ever see the traits. Ports are added in the milestone that
//! first needs them: `MarketStream` and `Broker` (M3), `LlmProvider` and `NewsSource`
//! (M5), `TraderSource` (M7), `Notifier` (integrations), `Journal` and `EventLog` (M2).

use std::error::Error as StdError;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::instrument::{InstrumentId, VenueId};
use crate::types::{Bar, Series};

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

/// Durable storage of bar series, one series per instrument and interval.
pub trait BarStore: Send + Sync + fmt::Debug {
    /// `None` when nothing has been stored for this instrument and interval yet.
    fn load(&self, instrument: &InstrumentId, interval: &str) -> Result<Option<Series>, PortError>;

    /// Replace the stored series. Bars must be sorted by time without duplicates.
    fn save(&self, series: &Series, interval: &str) -> Result<(), PortError>;
}
