use chrono::{DateTime, Utc};
use trader_core::InstrumentId;
use trader_core::news::{Direction, EventType, Horizon, Magnitude, NewsEvent};

use super::*;

fn at(hours: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_790_000_000 + hours * 3_600, 0).expect("valid timestamp")
}

fn btc() -> InstrumentId {
    InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
}

fn item(url: &str, source: &str, title: &str, hour: i64) -> NewsItem {
    NewsItem {
        url: format!("https://news.example.com/{url}"),
        source: source.into(),
        title: title.into(),
        summary: format!("{title}. More detail follows."),
        published_at: at(hour),
        fetched_at: at(hour + 1),
        instruments: vec![],
    }
}

fn query(limit: usize) -> NewsQuery {
    NewsQuery {
        limit,
        ..NewsQuery::default()
    }
}

#[tokio::test]
async fn duplicates_by_url_and_by_headline_window_are_skipped() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let mut tagged = item("a", "coindesk", "Bitcoin ETF sees record inflows", 0);
    tagged.instruments = vec![btc()];
    let stored = journal
        .store_news(&[
            tagged.clone(),
            item("b", "coindesk", "Solana upgrade ships", 1),
        ])
        .await
        .expect("store");
    assert_eq!(
        stored,
        StoredNews {
            inserted: 2,
            duplicates: 0
        }
    );

    let again = journal
        .store_news(&[
            tagged.clone(),
            item(
                "other-site",
                "decrypt",
                "Bitcoin ETF Sees Record Inflows!",
                10,
            ),
            item("c", "decrypt", "Bitcoin ETF sees record inflows", 72),
        ])
        .await
        .expect("store");
    assert_eq!(
        again,
        StoredNews {
            inserted: 1,
            duplicates: 2
        },
        "same URL and same headline within 48h are one story; 72h later it is news again"
    );

    let all = journal.news(&query(10)).await.expect("list");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].published_at, at(72), "newest first");
    let first = all.iter().find(|n| n.url.ends_with("/a")).expect("kept");
    assert_eq!(first, &tagged);
}

#[tokio::test]
async fn news_filters_by_source_instrument_time_and_words() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let mut tagged = item("a", "coindesk", "Bitcoin miners sell reserves", 0);
    tagged.instruments = vec![btc()];
    journal
        .store_news(&[
            tagged,
            item("b", "decrypt", "Regulators publish stablecoin rules", 5),
            item("c", "coindesk", "Ether gas fees fall", 10),
        ])
        .await
        .expect("store");

    let by_source = NewsQuery {
        source: Some("coindesk".into()),
        ..query(10)
    };
    assert_eq!(journal.news(&by_source).await.expect("list").len(), 2);
    let by_instrument = NewsQuery {
        instrument: Some(btc()),
        ..query(10)
    };
    let btc_news = journal.news(&by_instrument).await.expect("list");
    assert_eq!(btc_news.len(), 1);
    assert!(btc_news[0].title.contains("miners"));
    let recent = NewsQuery {
        since: Some(at(5)),
        ..query(10)
    };
    assert_eq!(journal.news(&recent).await.expect("list").len(), 2);
    let words = NewsQuery {
        text: Some("STABLECOIN".into()),
        ..query(10)
    };
    let hits = journal.news(&words).await.expect("search");
    assert_eq!(hits.len(), 1, "summaries and titles are searchable");
    let hostile = NewsQuery {
        text: Some("fees\" OR *".into()),
        ..query(10)
    };
    assert!(journal.news(&hostile).await.is_ok());
    assert_eq!(journal.news(&query(1)).await.expect("list").len(), 1);

    let stats = journal.news_stats().await.expect("stats");
    assert_eq!(stats.len(), 2);
    assert_eq!((stats[0].source.as_str(), stats[0].items), ("coindesk", 2));
    assert_eq!(stats[0].latest, Some(at(10)));
}

fn extraction(
    url: &str,
    tier: &str,
    status: ExtractionStatus,
    tokens: u64,
    hour: i64,
) -> Extraction {
    Extraction {
        url: format!("https://news.example.com/{url}"),
        prompt_version: "news-v1".into(),
        tier: tier.into(),
        model: format!("{tier}/model"),
        ts: at(hour),
        status,
        relevant: status == ExtractionStatus::Ok,
        events: if status == ExtractionStatus::Ok {
            vec![NewsEvent {
                url: format!("https://news.example.com/{url}"),
                instrument: btc(),
                direction: Direction::Bullish,
                magnitude: Magnitude::High,
                confidence: 0.8,
                horizon: Horizon::Days,
                event_type: EventType::Etf,
                rationale: "Record ETF inflows.".into(),
                published_at: at(0),
                model: format!("{tier}/model"),
                extracted_at: at(hour),
            }]
        } else {
            vec![]
        },
        error: (status != ExtractionStatus::Ok).then(|| "schema violation".into()),
        input_tokens: tokens,
        output_tokens: tokens / 10,
    }
}

#[tokio::test]
async fn extractions_are_cached_per_prompt_and_tier_and_count_tokens() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    journal
        .store_news(&[
            item("a", "coindesk", "Bitcoin ETF sees record inflows", 0),
            item("b", "coindesk", "Solana upgrade ships", 1),
        ])
        .await
        .expect("store");
    let pending = journal
        .unextracted("news-v1", "local", 10)
        .await
        .expect("pending");
    assert_eq!(pending.len(), 2);

    let local = extraction("a", "local", ExtractionStatus::Ok, 500, 2);
    journal.record_extraction(&local).await.expect("record");
    journal
        .record_extraction(&extraction(
            "b",
            "local",
            ExtractionStatus::Rejected,
            400,
            2,
        ))
        .await
        .expect("record");
    let remote = extraction("a", "remote", ExtractionStatus::Ok, 1_000, 3);
    journal.record_extraction(&remote).await.expect("record");

    assert!(
        journal
            .unextracted("news-v1", "local", 10)
            .await
            .expect("pending")
            .is_empty(),
        "attempted items, rejected ones included, are not sent again"
    );
    assert_eq!(
        journal
            .unextracted("news-v2", "local", 10)
            .await
            .expect("pending")
            .len(),
        2,
        "a new prompt version re-reads everything"
    );
    assert_eq!(
        journal
            .unextracted("news-v1", "remote", 10)
            .await
            .expect("pending")
            .len(),
        1
    );
    journal
        .record_extraction(&extraction("b", "remote", ExtractionStatus::Failed, 0, 4))
        .await
        .expect("record");
    assert_eq!(
        journal
            .unextracted("news-v1", "remote", 10)
            .await
            .expect("pending")
            .len(),
        1,
        "a failed attempt (provider down) is retried later"
    );

    let urls = vec![local.url.clone()];
    let stored = journal
        .extractions(&urls, "news-v1")
        .await
        .expect("extractions");
    assert_eq!(stored, vec![local.clone(), remote.clone()]);

    let usage = journal.token_usage(at(0)).await.expect("usage");
    assert_eq!(usage.local, 500 + 50 + 400 + 40);
    assert_eq!((usage.remote, usage.remote_calls), (1_100, 1));
    assert_eq!(
        journal.token_usage(at(10)).await.expect("usage"),
        TokenUsage::default()
    );
}
