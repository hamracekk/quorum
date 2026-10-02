//! `NewsStore` on the journal's SQLite database (migration `0006_news.sql`).

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use trader_core::news::{
    DUPLICATE_WINDOW_HOURS, Extraction, ExtractionStatus, NewsItem, NewsQuery, NewsSourceStats,
    StoredNews, TokenUsage, title_key,
};
use trader_core::{NewsStore, PortError};

use crate::agents::{from_json, fts_query, limit, to_json};
use crate::{JournalError, SqliteJournal, format_ts, parse_ts, port};

fn news_from(row: &SqliteRow) -> Result<NewsItem, JournalError> {
    let published: String = row.try_get("published_at")?;
    let fetched: String = row.try_get("fetched_at")?;
    let instruments: String = row.try_get("instruments")?;
    Ok(NewsItem {
        url: row.try_get("url")?,
        source: row.try_get("source")?,
        title: row.try_get("title")?,
        summary: row.try_get("summary")?,
        published_at: parse_ts(&published)?,
        fetched_at: parse_ts(&fetched)?,
        instruments: from_json("instruments", &instruments)?,
    })
}

const fn status_text(status: ExtractionStatus) -> &'static str {
    match status {
        ExtractionStatus::Ok => "ok",
        ExtractionStatus::Rejected => "rejected",
        ExtractionStatus::Failed => "failed",
    }
}

fn extraction_from(row: &SqliteRow) -> Result<Extraction, JournalError> {
    let ts: String = row.try_get("ts")?;
    let status: String = row.try_get("status")?;
    let events: String = row.try_get("events")?;
    let tokens = |name: &str| -> Result<u64, JournalError> {
        let value: i64 = row.try_get(name)?;
        Ok(u64::try_from(value).unwrap_or(0))
    };
    Ok(Extraction {
        url: row.try_get("url")?,
        prompt_version: row.try_get("prompt_version")?,
        tier: row.try_get("tier")?,
        model: row.try_get("model")?,
        ts: parse_ts(&ts)?,
        status: match status.as_str() {
            "ok" => ExtractionStatus::Ok,
            "rejected" => ExtractionStatus::Rejected,
            "failed" => ExtractionStatus::Failed,
            other => {
                return Err(JournalError::Corrupt(format!(
                    "extraction status `{other}`"
                )));
            }
        },
        relevant: row.try_get("relevant")?,
        events: from_json("events", &events)?,
        error: row.try_get("error")?,
        input_tokens: tokens("input_tokens")?,
        output_tokens: tokens("output_tokens")?,
    })
}

