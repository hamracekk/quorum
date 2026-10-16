//! What slots and the operator send the risk process, and what it answers: one JSON object per
//! line, one request per connection.

use chrono::{DateTime, Utc};
use quorum_core::{InstrumentId, Side};
use quorum_envelope::Mode;
use serde::{Deserialize, Serialize};

use crate::state::{Halt, SlotRisk};

/// An order a slot wants to send, with the slot's book as it stands before the order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderCheck {
    pub slot: String,
    /// The slot's rung on the autonomy ladder.
    pub mode: Mode,
    pub instrument: InstrumentId,
    pub side: Side,
    /// Ordered quantity, always positive.
    pub qty: f64,
    /// The price the order is expected to fill near.
    pub price: f64,
    /// Signed quantity held in `instrument` before the order.
    pub position: f64,
    /// Sum of |position value| over the slot's book before the order, at the latest marks.
    pub gross: f64,
    pub equity: f64,
    /// When the slot decided the order (its bar time).
    pub ts: DateTime<Utc>,
}

impl OrderCheck {
    /// Signed quantity held after the order fills in full.
    #[must_use]
    pub fn position_after(&self) -> f64 {
        match self.side {
            Side::Buy => self.position + self.qty,
            Side::Sell => self.position - self.qty,
        }
    }

    /// Whether the order only shrinks the position: never grows it, never flips its side.
    #[must_use]
    pub fn reduces_risk(&self) -> bool {
        let after = self.position_after();
        let eps = 1e-12 * self.position.abs().max(1.0);
        after.abs() <= self.position.abs() + eps && after * self.position >= 0.0
    }
}

/// The risk process's answer to an [`OrderCheck`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub approved: bool,
    pub reason: String,
}

impl Verdict {
    #[must_use]
    pub fn approve(reason: impl Into<String>) -> Self {
        Self {
            approved: true,
            reason: reason.into(),
        }
    }

    #[must_use]
    pub fn reject(reason: impl Into<String>) -> Self {
        Self {
            approved: false,
            reason: reason.into(),
        }
    }
}

/// A slot's heartbeat: it is alive, and this is its equity now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {
    pub slot: String,
    pub mode: Mode,
    pub equity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Request {
    Check(OrderCheck),
    Ping(Ping),
    /// Kill switch: no new risk for `slot`, or for every slot when `None`. Anyone may pull it.
    Halt {
        slot: Option<String>,
        reason: String,
    },
    /// Lifts the halts of `slot`, or the kill switch and every slot's halts when `None`. The
    /// operator's call only.
    Resume {
        slot: Option<String>,
    },
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Response {
    Verdict(Verdict),
    /// The halt that applies to the pinging slot, if any.
    Pong {
        halt: Option<Halt>,
    },
    Done,
    Status(StatusReport),
    Error {
        message: String,
    },
}

/// Everything `quorum risk status` shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReport {
    /// The kill switch for every slot, if pulled.
    pub kill: Option<Halt>,
    /// Whether the envelope file reads and validates now.
    pub envelope_ok: bool,
    pub slots: Vec<(String, SlotRisk)>,
}
