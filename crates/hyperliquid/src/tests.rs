use serde_json::json;
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const ADDRESS: &str = "0x77375a8c9d13bf79afb2a87f1b0ac1dfd5f5bf66";

fn fill(time: i64, coin: &str, px: &str, sz: &str, dir: &str) -> Value {
    json!({ "coin": coin, "px": px, "sz": sz, "time": time, "dir": dir, "side": "B" })
}

#[tokio::test]
async fn an_account_reads_value_and_positions_largest_first() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "type": "clearinghouseState", "user": ADDRESS })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "marginSummary": { "accountValue": "3907244.5" },
            "assetPositions": [
                { "position": { "coin": "ETH", "szi": "10", "entryPx": "3000", "positionValue": "30000",
                  "unrealizedPnl": "-120.5", "leverage": { "type": "cross", "value": 5 } } },
                { "position": { "coin": "BTC", "szi": "-25.1552", "entryPx": "79550.5",
                  "positionValue": "2171645.9", "unrealizedPnl": "-170535.34",
                  "leverage": { "type": "cross", "value": 15 } } },
                { "position": { "coin": "BAD", "szi": "nan?", "positionValue": "1", "unrealizedPnl": "0" } }
            ]
        })))
        .mount(&server)
        .await;
    let source = HyperliquidSource::with_url(&server.uri()).expect("client");
    let account = source
        .account(&ADDRESS.to_ascii_uppercase().replace("0X", "0x"))
        .await
        .expect("account");
    assert_eq!(account.address, ADDRESS);
    assert!((account.account_value - 3_907_244.5).abs() < 1e-6);
    let coins: Vec<&str> = account.positions.iter().map(|p| p.coin.as_str()).collect();
    assert_eq!(coins, ["BTC", "ETH"], "largest first; unparseable skipped");
    assert!(account.positions[0].size < 0.0, "short");
    assert_eq!(account.positions[0].leverage, Some(15.0));
    assert!(account.url.ends_with(ADDRESS));
}

#[tokio::test]
async fn fills_page_through_and_mark_what_opens_a_position() {
    let server = MockServer::start().await;
    let first: Vec<Value> = (0..2_000)
        .map(|i| fill(1_790_000_000_000 + i, "BTC", "60000", "0.01", "Open Long"))
        .collect();
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({ "type": "userFillsByTime", "startTime": 1_789_999_000_000_i64 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(first))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({ "type": "userFillsByTime", "startTime": 1_790_000_002_000_i64 }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            fill(1_790_000_005_000, "ETH", "3000", "1", "Open Short"),
            fill(1_790_000_006_000, "ETH", "3000", "1", "Close Short"),
            fill(1_790_000_007_000, "SOL", "150", "2", "Long > Short"),
        ])))
        .mount(&server)
        .await;
    let source = HyperliquidSource::with_url(&server.uri()).expect("client");
    let start = DateTime::from_timestamp_millis(1_789_999_000_000).expect("ts");
    let end = DateTime::from_timestamp_millis(1_790_100_000_000).expect("ts");
    let fills = source.fills(ADDRESS, start, end).await.expect("fills");
    assert_eq!(fills.len(), 2_003, "both pages");
    let tail: Vec<(&str, Option<f64>)> = fills
        .iter()
        .rev()
        .take(3)
        .map(|f| (f.coin.as_str(), f.opens))
        .collect();
    assert_eq!(
        tail,
        [("SOL", Some(-1.0)), ("ETH", None), ("ETH", Some(-1.0))]
    );
}

#[test]
fn addresses_are_validated_and_directions_read() {
    assert_eq!(
        address(&format!(
            " {} ",
            ADDRESS.to_ascii_uppercase().replacen("0X", "0x", 1)
        ))
        .expect("valid"),
        ADDRESS
    );
    assert!(address("0x123").is_err());
    assert!(address("77375a8c9d13bf79afb2a87f1b0ac1dfd5f5bf66").is_err());
    assert!(address("0xZZ375a8c9d13bf79afb2a87f1b0ac1dfd5f5bf66").is_err());
    assert_eq!(opens("Open Long"), Some(1.0));
    assert_eq!(opens("Short > Long"), Some(1.0));
    assert_eq!(opens("Close Long"), None);
    assert_eq!(opens("Buy"), None);
}

#[tokio::test]
async fn rate_limits_are_retried_with_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "marginSummary": { "accountValue": "1" }, "assetPositions": []
        })))
        .mount(&server)
        .await;
    let source = HyperliquidSource::with_url(&server.uri()).expect("client");
    assert!(source.account(ADDRESS).await.is_ok());
}

#[tokio::test]
async fn the_leaderboard_reads_windows_and_skips_bad_rows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "leaderboardRows": [
            { "ethAddress": ADDRESS, "accountValue": "500000.5", "displayName": "whale\u{0007}",
              "windowPerformances": [
                ["day", { "pnl": "10", "roi": "0.001", "vlm": "1000" }],
                ["week", { "pnl": "100", "roi": "0.01", "vlm": "50000" }],
                ["month", { "pnl": "1000", "roi": "0.1", "vlm": "200000" }],
                ["allTime", { "pnl": "5000", "roi": "0.5", "vlm": "900000" }]
              ], "prize": 0 },
            { "ethAddress": "not-an-address", "accountValue": "1", "windowPerformances": [], "displayName": null }
        ]})))
        .mount(&server)
        .await;
    let source = HyperliquidSource::with_url(&server.uri())
        .expect("client")
        .with_leaderboard(&format!("{}/leaderboard", server.uri()));
    let rows = source.leaderboard().await.expect("leaderboard");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].display_name.as_deref(), Some("whale"));
    assert!((rows[0].month.roi - 0.1).abs() < 1e-12);
    assert!((rows[0].all_time.volume - 900_000.0).abs() < 1e-6);
}

#[test]
fn the_position_after_a_fill_follows_its_direction() {
    assert_eq!(position_after("Open Long", 0.0, 2.0), Some(2.0));
    assert_eq!(position_after("Close Long", 2.0, 2.0), Some(0.0));
    assert_eq!(position_after("Long > Short", 2.0, 3.0), Some(-1.0));
    assert_eq!(position_after("Close Short", -1.0, 1.0), Some(0.0));
    assert_eq!(position_after("Liquidated Isolated Long", 1.0, 1.0), None);
}
