//! Prediction markets, read only (SRC-2): Kalshi behind the [`PredictionSource`] port.
//!
//! Kalshi is a regulated US exchange whose market data API is public (no key, no account).
//! trader reads a market's state and its hourly probability history; it never authenticates and
//! has no way to place an order. Questions are untrusted text: trimmed, stripped of control
//! characters and bounded. Tickers are validated before they reach a URL path, and responses are
//! size-capped.
//!
//! Polymarket is not implemented: it is blocked in some jurisdictions (including at DNS level
//! by Czech providers), and trader does not work around such blocks.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use trader_core::predictions::{PredictionMarket, ProbabilityPoint};
use trader_core::{PortError, PredictionSource};

/// Public API root.
pub const KALSHI_API: &str = "https://api.elections.kalshi.com/trade-api/v2";
/// Longest question kept (characters).
pub const MAX_QUESTION: usize = 300;
/// Largest response accepted.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
/// Hourly candles per history request (Kalshi caps one request at 5000 periods).
const CHUNK_HOURS: i64 = 4_000;
const AGENT: &str = concat!("trader/", env!("CARGO_PKG_VERSION"), " (local research)");

#[derive(Debug, thiserror::Error)]
pub enum PredictionError {
    #[error("invalid ticker `{0}` (letters, digits, '.', '-' only)")]
    Ticker(String),
    #[error("request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("no market `{0}` on Kalshi")]
    NotFound(String),
    #[error("{url} answered {status}")]
    Status { url: String, status: u16 },
    #[error("{url} sent more than {MAX_BYTES} bytes")]
    TooLarge { url: String },
    #[error("{url} sent an unexpected response: {message}")]
    Shape { url: String, message: String },
}

impl From<PredictionError> for PortError {
    fn from(err: PredictionError) -> Self {
        Self::adapter("kalshi", err)
    }
}

/// Normalise a ticker or a pasted market URL (`…/KXBTCMAXY-26DEC31-109999.99`) and validate it.
pub fn ticker(text: &str) -> Result<String, PredictionError> {
    let last = text
        .trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let valid = (3..=80).contains(&last.len())
        && last.starts_with(|c: char| c.is_ascii_alphanumeric())
        && last
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '.' | '-'));
    if valid {
        Ok(last)
    } else {
        Err(PredictionError::Ticker(text.trim().to_owned()))
    }
}

/// Kalshi's public market data.
#[derive(Debug, Clone)]
pub struct KalshiSource {
    base: String,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
struct MarketResponse {
    market: Market,
}

#[derive(Debug, Deserialize)]
struct Market {
    ticker: String,
    event_ticker: String,
    title: String,
    status: String,
    close_time: Option<DateTime<Utc>>,
    last_price_dollars: Option<String>,
    yes_bid_dollars: Option<String>,
    yes_ask_dollars: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventResponse {
    event: Event,
}

#[derive(Debug, Deserialize)]
struct Event {
    series_ticker: String,
}

#[derive(Debug, Deserialize)]
struct Candles {
    candlesticks: Vec<Candle>,
}

#[derive(Debug, Deserialize)]
struct Candle {
    end_period_ts: i64,
    price: Close,
    yes_bid: Close,
    yes_ask: Close,
}

#[derive(Debug, Deserialize)]
struct Close {
    close_dollars: Option<String>,
}

/// A dollar price as a probability; `None` when absent or outside `0..=1`.
fn probability(text: Option<&str>) -> Option<f64> {
    text.and_then(|t| t.parse::<f64>().ok())
        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
}

/// Last trade, else the bid/ask mid (a quote on both sides).
fn implied(last: Option<&str>, bid: Option<&str>, ask: Option<&str>) -> Option<f64> {
    probability(last).or_else(|| {
        let (bid, ask) = (probability(bid)?, probability(ask)?);
        (ask > 0.0 && bid <= ask).then(|| f64::midpoint(bid, ask))
    })
}

fn question(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    clean.trim().chars().take(MAX_QUESTION).collect()
}

impl KalshiSource {
    pub fn new() -> Result<Self, PredictionError> {
        Self::with_base(KALSHI_API)
    }

