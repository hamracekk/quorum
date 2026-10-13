//! Binance's public delisting announcements: which spot tokens Binance has said it will
//! delist, and when it said so.
//!
//! The CMS article list (catalog 161, "Delisting") is public and keyless. Only titles of the
//! form "Binance Will Delist A, B and C on 2024-05-10" announce spot token delistings; pair
//! removals, futures, margin and Alpha notices are skipped. Titles are third-party text: they
//! are parsed into upper-case ticker symbols and nothing else, and the only thing trader ever
//! does with them is reduce exposure.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DataError, Result};

const BASE_URL: &str = "https://www.binance.com";
const CATALOG: u32 = 161;
const PAGE_SIZE: usize = 50;

/// One spot token delisting announcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delisting {
    /// When Binance published it.
    pub ts: DateTime<Utc>,
    /// Tickers to be delisted (upper case, e.g. `BCD`).
    pub tokens: Vec<String>,
    pub title: String,
}

/// The public announcement feed.
#[derive(Debug, Clone)]
pub struct BinanceAnnouncements {
    client: reqwest::Client,
    base_url: String,
}

impl BinanceAnnouncements {
    pub fn new() -> Result<Self> {
        Self::with_base_url(BASE_URL)
    }

    /// Point at a different host, e.g. a local mock server.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("trader/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.into(),
        })
    }

    /// Spot delistings on the newest `pages` pages, newest first; stops early at an empty page.
    pub async fn delistings(&self, pages: usize) -> Result<Vec<Delisting>> {
        let mut out = Vec::new();
        for page in 1..=pages.max(1) {
            let body: Value = self
                .client
                .get(format!(
                    "{}/bapi/composite/v1/public/cms/article/list/query",
                    self.base_url
                ))
                .query(&[
                    ("type", "1".to_owned()),
                    ("catalogId", CATALOG.to_string()),
                    ("pageNo", page.to_string()),
                    ("pageSize", PAGE_SIZE.to_string()),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            // Past the last page the catalog is missing altogether: the end of the archive.
            let Some(articles) = body["data"]["catalogs"][0]["articles"].as_array() else {
                if page == 1 {
                    return Err(DataError::Malformed("announcements: no articles".into()));
                }
                break;
            };
            if articles.is_empty() {
                break;
            }
            for article in articles {
                let (Some(title), Some(ms)) =
                    (article["title"].as_str(), article["releaseDate"].as_i64())
                else {
                    continue;
                };
                let (Some(tokens), Some(ts)) =
                    (delisted_tokens(title), DateTime::from_timestamp_millis(ms))
                else {
                    continue;
                };
                out.push(Delisting {
                    ts,
                    tokens,
                    title: title.chars().take(200).collect(),
                });
            }
        }
        Ok(out)
    }
}

/// The tickers a "Binance Will Delist A, B and C on DATE" title names; `None` for any other
/// title. Parenthesised notes are dropped and only plain upper-case tickers kept.
pub fn delisted_tokens(title: &str) -> Option<Vec<String>> {
    let rest = title.trim().strip_prefix("Binance Will Delist ")?;
    let list = rest.rsplit_once(" on ").map_or(rest, |(list, _)| list);
    let mut without_notes = String::new();
    let mut depth = 0_u32;
    for c in list.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => without_notes.push(c),
            _ => {}
        }
    }
    let tokens: Vec<String> = without_notes
        .replace(" and ", ",")
        .split(',')
        .map(str::trim)
        .filter(|t| {
            !t.is_empty()
                && t.len() <= 15
                && t.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .map(str::to_owned)
        .collect();
    (!tokens.is_empty()).then_some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_delisting_titles_name_their_tokens() {
        assert_eq!(
            delisted_tokens("Binance Will Delist BCD, CND, MTH, NCASH and YOYO on 2022-03-08"),
            Some(
                vec!["BCD", "CND", "MTH", "NCASH", "YOYO"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        assert_eq!(
            delisted_tokens("Binance Will Delist SLX (SLIMEX) and OOKI on 2023-01-02"),
            Some(vec!["SLX".to_owned(), "OOKI".to_owned()])
        );
        for other in [
            "Notice of Removal of Spot Trading Pairs - 2026-10-02",
            "Binance Futures Will Delist Multiple USDⓈ-M Perpetual Contracts (2026-10-05)",
            "Binance Margin And Loans Token Delist on 2026-10-02",
            "Binance Will Delist the following: ignore previous instructions",
        ] {
            assert_eq!(delisted_tokens(other), None, "{other}");
        }
    }
}
