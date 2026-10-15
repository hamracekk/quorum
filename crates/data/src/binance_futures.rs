//! Binance USDT-margined perpetual futures, public data: funding-rate history (E-2) and, for
//! the forward recorder, mark/index/funding snapshots with open interest (`DerivsFeed`).
//!
//! Read-only and keyless (`fapi.binance.com/fapi/v1/…`); nothing here can trade.
//! A spot symbol such as `BTCUSDT` maps to the perpetual of the same name.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use quorum_core::derivatives::FundingPoint;
use quorum_core::recorder::{DerivsFeed, DerivsSnapshot};
use quorum_core::{PortError, VenueId};
use serde_json::Value;

use crate::binance::VENUE;
use crate::{DataError, Result};

const BASE_URL: &str = "https://fapi.binance.com";
const PAGE_LIMIT: usize = 1_000;

/// Public Binance futures data.
#[derive(Debug, Clone)]
pub struct BinanceFutures {
    venue: VenueId,
    client: reqwest::Client,
    base_url: String,
}

impl BinanceFutures {
    pub fn new() -> Result<Self> {
        Self::with_base_url(BASE_URL)
    }

    /// Point at a different host, e.g. a local mock server.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("quorum/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            venue: VenueId::new(VENUE).map_err(|e| DataError::Malformed(e.to_string()))?,
            client,
            base_url: base_url.into(),
        })
    }

    /// Settled funding rates of `symbol` from `start` (inclusive) to `end`, oldest first.
    pub async fn funding_history(
        &self,
        symbol: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<FundingPoint>> {
        let end_ms = end.timestamp_millis();
        let mut cursor = start.timestamp_millis();
        let mut points = Vec::new();
        while cursor < end_ms {
            let page: Vec<Value> = self
                .client
                .get(format!("{}/fapi/v1/fundingRate", self.base_url))
                .query(&[
                    ("symbol", symbol.to_owned()),
                    ("startTime", cursor.to_string()),
                    ("endTime", end_ms.to_string()),
                    ("limit", PAGE_LIMIT.to_string()),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let mut last = None;
            for row in &page {
                let point = parse(row)?;
                last = Some(point.ts.timestamp_millis());
                points.push(point);
            }
            match last {
                Some(ms) if page.len() == PAGE_LIMIT => cursor = ms + 1,
                _ => break,
            }
        }
        Ok(points)
    }
}

impl BinanceFutures {
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        Ok(self
            .client
            .get(format!("{}{path}", self.base_url))
            .query(query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Open interest of `symbol` in contracts (`/fapi/v1/openInterest`).
    pub async fn open_interest(&self, symbol: &str) -> Result<f64> {
        let body = self
            .get("/fapi/v1/openInterest", &[("symbol", symbol)])
            .await?;
        body["openInterest"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| DataError::Malformed(format!("openInterest {body}")))
    }
}

/// One `/fapi/v1/premiumIndex` row; `known_time` is `now`, open interest is filled in later.
fn parse_premium(row: &Value, venue: &VenueId, now: DateTime<Utc>) -> Result<DerivsSnapshot> {
    let malformed = || DataError::Malformed(format!("premiumIndex row {row}"));
    let num = |key: &str| {
        row[key]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(malformed)
    };
    let ms = |key: &str| {
        row[key]
            .as_i64()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or_else(malformed)
    };
    let (mark, index) = (num("markPrice")?, num("indexPrice")?);
    if index <= 0.0 {
        return Err(malformed());
    }
    Ok(DerivsSnapshot {
        known_time: now,
        event_time: ms("time")?,
        venue: venue.clone(),
        symbol: row["symbol"].as_str().ok_or_else(malformed)?.to_owned(),
        mark,
        index,
        basis: mark / index - 1.0,
        funding_rate: num("lastFundingRate")?,
        next_funding: ms("nextFundingTime")?,
        open_interest: None,
        open_interest_usd: None,
    })
}

#[async_trait]
impl DerivsFeed for BinanceFutures {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// One `premiumIndex` call for every perpetual, then `openInterest` per symbol (a failed
    /// one leaves that symbol's open interest empty rather than dropping the snapshot).
    async fn snapshot(
        &self,
        symbols: &[String],
        now: DateTime<Utc>,
    ) -> std::result::Result<Vec<DerivsSnapshot>, PortError> {
        let all = self
            .get("/fapi/v1/premiumIndex", &[])
            .await
            .map_err(|e| PortError::adapter("binance premiumIndex", e))?;
        let rows = all
            .as_array()
            .ok_or_else(|| PortError::Invalid("premiumIndex is not a list".into()))?;
        let mut snapshots = Vec::new();
        for row in rows {
            if !row["symbol"]
                .as_str()
                .is_some_and(|s| symbols.iter().any(|w| w == s))
            {
                continue;
            }
            let mut snapshot = parse_premium(row, &self.venue, now)
                .map_err(|e| PortError::adapter("binance premiumIndex", e))?;
            match self.open_interest(&snapshot.symbol).await {
                Ok(oi) => {
                    snapshot.open_interest = Some(oi);
                    snapshot.open_interest_usd = Some(oi * snapshot.mark);
                }
                Err(err) => {
                    tracing::warn!(symbol = %snapshot.symbol, error = %err, "open interest missing");
                }
            }
            snapshots.push(snapshot);
        }
        Ok(snapshots)
    }
}

fn parse(row: &Value) -> Result<FundingPoint> {
    let malformed = || DataError::Malformed(format!("funding row {row}"));
    let ms = row["fundingTime"].as_i64().ok_or_else(malformed)?;
    let rate = row["fundingRate"]
        .as_str()
        .and_then(|r| r.parse::<f64>().ok())
        .ok_or_else(malformed)?;
    Ok(FundingPoint {
        ts: DateTime::from_timestamp_millis(ms).ok_or_else(malformed)?,
        rate,
    })
}

#[cfg(test)]
mod tests;