    /// Against another API root (a test server).
    pub fn with_base(base: &str) -> Result<Self, PredictionError> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(AGENT)
            .build()
            .map_err(|source| PredictionError::Http {
                url: base.to_owned(),
                source,
            })?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            client,
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        ticker: &str,
    ) -> Result<T, PredictionError> {
        let url = format!("{}{path}", self.base);
        let http = |source| PredictionError::Http {
            url: url.clone(),
            source,
        };
        let response = self.client.get(&url).send().await.map_err(http)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(PredictionError::NotFound(ticker.to_owned()));
        }
        if !status.is_success() {
            return Err(PredictionError::Status {
                url,
                status: status.as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BYTES as u64)
        {
            return Err(PredictionError::TooLarge { url });
        }
        let bytes = response.bytes().await.map_err(http)?;
        if bytes.len() > MAX_BYTES {
            return Err(PredictionError::TooLarge { url });
        }
        serde_json::from_slice(&bytes).map_err(|e| PredictionError::Shape {
            url,
            message: e.to_string(),
        })
    }

    async fn market_and_series(&self, ticker: &str) -> Result<(Market, String), PredictionError> {
        let market: MarketResponse = self.get(&format!("/markets/{ticker}"), ticker).await?;
        let event_ticker = self::ticker(&market.market.event_ticker)?;
        let event: EventResponse = self.get(&format!("/events/{event_ticker}"), ticker).await?;
        let series = self::ticker(&event.event.series_ticker)?;
        Ok((market.market, series))
    }
}

#[async_trait]
impl PredictionSource for KalshiSource {
    fn venue(&self) -> &'static str {
        "kalshi"
    }

    async fn market(&self, ticker: &str) -> Result<PredictionMarket, PortError> {
        let ticker = self::ticker(ticker)?;
        let (market, series) = self.market_and_series(&ticker).await?;
        Ok(PredictionMarket {
            venue: "kalshi".into(),
            ticker: market.ticker,
            question: question(&market.title),
            probability: implied(
                market.last_price_dollars.as_deref(),
                market.yes_bid_dollars.as_deref(),
                market.yes_ask_dollars.as_deref(),
            ),
            active: market.status == "active" || market.status == "open",
            close_time: market.close_time,
            url: format!("https://kalshi.com/markets/{}", series.to_ascii_lowercase()),
        })
    }

    async fn history(
        &self,
        ticker: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProbabilityPoint>, PortError> {
        let ticker = self::ticker(ticker)?;
        let (_, series) = self.market_and_series(&ticker).await?;
        let mut points: Vec<ProbabilityPoint> = Vec::new();
        let mut from = start;
        while from < end {
            let to = (from + ChronoDuration::hours(CHUNK_HOURS)).min(end);
            let path = format!(
                "/series/{series}/markets/{ticker}/candlesticks?start_ts={}&end_ts={}&period_interval=60",
                from.timestamp(),
                to.timestamp()
            );
            let candles: Candles = self.get(&path, &ticker).await?;
            points.extend(candles.candlesticks.iter().filter_map(|c| {
                Some(ProbabilityPoint {
                    ts: DateTime::from_timestamp(c.end_period_ts, 0)?,
                    probability: implied(
                        c.price.close_dollars.as_deref(),
                        c.yes_bid.close_dollars.as_deref(),
                        c.yes_ask.close_dollars.as_deref(),
                    )?,
                })
            }));
            from = to;
        }
        points.sort_by_key(|p| p.ts);
        points.dedup_by_key(|p| p.ts);
        points.retain(|p| p.ts >= start && p.ts <= end);
        Ok(points)
    }
}

#[cfg(test)]
mod tests;
