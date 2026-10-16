//! The knowledge plane on fakes and an in-memory journal: fetch and tag, the two tiers, budgets,
//! the cache, and impact-map implications.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::DateTime;
use quorum_core::fakes::{MemoryLlm, MemoryNewsSource};
use quorum_core::news::{Direction, NewsItem, NewsQuery};
use quorum_core::{InstrumentId, LlmProvider, NewsSource, NewsStore};
use quorum_journal::SqliteJournal;
use quorum_knowledge::{LLMs, NewsConfig, extract, fetch, final_readings, load_impact, tag};

const BULLISH_HIGH: &str = r#"{"relevant": true, "events": [{"asset": "BTC", "direction": "bullish",
    "magnitude": "high", "confidence": 0.7, "horizon": "days", "event_type": "etf",
    "rationale": "Record ETF inflows."}]}"#;
const BEARISH_LOW: &str = r#"{"relevant": true, "events": [{"asset": "BTC", "direction": "bearish",
    "magnitude": "low", "confidence": 0.6, "horizon": "hours", "event_type": "flows",
    "rationale": "Inflows already priced in."}]}"#;
const OIL_UP: &str = r#"{"themes": [{"theme": "oil-prices", "direction": "up", "magnitude": "high",
    "confidence": 0.8, "rationale": "OPEC cut output."}]}"#;

fn config() -> NewsConfig {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/news.toml"),
    )
    .expect("news.toml");
    NewsConfig::parse(&text).expect("valid")
}

fn instruments() -> Vec<InstrumentId> {
    [
        "binance:BTCUSDT",
        "binance:ETHUSDT",
        "tiingo:USO:equity",
        "tiingo:SPY:equity",
    ]
    .iter()
    .map(|s| s.parse().expect("valid"))
    .collect()
}

fn item(i: usize, title: &str) -> NewsItem {
    NewsItem {
        url: format!("https://news.example.com/{i}"),
        source: "test".into(),
        title: title.into(),
        summary: String::new(),
        published_at: DateTime::from_timestamp(
            1_790_000_000 + i64::try_from(i).expect("small") * 60,
            0,
        )
        .expect("ts"),
        fetched_at: DateTime::from_timestamp(1_790_000_000, 0).expect("ts"),
        instruments: vec![],
    }
}

async fn seeded(count: usize) -> SqliteJournal {
    let store = SqliteJournal::in_memory().await.expect("journal");
    let items: Vec<NewsItem> = (0..count)
        .map(|i| item(i, &format!("Story {i} about bitcoin")))
        .collect();
    store.store_news(&items).await.expect("seed");
    store
}

fn llms(first: Option<Arc<MemoryLlm>>, escalation: Option<Arc<MemoryLlm>>) -> LLMs {
    LLMs {
        first_pass: first.map(|l| l as Arc<dyn LlmProvider>),
        escalation: escalation.map(|l| l as Arc<dyn LlmProvider>),
    }
}

#[test]
fn tagging_matches_tickers_names_and_aliases_as_whole_words() {
    let aliases: BTreeMap<String, Vec<String>> = [(
        "USO".to_owned(),
        vec!["crude".to_owned(), "OPEC".to_owned()],
    )]
    .into();
    let tags = |title: &str| -> Vec<String> {
        tag(&item(0, title), &instruments(), &aliases)
            .iter()
            .map(|i| i.symbol().to_owned())
            .collect()
    };
    assert_eq!(
        tags("Bitcoin rallies as ETH lags"),
        vec!["BTCUSDT", "ETHUSDT"]
    );
    assert_eq!(tags("OPEC+ cuts output; crude jumps"), vec!["USO"]);
    assert_eq!(tags("S&P futures and SPY options"), vec!["SPY"]);
    assert!(tags("Ethernet cables").is_empty(), "whole words only");
}

#[tokio::test]
async fn fetch_stores_tagged_items_and_survives_a_failing_source() {
    let store = SqliteJournal::in_memory().await.expect("journal");
    let sources: Vec<Arc<dyn NewsSource>> = vec![
        Arc::new(MemoryNewsSource::failing("down")),
        Arc::new(MemoryNewsSource::new(
            "up",
            vec![item(1, "Bitcoin ETF inflows"), item(2, "Weather")],
        )),
    ];
    let reports = fetch(&sources, &store, &instruments(), &BTreeMap::new())
        .await
        .expect("fetch");
    assert!(reports[0].error.is_some());
    assert_eq!((reports[1].fetched, reports[1].inserted), (2, 2));
    let stored = store
        .news(&NewsQuery {
            limit: 10,
            ..NewsQuery::default()
        })
        .await
        .expect("news");
    let tagged = stored
        .iter()
        .find(|i| i.title.contains("Bitcoin"))
        .expect("bitcoin item");
    assert_eq!(tagged.instruments.len(), 1);
}

