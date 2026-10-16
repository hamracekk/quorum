use chrono::TimeZone;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// A token-shaped dummy key.
const TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

fn nvda() -> InstrumentId {
    "tiingo:NVDA:equity".parse().expect("instrument")
}

fn day(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0)
        .single()
        .expect("date")
}

fn row(date: &str, close: f64) -> serde_json::Value {
    json!({
        "date": format!("{date}T00:00:00.000Z"),
        "open": close * 10.0, "high": 0.0, "low": 0.0, "close": close * 10.0, "volume": 5,
        "adjOpen": close - 1.0, "adjHigh": close + 1.0, "adjLow": close - 2.0,
        "adjClose": close, "adjVolume": 1000.0, "divCash": 0.0, "splitFactor": 1.0
    })
}

#[tokio::test]
async fn daily_bars_are_adjusted_keyed_by_date_and_sent_with_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tiingo/daily/NVDA/prices"))
        .and(header(
            "Authorization",
            "Token 0123456789abcdef0123456789abcdef01234567",
        ))
        .and(query_param("startDate", "2024-01-02"))
        .and(query_param("endDate", "2024-01-04"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            row("2024-01-03", 48.0),
            row("2024-01-02", 47.0),
            row("2024-01-03", 48.0),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let tiingo = TiingoDaily::with_base_url(server.uri(), Some(TOKEN.into())).expect("client");
    let bars = tiingo
        .bars(&nvda(), "1d", day(2024, 1, 2), day(2024, 1, 5))
        .await
        .expect("bars");
    assert_eq!(bars.len(), 2, "sorted, duplicates dropped: {bars:?}");
    assert_eq!(bars[0].ts, day(2024, 1, 2));
    assert!(
        (bars[0].close - 47.0).abs() < 1e-12,
        "adjusted close, not the raw one"
    );
    assert!((bars[0].open - 46.0).abs() < 1e-12 && (bars[0].volume - 1000.0).abs() < 1e-12);
}

#[tokio::test]
async fn a_day_not_yet_over_is_never_returned() {
    let server = MockServer::start().await;
    let today = Utc::now().date_naive();
    let yesterday = today.pred_opt().expect("yesterday");
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            row(&yesterday.to_string(), 10.0),
            row(&today.to_string(), 11.0),
        ])))
        .mount(&server)
        .await;
    let tiingo = TiingoDaily::with_base_url(server.uri(), Some(TOKEN.into())).expect("client");
    let start = yesterday.and_time(chrono::NaiveTime::MIN).and_utc();
    let bars = tiingo
        .bars(&nvda(), "1d", start, Utc::now() + TimeDelta::days(2))
        .await
        .expect("bars");
    assert_eq!(bars.len(), 1, "today's bar is still forming: {bars:?}");
    assert_eq!(bars[0].ts, start);
}

#[tokio::test]
async fn refuses_other_venues_kinds_and_intervals_without_network() {
    let tiingo =
        TiingoDaily::with_base_url("http://127.0.0.1:9", Some(TOKEN.into())).expect("client");
    let (a, b) = (day(2024, 1, 1), day(2024, 2, 1));
    let coin = InstrumentId::spot("binance", "BTCUSDT").expect("instrument");
    let spot: InstrumentId = "tiingo:NVDA".parse().expect("instrument");
    for instrument in [coin, spot] {
        assert!(matches!(
            tiingo.bars(&instrument, "1d", a, b).await,
            Err(PortError::Unsupported(_))
        ));
    }
    assert!(matches!(
        tiingo.bars(&nvda(), "1h", a, b).await,
        Err(PortError::Unsupported(_))
    ));
}

#[tokio::test]
async fn without_a_key_it_says_how_to_set_one_and_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let tiingo = TiingoDaily::with_base_url(server.uri(), Some("  ".into())).expect("client");
    let err = tiingo
        .bars(&nvda(), "1d", day(2024, 1, 1), day(2024, 2, 1))
        .await
        .expect_err("no key");
    assert!(
        format!("{err:#}").contains("trader secrets set tiingo"),
        "{err:#}"
    );
    assert!(tiingo.check().await.is_err());
}

