//! Forward-recorded market structure (DESIGN §6).
//!
//! Liquidations, order-book depth and derivatives snapshots at minute resolution have little or
//! no public history; recording them from now on makes them history.
//!
//! Every record carries `known_time` (when quorum received it) and `event_time` (the venue's
//! timestamp): research reads it "as known at T" (DESIGN principle 4). Markers make outages
//! explicit, so a quiet stretch never reads as "nothing happened" when the feed was down.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_core::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::instrument::VenueId;
use crate::ports::PortError;

/// Depth bands around the mid price, in percent, that [`summarize_book`] measures.
pub const DEPTH_BANDS_PCT: [f64; 4] = [0.1, 0.5, 1.0, 2.0];

/// One forward-recorded line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Liquidation(Liquidation),
    Book(BookSummary),
    Derivs(DerivsSnapshot),
    Marker(Marker),
}

impl Record {
    /// The feed this record belongs to (its directory in the store).
    #[must_use]
    pub const fn feed(&self) -> Feed {
        match self {
            Self::Liquidation(_) => Feed::Liquidations,
            Self::Book(_) => Feed::Book,
            Self::Derivs(_) => Feed::Derivs,
            Self::Marker(m) => m.feed,
        }
    }

    #[must_use]
    pub const fn known_time(&self) -> DateTime<Utc> {
        match self {
            Self::Liquidation(r) => r.known_time,
            Self::Book(r) => r.known_time,
            Self::Derivs(r) => r.known_time,
            Self::Marker(r) => r.known_time,
        }
    }
}

/// A recorded feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feed {
    Liquidations,
    Book,
    Derivs,
}

impl Feed {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Liquidations => "liquidations",
            Self::Book => "book",
            Self::Derivs => "derivs",
        }
    }
}

/// A forced liquidation on a perpetual.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Liquidation {
    pub known_time: DateTime<Utc>,
    pub event_time: DateTime<Utc>,
    pub venue: VenueId,
    pub symbol: String,
    /// Side of the liquidation order: `sell` liquidates a long, `buy` a short.
    pub side: String,
    pub price: f64,
    pub avg_price: f64,
    pub qty: f64,
}

/// Depth notional on one side within a band around the mid.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DepthBand {
    pub pct: f64,
    /// Quote notional within the band; `None` when the snapshot does not reach that far.
    pub notional: Option<f64>,
}

/// An order-book snapshot reduced to what research uses (raw books are too heavy to keep).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookSummary {
    pub known_time: DateTime<Utc>,
    pub event_time: DateTime<Utc>,
    pub venue: VenueId,
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
    pub spread_bps: f64,
    pub bids: Vec<DepthBand>,
    pub asks: Vec<DepthBand>,
    /// `(bid − ask) / (bid + ask)` notional within 1%: +1 all bids, −1 all asks.
    pub imbalance_1pct: Option<f64>,
    /// Levels per side in the snapshot.
    pub levels: usize,
}

/// A perpetual's state at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DerivsSnapshot {
    pub known_time: DateTime<Utc>,
    pub event_time: DateTime<Utc>,
    pub venue: VenueId,
    pub symbol: String,
    pub mark: f64,
    pub index: f64,
    /// `mark / index − 1`.
    pub basis: f64,
    /// The current period's funding rate estimate (fraction per period).
    pub funding_rate: f64,
    pub next_funding: DateTime<Utc>,
    /// Open interest in contracts; `None` when that call failed.
    pub open_interest: Option<f64>,
    /// Open interest × mark.
    pub open_interest_usd: Option<f64>,
}

/// A feed's state change: recording started or stopped, a stream went up or down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub known_time: DateTime<Utc>,
    pub feed: Feed,
    pub state: MarkerState,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerState {
    /// The recorder started recording the feed.
    Start,
    /// The recorder stopped cleanly.
    Stop,
    /// A stream connected.
    Up,
    /// A stream disconnected; nothing is recorded until the next `up`.
    Down,
    /// A poll failed; that sample is missing.
    Error,
}

/// One price level: `(price, quantity)`.
pub type Level = (f64, f64);

