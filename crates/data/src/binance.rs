//! Binance spot market data from the public REST API (no API key required).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use quorum_core::recorder::{BookFeed, BookSnapshot, Level};
use quorum_core::{Bar, InstrumentId, InstrumentKind, MarketData, PortError, VenueId};
use serde_json::Value;

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
            .user_agent("quorum/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        let venue = VenueId::new(VENUE).map_err(|e| DataError::Malformed(e.to_string()))?;
        Ok(Self {
            venue,
            client,
            base_url: base_url.into(),
        })
    }

    /// Every spot pair quoted in USDT that is trading now, excluding stablecoins, fiat and
    /// leveraged tokens (candidates for a point-in-time universe, S-3).
    pub async fn usdt_pairs(&self) -> Result<Vec<String>> {
        let body: Value = self
            .client
            .get(format!("{}/api/v3/exchangeInfo", self.base_url))
            .query(&[("permissions", "SPOT")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut pairs: Vec<String> = body["symbols"]
            .as_array()
            .ok_or_else(|| DataError::Malformed("exchangeInfo without symbols".into()))?
            .iter()
            .filter(|s| s["status"] == "TRADING" && s["quoteAsset"] == "USDT")
            .filter_map(|s| {
                let base = s["baseAsset"].as_str()?;
                let symbol = s["symbol"].as_str()?;
                // Plain ASCII symbols only (Binance lists a few in other scripts).
                let plain = symbol.chars().all(|c| c.is_ascii_alphanumeric());
                (plain && is_candidate(base)).then(|| symbol.to_owned())
            })
            .collect();
        pairs.sort();
        Ok(pairs)
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

/// Whether a base asset belongs in a crypto universe: not a stablecoin, fiat or gold token.
/// (Binance's leveraged tokens were delisted in 2024, so none trade any more.)
pub fn is_candidate(base: &str) -> bool {
    const EXCLUDED: [&str; 16] = [
        "USDC", "BUSD", "TUSD", "FDUSD", "USDP", "DAI", "UST", "USTC", "EUR", "GBP", "AUD", "TRY",
        "BRL", "AEUR", "PAXG", "XUSD",
    ];
    !EXCLUDED.contains(&base) && !base.starts_with("USD")
}

/// Whether `base` is one of Binance's leveraged tokens (BTCUP, ETHDOWN, BNBBULL, BEAR …).
///
/// They are decaying 3× products, not coins, all shut down — never universe members. `known`
/// holds the other bases, so real coins ending in the same letters (JUP, SYRUP) are kept.
pub fn is_leveraged_token(base: &str, known: &std::collections::BTreeSet<String>) -> bool {
    if base == "BULL" || base == "BEAR" {
        return true;
    }
    ["UP", "DOWN", "BULL", "BEAR"].iter().any(|suffix| {
        base.strip_suffix(suffix)
            .is_some_and(|coin| !coin.is_empty() && known.contains(coin))
    })
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

/// Levels per side for a book snapshot: weight 50, enough to reach ±2% for most coins.
const BOOK_LEVELS: &str = "1000";
/// Retry depth when 1000 levels stop short of ±2% (BTC, ETH: tight books): weight 250, used
/// for a few symbols only. Bands still beyond the snapshot are recorded as unknown.
const DEEP_BOOK_LEVELS: &str = "5000";
/// The farthest depth band the recorder measures, as a fraction.
const BOOK_REACH: f64 = 0.02;

/// Whether a snapshot reaches `BOOK_REACH` around the mid on both sides.
fn reaches(bids: &[Level], asks: &[Level]) -> bool {
    match (bids.first(), bids.last(), asks.first(), asks.last()) {
        (Some(&(bid, _)), Some(&(low, _)), Some(&(ask, _)), Some(&(high, _))) => {
            let mid = f64::midpoint(bid, ask);
            low <= mid * (1.0 - BOOK_REACH) && high >= mid * (1.0 + BOOK_REACH)
        }
        _ => true, // an empty side is the summary's to reject, not a reason to fetch more
    }
}

fn levels(side: &Value) -> Result<Vec<Level>> {
    side.as_array()
        .ok_or_else(|| DataError::Malformed("depth side is not a list".into()))?
        .iter()
        .map(|level| {
            let num = |i: usize| {
                level[i]
                    .as_str()
                    .and_then(|v| v.parse::<f64>().ok())
                    .ok_or_else(|| DataError::Malformed(format!("depth level {level}")))
            };
            Ok((num(0)?, num(1)?))
        })
        .collect()
}

#[async_trait]
impl BookFeed for BinanceSpot {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    /// `/api/v3/depth` has no timestamp of its own: `event_time` is when the response arrived.
    /// A snapshot that stops short of ±2% is fetched once more, deeper.
    async fn book(&self, symbol: &str) -> std::result::Result<BookSnapshot, PortError> {
        let fetch = |limit: &'static str| async move {
            let body: Value = self
                .client
                .get(format!("{}/api/v3/depth", self.base_url))
                .query(&[("symbol", symbol), ("limit", limit)])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok::<_, DataError>(BookSnapshot {
                event_time: Utc::now(),
                bids: levels(&body["bids"])?,
                asks: levels(&body["asks"])?,
            })
        };
        let context = || format!("binance depth {symbol}");
        let book = fetch(BOOK_LEVELS)
            .await
            .map_err(|e| PortError::adapter(context(), e))?;
        if reaches(&book.bids, &book.asks) {
            return Ok(book);
        }
        fetch(DEEP_BOOK_LEVELS)
            .await
            .map_err(|e| PortError::adapter(context(), e))
    }
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

    #[test]
    fn leveraged_tokens_are_told_apart_from_real_coins() {
        let known: std::collections::BTreeSet<String> = ["BTC", "ETH", "BNB", "J", "SYR"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for token in ["BTCUP", "ETHDOWN", "BNBBULL", "ETHBEAR", "BULL", "BEAR"] {
            assert!(is_leveraged_token(token, &known), "{token}");
        }
        let without_parents: std::collections::BTreeSet<String> =
            ["BTC", "ETH"].iter().map(|s| (*s).to_owned()).collect();
        for coin in ["JUP", "SYRUP", "BTC", "SETUP"] {
            assert!(!is_leveraged_token(coin, &without_parents), "{coin}");
        }
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

#[cfg(test)]
mod book_tests {
    use quorum_core::recorder::BookFeed as _;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn depth_snapshots_parse_both_sides_best_first() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/depth"))
            .and(query_param("symbol", "BTCUSDT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "lastUpdateId": 1,
                "bids": [["99.9", "1.5"], ["99.0", "2"]],
                "asks": [["100.1", "0.5"], ["101.0", "3"]]
            })))
            .mount(&server)
            .await;
        let spot = BinanceSpot::with_base_url(server.uri()).expect("client");
        let book = spot.book("BTCUSDT").await.expect("book");
        assert_eq!(book.bids, vec![(99.9, 1.5), (99.0, 2.0)]);
        assert_eq!(book.asks[1], (101.0, 3.0));
    }

    #[tokio::test]
    async fn a_shallow_book_is_fetched_again_deeper() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/depth"))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "bids": [["99.99", "1"]], "asks": [["100.01", "1"]]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/depth"))
            .and(query_param("limit", "5000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "bids": [["99.99", "1"], ["97.0", "5"]], "asks": [["100.01", "1"], ["103.0", "5"]]
            })))
            .mount(&server)
            .await;
        let spot = BinanceSpot::with_base_url(server.uri()).expect("client");
        let book = spot.book("BTCUSDT").await.expect("book");
        assert_eq!(book.bids.len(), 2, "the deeper snapshot");
    }

    #[tokio::test]
    async fn malformed_or_failed_depth_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/depth"))
            .and(query_param("symbol", "BAD"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "bids": [["x", "1"]], "asks": [] })),
            )
            .mount(&server)
            .await;
        let spot = BinanceSpot::with_base_url(server.uri()).expect("client");
        assert!(spot.book("BAD").await.is_err());
        assert!(spot.book("MISSING").await.is_err(), "404 from the mock");
    }
}
