use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn credentials() -> Credentials {
    Credentials {
        key: "test-key".into(),
        secret: "test-secret".into(),
    }
}

#[test]
fn signatures_match_binances_documented_example() {
    // From Binance's "SIGNED endpoint examples" documentation.
    let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
    let query = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1\
                 &recvWindow=5000&timestamp=1499827319559";
    assert_eq!(
        sign(secret, query),
        "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
    );
}

#[test]
fn only_the_testnet_or_a_loopback_mock_is_allowed() {
    for ok in [
        "https://testnet.binance.vision",
        "https://testnet.binance.vision/",
        "http://127.0.0.1:4242",
        "http://localhost:8080",
    ] {
        assert!(allowed_url(ok), "{ok}");
    }
    for refused in [
        "https://api.binance.com",
        "http://testnet.binance.vision",
        "https://testnet.binance.vision.evil.com",
        "https://evil.com@testnet.binance.vision",
        "https://127.0.0.1:4242",
        "https://api1.binance.com",
        "testnet.binance.vision",
    ] {
        assert!(!allowed_url(refused), "{refused}");
    }
    let err = BinanceTestnet::with_base_url("https://api.binance.com", credentials())
        .expect_err("live host refused")
        .to_string();
    assert!(err.contains("testnet.binance.vision only"), "{err}");
    assert!(!format!("{:?}", credentials()).contains("secret\""));
    assert!(!format!("{:?}", credentials()).contains("test-secret"));
}

fn btc_rules() -> SymbolRules {
    SymbolRules {
        symbol: "BTCUSDT".into(),
        base_asset: "BTC".into(),
        quote_asset: "USDT".into(),
        step_size: 0.000_01,
        min_qty: 0.000_01,
        min_notional: 5.0,
    }
}

#[test]
fn quantities_round_down_to_the_step_and_print_its_decimals() {
    let rules = btc_rules();
    assert!((rules.floor_qty(0.012_345_67) - 0.012_34).abs() < 1e-12);
    assert_eq!(rules.format_qty(0.012_34), "0.01234");
    let tenth = SymbolRules {
        step_size: 0.1,
        ..btc_rules()
    };
    assert!((tenth.floor_qty(0.3) - 0.3).abs() < 1e-12);
    assert_eq!(tenth.format_qty(0.3), "0.3");
    assert!(tenth.floor_qty(-1.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn account_rules_and_orders_are_read_from_the_exchange() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/account"))
        .and(header("X-MBX-APIKEY", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "canTrade": true, "canWithdraw": false,
            "balances": [
                { "asset": "USDT", "free": "900.5", "locked": "100" },
                { "asset": "BTC", "free": "0.0", "locked": "0.0" }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/exchangeInfo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "symbols": [{
                "symbol": "BTCUSDT", "baseAsset": "BTC", "quoteAsset": "USDT",
                "filters": [
                    { "filterType": "PRICE_FILTER", "tickSize": "0.01" },
                    { "filterType": "LOT_SIZE", "minQty": "0.00001000", "stepSize": "0.00001000" },
                    { "filterType": "NOTIONAL", "minNotional": "5.00000000" }
                ]
            }]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/order"))
        .and(query_param("type", "MARKET"))
        .and(query_param("newClientOrderId", "t7-1-BTCUSDT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "orderId": 7, "clientOrderId": "t7-1-BTCUSDT", "status": "FILLED",
            "executedQty": "0.01000000", "cummulativeQuoteQty": "800.00000000",
            "fills": [
                { "price": "80000", "qty": "0.006", "commission": "0.000006", "commissionAsset": "BTC" },
                { "price": "80000", "qty": "0.004", "commission": "0.000004", "commissionAsset": "BTC" }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/order"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({ "code": -2013, "msg": "Order does not exist." })),
        )
        .mount(&server)
        .await;

    let client = BinanceTestnet::with_base_url(server.uri(), credentials()).expect("client");
    let account = client.account().await.expect("account");
    assert!(account.can_trade && !account.can_withdraw);
    assert_eq!(account.balances.get("USDT"), Some(&1000.5));
    assert!(
        !account.balances.contains_key("BTC"),
        "empty balances dropped"
    );

    let rules = client.rules(&["BTCUSDT".into()]).await.expect("rules");
    assert_eq!(rules, vec![btc_rules()]);

    let order = client
        .market_order("BTCUSDT", OrderSide::Buy, "0.01000", "t7-1-BTCUSDT")
        .await
        .expect("order");
    assert_eq!(order.status, "FILLED");
    assert_eq!(order.avg_price(), Some(80_000.0));
    assert!((order.fees["BTC"] - 0.000_01).abs() < 1e-12);

    let requests = server.received_requests().await.expect("requests");
    let signed = requests
        .iter()
        .find(|r| r.url.path() == "/api/v3/order")
        .expect("order request");
    let query = signed.url.query().expect("query");
    let (unsigned, signature) = query.rsplit_once("&signature=").expect("signed");
    assert_eq!(signature, sign("test-secret", unsigned));

    assert_eq!(client.order("BTCUSDT", "nope").await.expect("lookup"), None);
}
