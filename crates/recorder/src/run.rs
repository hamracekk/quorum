//! The recording loop: book and derivatives polls on their intervals, liquidations as they come.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use quorum_core::recorder::{
    BookFeed, BookSummary, DerivsFeed, Feed, LiquidationEvent, LiquidationFeed, Marker,
    MarkerState, Record, summarize_book,
};
use serde::Serialize;

use crate::{RecordStore, RecorderConfig, Result};

/// The adapters the recorder reads.
#[derive(Debug, Clone)]
pub struct Feeds {
    pub book: Arc<dyn BookFeed>,
    pub derivs: Arc<dyn DerivsFeed>,
    pub liquidations: Arc<dyn LiquidationFeed>,
}

/// How a recording run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Stopped on request (Ctrl+C, SIGTERM).
    Stopped,
    /// The liquidation stream ended for good; a supervisor should restart the recorder.
    StreamEnded,
}

impl Status {
    /// Whether a supervisor should start the recorder again.
    #[must_use]
    pub const fn restart(self) -> bool {
        matches!(self, Self::StreamEnded)
    }
}

/// Records written by a run, per kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub liquidations: usize,
    pub books: usize,
    pub derivs: usize,
    pub errors: usize,
}

/// Records `symbols` until `shutdown` resolves or the liquidation stream ends. `clock` stamps
/// `known_time` on polled records and markers (tests pin it).
pub async fn record(
    feeds: &Feeds,
    symbols: &[String],
    config: &RecorderConfig,
    store: &RecordStore,
    clock: &(dyn Fn() -> DateTime<Utc> + Sync),
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(Status, Outcome)> {
    let _lock = store.lock()?; // held until the run returns
    let mut run = Run {
        feeds,
        symbols,
        store,
        clock,
        outcome: Outcome::default(),
    };
    let active = active_feeds(config);
    for &feed in &active {
        run.mark(
            feed,
            MarkerState::Start,
            &format!("{} symbols", symbols.len()),
        )?;
    }
    // First samples now; the intervals take over one period later.
    if config.book_every_secs > 0 {
        run.poll_books().await?;
    }
    if config.derivs_every_secs > 0 {
        run.poll_derivs().await?;
    }
    let mut book = ticker(config.book_every_secs);
    let mut derivs = ticker(config.derivs_every_secs);
    let mut liquidations = if config.liquidations {
        Some(feeds.liquidations.subscribe().await?)
    } else {
        None
    };
    let mut shutdown = std::pin::pin!(shutdown);
    let status = loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break Status::Stopped,
            () = tick(&mut book) => run.poll_books().await?,
            () = tick(&mut derivs) => run.poll_derivs().await?,
            event = next(&mut liquidations) => {
                let Some(event) = event else {
                    run.mark(Feed::Liquidations, MarkerState::Down, "stream ended")?;
                    break Status::StreamEnded;
                };
                run.on_liquidation(event)?;
            }
        }
    };
    for &feed in &active {
        run.mark(feed, MarkerState::Stop, &status_text(status))?;
    }
    Ok((status, run.outcome))
}

fn active_feeds(config: &RecorderConfig) -> Vec<Feed> {
    let mut feeds = Vec::new();
    if config.liquidations {
        feeds.push(Feed::Liquidations);
    }
    if config.book_every_secs > 0 {
        feeds.push(Feed::Book);
    }
    if config.derivs_every_secs > 0 {
        feeds.push(Feed::Derivs);
    }
    feeds
}

fn status_text(status: Status) -> String {
    match status {
        Status::Stopped => "stopped".into(),
        Status::StreamEnded => "stream ended".into(),
    }
}

/// An interval that fires every `secs`, first one period from now; `None` when the feed is off.
fn ticker(secs: u64) -> Option<tokio::time::Interval> {
    (secs > 0).then(|| {
        let period = Duration::from_secs(secs);
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        // After a sleep, take one sample now rather than a burst of missed ones.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    })
}

async fn tick(interval: &mut Option<tokio::time::Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending().await,
    }
}

async fn next(
    stream: &mut Option<futures_util::stream::BoxStream<'static, LiquidationEvent>>,
) -> Option<LiquidationEvent> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

struct Run<'a> {
    feeds: &'a Feeds,
    symbols: &'a [String],
    store: &'a RecordStore,
    clock: &'a (dyn Fn() -> DateTime<Utc> + Sync),
    outcome: Outcome,
}

impl Run<'_> {
    fn mark(&self, feed: Feed, state: MarkerState, detail: &str) -> Result<()> {
        self.store.append(&Record::Marker(Marker {
            known_time: (self.clock)(),
            feed,
            state,
            detail: detail.to_owned(),
        }))
    }

    fn failed(&mut self, feed: Feed, detail: &str) -> Result<()> {
        tracing::warn!(feed = feed.as_str(), %detail, "poll failed");
        self.outcome.errors += 1;
        self.mark(feed, MarkerState::Error, detail)
    }

    /// One book snapshot per symbol. A failed symbol gets an error marker; the rest go on.
    async fn poll_books(&mut self) -> Result<()> {
        for symbol in self.symbols {
            let book = match self.feeds.book.book(symbol).await {
                Ok(book) => book,
                Err(err) => {
                    self.failed(Feed::Book, &format!("{symbol}: {err}"))?;
                    continue;
                }
            };
            let Some(parts) = summarize_book(&book.bids, &book.asks) else {
                self.failed(Feed::Book, &format!("{symbol}: empty or crossed book"))?;
                continue;
            };
            self.store.append(&Record::Book(BookSummary {
                known_time: (self.clock)(),
                event_time: book.event_time,
                venue: self.feeds.book.venue().clone(),
                symbol: symbol.clone(),
                bid: parts.bid,
                ask: parts.ask,
                mid: parts.mid,
                spread_bps: parts.spread_bps,
                bids: parts.bids,
                asks: parts.asks,
                imbalance_1pct: parts.imbalance_1pct,
                levels: parts.levels,
            }))?;
            self.outcome.books += 1;
        }
        Ok(())
    }

    async fn poll_derivs(&mut self) -> Result<()> {
        match self
            .feeds
            .derivs
            .snapshot(self.symbols, (self.clock)())
            .await
        {
            Ok(snapshots) => {
                for snapshot in snapshots {
                    self.store.append(&Record::Derivs(snapshot))?;
                    self.outcome.derivs += 1;
                }
                Ok(())
            }
            Err(err) => self.failed(Feed::Derivs, &err.to_string()),
        }
    }

    fn on_liquidation(&mut self, event: LiquidationEvent) -> Result<()> {
        match event {
            LiquidationEvent::Up => self.mark(Feed::Liquidations, MarkerState::Up, "connected"),
            LiquidationEvent::Down(reason) => {
                self.mark(Feed::Liquidations, MarkerState::Down, &reason)
            }
            LiquidationEvent::Liquidation(liquidation) => {
                self.outcome.liquidations += 1;
                self.store.append(&Record::Liquidation(liquidation))
            }
        }
    }
}
