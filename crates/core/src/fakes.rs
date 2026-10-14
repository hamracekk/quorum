//! In-memory implementations of the ports for deterministic tests in any crate.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Poll};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_core::Stream;
use futures_core::stream::BoxStream;

use crate::chat::{ChatReply, ChatRequest};
use crate::events::{Envelope, Event};
use crate::instrument::{InstrumentId, VenueId};
use crate::news::NewsItem;
use crate::ports::{
    BarStore, ChatModel, EventLog, LlmProvider, LlmRequest, LlmResponse, MarketData, MarketStream,
    NewsSource, Notification, Notifier, PortError, PredictionSource, StreamedBar, TraderSource,
};
use crate::predictions::{PredictionMarket, ProbabilityPoint};
use crate::traders::{LeaderboardEntry, TraderAccount, TraderFill};
use crate::types::{Bar, Series};

/// A recorded `MarketData::bars` call: instrument, start, end.
pub type BarsRequest = (InstrumentId, DateTime<Utc>, DateTime<Utc>);

/// Serves fixed bars for one venue, ignoring the interval.
#[derive(Debug)]
pub struct MemoryMarketData {
    venue: VenueId,
    bars: BTreeMap<InstrumentId, Vec<Bar>>,
    requests: Mutex<Vec<BarsRequest>>,
}

