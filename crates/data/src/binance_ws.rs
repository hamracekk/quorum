//! Binance spot live klines over the public websocket (no API key), as a `MarketStream`.
//!
//! One combined stream carries every subscribed instrument. Only **closed** klines are
//! yielded. On disconnect the adapter reconnects with exponential backoff; bars that closed
//! while disconnected are *not* replayed here — the consumer backfills gaps via REST.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::DateTime;
use futures_core::stream::BoxStream;
use futures_util::{SinkExt, StreamExt};
use quorum_core::{
    Bar, InstrumentId, InstrumentKind, MarketStream, PortError, StreamedBar, VenueId,
};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message;

use crate::binance::VENUE;
use crate::{DataError, Result};

const STREAM_URL: &str = "wss://stream.binance.com:9443";

/// `MarketStream` adapter for Binance spot klines.
#[derive(Debug, Clone)]
pub struct BinanceStream {
    venue: VenueId,
    base_url: String,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl BinanceStream {
    pub fn new() -> Result<Self> {
        Self::with_base_url(STREAM_URL)
    }

    /// Point at a different host, e.g. a local test server (`ws://127.0.0.1:port`).
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            venue: VenueId::new(VENUE).map_err(|e| DataError::Malformed(e.to_string()))?,
            base_url: base_url.into(),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_mins(1),
        })
    }

    /// Reconnect delays: start at `initial`, double per failure, cap at `max`.
    #[must_use]
    pub const fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max;
        self
    }
}

#[derive(Debug, Deserialize)]
struct Combined {
    stream: String,
    data: KlineEvent,
}

#[derive(Debug, Deserialize)]
struct KlineEvent {
    k: Kline,
}

#[derive(Debug, Deserialize)]
struct Kline {
    /// Open time, ms.
    t: i64,
    o: String,
    h: String,
    l: String,
    c: String,
    v: String,
    /// True once the kline has closed.
    x: bool,
}

fn number(text: &str, field: &str) -> Result<f64> {
    text.parse()
        .map_err(|_| DataError::Malformed(format!("kline {field} `{text}` is not a number")))
}

/// Parse one combined-stream message into `(stream name, bar)`; `None` for klines still forming.
fn parse_message(text: &str) -> Result<Option<(String, Bar)>> {
    let message: Combined = serde_json::from_str(text)
        .map_err(|e| DataError::Malformed(format!("stream message: {e}")))?;
    let k = message.data.k;
    if !k.x {
        return Ok(None);
    }
    let bar = Bar {
        ts: DateTime::from_timestamp_millis(k.t)
            .ok_or_else(|| DataError::Malformed(format!("invalid open time {}", k.t)))?,
        open: number(&k.o, "open")?,
        high: number(&k.h, "high")?,
        low: number(&k.l, "low")?,
        close: number(&k.c, "close")?,
        volume: number(&k.v, "volume")?,
    };
    Ok(Some((message.stream, bar)))
}

