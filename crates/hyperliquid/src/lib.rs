//! Followed traders, read only (SRC-3): Hyperliquid behind the [`TraderSource`] port.
//!
//! Hyperliquid is an on-chain exchange: every account's positions and fills are public through
//! its info API (no key, no account). trader reads them for addresses the operator chooses; it
//! never signs or sends anything. Addresses are validated before use, numbers are parsed
//! strictly (anything unparseable is skipped), and responses are size-capped.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use trader_core::traders::{TraderAccount, TraderFill, TraderPosition};
use trader_core::{PortError, TraderSource};

/// Public info endpoint.
pub const HYPERLIQUID_INFO: &str = "https://api.hyperliquid.xyz/info";
/// Largest response accepted.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;
/// Fills per request (the API's page size) and pages read at most (it keeps ~10k per account).
const PAGE: usize = 2_000;
const MAX_PAGES: usize = 10;
const TIMEOUT: Duration = Duration::from_secs(20);
/// Tries per request when rate-limited (waits 2 s, then 4 s).
const ATTEMPTS: u32 = 3;
const AGENT: &str = concat!("trader/", env!("CARGO_PKG_VERSION"), " (local research)");

#[derive(Debug, thiserror::Error)]
pub enum HyperliquidError {
    #[error("invalid address `{0}` (0x followed by 40 hex digits)")]
    Address(String),
    #[error("request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} answered {status}")]
    Status { url: String, status: u16 },
    #[error("{url} sent more than {MAX_BYTES} bytes")]
    TooLarge { url: String },
    #[error("{url} sent an unexpected response: {message}")]
    Shape { url: String, message: String },
}

impl From<HyperliquidError> for PortError {
    fn from(err: HyperliquidError) -> Self {
        Self::adapter("hyperliquid", err)
    }
}

/// Normalise (trim, lower-case) and validate an account address.
pub fn address(text: &str) -> Result<String, HyperliquidError> {
    let lower = text.trim().to_ascii_lowercase();
    let valid = lower.len() == 42
        && lower.starts_with("0x")
        && lower.chars().skip(2).all(|c| c.is_ascii_hexdigit());
    if valid {
        Ok(lower)
    } else {
        Err(HyperliquidError::Address(text.trim().to_owned()))
    }
}

/// Which way a fill moved the position, from Hyperliquid's `dir`: opening or flipping into a
/// long is `1.0`, into a short `-1.0`; closes, settlements and spot trades are `None`.
pub fn opens(dir: &str) -> Option<f64> {
    match dir {
        "Open Long" | "Short > Long" => Some(1.0),
        "Open Short" | "Long > Short" => Some(-1.0),
        _ => None,
    }
}

/// Hyperliquid's public info API.
#[derive(Debug, Clone)]
pub struct HyperliquidSource {
    url: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearinghouseState {
    margin_summary: MarginSummary,
    asset_positions: Vec<AssetPosition>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarginSummary {
    account_value: String,
}

#[derive(Debug, Deserialize)]
struct AssetPosition {
    position: Position,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_field_names,
    reason = "field names mirror Hyperliquid's position object"
)]
struct Position {
    coin: String,
    szi: String,
    entry_px: Option<String>,
    position_value: String,
    unrealized_pnl: String,
    leverage: Option<Leverage>,
}

#[derive(Debug, Deserialize)]
struct Leverage {
    value: f64,
}

#[derive(Debug, Deserialize)]
struct Fill {
    coin: String,
    px: String,
    sz: String,
    time: i64,
    dir: String,
}

fn number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|n| n.is_finite())
}

impl HyperliquidSource {
    pub fn new() -> Result<Self, HyperliquidError> {
        Self::with_url(HYPERLIQUID_INFO)
    }

    /// Against another endpoint (a test server).
    pub fn with_url(url: &str) -> Result<Self, HyperliquidError> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(AGENT)
            .build()
            .map_err(|source| HyperliquidError::Http {
                url: url.to_owned(),
                source,
            })?;
        Ok(Self {
            url: url.to_owned(),
            client,
        })
    }

    async fn info<T: DeserializeOwned>(&self, body: &Value) -> Result<T, HyperliquidError> {
        let url = self.url.clone();
        let http = |source| HyperliquidError::Http {
            url: url.clone(),
            source,
        };
        let mut attempt = 0;
        let response = loop {
            attempt += 1;
            let response = self
                .client
                .post(&self.url)
                .json(body)
                .send()
                .await
                .map_err(http)?;
            // The info API rate-limits by weight; fills pages are heavy. Back off and retry.
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(2_u64.pow(attempt))).await;
                continue;
            }
            break response;
        };
        let status = response.status();
        if !status.is_success() {
            return Err(HyperliquidError::Status {
                url,
                status: status.as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return Err(HyperliquidError::TooLarge { url });
        }
        let bytes = response.bytes().await.map_err(http)?;
        if bytes.len() > MAX_BYTES {
            return Err(HyperliquidError::TooLarge { url });
        }
        serde_json::from_slice(&bytes).map_err(|e| HyperliquidError::Shape {
            url,
            message: e.to_string(),
        })
    }
}

#[async_trait]
impl TraderSource for HyperliquidSource {
    fn venue(&self) -> &'static str {
        "hyperliquid"
    }

    async fn account(&self, address: &str) -> Result<TraderAccount, PortError> {
        let address = self::address(address)?;
        let state: ClearinghouseState = self
            .info(&json!({ "type": "clearinghouseState", "user": address }))
            .await?;
        let mut positions: Vec<TraderPosition> = state
            .asset_positions
            .into_iter()
            .filter_map(|p| {
                let p = p.position;
                Some(TraderPosition {
                    size: number(&p.szi)?,
                    entry_price: p.entry_px.as_deref().and_then(number),
                    value: number(&p.position_value)?,
                    unrealized_pnl: number(&p.unrealized_pnl)?,
                    leverage: p.leverage.map(|l| l.value).filter(|v| v.is_finite()),
                    coin: p.coin,
                })
            })
            .filter(|p| p.size != 0.0)
            .collect();
        positions.sort_by(|a, b| b.value.total_cmp(&a.value));
        Ok(TraderAccount {
            venue: "hyperliquid".into(),
            url: format!("https://app.hyperliquid.xyz/explorer/address/{address}"),
            address,
            account_value: number(&state.margin_summary.account_value).unwrap_or(0.0),
            positions,
        })
    }

    async fn fills(
        &self,
        address: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<TraderFill>, PortError> {
        let address = self::address(address)?;
        let mut from = start.timestamp_millis();
        let to = end.timestamp_millis();
        let mut fills: Vec<TraderFill> = Vec::new();
        for _ in 0..MAX_PAGES {
            let page: Vec<Fill> = self
                .info(&json!({
                    "type": "userFillsByTime",
                    "user": address,
                    "startTime": from,
                    "endTime": to,
                }))
                .await?;
            let count = page.len();
            let newest = page.iter().map(|f| f.time).max();
            fills.extend(page.into_iter().filter_map(|f| {
                Some(TraderFill {
                    ts: DateTime::from_timestamp_millis(f.time)?,
                    price: number(&f.px)?,
                    size: number(&f.sz)?,
                    opens: opens(&f.dir),
                    coin: f.coin,
                })
            }));
            match newest {
                Some(newest) if count >= PAGE && newest < to => from = newest + 1,
                _ => break,
            }
        }
        fills.sort_by_key(|f| f.ts);
        Ok(fills)
    }
}

#[cfg(test)]
mod tests;
