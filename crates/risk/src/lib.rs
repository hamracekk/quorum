//! The risk process (DESIGN §9): a separate OS process and the only component that approves
//! orders.
//!
//! Slots ask it about every order before the order reaches a venue ([`RiskGate`]); it answers
//! from the operator's envelope (re-read on every request, so a lowered envelope applies at
//! once), its own loss tracking per slot, and the kill switch. Risk-reducing orders are always
//! approved: anything may reduce risk. A slot that cannot reach it sends no order that adds
//! risk.
//!
//! - [`protocol`]: the requests and answers, one JSON line each over `<data>/risk/risk.sock`.
//! - [`rules`]: the decisions as pure functions.
//! - [`state`]: halts and loss references, kept in `<data>/risk/state.json` across restarts.
//! - [`serve`]: the process; [`SocketGate`]: the client.

pub mod client;
pub mod fakes;
pub mod protocol;
pub mod rules;
mod server;
pub mod state;

pub use client::{RiskGate, SocketGate};
pub use protocol::{OrderCheck, Ping, Request, Response, StatusReport, Verdict};
pub use server::{RiskPaths, serve};
pub use state::{Halt, RiskState, SlotRisk};

#[derive(Debug, thiserror::Error)]
pub enum RiskError {
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
    #[error("another risk process is running ({0} is locked)")]
    Locked(String),
    #[error("risk process unreachable: {0}")]
    Unreachable(String),
    #[error("risk protocol: {0}")]
    Protocol(String),
}