impl MemoryMarketData {
    pub const fn new(venue: VenueId) -> Self {
        Self {
            venue,
            bars: BTreeMap::new(),
            requests: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn with_bars(mut self, instrument: InstrumentId, bars: Vec<Bar>) -> Self {
        self.bars.insert(instrument, bars);
        self
    }

    /// Every `(instrument, start, end)` requested so far, in order.
    pub fn requests(&self) -> Vec<BarsRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl MarketData for MemoryMarketData {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn bars(
        &self,
        instrument: &InstrumentId,
        _interval: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Bar>, PortError> {
        if instrument.venue() != &self.venue {
            return Err(PortError::Unsupported(instrument.to_string()));
        }
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((instrument.clone(), start, end));
        Ok(self
            .bars
            .get(instrument)
            .map(|bars| {
                bars.iter()
                    .filter(|b| b.ts >= start && b.ts < end)
                    .copied()
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// Keeps series in memory, keyed by instrument and interval.
#[derive(Debug, Default)]
pub struct MemoryBarStore {
    series: Mutex<BTreeMap<(InstrumentId, String), Series>>,
}

impl BarStore for MemoryBarStore {
    fn load(&self, instrument: &InstrumentId, interval: &str) -> Result<Option<Series>, PortError> {
        Ok(self
            .series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(instrument.clone(), interval.to_owned()))
            .cloned())
    }

    fn save(&self, series: &Series, interval: &str) -> Result<(), PortError> {
        if !series.bars.windows(2).all(|w| w[0].ts < w[1].ts) {
            return Err(PortError::Invalid("bars are unsorted or duplicated".into()));
        }
        self.series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (series.instrument.clone(), interval.to_owned()),
                series.clone(),
            );
        Ok(())
    }
}

/// Yields a fixed list of closed bars, then ends (or, with [`keep_open`](Self::keep_open),
/// stays silent forever like a quiet market) — a stand-in for a live websocket feed.
#[derive(Debug, Clone)]
pub struct MemoryMarketStream {
    venue: VenueId,
    bars: Vec<(InstrumentId, Bar)>,
    keep_open: bool,
}

impl MemoryMarketStream {
    pub const fn new(venue: VenueId, bars: Vec<(InstrumentId, Bar)>) -> Self {
        Self {
            venue,
            bars,
            keep_open: false,
        }
    }

    /// After the bars, stay pending instead of ending the stream.
    #[must_use]
    pub const fn keep_open(mut self) -> Self {
        self.keep_open = true;
        self
    }
}

/// `Stream` over a queue, without pulling in a stream-utilities crate.
struct Queue {
    items: VecDeque<StreamedBar>,
    keep_open: bool,
}

impl Stream for Queue {
    type Item = StreamedBar;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.items.pop_front() {
            Some(item) => Poll::Ready(Some(item)),
            // Never woken: nothing more will arrive, exactly like an idle feed.
            None if self.keep_open => Poll::Pending,
            None => Poll::Ready(None),
        }
    }
}

#[async_trait]
impl MarketStream for MemoryMarketStream {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn subscribe(
        &self,
        instruments: &[InstrumentId],
        _interval: &str,
    ) -> Result<BoxStream<'static, StreamedBar>, PortError> {
        let items = self
            .bars
            .iter()
            .filter(|(instrument, _)| instruments.contains(instrument))
            .cloned()
            .map(Ok)
            .collect();
        Ok(Box::pin(Queue {
            items,
            keep_open: self.keep_open,
        }))
    }
}

/// Keeps the event log in memory; sequence numbers start at 1.
#[derive(Debug, Default)]
pub struct MemoryEventLog {
    entries: Mutex<Vec<Envelope>>,
}

impl EventLog for MemoryEventLog {
    fn append(&self, event: &Event) -> Result<Envelope, PortError> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let envelope = Envelope {
            seq: entries.len() as u64 + 1,
            event: event.clone(),
        };
        entries.push(envelope.clone());
        drop(entries);
        Ok(envelope)
    }

    fn read(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Envelope>, PortError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|e| e.event.ts() >= start && e.event.ts() < end)
            .cloned()
            .collect())
    }
}

/// A news feed serving fixed items (or always failing), counting fetches.
#[derive(Debug, Default)]
pub struct MemoryNewsSource {
    name: String,
    items: Vec<NewsItem>,
    fail: bool,
    fetches: Mutex<usize>,
}

impl MemoryNewsSource {
    pub fn new(name: &str, items: Vec<NewsItem>) -> Self {
        Self {
            name: name.to_owned(),
            items,
            ..Self::default()
        }
    }

    /// A source whose every fetch fails, like a feed that is down.
    pub fn failing(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            fail: true,
            ..Self::default()
        }
    }

    pub fn fetches(&self) -> usize {
        *self.fetches.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl NewsSource for MemoryNewsSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn fetch(&self) -> Result<Vec<NewsItem>, PortError> {
        *self.fetches.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        if self.fail {
            Err(PortError::Invalid(format!("{} is down", self.name)))
        } else {
            Ok(self.items.clone())
        }
    }
}

/// A language model that gives the same answer to every request (or always fails), counting
/// calls and keeping the requests it saw.
#[derive(Debug, Default)]
pub struct MemoryLlm {
    name: String,
    model: String,
    answer: Option<String>,
    /// `(marker, text)`: answer `text` when the system prompt contains `marker`.
    routes: Vec<(String, String)>,
    tokens: u64,
    requests: Mutex<Vec<LlmRequest>>,
}

impl MemoryLlm {
    /// Answers `text`, reporting `tokens` input tokens and `tokens / 10` output tokens.
    pub fn answering(name: &str, text: &str, tokens: u64) -> Self {
        Self {
            name: name.to_owned(),
            model: format!("{name}-model"),
            answer: Some(text.to_owned()),
            tokens,
            ..Self::default()
        }
    }

    /// Also answers `text` to requests whose system prompt contains `marker` (one fake serving
    /// two different prompts, e.g. news readings and theme readings).
    #[must_use]
    pub fn or_when(mut self, marker: &str, text: &str) -> Self {
        self.routes.push((marker.to_owned(), text.to_owned()));
        self
    }

    /// Every request fails, like a model server that is down.
    pub fn failing(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            model: format!("{name}-model"),
            ..Self::default()
        }
    }

    pub fn calls(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl LlmProvider for MemoryLlm {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, PortError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let routed = self
            .routes
            .iter()
            .find(|(marker, _)| request.system.contains(marker.as_str()))
            .map(|(_, text)| text);
        routed.or(self.answer.as_ref()).map_or_else(
            || Err(PortError::Invalid(format!("{} is down", self.name))),
            |text| {
                Ok(LlmResponse {
                    text: text.clone(),
                    input_tokens: self.tokens,
                    output_tokens: self.tokens / 10,
                })
            },
        )
    }
}

/// Records every notification it is given.
#[derive(Debug, Default)]
pub struct MemoryNotifier {
    sent: Mutex<Vec<Notification>>,
}

impl MemoryNotifier {
    /// Everything notified so far, in order.
    pub fn sent(&self) -> Vec<Notification> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Notifier for MemoryNotifier {
    fn name(&self) -> &'static str {
        "memory"
    }

    async fn notify(&self, notification: &Notification) -> Result<(), PortError> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(notification.clone());
        Ok(())
    }
}

/// A prediction-market exchange in memory: markets with their probability history.
#[derive(Debug, Default)]
pub struct MemoryPredictions {
    markets: BTreeMap<String, (PredictionMarket, Vec<ProbabilityPoint>)>,
}

impl MemoryPredictions {
    #[must_use]
    pub fn with_market(mut self, market: PredictionMarket, history: Vec<ProbabilityPoint>) -> Self {
        self.markets
            .insert(market.ticker.clone(), (market, history));
        self
    }
}

#[async_trait]
impl PredictionSource for MemoryPredictions {
    fn venue(&self) -> &'static str {
        "kalshi"
    }

    async fn market(&self, ticker: &str) -> Result<PredictionMarket, PortError> {
        self.markets
            .get(ticker)
            .map(|(market, _)| market.clone())
            .ok_or_else(|| PortError::Invalid(format!("no market `{ticker}`")))
    }

