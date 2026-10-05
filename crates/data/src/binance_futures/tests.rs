use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
