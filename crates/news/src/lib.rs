//! News ingestion (M5a): RSS / Atom / JSON feeds — crypto news sites, SEC EDGAR filing feeds —
//! turned into sanitized [`NewsItem`]s behind the [`NewsSource`] port.
//!
//! Feed content is untrusted. Titles and summaries become plain text with bounded length,
//! links must be http(s) and are canonicalized (no fragment, no tracking parameters), entries
//! without a usable title or link are dropped, and response size and entry count are capped.
//! Nothing inside an item is followed, fetched or executed.

pub mod sanitize;

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use quorum_core::news::NewsItem;
use quorum_core::{NewsSource, PortError};
use reqwest::Url;

use crate::sanitize::{canonical_url, plain_text};

/// Longest title kept (characters).
pub const MAX_TITLE: usize = 300;
/// Longest summary kept (characters).
pub const MAX_SUMMARY: usize = 1_000;
/// Most entries taken from one fetch.
pub const MAX_ENTRIES: usize = 200;
/// Largest feed response accepted.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;

const TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_AGENT: &str = concat!("quorum/", env!("CARGO_PKG_VERSION"), " (local research)");

#[derive(Debug, thiserror::Error)]
pub enum NewsError {
    #[error("invalid feed url `{0}` (http or https only)")]
    Url(String),
    #[error("request to {url} failed: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} answered {status}")]
    Status { url: String, status: u16 },
    #[error("{url} sent more than {MAX_BYTES} bytes")]
    TooLarge { url: String },
    #[error("{url} is not a readable feed: {message}")]
    Parse { url: String, message: String },
}

/// One RSS/Atom/JSON feed.
#[derive(Debug, Clone)]
pub struct RssSource {
    name: String,
    url: Url,
    client: reqwest::Client,
}

impl RssSource {
    /// `user_agent` overrides the default, e.g. SEC EDGAR asks for "Name contact@example.com".
    pub fn new(name: &str, url: &str, user_agent: Option<&str>) -> Result<Self, NewsError> {
        let parsed = Url::parse(url).map_err(|_| NewsError::Url(url.to_owned()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(NewsError::Url(url.to_owned()));
        }
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(user_agent.unwrap_or(DEFAULT_AGENT))
            .build()
            .map_err(|source| NewsError::Http {
                url: url.to_owned(),
                source,
            })?;
        Ok(Self {
            name: name.to_owned(),
            url: parsed,
            client,
        })
    }

    async fn download(&self) -> Result<Vec<u8>, NewsError> {
        let url = self.url.to_string();
        let http = |source| NewsError::Http {
            url: url.clone(),
            source,
        };
        let mut response = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .map_err(http)?;
        let status = response.status();
        if !status.is_success() {
            return Err(NewsError::Status {
                url,
                status: status.as_u16(),
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(http)? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BYTES {
                return Err(NewsError::TooLarge { url });
            }
        }
        Ok(body)
    }
}

#[async_trait]
impl NewsSource for RssSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn fetch(&self) -> Result<Vec<NewsItem>, PortError> {
        let context = || format!("news source {}", self.name);
        let body = self
            .download()
            .await
            .map_err(|e| PortError::adapter(context(), e))?;
        parse_feed(&self.name, self.url.as_str(), &body, Utc::now())
            .map_err(|e| PortError::adapter(context(), e))
    }
}

/// Sanitized items from a feed document. Entries without a title or an http(s) link are
/// dropped; an entry without a date is stamped `fetched_at`.
pub fn parse_feed(
    source: &str,
    url: &str,
    body: &[u8],
    fetched_at: DateTime<Utc>,
) -> Result<Vec<NewsItem>, NewsError> {
    let feed = feed_rs::parser::parse(body).map_err(|e| NewsError::Parse {
        url: url.to_owned(),
        message: e.to_string(),
    })?;
    let items = feed
        .entries
        .into_iter()
        .take(MAX_ENTRIES)
        .filter_map(|entry| {
            let title = plain_text(
                &entry.title.map(|t| t.content).unwrap_or_default(),
                MAX_TITLE,
            );
            let link = entry
                .links
                .iter()
                .find_map(|l| canonical_url(&l.href))
                .or_else(|| canonical_url(&entry.id))?;
            if title.is_empty() {
                return None;
            }
            let summary_html = entry
                .summary
                .map(|t| t.content)
                .or_else(|| entry.content.and_then(|c| c.body))
                .unwrap_or_default();
            Some(NewsItem {
                url: link,
                source: source.to_owned(),
                title,
                summary: plain_text(&summary_html, MAX_SUMMARY),
                published_at: entry.published.or(entry.updated).unwrap_or(fetched_at),
                fetched_at,
                instruments: Vec::new(),
            })
        })
        .collect();
    Ok(items)
}

#[cfg(test)]
mod tests;