    async fn history(
        &self,
        ticker: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProbabilityPoint>, PortError> {
        let (_, history) = self
            .markets
            .get(ticker)
            .ok_or_else(|| PortError::Invalid(format!("no market `{ticker}`")))?;
        Ok(history
            .iter()
            .filter(|p| p.ts >= start && p.ts <= end)
            .copied()
            .collect())
    }
}

/// A venue with public accounts in memory: accounts with their fills.
#[derive(Debug, Default)]
pub struct MemoryTraders {
    accounts: BTreeMap<String, (TraderAccount, Vec<TraderFill>)>,
    leaderboard: Vec<LeaderboardEntry>,
    leaderboard_reads: Mutex<usize>,
}

impl MemoryTraders {
    #[must_use]
    pub fn with_account(mut self, account: TraderAccount, fills: Vec<TraderFill>) -> Self {
        self.accounts
            .insert(account.address.clone(), (account, fills));
        self
    }

    #[must_use]
    pub fn with_leaderboard(mut self, rows: Vec<LeaderboardEntry>) -> Self {
        self.leaderboard = rows;
        self
    }

    /// How often the leaderboard was downloaded.
    pub fn leaderboard_reads(&self) -> usize {
        *self
            .leaderboard_reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl TraderSource for MemoryTraders {
    fn venue(&self) -> &'static str {
        "hyperliquid"
    }

    async fn account(&self, address: &str) -> Result<TraderAccount, PortError> {
        self.accounts
            .get(address)
            .map(|(account, _)| account.clone())
            .ok_or_else(|| PortError::Invalid(format!("no account `{address}`")))
    }

    async fn fills(
        &self,
        address: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<TraderFill>, PortError> {
        let (_, fills) = self
            .accounts
            .get(address)
            .ok_or_else(|| PortError::Invalid(format!("no account `{address}`")))?;
        Ok(fills
            .iter()
            .filter(|f| f.ts >= start && f.ts <= end)
            .cloned()
            .collect())
    }

    async fn leaderboard(&self) -> Result<Vec<LeaderboardEntry>, PortError> {
        *self
            .leaderboard_reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        Ok(self.leaderboard.clone())
    }
}

/// A chat model that plays back scripted replies, recording every request it got.
#[derive(Debug, Default)]
pub struct ScriptedChat {
    replies: Mutex<VecDeque<ChatReply>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedChat {
    pub fn new(replies: Vec<ChatReply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ChatModel for ScriptedChat {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn model(&self) -> &'static str {
        "script"
    }

    async fn chat(&self, request: &ChatRequest) -> Result<ChatReply, PortError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        self.replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| PortError::Invalid("the script ran out of replies".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn bar(hour: i64) -> Bar {
        Bar {
            ts: at(hour),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }
    }

    fn id(venue: &str) -> InstrumentId {
        InstrumentId::spot(venue, "A").expect("valid instrument")
    }

    #[tokio::test]
    async fn market_data_serves_a_half_open_range_and_records_requests() {
        let data = MemoryMarketData::new(VenueId::new("test").expect("valid venue"))
            .with_bars(id("test"), (0..5).map(bar).collect());
        let bars = data
            .bars(&id("test"), "1h", at(1), at(3))
            .await
            .expect("bars");
        let hours: Vec<_> = bars.iter().map(|b| b.ts).collect();
        assert_eq!(hours, vec![at(1), at(2)]);
        assert_eq!(data.requests(), vec![(id("test"), at(1), at(3))]);
    }

    #[tokio::test]
    async fn market_data_rejects_other_venues() {
        let data = MemoryMarketData::new(VenueId::new("test").expect("valid venue"));
        let err = data
            .bars(&id("other"), "1h", at(0), at(1))
            .await
            .expect_err("wrong venue");
        assert!(matches!(err, PortError::Unsupported(_)));
    }

    #[test]
    fn bar_store_round_trips_and_rejects_unsorted_series() {
        let store = MemoryBarStore::default();
        let series = Series::new(id("test"), vec![bar(0), bar(1)]);
        store.save(&series, "1h").expect("save");
        assert_eq!(store.load(&id("test"), "1h").expect("load"), Some(series));
        assert!(store.load(&id("test"), "4h").expect("load").is_none());

        let unsorted = Series::new(id("test"), vec![bar(1), bar(0)]);
        assert!(matches!(
            store.save(&unsorted, "1h"),
            Err(PortError::Invalid(_))
        ));
    }

    #[test]
    fn event_log_assigns_increasing_sequence_numbers_and_filters_by_time() {
        let log = MemoryEventLog::default();
        for (hour, seq) in (0..4).zip(1_u64..) {
            let event = Event::RiskHalt {
                ts: at(hour),
                equity: 1.0,
            };
            assert_eq!(log.append(&event).expect("append").seq, seq);
        }
        let seqs: Vec<u64> = log
            .read(at(1), at(3))
            .expect("read")
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![2, 3]);
    }
}
