//! `NewsStore` on the journal's SQLite database (migration `0006_news.sql`).

use async_trait::async_trait;
use chrono::Duration;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use trader_core::news::{
    DUPLICATE_WINDOW_HOURS, NewsItem, NewsQuery, NewsSourceStats, StoredNews, title_key,
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

impl SqliteJournal {
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
}

#[cfg(test)]
mod tests;
