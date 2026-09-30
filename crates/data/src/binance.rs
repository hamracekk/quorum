//! Binance spot market data from the public REST API (no API key required).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use trader_core::{Bar, InstrumentId, InstrumentKind, MarketData, PortError, VenueId};

use crate::{DataError, Result};

/// Venue id served by [`BinanceSpot`].
pub const VENUE: &str = "binance";
const BASE_URL: &str = "https://api.binance.com";
const PAGE_LIMIT: usize = 1_000;

/// `MarketData` adapter for Binance spot klines.
#[derive(Debug, Clone)]
pub struct BinanceSpot {
    venue: VenueId,
    client: reqwest::Client,
    base_url: String,
}

impl BinanceSpot {
    pub fn new() -> Result<Self> {
        Self::with_base_url(BASE_URL)
    }

    /// Point at a different host, e.g. a local mock server.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("trader/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        let venue = VenueId::new(VENUE).map_err(|e| DataError::Malformed(e.to_string()))?;
        Ok(Self {
            venue,
            client,
            base_url: base_url.into(),
        })
    }

    /// Page through klines; drops bars still forming and bars at or after `end`.
    async fn klines(
        &self,
        symbol: &str,
        interval: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Bar>> {
        let end_ms = end.timestamp_millis();
        let mut cursor = start.timestamp_millis();
        let mut bars = Vec::new();

        while cursor < end_ms {
            let page: Vec<Vec<Value>> = self
                .client
                .get(format!("{}/api/v3/klines", self.base_url))
                .query(&[
                    ("symbol", symbol.to_owned()),
                    ("interval", interval.to_owned()),
                    ("startTime", cursor.to_string()),
                    ("endTime", end_ms.to_string()),
                    ("limit", PAGE_LIMIT.to_string()),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            tracing::debug!(symbol, interval, rows = page.len(), "fetched klines page");

            let now_ms = Utc::now().timestamp_millis();
            for row in &page {
                if int_field(row, 6)? < now_ms {
                    let bar = parse_row(row)?;
                    if bar.ts < end {
                        bars.push(bar);
                    }
                }
            }

            let Some(last) = page.last() else { break };
            if page.len() < PAGE_LIMIT {
                break;
            }
            cursor = int_field(last, 0)? + 1;
            // Stay well inside Binance's request-weight limits.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        Ok(bars)
    }
}

#[async_trait]
impl MarketData for BinanceSpot {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn check(&self) -> std::result::Result<(), PortError> {
        self.client
            .get(format!("{}/api/v3/ping", self.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(drop)
            .map_err(|e| PortError::adapter("binance ping", e))
    }

    async fn bars(
        &self,
        instrument: &InstrumentId,
        interval: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> std::result::Result<Vec<Bar>, PortError> {
        if instrument.venue() != &self.venue || instrument.kind() != InstrumentKind::Spot {
            return Err(PortError::Unsupported(instrument.to_string()));
        }
        self.klines(instrument.symbol(), interval, start, end)
            .await
            .map_err(|e| PortError::adapter(format!("binance klines for {instrument}"), e))
    }
}

fn int_field(row: &[Value], i: usize) -> Result<i64> {
    row.get(i)
        .and_then(Value::as_i64)
        .ok_or_else(|| DataError::Malformed(format!("kline field {i} is not an integer")))
}

fn num_field(row: &[Value], i: usize) -> Result<f64> {
    row.get(i)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| DataError::Malformed(format!("kline field {i} is not a number")))
}

fn parse_row(row: &[Value]) -> Result<Bar> {
    let open_ms = int_field(row, 0)?;
    Ok(Bar {
        ts: DateTime::from_timestamp_millis(open_ms)
            .ok_or_else(|| DataError::Malformed(format!("invalid open time {open_ms}")))?,
        open: num_field(row, 1)?,
        high: num_field(row, 2)?,
        low: num_field(row, 3)?,
        close: num_field(row, 4)?,
        volume: num_field(row, 5)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_a_kline_row() {
        let row = json!([
            1_700_000_000_000_i64,
            "100.5",
            "101",
            "99",
            "100.75",
            "12.5",
            1_700_000_059_999_i64
        ]);
        let row = row.as_array().expect("array");
        let bar = parse_row(row).expect("valid row");
        assert_eq!(bar.ts.timestamp_millis(), 1_700_000_000_000);
        assert!((bar.close - 100.75).abs() < 1e-12);
        assert!((bar.volume - 12.5).abs() < 1e-12);
    }

    #[test]
    fn rejects_non_numeric_prices() {
        let row = json!([0, "abc", "1", "1", "1", "1", 0]);
        let row = row.as_array().expect("array");
        assert!(matches!(parse_row(row), Err(DataError::Malformed(_))));
    }

    #[tokio::test]
    async fn refuses_other_venues_and_kinds_without_network() {
        let binance = BinanceSpot::new().expect("client");
        let start = DateTime::from_timestamp(0, 0).expect("valid timestamp");
        for text in ["hyperliquid:BTC", "binance:BTCUSDT:perp"] {
            let instrument: InstrumentId = text.parse().expect("valid instrument");
            let err = binance
                .bars(&instrument, "1h", start, start)
                .await
                .expect_err("unsupported");
            assert!(matches!(err, PortError::Unsupported(_)), "{text}");
        }
    }

    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HOUR: i64 = 3_600_000;
    /// 2100-01-01: always in the future, so bars closing then are "still forming".
    const FAR_FUTURE_MS: i64 = 4_102_444_800_000;

    fn kline(open_ms: i64, close_ms: i64) -> Value {
        json!([open_ms, "1", "2", "0.5", "1.5", "10", close_ms])
    }

    fn closed(i: i64) -> Value {
        kline(i * HOUR, i * HOUR + HOUR - 1)
    }

    fn btc() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
    }

    fn ms(millis: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(millis).expect("valid timestamp")
    }

    #[tokio::test]
    async fn pages_through_klines_and_drops_forming_bars() {
        let server = MockServer::start().await;
        let first: Vec<Value> = (0..1_000).map(closed).collect();
        let second = vec![closed(1_000), kline(FAR_FUTURE_MS - HOUR, FAR_FUTURE_MS)];
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("startTime", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(first))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .and(query_param("startTime", (999 * HOUR + 1).to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(second))
            .expect(1)
            .mount(&server)
            .await;

        let binance = BinanceSpot::with_base_url(server.uri()).expect("client");
        let bars = binance
            .bars(&btc(), "1h", ms(0), ms(FAR_FUTURE_MS + HOUR))
            .await
            .expect("bars");

        assert_eq!(bars.len(), 1_001);
        assert!(bars.windows(2).all(|w| w[0].ts < w[1].ts));
        assert_eq!(bars.last().map(|b| b.ts), Some(ms(1_000 * HOUR)));
    }

    #[tokio::test]
    async fn end_is_exclusive() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                closed(0),
                closed(1),
                closed(2),
            ]))
            .mount(&server)
            .await;

        let binance = BinanceSpot::with_base_url(server.uri()).expect("client");
        let bars = binance
            .bars(&btc(), "1h", ms(0), ms(2 * HOUR))
            .await
            .expect("bars");
        assert_eq!(bars.len(), 2);
    }

    #[tokio::test]
    async fn http_errors_surface_as_adapter_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/klines"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("{\"msg\":\"Invalid symbol.\"}"),
            )
            .mount(&server)
            .await;

        let binance = BinanceSpot::with_base_url(server.uri()).expect("client");
        let err = binance
            .bars(&btc(), "1h", ms(0), ms(HOUR))
            .await
            .expect_err("http 400");
        assert!(matches!(err, PortError::Adapter { .. }), "{err}");
    }

    #[tokio::test]
    async fn check_pings_the_exchange() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ping"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let binance = BinanceSpot::with_base_url(server.uri()).expect("client");
        binance.check().await.expect("reachable");
    }

    #[tokio::test]
    async fn check_reports_an_unhealthy_exchange() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/ping"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let binance = BinanceSpot::with_base_url(server.uri()).expect("client");
        assert!(binance.check().await.is_err());
    }
}
