//! Orders and their state machine.
//!
//! An [`Order`] is created by the engine in [`OrderStatus::New`], handed to a
//! [`Broker`](crate::broker::Broker), and then moved only by [`ExecutionReport`]s coming back:
//!
//! ```text
//! New ──accepted──▶ Accepted ──fill──▶ PartiallyFilled ──fill──▶ Filled
//!  │                 │  │                   │
//!  │                 │  └────fill (all)─────┼──────────────────▶ Filled
//!  ├──rejected──▶ Rejected                  │
//!  └─────────────────┴──canceled──▶ Canceled ◀──────────────────┘
//! ```
//!
//! `Filled`, `Canceled` and `Rejected` are terminal. Any other transition, a fill for more
//! than the remaining quantity, or a report for an unknown order is an error: with the paper
//! broker it would be a bug, and with a real exchange it means the local view has drifted and
//! trading must stop rather than guess.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;
use crate::types::{Fill, Side};

/// Relative tolerance when comparing filled against ordered quantity.
const QTY_EPSILON: f64 = 1e-9;

/// Engine-assigned order id: sequential per engine, so replays reproduce the same ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderId(pub u64);

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "o-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Created locally, not yet acknowledged by the broker.
    New,
    /// Acknowledged by the broker and working.
    Accepted,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
}

impl OrderStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Rejected)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Accepted => "accepted",
            Self::PartiallyFilled => "partially_filled",
            Self::Filled => "filled",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
        }
    }
}

impl FromStr for OrderStatus {
    type Err = OrderError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Self::New,
            Self::Accepted,
            Self::PartiallyFilled,
            Self::Filled,
            Self::Canceled,
            Self::Rejected,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
        .ok_or_else(|| OrderError::UnknownStatus(text.to_owned()))
    }
}

impl fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A market order and its current state. Serialized as-is into `order` events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub instrument: InstrumentId,
    pub side: Side,
    /// Ordered quantity, always positive.
    pub qty: f64,
    pub status: OrderStatus,
    pub filled_qty: f64,
    /// Volume-weighted fill price; 0 until the first fill.
    pub avg_price: f64,
    pub fees: f64,
    /// Why the order was rejected or canceled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Time of the latest transition.
    pub updated_at: DateTime<Utc>,
}

/// What a broker reports back about an order it was given.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionReport {
    Accepted {
        id: OrderId,
        ts: DateTime<Utc>,
    },
    /// A (partial or complete) execution; `fill.qty` is this execution only.
    Filled {
        id: OrderId,
        fill: Fill,
    },
    Canceled {
        id: OrderId,
        ts: DateTime<Utc>,
        reason: String,
    },
    Rejected {
        id: OrderId,
        ts: DateTime<Utc>,
        reason: String,
    },
}