/// Keep one websocket connection alive, forwarding closed bars until the consumer goes away.
async fn pump(
    url: String,
    streams: HashMap<String, InstrumentId>,
    tx: mpsc::Sender<StreamedBar>,
    initial_backoff: Duration,
    max_backoff: Duration,
) {
    let mut backoff = initial_backoff;
    loop {
        match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((mut socket, _)) => {
                tracing::info!(streams = streams.len(), "binance stream connected");
                while let Some(message) = socket.next().await {
                    match message {
                        Ok(Message::Text(text)) => match parse_message(&text) {
                            Ok(Some((name, bar))) => {
                                backoff = initial_backoff;
                                if let Some(instrument) = streams.get(&name)
                                    && tx.send(Ok((instrument.clone(), bar))).await.is_err()
                                {
                                    return; // consumer dropped the stream
                                }
                            }
                            Ok(None) => backoff = initial_backoff,
                            Err(err) => tracing::warn!(error = %err, "skipping stream message"),
                        },
                        Ok(Message::Ping(payload)) => {
                            if let Err(err) = socket.send(Message::Pong(payload)).await {
                                tracing::warn!(error = %err, "pong failed");
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "binance stream error");
                            break;
                        }
                    }
                }
                tracing::warn!("binance stream disconnected");
            }
            Err(err) => tracing::warn!(error = %err, "binance stream connect failed"),
        }
        if tx.is_closed() {
            return;
        }
        tracing::info!(
            delay_ms = backoff.as_millis(),
            "reconnecting binance stream"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

#[async_trait]
impl MarketStream for BinanceStream {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn subscribe(
        &self,
        instruments: &[InstrumentId],
        interval: &str,
    ) -> std::result::Result<BoxStream<'static, StreamedBar>, PortError> {
        let mut streams = HashMap::new();
        for instrument in instruments {
            if instrument.venue() != &self.venue || instrument.kind() != InstrumentKind::Spot {
                return Err(PortError::Unsupported(instrument.to_string()));
            }
            let name = format!("{}@kline_{interval}", instrument.symbol().to_lowercase());
            streams.insert(name, instrument.clone());
        }
        if streams.is_empty() {
            return Err(PortError::Invalid("no instruments to stream".into()));
        }
        let mut names: Vec<&str> = streams.keys().map(String::as_str).collect();
        names.sort_unstable();
        let url = format!("{}/stream?streams={}", self.base_url, names.join("/"));
        let (tx, rx) = mpsc::channel(1_024);
        tokio::spawn(pump(
            url,
            streams,
            tx,
            self.initial_backoff,
            self.max_backoff,
        ));
        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    use super::*;

    const HOUR: i64 = 3_600_000;

    fn kline(symbol: &str, hour: i64, close: f64, closed: bool) -> String {
        serde_json::json!({
            "stream": format!("{}@kline_1h", symbol.to_lowercase()),
            "data": { "e": "kline", "s": symbol, "k": {
                "t": hour * HOUR, "T": hour * HOUR + HOUR - 1, "s": symbol, "i": "1h",
                "o": "1", "h": "2", "l": "0.5", "c": close.to_string(), "v": "10", "x": closed
            }}
        })
        .to_string()
    }

    fn btc() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
    }

    #[test]
    fn only_closed_klines_become_bars() {
        let (name, bar) = parse_message(&kline("BTCUSDT", 3, 1.5, true))
            .expect("parse")
            .expect("closed");
        assert_eq!(name, "btcusdt@kline_1h");
        assert_eq!(bar.ts.timestamp_millis(), 3 * HOUR);
        assert!((bar.close - 1.5).abs() < 1e-12);
        assert!(
            parse_message(&kline("BTCUSDT", 4, 1.0, false))
                .expect("parse")
                .is_none()
        );
        assert!(parse_message("{\"nope\":1}").is_err());
    }

    #[tokio::test]
    async fn refuses_other_venues_and_kinds() {
        let stream = BinanceStream::new().expect("adapter");
        for text in ["hyperliquid:BTC", "binance:BTCUSDT:perp"] {
            let instrument: InstrumentId = text.parse().expect("valid instrument");
            assert!(matches!(
                stream.subscribe(&[instrument], "1h").await,
                Err(PortError::Unsupported(_))
            ));
        }
    }

    /// Serve two connections: the first sends a forming and a closed kline then drops; the
    /// second sends one more closed kline. The adapter must yield both closed bars.
    #[tokio::test]
    async fn yields_closed_bars_and_reconnects_after_a_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let batches = [
                vec![
                    kline("BTCUSDT", 1, 1.0, false),
                    kline("BTCUSDT", 1, 1.1, true),
                ],
                vec![kline("BTCUSDT", 2, 1.2, true)],
            ];
            for batch in batches {
                let (tcp, _) = listener.accept().await.expect("accept");
                let mut ws = accept_async(tcp).await.expect("handshake");
                for message in batch {
                    ws.send(Message::text(message)).await.expect("send");
                }
                let _ = ws.close(None).await; // the client may already be gone
            }
        });

        let stream = BinanceStream::with_base_url(format!("ws://{addr}"))
            .expect("adapter")
            .with_backoff(Duration::from_millis(10), Duration::from_millis(50));
        let mut bars = stream.subscribe(&[btc()], "1h").await.expect("subscribe");

        let first = bars.next().await.expect("first").expect("ok");
        let second = bars.next().await.expect("second").expect("ok");
        assert_eq!(first.0, btc());
        assert!((first.1.close - 1.1).abs() < 1e-12);
        assert_eq!(second.1.ts.timestamp_millis(), 2 * HOUR);
    }
}
