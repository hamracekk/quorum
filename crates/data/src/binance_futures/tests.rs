use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use quorum_core::recorder::DerivsFeed as _;

use super::*;

#[tokio::test]
async fn funding_history_is_read_page_by_page() {
    let server = MockServer::start().await;
    let row = |ms: i64, rate: &str| json!({ "symbol": "BTCUSDT", "fundingTime": ms, "fundingRate": rate });
    let full: Vec<Value> = (0..1_000).map(|i| row(i * 1_000, "0.0001")).collect();
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .and(query_param("startTime", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(full))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/fundingRate"))
        .and(query_param("startTime", "999001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![row(1_000_000, "-0.00025")]))
        .mount(&server)
        .await;
    let futures = BinanceFutures::with_base_url(server.uri()).expect("client");
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    let end = DateTime::from_timestamp(2_000, 0).expect("ts");
    let points = futures
        .funding_history("BTCUSDT", start, end)
        .await
        .expect("history");
    assert_eq!(points.len(), 1_001);
    assert!((points[1_000].rate + 0.000_25).abs() < 1e-12);
    assert_eq!(points[1_000].ts.timestamp_millis(), 1_000_000);
}

fn premium(symbol: &str, mark: &str, index: &str) -> Value {
    json!({
        "symbol": symbol, "markPrice": mark, "indexPrice": index,
        "estimatedSettlePrice": index, "lastFundingRate": "0.00010000",
        "interestRate": "0.00010000", "nextFundingTime": 1_760_544_000_000_i64,
        "time": 1_760_529_600_000_i64
    })
}

#[tokio::test]
async fn derivs_snapshots_carry_basis_funding_and_open_interest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/premiumIndex"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![
            premium("BTCUSDT", "101.0", "100.0"),
            premium("ETHUSDT", "10.0", "10.0"),
            premium("XYZUSDT", "1.0", "1.0"),
        ]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/openInterest"))
        .and(query_param("symbol", "BTCUSDT"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "symbol": "BTCUSDT", "openInterest": "2.5", "time": 1 })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/openInterest"))
        .and(query_param("symbol", "ETHUSDT"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let futures = BinanceFutures::with_base_url(server.uri()).expect("client");
    let now = DateTime::from_timestamp(1_760_530_000, 0).expect("ts");
    let symbols = vec!["BTCUSDT".to_owned(), "ETHUSDT".to_owned()];
    let snapshots = futures.snapshot(&symbols, now).await.expect("snapshot");

    assert_eq!(snapshots.len(), 2, "only the requested symbols");
    let btc = &snapshots[0];
    assert_eq!(btc.symbol, "BTCUSDT");
    assert_eq!(btc.known_time, now);
    assert_eq!(btc.event_time.timestamp_millis(), 1_760_529_600_000);
    assert!((btc.basis - 0.01).abs() < 1e-12);
    assert!((btc.funding_rate - 0.0001).abs() < 1e-12);
    assert!((btc.open_interest_usd.expect("oi") - 252.5).abs() < 1e-9);
    assert_eq!(
        snapshots[1].open_interest, None,
        "a failed open-interest call leaves it empty"
    );
}

#[tokio::test]
async fn a_failed_premium_index_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fapi/v1/premiumIndex"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let futures = BinanceFutures::with_base_url(server.uri()).expect("client");
    let now = DateTime::from_timestamp(0, 0).expect("ts");
    assert!(
        futures
            .snapshot(&["BTCUSDT".to_owned()], now)
            .await
            .is_err()
    );
}
