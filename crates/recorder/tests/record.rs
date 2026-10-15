//! Recording runs on fakes: what lands in the files, markers for every gap, stop and lock.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, TimeZone, Utc};
use quorum_core::VenueId;
use quorum_core::fakes::{MemoryBookFeed, MemoryDerivsFeed, MemoryLiquidationFeed};
use quorum_core::recorder::{
    BookSnapshot, DerivsSnapshot, Feed, Liquidation, LiquidationEvent, MarkerState, Record,
};
use quorum_recorder::{Feeds, RecordStore, RecorderConfig, Status, record};

const CONFIG: &str = r#"
symbols = ["BTCUSDT", "ETHUSDT"]
book_every_secs = 3600
derivs_every_secs = 3600
liquidations = true
"#;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0)
        .single()
        .expect("valid")
}

fn venue() -> VenueId {
    VenueId::new("binance").expect("valid")
}

fn store(name: &str) -> RecordStore {
    let dir = std::env::temp_dir().join(format!("quorum-recorder-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir); // a leftover from an earlier run, if any
    RecordStore::new(dir)
}

fn liquidation(symbol: &str) -> LiquidationEvent {
    LiquidationEvent::Liquidation(Liquidation {
        known_time: now(),
        event_time: now(),
        venue: venue(),
        symbol: symbol.into(),
        side: "sell".into(),
        price: 100.0,
        avg_price: 100.0,
        qty: 1.0,
    })
}

fn feeds(liquidations: MemoryLiquidationFeed) -> Feeds {
    let book = BookSnapshot {
        event_time: now(),
        bids: vec![(99.0, 1.0), (97.0, 1.0)],
        asks: vec![(101.0, 1.0), (103.0, 1.0)],
    };
    let derivs = DerivsSnapshot {
        known_time: now(),
        event_time: now(),
        venue: venue(),
        symbol: "BTCUSDT".into(),
        mark: 100.0,
        index: 100.0,
        basis: 0.0,
        funding_rate: 0.0001,
        next_funding: now(),
        open_interest: Some(1.0),
        open_interest_usd: Some(100.0),
    };
    Feeds {
        // ETHUSDT has no book: its poll fails and gets an error marker.
        book: Arc::new(MemoryBookFeed::new(venue()).with_book("BTCUSDT", book)),
        derivs: Arc::new(MemoryDerivsFeed::new(venue(), vec![derivs])),
        liquidations: Arc::new(liquidations),
    }
}

fn states(records: &[Record]) -> Vec<MarkerState> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::Marker(m) => Some(m.state),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn every_feed_is_recorded_with_its_gaps_marked() {
    let store = store("all");
    let events = vec![
        LiquidationEvent::Up,
        liquidation("BTCUSDT"),
        LiquidationEvent::Down("connection reset".into()),
        LiquidationEvent::Up,
        liquidation("SOLUSDT"),
    ];
    let config = RecorderConfig::parse(CONFIG).expect("valid");
    let symbols = config.symbols.clone();
    let (status, outcome) = record(
        &feeds(MemoryLiquidationFeed::new(venue(), events)),
        &symbols,
        &config,
        &store,
        &now,
        std::future::pending(),
    )
    .await
    .expect("record");

    assert_eq!(status, Status::StreamEnded);
    assert!(status.restart());
    assert_eq!(
        (outcome.liquidations, outcome.books, outcome.derivs),
        (2, 1, 1)
    );
    assert_eq!(outcome.errors, 1, "ETHUSDT's book failed");

    let day = now().date_naive();
    let liqs = store.read(Feed::Liquidations, day).expect("read");
    let (start, up, down, stop) = (
        MarkerState::Start,
        MarkerState::Up,
        MarkerState::Down,
        MarkerState::Stop,
    );
    assert_eq!(states(&liqs), vec![start, up, down, up, down, stop]);
    assert_eq!(
        liqs.iter()
            .filter(|r| matches!(r, Record::Liquidation(_)))
            .count(),
        2
    );

    let books = store.read(Feed::Book, day).expect("read");
    assert_eq!(states(&books), vec![start, MarkerState::Error, stop]);
    let Some(Record::Book(btc)) = books.iter().find(|r| matches!(r, Record::Book(_))) else {
        panic!("a BTCUSDT book summary");
    };
    assert!((btc.mid - 100.0).abs() < 1e-9);
    assert!((btc.spread_bps - 200.0).abs() < 1e-9);

    let derivs = store.read(Feed::Derivs, day).expect("read");
    assert_eq!(
        derivs
            .iter()
            .filter(|r| matches!(r, Record::Derivs(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_stop_closes_every_feed_with_a_marker() {
    let store = store("stop");
    let config = RecorderConfig::parse(CONFIG).expect("valid");
    let feeds = feeds(MemoryLiquidationFeed::new(venue(), vec![LiquidationEvent::Up]).keep_open());
    let (status, _) = record(
        &feeds,
        &config.symbols,
        &config,
        &store,
        &now,
        tokio::time::sleep(StdDuration::from_millis(50)),
    )
    .await
    .expect("record");
    assert_eq!(status, Status::Stopped);
    assert!(!status.restart());
    for feed in [Feed::Liquidations, Feed::Book, Feed::Derivs] {
        let records = store.read(feed, now().date_naive()).expect("read");
        assert_eq!(
            states(&records).last(),
            Some(&MarkerState::Stop),
            "{feed:?}"
        );
    }
}

#[tokio::test]
async fn feeds_switched_off_are_not_recorded() {
    let store = store("off");
    let config = RecorderConfig::parse(
        &CONFIG
            .replace("book_every_secs = 3600", "book_every_secs = 0")
            .replace("liquidations = true", "liquidations = false"),
    )
    .expect("valid");
    let feeds = feeds(MemoryLiquidationFeed::new(venue(), Vec::new()));
    record(
        &feeds,
        &config.symbols,
        &config,
        &store,
        &now,
        tokio::time::sleep(StdDuration::from_millis(50)),
    )
    .await
    .expect("record");
    let day = now().date_naive();
    assert!(store.read(Feed::Book, day).expect("read").is_empty());
    assert!(
        store
            .read(Feed::Liquidations, day)
            .expect("read")
            .is_empty()
    );
    assert!(!store.read(Feed::Derivs, day).expect("read").is_empty());
}

#[tokio::test]
async fn a_second_recorder_on_the_same_data_is_refused() {
    let store = store("lock");
    let held = store.lock().expect("first recorder");
    let config = RecorderConfig::parse(CONFIG).expect("valid");
    let feeds = feeds(MemoryLiquidationFeed::new(venue(), Vec::new()));
    let err = record(
        &feeds,
        &config.symbols,
        &config,
        &store,
        &now,
        std::future::pending(),
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("another recorder"), "{err}");
    drop(held);
}