#[tokio::test]
async fn the_first_pass_reads_everything_and_escalation_only_what_matters() {
    let store = seeded(3).await;
    let quiet_first = Arc::new(MemoryLlm::answering("haiku", BEARISH_LOW, 100));
    let escalation = Arc::new(MemoryLlm::answering("opus", BULLISH_HIGH, 1_000));
    let map = quorum_core::impact::ImpactMap::default();
    let report = extract(
        &config(),
        &store,
        &llms(Some(quiet_first.clone()), Some(escalation.clone())),
        &instruments(),
        &map,
    )
    .await
    .expect("run");
    assert_eq!(report.first_pass.attempted, 3);
    assert_eq!(
        escalation.calls(),
        0,
        "low-magnitude readings stay on the first pass"
    );

    let store = seeded(3).await;
    let loud_first = Arc::new(MemoryLlm::answering("haiku", BULLISH_HIGH, 100));
    let report = extract(
        &config(),
        &store,
        &llms(Some(loud_first), Some(escalation.clone())),
        &instruments(),
        &map,
    )
    .await
    .expect("run");
    assert_eq!(
        report.escalation.attempted, 3,
        "high-magnitude readings escalate"
    );
    assert!(
        report.events.iter().all(|e| e.model.starts_with("opus/")),
        "escalation supersedes"
    );
    assert_eq!(report.events[0].direction, Direction::Bullish);
}

#[tokio::test]
async fn an_item_is_never_read_twice_by_the_same_tier() {
    let store = seeded(2).await;
    let first = Arc::new(MemoryLlm::answering("haiku", BEARISH_LOW, 10));
    let map = quorum_core::impact::ImpactMap::default();
    let cfg = config();
    extract(
        &cfg,
        &store,
        &llms(Some(first.clone()), None),
        &instruments(),
        &map,
    )
    .await
    .expect("first run");
    let again = extract(
        &cfg,
        &store,
        &llms(Some(first.clone()), None),
        &instruments(),
        &map,
    )
    .await
    .expect("second run");
    assert_eq!(again.first_pass.attempted, 0);
    assert_eq!(first.calls(), 2);
}

#[tokio::test]
async fn each_tier_stops_at_its_daily_budget() {
    let store = seeded(5).await;
    let mut cfg = config();
    cfg.first_pass.daily_tokens = 250; // two calls of 100 + 100 tokens each fit… then stop
    let first = Arc::new(MemoryLlm::answering("haiku", BEARISH_LOW, 100));
    let map = quorum_core::impact::ImpactMap::default();
    let report = extract(&cfg, &store, &llms(Some(first), None), &instruments(), &map)
        .await
        .expect("run");
    assert!(report.first_pass.attempted < 5, "{:?}", report.first_pass);
    assert!(report.first_pass.skipped_for_budget > 0);
}

#[tokio::test]
async fn themes_expand_into_moves_across_markets() {
    let store = seeded(1).await;
    let map = load_impact(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/impact.toml"))
        .expect("map");
    let first = Arc::new(
        MemoryLlm::answering("haiku", BEARISH_LOW, 10).or_when("which of these themes", OIL_UP),
    );
    let report = extract(
        &config(),
        &store,
        &llms(Some(first), None),
        &instruments(),
        &map,
    )
    .await
    .expect("run");
    assert_eq!(report.themes.ok, 1, "{:?}", report.warnings);
    let moved: Vec<String> = report
        .implications
        .iter()
        .map(|e| e.instrument.to_string())
        .collect();
    assert!(moved.contains(&"tiingo:USO:equity".to_owned()), "{moved:?}");
    assert!(
        moved.contains(&"binance:BTCUSDT".to_owned()),
        "oil links reach crypto too: {moved:?}"
    );
}

#[tokio::test]
async fn the_escalation_reading_counts_over_the_first_pass() {
    let store = seeded(1).await;
    let map = quorum_core::impact::ImpactMap::default();
    extract(
        &config(),
        &store,
        &llms(
            Some(Arc::new(MemoryLlm::answering("haiku", BULLISH_HIGH, 10))),
            Some(Arc::new(MemoryLlm::answering("opus", BEARISH_LOW, 10))),
        ),
        &instruments(),
        &map,
    )
    .await
    .expect("run");
    let urls = vec!["https://news.example.com/0".to_owned()];
    let all = store
        .extractions(&urls, quorum_ai::extract::PROMPT_VERSION)
        .await
        .expect("readings");
    let chosen = final_readings(all);
    assert!(
        chosen["https://news.example.com/0"]
            .model
            .starts_with("opus/")
    );
}