impl ExecutionReport {
    pub const fn id(&self) -> OrderId {
        match self {
            Self::Accepted { id, .. }
            | Self::Filled { id, .. }
            | Self::Canceled { id, .. }
            | Self::Rejected { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OrderError {
    #[error("report for unknown order {0}")]
    Unknown(OrderId),
    #[error("unknown order status `{0}`")]
    UnknownStatus(String),
    #[error("order {id}: cannot go from {from} to {to}")]
    Transition {
        id: OrderId,
        from: OrderStatus,
        to: OrderStatus,
    },
    #[error("order {id}: fill of {fill} exceeds the remaining {remaining}")]
    Overfill {
        id: OrderId,
        fill: f64,
        remaining: f64,
    },
    #[error("order {id}: fill does not match the order ({detail})")]
    Mismatch { id: OrderId, detail: String },
}

impl Order {
    pub const fn new(
        id: OrderId,
        ts: DateTime<Utc>,
        instrument: InstrumentId,
        side: Side,
        qty: f64,
    ) -> Self {
        Self {
            id,
            instrument,
            side,
            qty,
            status: OrderStatus::New,
            filled_qty: 0.0,
            avg_price: 0.0,
            fees: 0.0,
            reason: None,
            created_at: ts,
            updated_at: ts,
        }
    }

    pub fn remaining(&self) -> f64 {
        (self.qty - self.filled_qty).max(0.0)
    }

    /// Apply one report, enforcing the state machine. On error the order is unchanged.
    pub fn apply(&mut self, report: &ExecutionReport) -> Result<(), OrderError> {
        match report {
            ExecutionReport::Accepted { ts, .. } => {
                self.transition(OrderStatus::Accepted, &[OrderStatus::New], *ts)
            }
            ExecutionReport::Filled { fill, .. } => self.fill(fill),
            ExecutionReport::Canceled { ts, reason, .. } => {
                self.transition(
                    OrderStatus::Canceled,
                    &[
                        OrderStatus::New,
                        OrderStatus::Accepted,
                        OrderStatus::PartiallyFilled,
                    ],
                    *ts,
                )?;
                self.reason = Some(reason.clone());
                Ok(())
            }
            ExecutionReport::Rejected { ts, reason, .. } => {
                self.transition(OrderStatus::Rejected, &[OrderStatus::New], *ts)?;
                self.reason = Some(reason.clone());
                Ok(())
            }
        }
    }

    fn transition(
        &mut self,
        to: OrderStatus,
        allowed_from: &[OrderStatus],
        ts: DateTime<Utc>,
    ) -> Result<(), OrderError> {
        if !allowed_from.contains(&self.status) {
            return Err(OrderError::Transition {
                id: self.id,
                from: self.status,
                to,
            });
        }
        self.status = to;
        self.updated_at = ts;
        Ok(())
    }

    fn fill(&mut self, fill: &Fill) -> Result<(), OrderError> {
        let id = self.id;
        if !matches!(
            self.status,
            OrderStatus::Accepted | OrderStatus::PartiallyFilled
        ) {
            let to = if fill.qty >= self.remaining() {
                OrderStatus::Filled
            } else {
                OrderStatus::PartiallyFilled
            };
            return Err(OrderError::Transition {
                id,
                from: self.status,
                to,
            });
        }
        if fill.instrument != self.instrument || fill.side != self.side {
            return Err(OrderError::Mismatch {
                id,
                detail: format!(
                    "{} {:?} vs {} {:?}",
                    fill.instrument, fill.side, self.instrument, self.side
                ),
            });
        }
        if !(fill.qty > 0.0 && fill.qty.is_finite() && fill.price.is_finite()) {
            return Err(OrderError::Mismatch {
                id,
                detail: format!("qty {} at {}", fill.qty, fill.price),
            });
        }
        let remaining = self.remaining();
        let tolerance = QTY_EPSILON * self.qty;
        if fill.qty > remaining + tolerance {
            return Err(OrderError::Overfill {
                id,
                fill: fill.qty,
                remaining,
            });
        }

        let filled = self.filled_qty + fill.qty;
        self.avg_price = self
            .avg_price
            .mul_add(self.filled_qty, fill.price * fill.qty)
            / filled;
        self.filled_qty = filled;
        self.fees += fill.fee;
        self.status = if self.qty - filled <= tolerance {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        self.updated_at = fill.ts;
        Ok(())
    }
}

/// Every order an engine has created, by id. Terminal orders are kept for the session so
/// late or duplicate reports are caught instead of silently ignored.
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    next: u64,
    orders: BTreeMap<OrderId, Order>,
}

impl OrderBook {
    /// Create a new market order in `New` and return a copy for the broker.
    pub fn create(
        &mut self,
        ts: DateTime<Utc>,
        instrument: &InstrumentId,
        side: Side,
        qty: f64,
    ) -> Order {
        self.next += 1;
        let order = Order::new(OrderId(self.next), ts, instrument.clone(), side, qty);
        self.orders.insert(order.id, order.clone());
        order
    }

    /// Apply a broker report; returns the order's new state.
    pub fn apply(&mut self, report: &ExecutionReport) -> Result<&Order, OrderError> {
        let id = report.id();
        let order = self.orders.get_mut(&id).ok_or(OrderError::Unknown(id))?;
        order.apply(report)?;
        Ok(order)
    }

    pub fn get(&self, id: OrderId) -> Option<&Order> {
        self.orders.get(&id)
    }

    /// Orders that are not yet in a terminal state, oldest first.
    pub fn open(&self) -> impl Iterator<Item = &Order> {
        self.orders.values().filter(|o| !o.status.is_terminal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn id() -> InstrumentId {
        InstrumentId::spot("test", "AAA").expect("valid instrument")
    }

    fn fill(qty: f64, price: f64) -> Fill {
        Fill {
            ts: at(1),
            instrument: id(),
            side: Side::Buy,
            qty,
            price,
            fee: 0.1,
        }
    }

    fn accepted() -> (OrderBook, OrderId) {
        let mut book = OrderBook::default();
        let order = book.create(at(0), &id(), Side::Buy, 2.0);
        book.apply(&ExecutionReport::Accepted {
            id: order.id,
            ts: at(0),
        })
        .expect("accept");
        (book, order.id)
    }

    #[test]
    fn ids_are_sequential_and_displayed_with_a_prefix() {
        let mut book = OrderBook::default();
        let a = book.create(at(0), &id(), Side::Buy, 1.0);
        let b = book.create(at(0), &id(), Side::Sell, 1.0);
        assert_eq!((a.id, b.id), (OrderId(1), OrderId(2)));
        assert_eq!(a.id.to_string(), "o-1");
        assert_eq!(a.status, OrderStatus::New);
    }

    #[test]
    fn partial_fills_accumulate_to_filled_with_a_weighted_price() {
        let (mut book, oid) = accepted();
        let order = book
            .apply(&ExecutionReport::Filled {
                id: oid,
                fill: fill(0.5, 100.0),
            })
            .expect("partial");
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        assert!((order.remaining() - 1.5).abs() < 1e-12);

        let order = book
            .apply(&ExecutionReport::Filled {
                id: oid,
                fill: fill(1.5, 104.0),
            })
            .expect("rest");
        assert_eq!(order.status, OrderStatus::Filled);
        assert!((order.avg_price - 103.0).abs() < 1e-12);
        assert!((order.fees - 0.2).abs() < 1e-12);
        assert_eq!(book.open().count(), 0);
    }

    #[test]
    fn overfills_are_refused_and_leave_the_order_unchanged() {
        let (mut book, oid) = accepted();
        let err = book
            .apply(&ExecutionReport::Filled {
                id: oid,
                fill: fill(2.5, 100.0),
            })
            .expect_err("overfill");
        assert!(matches!(err, OrderError::Overfill { .. }));
        let order = book.get(oid).expect("order");
        assert_eq!(order.status, OrderStatus::Accepted);
        assert!(order.filled_qty.abs() < f64::EPSILON);
    }

    #[test]
    fn fills_before_acceptance_are_refused() {
        let mut book = OrderBook::default();
        let order = book.create(at(0), &id(), Side::Buy, 2.0);
        let err = book
            .apply(&ExecutionReport::Filled {
                id: order.id,
                fill: fill(2.0, 100.0),
            })
            .expect_err("not accepted");
        assert_eq!(
            err,
            OrderError::Transition {
                id: order.id,
                from: OrderStatus::New,
                to: OrderStatus::Filled
            }
        );
    }

    #[test]
    fn terminal_states_accept_nothing_more() {
        let (mut book, oid) = accepted();
        book.apply(&ExecutionReport::Filled {
            id: oid,
            fill: fill(2.0, 100.0),
        })
        .expect("fill");
        for report in [
            ExecutionReport::Accepted { id: oid, ts: at(2) },
            ExecutionReport::Filled {
                id: oid,
                fill: fill(0.1, 100.0),
            },
            ExecutionReport::Canceled {
                id: oid,
                ts: at(2),
                reason: "late".into(),
            },
            ExecutionReport::Rejected {
                id: oid,
                ts: at(2),
                reason: "late".into(),
            },
        ] {
            assert!(
                matches!(book.apply(&report), Err(OrderError::Transition { .. })),
                "{report:?}"
            );
        }
    }

    #[test]
    fn rejection_is_only_possible_before_acceptance() {
        let mut book = OrderBook::default();
        let order = book.create(at(0), &id(), Side::Buy, 1.0);
        let rejected = book
            .apply(&ExecutionReport::Rejected {
                id: order.id,
                ts: at(0),
                reason: "insufficient balance".into(),
            })
            .expect("reject");
        assert_eq!(rejected.status, OrderStatus::Rejected);
        assert_eq!(rejected.reason.as_deref(), Some("insufficient balance"));

        let (mut book, oid) = accepted();
        assert!(
            book.apply(&ExecutionReport::Rejected {
                id: oid,
                ts: at(1),
                reason: "x".into()
            })
            .is_err()
        );
    }

    #[test]
    fn a_partially_filled_order_can_be_canceled() {
        let (mut book, oid) = accepted();
        book.apply(&ExecutionReport::Filled {
            id: oid,
            fill: fill(0.5, 100.0),
        })
        .expect("partial");
        let order = book
            .apply(&ExecutionReport::Canceled {
                id: oid,
                ts: at(2),
                reason: "kill switch".into(),
            })
            .expect("cancel");
        assert_eq!(order.status, OrderStatus::Canceled);
        assert!((order.filled_qty - 0.5).abs() < 1e-12);
        assert_eq!(order.updated_at, at(2));
    }

    #[test]
    fn fills_for_another_instrument_or_side_are_refused() {
        let (mut book, oid) = accepted();
        let mut wrong_side = fill(1.0, 100.0);
        wrong_side.side = Side::Sell;
        assert!(matches!(
            book.apply(&ExecutionReport::Filled {
                id: oid,
                fill: wrong_side
            }),
            Err(OrderError::Mismatch { .. })
        ));
        let mut zero = fill(0.0, 100.0);
        zero.qty = 0.0;
        assert!(matches!(
            book.apply(&ExecutionReport::Filled {
                id: oid,
                fill: zero
            }),
            Err(OrderError::Mismatch { .. })
        ));
    }

    #[test]
    fn reports_for_unknown_orders_are_errors() {
        let mut book = OrderBook::default();
        assert_eq!(
            book.apply(&ExecutionReport::Accepted {
                id: OrderId(42),
                ts: at(0)
            })
            .expect_err("unknown"),
            OrderError::Unknown(OrderId(42))
        );
    }

    #[test]
    fn statuses_parse_from_their_text_form() {
        for status in [
            OrderStatus::New,
            OrderStatus::Accepted,
            OrderStatus::PartiallyFilled,
            OrderStatus::Filled,
            OrderStatus::Canceled,
            OrderStatus::Rejected,
        ] {
            assert_eq!(status.as_str().parse::<OrderStatus>(), Ok(status));
        }
        assert!("done".parse::<OrderStatus>().is_err());
    }

    #[test]
    fn orders_round_trip_through_json() {
        let (book, oid) = accepted();
        let order = book.get(oid).expect("order").clone();
        let json = serde_json::to_string(&order).expect("serialize");
        assert!(json.contains("\"status\":\"accepted\""), "{json}");
        assert!(!json.contains("reason"), "{json}");
        let back: Order = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, order);
    }
}
