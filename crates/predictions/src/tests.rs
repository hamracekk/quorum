use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const TICKER: &str = "KXBTCMAXY-26DEC31-109999.99";

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/markets/{TICKER}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "market": {
            "ticker": TICKER,
            "event_ticker": "KXBTCMAXY-26DEC31",
            "title": "Will Bitcoin be above $109,999.99\u{0007} by Dec 31, 2026?",
            "status": "active",
            "close_time": "2027-01-01T04:59:00Z",
            "last_price_dollars": null,
            "yes_bid_dollars": "0.2200",
            "yes_ask_dollars": "0.2400"
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/events/KXBTCMAXY-26DEC31"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event": { "series_ticker": "KXBTCMAXY" }, "markets": []
        })))
        .mount(&server)
        .await;
    server
}

fn candle(ts: i64, last: Option<&str>, bid: &str, ask: &str) -> serde_json::Value {
    json!({
        "end_period_ts": ts,
        "price": { "close_dollars": last },
        "yes_bid": { "close_dollars": bid },
        "yes_ask": { "close_dollars": ask }
    })
}

#[tokio::test]
async fn a_market_reads_its_question_probability_and_link() {
    let server = server().await;
    let kalshi = KalshiSource::with_base(&server.uri()).expect("client");
    let market = kalshi
        .market(&TICKER.to_ascii_lowercase())
        .await
        .expect("market");
    assert_eq!(market.ticker, TICKER);
    assert_eq!(
        market.question,
        "Will Bitcoin be above $109,999.99 by Dec 31, 2026?"
    );
    assert!(
        (market.probability.expect("mid") - 0.23).abs() < 1e-12,
        "bid/ask mid"
    );
    assert!(market.active);
    assert_eq!(market.url, "https://kalshi.com/markets/kxbtcmaxy");
}

#[tokio::test]
async fn history_uses_trades_then_quotes_and_drops_empty_hours() {
    let server = server().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/series/KXBTCMAXY/markets/{TICKER}/candlesticks"
        )))
        .and(query_param("period_interval", "60"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "candlesticks": [
                candle(1_790_003_600, Some("0.2500"), "0.2400", "0.2600"),
                candle(1_790_000_000, None, "0.2000", "0.2200"),
                candle(1_790_007_200, None, "0.3000", "0.0000"),
                candle(1_790_010_800, Some("7.0"), "0.2000", "0.2200"),
            ]})),
        )
        .mount(&server)
        .await;
    let kalshi = KalshiSource::with_base(&server.uri()).expect("client");
    let start = DateTime::from_timestamp(1_789_990_000, 0).expect("ts");
    let end = DateTime::from_timestamp(1_790_020_000, 0).expect("ts");
    let points = kalshi.history(TICKER, start, end).await.expect("history");
    let values: Vec<(i64, f64)> = points
        .iter()
        .map(|p| (p.ts.timestamp(), (p.probability * 1e6).round() / 1e6))
        .collect();
    assert_eq!(
        values,
        [
            (1_790_000_000, 0.21),
            (1_790_003_600, 0.25),
            (1_790_010_800, 0.21)
        ],
        "sorted; a crossed quote is skipped; an out-of-range trade falls back to the mid"
    );
}

#[tokio::test]
async fn unknown_markets_and_bad_tickers_are_clear_errors() {
    let server = server().await;
    let kalshi = KalshiSource::with_base(&server.uri()).expect("client");
    let missing = kalshi.market("KXNOPE-1").await.expect_err("missing");
    assert!(missing.to_string().contains("no market"), "{missing}");
    assert!(kalshi.market("../markets").await.is_err());
}

#[test]
fn tickers_come_from_urls_or_text_and_are_validated() {
    assert_eq!(
        ticker("https://kalshi.com/markets/kxbtcmaxy/how-high/kxbtcmaxy-26dec31-109999.99?x=1")
            .expect("from url"),
        "KXBTCMAXY-26DEC31-109999.99"
    );
    assert_eq!(ticker(" kxbtcd ").expect("plain"), "KXBTCD");
    assert!(ticker("a b").is_err());
    assert_eq!(ticker("KX/../../etc").expect("last segment only"), "ETC");
    assert!(ticker("...").is_err());
    assert!(ticker("bad!").is_err());
    assert!(ticker("").is_err());
}
