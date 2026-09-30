//! Event plumbing (ADR 0002): the append-only event log, the in-process broadcast bus, and
//! replay of recorded market data through the trading engine.
//!
//! - [`JsonlEventLog`] implements the `EventLog` port: one JSON Lines file per UTC day under
//!   `data/events/`, sequence numbers strictly increasing across files and restarts.
//! - [`Bus`] persists every event to the log first, then broadcasts it to subscribers.
//! - [`replay`] re-derives decisions from recorded bars with the same `Engine` backtests use.

mod bus;
mod error;
mod log;
mod replay;

pub use bus::Bus;
pub use error::BusError;
pub use log::JsonlEventLog;
pub use replay::{ReplayError, replay};
