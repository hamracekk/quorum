use std::sync::Arc;

use tokio::sync::broadcast;
use trader_core::{Envelope, Event, EventLog, PortError};

/// In-process event bus: every event is appended to the event log **first**, then broadcast
/// to subscribers, so a subscriber never sees an event the log doesn't have.
///
/// Subscribers that fall more than `capacity` events behind receive
/// `RecvError::Lagged`; they must catch up by reading the log from their last `seq`.
#[derive(Debug, Clone)]
pub struct Bus {
    log: Arc<dyn EventLog>,
    tx: broadcast::Sender<Envelope>,
}

impl Bus {
    pub const DEFAULT_CAPACITY: usize = 4_096;

    pub fn new(log: Arc<dyn EventLog>, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { log, tx }
    }

    /// Persist `event`, then broadcast it. Fails only if the log write fails, in which case
    /// nothing is broadcast.
    pub fn publish(&self, event: &Event) -> Result<Envelope, PortError> {
        let envelope = self.log.append(event)?;
        // No subscribers is not an error: the event is already durable in the log.
        let _ = self.tx.send(envelope.clone());
        Ok(envelope)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.tx.subscribe()
    }

    pub fn log(&self) -> &dyn EventLog {
        self.log.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use trader_core::fakes::MemoryEventLog;

    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn halt(hours: i64) -> Event {
        Event::RiskHalt {
            ts: at(hours),
            equity: 1.0,
        }
    }

    #[tokio::test]
    async fn subscribers_receive_events_after_they_are_logged() {
        let bus = Bus::new(Arc::new(MemoryEventLog::default()), 8);
        let mut rx = bus.subscribe();

        let published = bus.publish(&halt(1)).expect("publish");
        let received = rx.recv().await.expect("receive");

        assert_eq!(received, published);
        let logged = bus.log().read(at(0), at(2)).expect("read");
        assert_eq!(logged, vec![published]);
    }

    #[test]
    fn publishing_without_subscribers_still_logs() {
        let bus = Bus::new(Arc::new(MemoryEventLog::default()), 8);
        bus.publish(&halt(1)).expect("publish");
        bus.publish(&halt(2)).expect("publish");
        assert_eq!(bus.log().read(at(0), at(3)).expect("read").len(), 2);
    }

    #[tokio::test]
    async fn slow_subscribers_are_told_they_lagged() {
        let bus = Bus::new(Arc::new(MemoryEventLog::default()), 2);
        let mut rx = bus.subscribe();
        for hour in 0..5 {
            bus.publish(&halt(hour)).expect("publish");
        }
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
