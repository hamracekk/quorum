//! Binance's public data archive (data.binance.vision): monthly kline files for every spot
//! symbol Binance ever listed, delisted ones included (R-1, survivorship-free history).
//!
//! The archive is a public S3 bucket: a listing (XML, paged by `marker`) names the files, each a
//! zip holding one CSV (`open_time, open, high, low, close, volume, close_time, …`; times in
//! milliseconds, microseconds from 2025). Public, keyless, read-only.

use std::io::{Cursor, Read};
use std::time::Duration;

use chrono::DateTime;
use quorum_core::Bar;

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
            .user_agent("quorum/0.1")
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

/// One day of a USDT perpetual's positioning, from the archive's 5-minute metrics: the last
/// reading of the day (data-sources plan; the REST endpoints keep only 30 days).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricsDay {
    /// The UTC day (its open time).
    pub ts: DateTime<chrono::Utc>,
    /// Open interest in USD at the day's last reading.
    pub open_interest_value: f64,
    /// Accounts long over accounts short (all traders) at the day's last reading.
    pub long_short_ratio: f64,
}

impl BinanceArchive {
    /// Daily positioning of USDT perpetual `symbol` for every archived day after `after`.
    ///
    /// Thousands of small files: each is retried, and on a lasting failure the days read so
    /// far come back with the error, so a caller keeps them and resumes later.
    pub async fn futures_metrics(
        &self,
        symbol: &str,
        after: Option<chrono::NaiveDate>,
    ) -> (Vec<MetricsDay>, Option<DataError>) {
        let mut out = Vec::new();
        let error = self.read_metrics(symbol, after, &mut out).await.err();
        out.sort_by_key(|m| m.ts);
        (out, error)
    }

    /// GET `url` as bytes, up to three tries a second apart.
    async fn bytes_with_retry(&self, url: &str) -> Result<Vec<u8>> {
        let mut last = None;
        for attempt in 0..3_u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt)).await;
            }
            match self
                .client
                .get(url)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            {
                Ok(response) => match response.bytes().await {
                    Ok(bytes) => return Ok(bytes.to_vec()),
                    Err(e) => last = Some(e),
                },
                Err(e) => last = Some(e),
            }
        }
        Err(last.map_or_else(
            || DataError::Malformed(format!("{url}: no answer")),
            DataError::from,
        ))
    }

    async fn read_metrics(
        &self,
        symbol: &str,
        after: Option<chrono::NaiveDate>,
        out: &mut Vec<MetricsDay>,
    ) -> Result<()> {
        let prefix = format!("data/futures/um/daily/metrics/{symbol}/");
        let (_, keys) = self.list(&prefix).await?;
        for key in keys.iter().filter(|k| {
            std::path::Path::new(k)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
        }) {
            let Some(day) = key
                .rsplit('-')
                .take(3)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("-")
                .strip_suffix(".zip")
                .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            else {
                continue;
            };
            if after.is_some_and(|a| day <= a) {
                continue;
            }
            let bytes = self
                .bytes_with_retry(&format!("{}/{key}", self.files_url))
                .await?;
            let mut csv = String::new();
            zip::ZipArchive::new(Cursor::new(&bytes[..]))
                .and_then(|mut z| {
                    z.by_index(0)
                        .map(|mut f| f.read_to_string(&mut csv).map(|_| ()))
                })
                .map_err(|e| DataError::Malformed(format!("metrics zip: {e}")))?
                .map_err(|e| DataError::Malformed(format!("metrics csv: {e}")))?;
            if let Some(m) = parse_metrics(&csv, day) {
                out.push(m);
            }
        }
        Ok(())
    }
}

/// The last reading of one day's metrics CSV.
///
/// Columns: `create_time, symbol, sum_open_interest, sum_open_interest_value,
/// count_toptrader_long_short_ratio, sum_toptrader_long_short_ratio, count_long_short_ratio,
/// sum_taker_long_short_vol_ratio`.
pub fn parse_metrics(csv: &str, day: chrono::NaiveDate) -> Option<MetricsDay> {
    let last = csv.lines().rev().find(|l| {
        l.split(',')
            .nth(3)
            .is_some_and(|v| v.trim().parse::<f64>().is_ok())
    })?;
    let fields: Vec<&str> = last.split(',').collect();
    Some(MetricsDay {
        ts: day.and_time(chrono::NaiveTime::MIN).and_utc(),
        open_interest_value: fields.get(3)?.trim().parse().ok()?,
        long_short_ratio: fields.get(6)?.trim().parse().ok()?,
    })
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