/// Reduces a book (bids best first, asks best first) to spread, depth bands and imbalance.
/// `None` for an empty or crossed book.
#[must_use]
pub fn summarize_book(bids: &[Level], asks: &[Level]) -> Option<BookParts> {
    let (&(bid, _), &(ask, _)) = (bids.first()?, asks.first()?);
    if !(bid > 0.0 && ask > bid) {
        return None;
    }
    let mid = f64::midpoint(bid, ask);
    let band = |levels: &[Level], pct: f64, below: bool| {
        let edge = if below {
            mid * (1.0 - pct / 100.0)
        } else {
            mid * (1.0 + pct / 100.0)
        };
        let reaches = levels
            .last()
            .is_some_and(|&(p, _)| if below { p <= edge } else { p >= edge });
        let notional: f64 = levels
            .iter()
            .filter(|&&(p, _)| if below { p >= edge } else { p <= edge })
            .map(|&(p, q)| p * q)
            .sum();
        DepthBand {
            pct,
            notional: reaches.then_some(notional),
        }
    };
    let bands = |levels: &[Level], below: bool| -> Vec<DepthBand> {
        DEPTH_BANDS_PCT
            .iter()
            .map(|&pct| band(levels, pct, below))
            .collect()
    };
    let (bid_bands, ask_bands) = (bands(bids, true), bands(asks, false));
    let one = |bands: &[DepthBand]| {
        bands
            .iter()
            .find(|b| (b.pct - 1.0).abs() < f64::EPSILON)
            .and_then(|b| b.notional)
    };
    let imbalance_1pct = match (one(&bid_bands), one(&ask_bands)) {
        (Some(b), Some(a)) if b + a > 0.0 => Some((b - a) / (b + a)),
        _ => None,
    };
    Some(BookParts {
        bid,
        ask,
        mid,
        spread_bps: (ask - bid) / mid * 10_000.0,
        bids: bid_bands,
        asks: ask_bands,
        imbalance_1pct,
        levels: bids.len().min(asks.len()),
    })
}

/// The computed part of a [`BookSummary`].
#[derive(Debug, Clone, PartialEq)]
pub struct BookParts {
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
    pub spread_bps: f64,
    pub bids: Vec<DepthBand>,
    pub asks: Vec<DepthBand>,
    pub imbalance_1pct: Option<f64>,
    pub levels: usize,
}

/// A raw book snapshot from a venue.
#[derive(Debug, Clone, PartialEq)]
pub struct BookSnapshot {
    pub event_time: DateTime<Utc>,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

/// Order-book snapshots (REST).
#[async_trait]
pub trait BookFeed: Send + Sync + std::fmt::Debug {
    fn venue(&self) -> &VenueId;

    async fn book(&self, symbol: &str) -> Result<BookSnapshot, PortError>;
}

/// Perpetual derivatives state (REST). `known_time` is stamped by the caller.
#[async_trait]
pub trait DerivsFeed: Send + Sync + std::fmt::Debug {
    fn venue(&self) -> &VenueId;

    /// Mark, index, basis and funding of `symbols` (unknown symbols are left out); open
    /// interest when available. `known_time` on the results is set to `now`.
    async fn snapshot(
        &self,
        symbols: &[String],
        now: DateTime<Utc>,
    ) -> Result<Vec<DerivsSnapshot>, PortError>;
}

/// What a liquidation stream yields.
#[derive(Debug, Clone, PartialEq)]
pub enum LiquidationEvent {
    Up,
    Down(String),
    /// A liquidation; `known_time` is when the adapter received it.
    Liquidation(Liquidation),
}

/// Liquidations as they happen (websocket). The adapter reconnects on its own and reports each
/// connect and disconnect, so the recorder can mark the gaps.
#[async_trait]
pub trait LiquidationFeed: Send + Sync + std::fmt::Debug {
    fn venue(&self) -> &VenueId;

    async fn subscribe(&self) -> Result<BoxStream<'static, LiquidationEvent>, PortError>;
}

#[cfg(test)]
mod tests;