fn tokens(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl SqliteJournal {
    async fn select_unextracted(
        &self,
        prompt_version: &str,
        tier: &str,
        max: usize,
    ) -> Result<Vec<NewsItem>, JournalError> {
        let rows = sqlx::query(
            "SELECT n.url, n.source, n.title, n.summary, n.published_at, n.fetched_at, \
             n.instruments FROM news_items n \
             WHERE NOT EXISTS (SELECT 1 FROM news_extractions e WHERE e.url = n.url \
                               AND e.prompt_version = ? AND e.tier = ? \
                               AND e.status <> 'failed') \
             ORDER BY n.published_at DESC, n.url LIMIT ?",
        )
        .bind(prompt_version)
        .bind(tier)
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(news_from).collect()
    }

    async fn upsert_extraction(&self, extraction: &Extraction) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO news_extractions (url, prompt_version, tier, model, ts, status, \
             relevant, events, error, input_tokens, output_tokens) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (url, prompt_version, tier) DO UPDATE SET model = excluded.model, \
             ts = excluded.ts, status = excluded.status, relevant = excluded.relevant, \
             events = excluded.events, error = excluded.error, \
             input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens",
        )
        .bind(&extraction.url)
        .bind(&extraction.prompt_version)
        .bind(&extraction.tier)
        .bind(&extraction.model)
        .bind(format_ts(extraction.ts))
        .bind(status_text(extraction.status))
        .bind(extraction.relevant)
        .bind(to_json(&extraction.events)?)
        .bind(extraction.error.as_deref())
        .bind(tokens(extraction.input_tokens))
        .bind(tokens(extraction.output_tokens))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn select_extractions(
        &self,
        urls: &[String],
        prompt_version: &str,
    ) -> Result<Vec<Extraction>, JournalError> {
        let rows = sqlx::query(
            "SELECT url, prompt_version, tier, model, ts, status, relevant, events, error, \
             input_tokens, output_tokens FROM news_extractions \
             WHERE url IN (SELECT value FROM json_each(?)) AND prompt_version = ? \
             ORDER BY url, tier",
        )
        .bind(to_json(&urls)?)
        .bind(prompt_version)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(extraction_from).collect()
    }

    async fn select_readings(&self, since: DateTime<Utc>) -> Result<Vec<Extraction>, JournalError> {
        let rows = sqlx::query(
            "SELECT url, prompt_version, tier, model, ts, status, relevant, events, error, \
             input_tokens, output_tokens FROM news_extractions \
             WHERE status = 'ok' AND relevant = 1 AND ts >= ? ORDER BY ts, url, tier",
        )
        .bind(format_ts(since))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(extraction_from).collect()
    }

    async fn select_token_usage(&self, since: DateTime<Utc>) -> Result<TokenUsage, JournalError> {
        let rows = sqlx::query(
            "SELECT tier, SUM(input_tokens + output_tokens) AS spent, \
             SUM(CASE WHEN status <> 'failed' THEN 1 ELSE 0 END) AS calls \
             FROM news_extractions WHERE ts >= ? GROUP BY tier",
        )
        .bind(format_ts(since))
        .fetch_all(&self.pool)
        .await?;
        let mut usage = TokenUsage::default();
        for row in &rows {
            let tier: String = row.try_get("tier")?;
            let spent: i64 = row.try_get("spent")?;
            let calls: i64 = row.try_get("calls")?;
            let spent = u64::try_from(spent).unwrap_or(0);
            if tier == "local" {
                usage.local = spent;
            } else {
                usage.remote = spent;
                usage.remote_calls = u64::try_from(calls).unwrap_or(0);
            }
        }
        Ok(usage)
    }

    async fn insert_news(&self, items: &[NewsItem]) -> Result<StoredNews, JournalError> {
        let mut stored = StoredNews::default();
        let window = Duration::hours(DUPLICATE_WINDOW_HOURS);
        let mut tx = self.pool.begin().await?;
        for item in items {
            let key = title_key(&item.title);
            let same_story: Option<String> = sqlx::query_scalar(
                "SELECT url FROM news_items WHERE title_key = ? \
                 AND published_at BETWEEN ? AND ? LIMIT 1",
            )
            .bind(&key)
            .bind(format_ts(item.published_at - window))
            .bind(format_ts(item.published_at + window))
            .fetch_optional(&mut *tx)
            .await?;
            if same_story.is_some() {
                stored.duplicates += 1;
                continue;
            }
            let inserted = sqlx::query(
                "INSERT OR IGNORE INTO news_items \
                 (url, source, title, title_key, summary, published_at, fetched_at, instruments) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&item.url)
            .bind(&item.source)
            .bind(&item.title)
            .bind(&key)
            .bind(&item.summary)
            .bind(format_ts(item.published_at))
            .bind(format_ts(item.fetched_at))
            .bind(to_json(&item.instruments)?)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if inserted == 1 {
                stored.inserted += 1;
            } else {
                stored.duplicates += 1;
            }
        }
        tx.commit().await?;
        Ok(stored)
    }

    async fn select_news(&self, query: &NewsQuery) -> Result<Vec<NewsItem>, JournalError> {
        let instrument = query.instrument.as_ref().map(|i| format!("\"{i}\""));
        let since = query.since.map(format_ts);
        let rows = match query.text.as_deref().map(fts_query) {
            Some(None) => return Ok(Vec::new()),
            Some(Some(fts)) => {
                sqlx::query(
                    "SELECT url, source, title, summary, published_at, fetched_at, instruments \
                     FROM news_items \
                     WHERE (?1 IS NULL OR source = ?1) \
                     AND (?2 IS NULL OR instr(instruments, ?2) > 0) \
                     AND (?3 IS NULL OR published_at >= ?3) \
                     AND rowid IN (SELECT rowid FROM news_fts WHERE news_fts MATCH ?5) \
                     ORDER BY published_at DESC, url LIMIT ?4",
                )
                .bind(query.source.as_deref())
                .bind(instrument.as_deref())
                .bind(since.as_deref())
                .bind(limit(query.limit))
                .bind(fts)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT url, source, title, summary, published_at, fetched_at, instruments \
                     FROM news_items \
                     WHERE (?1 IS NULL OR source = ?1) \
                     AND (?2 IS NULL OR instr(instruments, ?2) > 0) \
                     AND (?3 IS NULL OR published_at >= ?3) \
                     ORDER BY published_at DESC, url LIMIT ?4",
                )
                .bind(query.source.as_deref())
                .bind(instrument.as_deref())
                .bind(since.as_deref())
                .bind(limit(query.limit))
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(news_from).collect()
    }

    async fn select_news_stats(&self) -> Result<Vec<NewsSourceStats>, JournalError> {
        let rows = sqlx::query(
            "SELECT source, COUNT(*) AS items, MAX(published_at) AS latest \
             FROM news_items GROUP BY source ORDER BY source",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let items: i64 = row.try_get("items")?;
                let latest: Option<String> = row.try_get("latest")?;
                Ok(NewsSourceStats {
                    source: row.try_get("source")?,
                    items: usize::try_from(items).unwrap_or(0),
                    latest: latest.as_deref().map(parse_ts).transpose()?,
                })
            })
            .collect()
    }
}

#[async_trait]
impl NewsStore for SqliteJournal {
    async fn store_news(&self, items: &[NewsItem]) -> Result<StoredNews, PortError> {
        self.insert_news(items).await.map_err(port)
    }

    async fn news(&self, query: &NewsQuery) -> Result<Vec<NewsItem>, PortError> {
        self.select_news(query).await.map_err(port)
    }

    async fn news_stats(&self) -> Result<Vec<NewsSourceStats>, PortError> {
        self.select_news_stats().await.map_err(port)
    }

    async fn unextracted(
        &self,
        prompt_version: &str,
        tier: &str,
        limit: usize,
    ) -> Result<Vec<NewsItem>, PortError> {
        self.select_unextracted(prompt_version, tier, limit)
            .await
            .map_err(port)
    }

    async fn record_extraction(&self, extraction: &Extraction) -> Result<(), PortError> {
        self.upsert_extraction(extraction).await.map_err(port)
    }

    async fn extractions(
        &self,
        urls: &[String],
        prompt_version: &str,
    ) -> Result<Vec<Extraction>, PortError> {
        self.select_extractions(urls, prompt_version)
            .await
            .map_err(port)
    }

    async fn token_usage(&self, since: DateTime<Utc>) -> Result<TokenUsage, PortError> {
        self.select_token_usage(since).await.map_err(port)
    }

    async fn readings(&self, since: DateTime<Utc>) -> Result<Vec<Extraction>, PortError> {
        self.select_readings(since).await.map_err(port)
    }
}

#[cfg(test)]
mod tests;