#[tokio::test]
async fn http_errors_and_malformed_rows_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tiingo/daily/NVDA/prices"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/tiingo/daily/AMD/prices"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"date": "2024-01-02", "adjOpen": 1.0}])),
        )
        .mount(&server)
        .await;
    let tiingo = TiingoDaily::with_base_url(server.uri(), Some(TOKEN.into())).expect("client");
    let (a, b) = (day(2024, 1, 1), day(2024, 2, 1));
    assert!(tiingo.bars(&nvda(), "1d", a, b).await.is_err(), "401");
    let amd: InstrumentId = "tiingo:AMD:equity".parse().expect("instrument");
    assert!(
        tiingo.bars(&amd, "1d", a, b).await.is_err(),
        "missing prices"
    );
}

#[test]
fn the_key_never_shows_in_debug_output() {
    let tiingo = TiingoDaily::with_base_url("http://x", Some(TOKEN.into())).expect("client");
    let text = format!("{tiingo:?}");
    assert!(!text.contains(TOKEN) && text.contains("<set>"), "{text}");
}

#[tokio::test]
async fn an_empty_range_asks_for_nothing() {
    let tiingo =
        TiingoDaily::with_base_url("http://127.0.0.1:9", Some(TOKEN.into())).expect("client");
    let bars = tiingo
        .bars(&nvda(), "1d", day(2024, 1, 2), day(2024, 1, 2))
        .await
        .expect("empty");
    assert!(bars.is_empty());
}

#[tokio::test]
async fn another_services_key_is_never_sent_to_tiingo() {
    // Regression (2026-10-14): `trader secrets set tiingo` asked for "the Anthropic API key",
    // so one was stored as the Tiingo key and sent as its token.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .expect(0)
        .mount(&server)
        .await;
    let anthropic_like = format!("sk-ant-api03-{}", "x".repeat(90));
    let tiingo = TiingoDaily::with_base_url(server.uri(), Some(anthropic_like)).expect("client");
    let err = tiingo
        .bars(&nvda(), "1d", day(2024, 1, 1), day(2024, 2, 1))
        .await
        .expect_err("refused");
    assert!(format!("{err:#}").contains("nothing was sent"), "{err:#}");
    assert!(tiingo.check().await.is_err());
}

#[test]
fn the_next_bar_is_ready_once_the_next_weekday_is_over() {
    // Thursday's bar → Friday's, ready Saturday 00:00 UTC.
    assert_eq!(next_bar_ready(day(2026, 10, 15)), day(2026, 10, 17));
    // Friday's bar → Monday's, ready Tuesday 00:00 UTC (the weekend has none).
    assert_eq!(next_bar_ready(day(2026, 10, 16)), day(2026, 10, 20));
}

#[tokio::test]
async fn the_stream_yields_each_days_bars_in_time_and_subscription_order() {
    use futures_util::StreamExt as _;
    use quorum_core::MarketStream as _;

    let server = MockServer::start().await;
    // Two weekdays well inside the stream's first look-back window.
    let today = Utc::now().date_naive();
    let mut days = Vec::new();
    let mut d = today - TimeDelta::days(1);
    while days.len() < 2 {
        if !matches!(
            chrono::Datelike::weekday(&d),
            chrono::Weekday::Sat | chrono::Weekday::Sun
        ) {
            days.push(d);
        }
        d -= TimeDelta::days(1);
    }
    days.reverse();
    for (ticker, base) in [("SPY", 500.0), ("TLT", 90.0)] {
        Mock::given(method("GET"))
            .and(path(format!("/tiingo/daily/{ticker}/prices")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                row(&days[0].to_string(), base),
                row(&days[1].to_string(), base + 1.0),
            ])))
            .mount(&server)
            .await;
    }
    let tiingo = TiingoDaily::with_base_url(server.uri(), Some(TOKEN.into())).expect("client");
    let stream =
        TiingoStream::new(std::sync::Arc::new(tiingo)).with_poll(Duration::from_millis(50));
    let spy: InstrumentId = "tiingo:SPY:equity".parse().expect("instrument");
    let tlt: InstrumentId = "tiingo:TLT:equity".parse().expect("instrument");
    let mut bars = stream
        .subscribe(&[spy.clone(), tlt.clone()], "1d")
        .await
        .expect("stream");
    let mut got = Vec::new();
    for _ in 0..4 {
        let (instrument, bar) = bars.next().await.expect("item").expect("bar");
        got.push((instrument, bar.ts.date_naive()));
    }
    assert_eq!(
        got,
        [
            (spy.clone(), days[0]),
            (tlt.clone(), days[0]),
            (spy, days[1]),
            (tlt, days[1]),
        ]
    );
    assert!(stream.subscribe(&[], "1d").await.is_err());
}
