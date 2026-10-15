//! Market-wide indicator series from public, keyless sources (data-sources plan):
//!
//! - **Deribit DVOL** (`/api/v2/public/get_volatility_index_data`): the 30-day implied
//!   volatility index of BTC and ETH options, daily, from 2021-03.
//! - **`DefiLlama`** (`stablecoins.llama.fi/stablecoincharts/all`): total stablecoin supply in
//!   USD, daily, from 2017-11.
//! - **Coinbase Exchange** (`/products/{BASE}-USD/candles`): daily closes of a second venue, to
//!   cross-check stored Binance closes.
//!
//! Each returns `(day, value)` points, oldest first, with `day` the UTC day the value
//! describes (its open time); callers act on a value only from the next day on, when it is
//! certainly known. Read-only; nothing here can trade.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{DataError, Result};

const DERIBIT: &str = "https://www.deribit.com";
const DEFILLAMA: &str = "https://stablecoins.llama.fi";
const COINBASE: &str = "https://api.exchange.coinbase.com";
/// Coinbase returns at most this many candles per call.
const COINBASE_PAGE: i64 = 300;

/// One daily value.
pub type Point = (DateTime<Utc>, f64);

/// Public indicator sources (each host overridable for tests).
#[derive(Debug, Clone)]
pub struct Indicators {
    client: reqwest::Client,
    deribit: String,
    defillama: String,
    coinbase: String,
}

impl Indicators {
    pub fn new() -> Result<Self> {
        Self::with_hosts(DERIBIT, DEFILLAMA, COINBASE)
    }

    /// Point at other hosts, e.g. local mock servers.
    pub fn with_hosts(
        deribit: impl Into<String>,
        defillama: impl Into<String>,
        coinbase: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("quorum/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            deribit: deribit.into(),
            defillama: defillama.into(),
            coinbase: coinbase.into(),
        })
    }

    async fn get(&self, url: String, query: &[(&str, String)]) -> Result<Value> {
        Ok(self
            .client
            .get(url)
            .query(query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Daily DVOL closes of `currency` (`BTC` or `ETH`) from `start` to now.
    pub async fn dvol(&self, currency: &str, start: DateTime<Utc>) -> Result<Vec<Point>> {
        let mut end = Utc::now().timestamp_millis();
        let mut out = Vec::new();
        // Pages run backwards: `continuation` is the next (older) end.
        loop {
            let body = self
                .get(
                    format!("{}/api/v2/public/get_volatility_index_data", self.deribit),
                    &[
                        ("currency", currency.to_owned()),
                        ("resolution", "1D".to_owned()),
                        ("start_timestamp", start.timestamp_millis().to_string()),
                        ("end_timestamp", end.to_string()),
                    ],
                )
                .await?;
            let result = &body["result"];
            let rows = result["data"]
                .as_array()
                .ok_or_else(|| DataError::Malformed("dvol: no data".into()))?;
            for row in rows {
                let (Some(ms), Some(close)) = (row[0].as_i64(), row[4].as_f64()) else {
                    continue;
                };
                if let Some(ts) = DateTime::from_timestamp_millis(ms) {
                    out.push((ts, close));
                }
            }
            match result["continuation"].as_i64() {
                Some(next) if next < end && !rows.is_empty() => end = next,
                _ => break,
            }
        }
        out.sort_by_key(|p| p.0);
        out.dedup_by_key(|p| p.0);
        Ok(out)
    }

    /// Total stablecoin supply in USD, daily, its whole history.
    pub async fn stablecoin_supply(&self) -> Result<Vec<Point>> {
        let body = self
            .get(format!("{}/stablecoincharts/all", self.defillama), &[])
            .await?;
        let rows = body
            .as_array()
            .ok_or_else(|| DataError::Malformed("stablecoins: not a list".into()))?;
        let mut out: Vec<Point> = rows
            .iter()
            .filter_map(|r| {
                let secs = r["date"].as_str()?.parse::<i64>().ok()?;
                let usd = r["totalCirculatingUSD"]["peggedUSD"].as_f64()?;
                Some((DateTime::from_timestamp(secs, 0)?, usd))
            })
            .collect();
        out.sort_by_key(|p| p.0);
        Ok(out)
    }

    /// Daily closes of `{base}-USD` on Coinbase between `start` and `end` (empty when Coinbase
    /// does not list it).
    pub async fn coinbase_closes(
        &self,
        base: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Point>> {
        let mut out = Vec::new();
        let mut from = start;
        while from < end {
            let to = (from + chrono::Duration::days(COINBASE_PAGE)).min(end);
            let response = self
                .client
                .get(format!("{}/products/{base}-USD/candles", self.coinbase))
                .query(&[
                    ("granularity", "86400".to_owned()),
                    ("start", from.to_rfc3339()),
                    ("end", to.to_rfc3339()),
                ])
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }
            let body: Value = response.error_for_status()?.json().await?;
            for row in body.as_array().into_iter().flatten() {
                let (Some(secs), Some(close)) = (row[0].as_i64(), row[4].as_f64()) else {
                    continue;
                };
                if let Some(ts) = DateTime::from_timestamp(secs, 0) {
                    out.push((ts, close));
                }
            }
            from = to;
        }
        out.sort_by_key(|p| p.0);
        out.dedup_by_key(|p| p.0);
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
