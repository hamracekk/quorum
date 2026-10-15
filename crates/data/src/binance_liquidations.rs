//! Binance USDT-perpetual liquidations over the public websocket (`!forceOrder@arr`), as a
//! `LiquidationFeed`. Keyless and read-only.
//!
//! Binance pushes at most one liquidation per symbol per second (the largest in that second),
//! for every symbol. Market streams live under `/market` since Binance's 2026 base-URL
//! migration; the legacy `/ws` path still accepts connections but sends nothing.
//! There is no REST history: what is not recorded live is gone, which is why each connect and
//! disconnect is reported, so the recorder can mark the gaps.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_core::stream::BoxStream;
use futures_util::{SinkExt, StreamExt};
use quorum_core::recorder::{Liquidation, LiquidationEvent, LiquidationFeed};
use quorum_core::{PortError, VenueId};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message;

use crate::binance::VENUE;
use crate::{DataError, Result};

const STREAM_URL: &str = "wss://fstream.binance.com";

/// `LiquidationFeed` adapter for Binance USDT perpetuals.
#[derive(Debug, Clone)]
pub struct BinanceLiquidations {
    venue: VenueId,
    base_url: String,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl BinanceLiquidations {
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
struct ForceOrder {
    o: Order,
}

#[derive(Debug, Deserialize)]
#[expect(non_snake_case, reason = "Binance's single-letter field names")]
struct Order {
    s: String,
    S: String,
    q: String,
    p: String,
    ap: String,
    /// Trade time, ms.
    T: i64,
}

fn number(text: &str, field: &str) -> Result<f64> {
    text.parse()
        .map_err(|_| DataError::Malformed(format!("forceOrder {field} `{text}` is not a number")))
}

/// Parses one `forceOrder` message received at `known_time`.
fn parse_message(text: &str, venue: &VenueId, known_time: DateTime<Utc>) -> Result<Liquidation> {
    let message: ForceOrder = serde_json::from_str(text)
        .map_err(|e| DataError::Malformed(format!("forceOrder message: {e}")))?;
    let o = message.o;
    Ok(Liquidation {
        known_time,
        event_time: DateTime::from_timestamp_millis(o.T)
            .ok_or_else(|| DataError::Malformed(format!("invalid trade time {}", o.T)))?,
        venue: venue.clone(),
        symbol: o.s,
        side: o.S.to_lowercase(),
        price: number(&o.p, "price")?,
        avg_price: number(&o.ap, "avg price")?,
        qty: number(&o.q, "qty")?,
    })
}

/// Keeps one connection alive, forwarding liquidations and up/down events until the consumer
/// goes away.
async fn pump(
    url: String,
    venue: VenueId,
    tx: mpsc::Sender<LiquidationEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
) {
    let mut backoff = initial_backoff;
    loop {
        let reason = match tokio_tungstenite::connect_async(url.as_str()).await {
            Ok((mut socket, _)) => {
                tracing::info!("binance liquidation stream connected");
                if tx.send(LiquidationEvent::Up).await.is_err() {
                    return;
                }
                backoff = initial_backoff;
                let mut reason = "closed by the server".to_owned();
                while let Some(message) = socket.next().await {
                    match message {
                        Ok(Message::Text(text)) => match parse_message(&text, &venue, Utc::now()) {
                            Ok(liq) => {
                                if tx.send(LiquidationEvent::Liquidation(liq)).await.is_err() {
                                    return; // consumer dropped the stream
                                }
                            }
                            Err(err) => {
                                tracing::warn!(error = %err, "skipping liquidation message");
                            }
                        },
                        Ok(Message::Ping(payload)) => {
                            if let Err(err) = socket.send(Message::Pong(payload)).await {
                                reason = format!("pong failed: {err}");
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(err) => {
                            reason = err.to_string();
                            break;
                        }
                    }
                }
                reason
            }
            Err(err) => format!("connect failed: {err}"),
        };
        tracing::warn!(%reason, "binance liquidation stream down");
        if tx.send(LiquidationEvent::Down(reason)).await.is_err() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

#[async_trait]
impl LiquidationFeed for BinanceLiquidations {
    fn venue(&self) -> &VenueId {
        &self.venue
    }

    async fn subscribe(
        &self,
    ) -> std::result::Result<BoxStream<'static, LiquidationEvent>, PortError> {
        let url = format!("{}/market/ws/!forceOrder@arr", self.base_url);
        let (tx, rx) = mpsc::channel(4_096);
        tokio::spawn(pump(
            url,
            self.venue.clone(),
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
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    use super::*;

    fn force_order(symbol: &str, side: &str, ms: i64) -> String {
        serde_json::json!({
            "e": "forceOrder", "E": ms + 5,
            "o": { "s": symbol, "S": side, "o": "LIMIT", "f": "IOC", "q": "0.014",
                   "p": "9910", "ap": "9915.5", "X": "FILLED", "l": "0.014", "z": "0.014", "T": ms }
        })
        .to_string()
    }

    fn venue() -> VenueId {
        VenueId::new("binance").expect("valid")
    }

    #[test]
    fn force_orders_become_liquidations() {
        let now = DateTime::from_timestamp(1_000, 0).expect("ts");
        let liq = parse_message(
            &force_order("BTCUSDT", "SELL", 1_568_014_460_893),
            &venue(),
            now,
        )
        .expect("parse");
        assert_eq!(liq.symbol, "BTCUSDT");
        assert_eq!(liq.side, "sell");
        assert_eq!(liq.known_time, now);
        assert_eq!(liq.event_time.timestamp_millis(), 1_568_014_460_893);
        assert!((liq.avg_price - 9915.5).abs() < 1e-9);
        assert!(parse_message("{\"o\":{}}", &venue(), now).is_err());
    }

    /// Two connections: the first sends one liquidation then drops; the second sends another.
    /// The feed reports up, the liquidation, down, up, the second liquidation — and connects to
    /// the `/market` path (the legacy `/ws` path connects but stays silent since 2026-04-23).
    #[tokio::test]
    async fn reports_ups_downs_and_liquidations_across_a_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (paths_tx, mut paths) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            for symbol in ["BTCUSDT", "ETHUSDT"] {
                let (tcp, _) = listener.accept().await.expect("accept");
                let paths_tx = paths_tx.clone();
                #[expect(
                    clippy::result_large_err,
                    reason = "tungstenite fixes the handshake callback signature"
                )]
                let record_path = move |request: &Request, response: Response| {
                    let _ = paths_tx.send(request.uri().path().to_owned()); // test bookkeeping
                    Ok(response)
                };
                let mut ws = accept_hdr_async(tcp, record_path).await.expect("handshake");
                ws.send(Message::text(force_order(symbol, "BUY", 1_000)))
                    .await
                    .expect("send");
                let _ = ws.close(None).await; // the client may already be gone
            }
        });
        let feed = BinanceLiquidations::with_base_url(format!("ws://{addr}"))
            .expect("adapter")
            .with_backoff(Duration::from_millis(10), Duration::from_millis(50));
        let events: Vec<LiquidationEvent> = feed
            .subscribe()
            .await
            .expect("subscribe")
            .take(5)
            .collect()
            .await;

        assert_eq!(events[0], LiquidationEvent::Up);
        assert!(matches!(&events[1], LiquidationEvent::Liquidation(l) if l.symbol == "BTCUSDT"));
        assert!(matches!(events[2], LiquidationEvent::Down(_)));
        assert_eq!(events[3], LiquidationEvent::Up);
        assert!(matches!(&events[4], LiquidationEvent::Liquidation(l) if l.side == "buy"));
        assert_eq!(
            paths.recv().await.as_deref(),
            Some("/market/ws/!forceOrder@arr")
        );
    }
}
