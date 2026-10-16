//! US equities daily bars from Tiingo's end-of-day API (TS-3: read-only stock prices, so the
//! implications of news for stocks can be scored like any other source).
//!
//! Needs a free API key (`trader secrets set tiingo`); it travels only in the `Authorization`
//! header and is never logged. Instruments are `tiingo:TICKER:equity`. Prices are split- and
//! dividend-adjusted, so a long history compares like with like (a new dividend can revise
//! earlier adjusted prices slightly — research data, not a ledger). A bar is keyed by its trading
//! date (00:00 UTC) and is returned only once that UTC day is over: by then the US session has
//! closed, so a daily decision never sees a bar still forming.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use quorum_core::{Bar, InstrumentId, InstrumentKind, MarketData, PortError, VenueId};
use serde::Deserialize;

use crate::{DataError, Result};

/// Venue id served by [`TiingoDaily`].
pub const VENUE: &str = "tiingo";
const BASE_URL: &str = "https://api.tiingo.com";
/// The only interval Tiingo's end-of-day API serves.
const DAILY: &str = "1d";

/// `MarketData` adapter for Tiingo end-of-day prices.
pub struct TiingoDaily {
    venue: VenueId,
    client: reqwest::Client,
    base_url: String,
    key: Option<String>,
}

impl std::fmt::Debug for TiingoDaily {
    // Never print the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TiingoDaily")
            .field("base_url", &self.base_url)
            .field("key", &self.key.as_ref().map(|_| "<set>"))
            .finish_non_exhaustive()
    }
}

/// One row of `/tiingo/daily/{ticker}/prices`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    date: String,
    adj_open: Option<f64>,
    adj_high: Option<f64>,
    adj_low: Option<f64>,
    adj_close: Option<f64>,
    adj_volume: Option<f64>,
}

impl TiingoDaily {
    /// `key` is the operator's API key; without one every request fails with how to set it.
    pub fn new(key: Option<String>) -> Result<Self> {
        Self::with_base_url(BASE_URL, key)
    }

    /// Point at a different host, e.g. a local mock server.
    pub fn with_base_url(base_url: impl Into<String>, key: Option<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("quorum/0.1")
            .timeout(Duration::from_secs(30))
            .build()?;
        let venue = VenueId::new(VENUE).map_err(|e| DataError::Malformed(e.to_string()))?;
        Ok(Self {
            venue,
            client,
            base_url: base_url.into(),
            key: key.filter(|k| !k.trim().is_empty()),
        })
    }

