//! Events: everything that happens, as data (ADR 0002).
//!
//! Market input (`Bar`) and engine output (`Signal`, `Target`, `Order`, `Fill`, `RiskHalt`) share one
//! type so they can flow over the bus, be appended to the event log and be replayed. The
//! serialized form is internally tagged: `{"type":"fill", ...}`. New variants will be added
//! by later milestones (orders, news, trader activity), hence `#[non_exhaustive]`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;
use crate::news::NewsEvent;
use crate::order::Order;
use crate::types::{Bar, Fill};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// A closed market bar (input). `bar.ts` is the bar's open time.
    Bar { instrument: InstrumentId, bar: Bar },
    /// A strategy changed its desired exposure; `ts` is the bar that produced it.
    Signal {
        ts: DateTime<Utc>,
        instrument: InstrumentId,
        strategy: String,
        target: f64,
    },
    /// A risk-adjusted target queued for execution at the instrument's next bar open.
    Target {
        ts: DateTime<Utc>,
        instrument: InstrumentId,
        target: f64,
    },
    /// An order changed state (accepted, filled, canceled, rejected); carries the whole order.
    Order(Order),
    /// An executed (simulated or real) trade.
    Fill(Fill),
    /// The daily-loss kill switch tripped; all targets go to zero for the rest of the day.
    RiskHalt { ts: DateTime<Utc>, equity: f64 },
    /// A typed event extracted from a news item (M5b). Recorded so backtests replay the
    /// reading instead of calling the model again.
    NewsEvent(NewsEvent),
}

impl Event {
    /// Event time: bar open time for bars, decision time otherwise.
    pub const fn ts(&self) -> DateTime<Utc> {
        match self {
            Self::Bar { bar, .. } => bar.ts,
            Self::Order(order) => order.updated_at,
            Self::Fill(fill) => fill.ts,
            Self::Signal { ts, .. } | Self::Target { ts, .. } | Self::RiskHalt { ts, .. } => *ts,
            Self::NewsEvent(event) => event.extracted_at,
        }
    }

    /// Short type name, as used in the serialized `type` tag.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Bar { .. } => "bar",
            Self::Signal { .. } => "signal",
            Self::Target { .. } => "target",
            Self::Order(_) => "order",
            Self::Fill(_) => "fill",
            Self::RiskHalt { .. } => "risk_halt",
            Self::NewsEvent(_) => "news_event",
        }
    }
}

/// An event with its position in the event log. `seq` is strictly increasing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub seq: u64,
    #[serde(flatten)]
    pub event: Event,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{OrderId, OrderStatus};
    use crate::types::Side;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn id() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
    }

    fn samples() -> Vec<Event> {
        vec![
            Event::Bar {
                instrument: id(),
                bar: Bar {
                    ts: at(1),
                    open: 1.0,
                    high: 2.0,
                    low: 0.5,
                    close: 1.5,
                    volume: 3.0,
                },
            },
            Event::Signal {
                ts: at(2),
                instrument: id(),
                strategy: "sma-cross".into(),
                target: 1.0,
            },
            Event::Target {
                ts: at(3),
                instrument: id(),
                target: 0.5,
            },
            Event::Fill(Fill {
                ts: at(4),
                instrument: id(),
                side: Side::Buy,
                qty: 2.0,
                price: 100.0,
                fee: 0.2,
            }),
            Event::RiskHalt {
                ts: at(5),
                equity: 900.0,
            },
            Event::Order(Order {
                reason: Some("kill switch".into()),
                status: OrderStatus::Canceled,
                updated_at: at(6),
                ..Order::new(OrderId(7), at(4), id(), Side::Sell, 1.5)
            }),
        ]
    }

    #[test]
    fn every_event_round_trips_through_json_with_its_tag() {
        for (i, event) in samples().into_iter().enumerate() {
            let envelope = Envelope {
                seq: i as u64 + 1,
                event,
            };
            let json = serde_json::to_string(&envelope).expect("serialize");
            assert!(
                json.contains(&format!("\"type\":\"{}\"", envelope.event.kind())),
                "{json}"
            );
            let back: Envelope = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, envelope);
        }
    }

    #[test]
    fn ts_reports_the_event_time() {
        let hours: Vec<_> = samples().iter().map(Event::ts).collect();
        assert_eq!(hours, vec![at(1), at(2), at(3), at(4), at(5), at(6)]);
    }

    #[test]
    fn instruments_serialize_as_text() {
        let json = serde_json::to_string(&samples()[1]).expect("serialize");
        assert!(
            json.contains("\"instrument\":\"binance:BTCUSDT\""),
            "{json}"
        );
    }

    #[test]
    fn prices_survive_json_bit_for_bit() {
        // Regression: without serde_json's `float_roundtrip` feature ~15% of these prices come
        // back one bit off, and replayed decisions drift from recorded ones.
        let mut price = 100.0_f64;
        for i in 0..10_000 {
            price *= if i % 5 == 4 { 0.95 } else { 1.01 };
            if price > 1e12 {
                price = 100.0;
            }
            let json = serde_json::to_string(&price).expect("serialize");
            let back: f64 = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back.to_bits(), price.to_bits(), "{price} became {back}");
        }
    }
}
