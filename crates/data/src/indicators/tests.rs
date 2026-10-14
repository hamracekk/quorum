use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

#[tokio::test]
async fn dvol_pages_backwards_until_the_start() {
    let server = MockServer::start().await;
    // The first page (open-ended) points to an older one; the older one ends the history.
    Mock::given(method("GET"))
        .and(path("/api/v2/public/get_volatility_index_data"))
        .and(query_param("end_timestamp", "86400000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": { "data": [[0, 1.0, 1.0, 1.0, 50.0]], "continuation": null }
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/public/get_volatility_index_data"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": { "data": [[86_400_000, 1.0, 1.0, 1.0, 55.0]], "continuation": 86_400_000 }
        })))
        .with_priority(2)
        .mount(&server)
        .await;
    let sources = Indicators::with_hosts(server.uri(), server.uri(), server.uri()).expect("client");
    let points = sources
        .dvol("BTC", DateTime::from_timestamp(0, 0).expect("ts"))
        .await
        .expect("dvol");
    assert_eq!(points.len(), 2);
    assert!(points[0].0 < points[1].0, "oldest first");
    assert!((points[1].1 - 55.0).abs() < 1e-12);
}

#[tokio::test]
async fn stablecoin_supply_and_coinbase_closes_parse() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stablecoincharts/all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "date": "86400", "totalCirculatingUSD": { "peggedUSD": 2.0e9 } },
            { "date": "0", "totalCirculatingUSD": { "peggedUSD": 1.0e9 } }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/products/BTC-USD/candles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            [86_400, 1.0, 2.0, 1.5, 1.8, 10.0],
            [0, 1.0, 2.0, 1.5, 1.6, 10.0]
        ])))
        .mount(&server)
        .await;
    let sources = Indicators::with_hosts(server.uri(), server.uri(), server.uri()).expect("client");
    let supply = sources.stablecoin_supply().await.expect("supply");
    assert_eq!(supply.len(), 2);
    assert!(supply[0].1 < supply[1].1, "oldest first");
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    let closes = sources
        .coinbase_closes("BTC", start, start + chrono::Duration::days(2))
        .await
        .expect("closes");
    assert_eq!(closes.len(), 2);
    assert!((closes[0].1 - 1.6).abs() < 1e-12);
    let missing = sources
        .coinbase_closes("NOPE", start, start + chrono::Duration::days(2))
        .await
        .expect("an unlisted coin is not an error");
    assert!(missing.is_empty());
}
