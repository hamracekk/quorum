//! Binance USDT-margined perpetual futures, public data (E-2): funding-rate history.
//!
//! Read-only and keyless (`fapi.binance.com/fapi/v1/fundingRate`); nothing here can trade.
//! A spot symbol such as `BTCUSDT` maps to the perpetual of the same name.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use trader_core::derivatives::FundingPoint;

use crate::{DataError, Result};

const BASE_URL: &str = "https://fapi.binance.com";
const PAGE_LIMIT: usize = 1_000;

/// Public Binance futures data.
#[derive(Debug, Clone)]
pub struct BinanceFutures {
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
            .user_agent("trader/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
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