    fn key(&self) -> Result<&str> {
        let key = self.key.as_deref().ok_or_else(|| {
            DataError::Setup(
                "no Tiingo API key: create a free one at tiingo.com, then run \
                 `trader secrets set tiingo`"
                    .into(),
            )
        })?;
        // Never send something that is not a Tiingo token (e.g. another service's key pasted
        // by mistake) to Tiingo: tokens are short and alphanumeric.
        if !(20..=64).contains(&key.len()) || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(DataError::Setup(
                "the stored Tiingo key is not a Tiingo token (40 characters, 0-9 and a-f): \
                 run `trader secrets set tiingo` again; nothing was sent"
                    .into(),
            ));
        }
        Ok(key)
    }

    async fn prices(
        &self,
        ticker: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Bar>> {
        let key = self.key()?;
        // Whole days: the last one asked for is the day before `end` (exclusive).
        let first = start.date_naive();
        let last = (end - TimeDelta::nanoseconds(1)).date_naive();
        if last < first {
            return Ok(Vec::new());
        }
        let rows: Vec<Row> = self
            .client
            .get(format!("{}/tiingo/daily/{ticker}/prices", self.base_url))
            .header("Authorization", format!("Token {key}"))
            .query(&[
                ("startDate", first.to_string()),
                ("endDate", last.to_string()),
                ("format", "json".to_owned()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let now = Utc::now();
        let mut bars: Vec<Bar> = rows
            .iter()
            .map(parse_row)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|b| b.ts >= start && b.ts < end && b.ts + TimeDelta::days(1) <= now)
            .collect();
        bars.sort_by_key(|b| b.ts);
        bars.dedup_by_key(|b| b.ts);
        tracing::debug!(
            ticker,
            rows = rows.len(),
            kept = bars.len(),
            "fetched tiingo prices"
        );
        Ok(bars)
    }
}

fn parse_row(row: &Row) -> Result<Bar> {
    let day = row
        .date
        .get(..10)
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .ok_or_else(|| DataError::Malformed(format!("tiingo date `{}`", row.date)))?;
    let field = |value: Option<f64>, name: &str| {
        value
            .filter(|v| v.is_finite())
            .ok_or_else(|| DataError::Malformed(format!("tiingo {name} missing on {day}")))
    };
    Ok(Bar {
        ts: day.and_time(chrono::NaiveTime::MIN).and_utc(),
        open: field(row.adj_open, "adjOpen")?,
        high: field(row.adj_high, "adjHigh")?,
        low: field(row.adj_low, "adjLow")?,
        close: field(row.adj_close, "adjClose")?,
        volume: row.adj_volume.filter(|v| v.is_finite()).unwrap_or(0.0),
    })
}

#[async_trait]
impl MarketData for TiingoDaily {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn check(&self) -> std::result::Result<(), PortError> {
        let key = self.key().map_err(|e| PortError::adapter("tiingo", e))?;
        self.client
            .get(format!("{}/api/test", self.base_url))
            .header("Authorization", format!("Token {key}"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(drop)
            .map_err(|e| PortError::adapter("tiingo test", e))
    }

    async fn bars(
        &self,
        instrument: &InstrumentId,
        interval: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> std::result::Result<Vec<Bar>, PortError> {
        if instrument.venue() != &self.venue || instrument.kind() != InstrumentKind::Equity {
            return Err(PortError::Unsupported(instrument.to_string()));
        }
        if interval != DAILY {
            return Err(PortError::Unsupported(format!(
                "{instrument} at {interval}: Tiingo serves daily bars (1d) only"
            )));
        }
        self.prices(instrument.symbol(), start, end)
            .await
            .map_err(|e| PortError::adapter(format!("tiingo prices for {instrument}"), e))
    }
}

/// When the first daily bar after `last` can be fetched: the next US trading day's bar
/// (`core::calendar`, NYSE holidays skipped), once that UTC day is over.
#[must_use]
pub fn next_bar_ready(last: DateTime<Utc>) -> DateTime<Utc> {
    let day = quorum_core::calendar::Exchange::Nyse.next_trading_day(last.date_naive());
    (day + TimeDelta::days(1))
        .and_time(chrono::NaiveTime::MIN)
        .and_utc()
}

/// End-of-day bars as a [`MarketStream`]: polls Tiingo once a new trading day can be complete
/// and yields every instrument's new bars, in time order and in the order subscribed within a
/// day.
///
/// Mindful of Tiingo's free limits (50 requests an hour): it sleeps until the next trading day's
/// bar can exist (US holidays skipped), asks the first instrument first and fetches the rest only
/// once that one has a new bar; if a bar is late it asks once an hour.
#[derive(Debug, Clone)]
pub struct TiingoStream {
    source: std::sync::Arc<TiingoDaily>,
    poll: Duration,
}

impl TiingoStream {
    #[must_use]
    pub const fn new(source: std::sync::Arc<TiingoDaily>) -> Self {
        Self {
            source,
            poll: Duration::from_hours(1),
        }
    }

    /// Check again this often while waiting for a bar (tests use a short one).
    #[must_use]
    pub const fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }
}

/// Every instrument's bars after `last`, sorted by time then by position in `instruments`.
async fn new_bars(
    source: &TiingoDaily,
    instruments: &[InstrumentId],
    last: &std::collections::BTreeMap<InstrumentId, DateTime<Utc>>,
    now: DateTime<Utc>,
) -> std::result::Result<Vec<(usize, InstrumentId, Bar)>, PortError> {
    let mut out = Vec::new();
    for (rank, instrument) in instruments.iter().enumerate() {
        let after = last[instrument];
        let bars = source
            .bars(instrument, DAILY, after + TimeDelta::milliseconds(1), now)
            .await?;
        out.extend(
            bars.into_iter()
                .filter(|b| b.ts > after)
                .map(|b| (rank, instrument.clone(), b)),
        );
    }
    out.sort_by_key(|(rank, _, b)| (b.ts, *rank));
    Ok(out)
}

#[async_trait]
impl quorum_core::MarketStream for TiingoStream {
    fn venue(&self) -> &VenueId {
        &self.source.venue
    }

    async fn subscribe(
        &self,
        instruments: &[InstrumentId],
        interval: &str,
    ) -> std::result::Result<
        futures_core::stream::BoxStream<'static, quorum_core::StreamedBar>,
        PortError,
    > {
        use futures_util::StreamExt as _;
        if interval != DAILY {
            return Err(PortError::Unsupported(format!(
                "Tiingo streams daily bars (1d) only, not {interval}"
            )));
        }
        let Some(lead) = instruments.first().cloned() else {
            return Err(PortError::Invalid("nothing to stream".to_owned()));
        };
        let (tx, rx) = tokio::sync::mpsc::channel(512);
        let source = std::sync::Arc::clone(&self.source);
        let instruments = instruments.to_vec();
        let poll = self.poll;
        tokio::spawn(async move {
            // A few days back: the consumer skips what it has and backfills older gaps itself.
            let start = Utc::now() - TimeDelta::days(4);
            let mut last: std::collections::BTreeMap<InstrumentId, DateTime<Utc>> =
                instruments.iter().map(|i| (i.clone(), start)).collect();
            while !tx.is_closed() {
                let now = Utc::now();
                let due = next_bar_ready(last[&lead]);
                if now < due {
                    let wait = (due - now).to_std().unwrap_or(poll).min(poll);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                let found = new_bars(&source, std::slice::from_ref(&lead), &last, now).await;
                let result = match found {
                    Ok(lead_bars) if lead_bars.is_empty() => Ok(Vec::new()),
                    Ok(_) => new_bars(&source, &instruments, &last, now).await,
                    Err(err) => Err(err),
                };
                match result {
                    Ok(bars) if bars.is_empty() => tokio::time::sleep(poll).await,
                    Ok(bars) => {
                        for (_, instrument, bar) in bars {
                            last.insert(instrument.clone(), bar.ts);
                            if tx.send(Ok((instrument, bar))).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        if tx.send(Err(err)).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(poll).await;
                    }
                }
            }
        });
        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }
}

#[cfg(test)]
mod tests;
