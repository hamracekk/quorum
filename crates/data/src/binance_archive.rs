//! Binance's public data archive (data.binance.vision): monthly kline files for every spot
//! symbol Binance ever listed, delisted ones included (R-1, survivorship-free history).
//!
//! The archive is a public S3 bucket: a listing (XML, paged by `marker`) names the files, each a
//! zip holding one CSV (`open_time, open, high, low, close, volume, close_time, …`; times in
//! milliseconds, microseconds from 2025). Public, keyless, read-only.

use std::io::{Cursor, Read};
use std::time::Duration;

use chrono::DateTime;
use trader_core::Bar;

use crate::{DataError, Result};

const LISTING_URL: &str = "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision";
const FILES_URL: &str = "https://data.binance.vision";

/// The public archive.
#[derive(Debug, Clone)]
pub struct BinanceArchive {
    client: reqwest::Client,
    listing_url: String,
    files_url: String,
}

impl BinanceArchive {
    pub fn new() -> Result<Self> {
        Self::with_urls(LISTING_URL, FILES_URL)
    }

    /// Point at other hosts, e.g. a local mock server.
    pub fn with_urls(listing_url: impl Into<String>, files_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("trader/0.1")
            .timeout(Duration::from_mins(1))
            .build()?;
        Ok(Self {
            client,
            listing_url: listing_url.into(),
            files_url: files_url.into(),
        })
    }

    /// Every entry under `prefix`: sub-folders (with `delimiter = "/"`) and file keys.
    async fn list(&self, prefix: &str) -> Result<(Vec<String>, Vec<String>)> {
        let mut folders = Vec::new();
        let mut keys = Vec::new();
        let mut marker = String::new();
        loop {
            let body = self
                .client
                .get(&self.listing_url)
                .query(&[
                    ("delimiter", "/"),
                    ("prefix", prefix),
                    ("marker", marker.as_str()),
                ])
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let page_folders = tags(&body, "Prefix")
                .into_iter()
                .filter(|p| p != prefix)
                .collect::<Vec<_>>();
            let page_keys = tags(&body, "Key");
            let next = tags(&body, "NextMarker")
                .into_iter()
                .next()
                .or_else(|| page_keys.last().cloned())
                .or_else(|| page_folders.last().cloned());
            folders.extend(page_folders);
            keys.extend(page_keys);
            let truncated = tags(&body, "IsTruncated")
                .first()
                .is_some_and(|t| t == "true");
            match next {
                Some(n) if truncated && n != marker => marker = n,
                _ => break,
            }
        }
        Ok((folders, keys))
    }

    /// Every spot symbol with monthly klines in the archive.
    pub async fn spot_symbols(&self) -> Result<Vec<String>> {
        let prefix = "data/spot/monthly/klines/";
        let (folders, _) = self.list(prefix).await?;
        Ok(folders
            .into_iter()
            .filter_map(|f| {
                f.strip_prefix(prefix)
                    .map(|s| s.trim_end_matches('/').to_owned())
            })
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// Bars of `symbol` at `interval` from every monthly file, oldest first.
    pub async fn klines(&self, symbol: &str, interval: &str) -> Result<Vec<Bar>> {
        let prefix = format!("data/spot/monthly/klines/{symbol}/{interval}/");
        let (_, mut keys) = self.list(&prefix).await?;
        keys.retain(|k| {
            std::path::Path::new(k)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        });
        keys.sort();
        let mut bars = Vec::new();
        for key in keys {
            let bytes = self
                .client
                .get(format!("{}/{key}", self.files_url))
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            bars.extend(parse_zip(&bytes)?);
        }
        bars.sort_by_key(|b| b.ts);
        bars.dedup_by_key(|b| b.ts);
        Ok(bars)
    }
}

/// The text of every `<tag>…</tag>` in `xml`.
fn tags(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(after[..end].to_owned());
        rest = &after[end + close.len()..];
    }
    out
}

/// Bars from one archive zip (one CSV inside).
pub fn parse_zip(bytes: &[u8]) -> Result<Vec<Bar>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| DataError::Malformed(format!("archive zip: {e}")))?;
    let mut csv = String::new();
    archive
        .by_index(0)
        .map_err(|e| DataError::Malformed(format!("archive zip: {e}")))?
        .read_to_string(&mut csv)
        .map_err(|e| DataError::Malformed(format!("archive csv: {e}")))?;
    parse_csv(&csv)
}

/// Bars from an archive CSV (an optional header line is skipped).
pub fn parse_csv(csv: &str) -> Result<Vec<Bar>> {
    let mut bars = Vec::new();
    for line in csv.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split(',').collect();
        let Some(open_time) = fields.first().and_then(|f| f.trim().parse::<i64>().ok()) else {
            continue; // header
        };
        let num = |i: usize| -> Result<f64> {
            fields
                .get(i)
                .and_then(|f| f.trim().parse().ok())
                .ok_or_else(|| DataError::Malformed(format!("archive row `{line}`")))
        };
        // Microseconds from 2025 on, milliseconds before.
        let millis = if open_time > 100_000_000_000_000 {
            open_time / 1_000
        } else {
            open_time
        };
        let ts = DateTime::from_timestamp_millis(millis)
            .ok_or_else(|| DataError::Malformed(format!("archive time {open_time}")))?;
        bars.push(Bar {
            ts,
            open: num(1)?,
            high: num(2)?,
            low: num(3)?,
            close: num(4)?,
            volume: num(5)?,
        });
    }
    Ok(bars)
}

#[cfg(test)]
mod tests;
