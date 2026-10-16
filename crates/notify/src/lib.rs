//! Operator notifications (`core::Notifier`): phone alerts through ntfy, the log, and a
//! best-effort fan-out.
//!
//! **Best effort, always.** [`Fanout`] logs a failing channel and carries on: a notification
//! never stops trading or recording. Messages are built from quorum's own data and never contain
//! secrets.

mod phone;

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use quorum_core::{Notification, Notifier, PortError, Severity};
use serde::Deserialize;

pub use phone::{PhoneKeys, PhoneNotifier, PhoneSettings, message, valid_topic};

/// `config/notify.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    #[serde(default)]
    pub phone: PhoneSettings,
}

impl NotifyConfig {
    /// Reads the config; a missing file means defaults (phone off).
    pub fn load(path: &Path) -> Result<Self, PortError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| PortError::adapter(format!("{}", path.display()), e))?;
        toml::from_str(&text).map_err(|e| PortError::Invalid(format!("{}: {e}", path.display())))
    }
}

/// Writes notifications to the log (`tracing`), at a level matching their severity.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogNotifier;

#[async_trait]
impl Notifier for LogNotifier {
    fn name(&self) -> &'static str {
        "log"
    }

    async fn notify(&self, n: &Notification) -> Result<(), PortError> {
        match n.severity {
            Severity::Info => tracing::info!(title = %n.title, body = %n.body, "notification"),
            Severity::Warning => tracing::warn!(title = %n.title, body = %n.body, "notification"),
            Severity::Critical => tracing::error!(title = %n.title, body = %n.body, "notification"),
        }
        Ok(())
    }
}

/// Sends to every channel; a failing one is logged, never returned.
#[derive(Debug, Clone, Default)]
pub struct Fanout {
    channels: Vec<Arc<dyn Notifier>>,
}

impl Fanout {
    #[must_use]
    pub fn new(channels: Vec<Arc<dyn Notifier>>) -> Self {
        Self { channels }
    }
}

#[async_trait]
impl Notifier for Fanout {
    fn name(&self) -> &'static str {
        "fanout"
    }

    async fn notify(&self, n: &Notification) -> Result<(), PortError> {
        for channel in &self.channels {
            if let Err(err) = channel.notify(n).await {
                tracing::warn!(channel = channel.name(), error = %err, "notification not delivered");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use quorum_core::fakes::MemoryNotifier;

    use super::*;

    #[derive(Debug)]
    struct Broken;

    #[async_trait]
    impl Notifier for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }

        async fn notify(&self, _: &Notification) -> Result<(), PortError> {
            Err(PortError::Invalid("down".into()))
        }
    }

    #[tokio::test]
    async fn a_failing_channel_never_stops_the_others() {
        let memory = Arc::new(MemoryNotifier::default());
        let fanout = Fanout::new(vec![
            Arc::new(Broken),
            memory.clone(),
            Arc::new(LogNotifier),
        ]);
        let n = Notification {
            ts: DateTime::from_timestamp(0, 0).expect("ts"),
            severity: Severity::Warning,
            title: "t".into(),
            body: "b".into(),
        };
        fanout.notify(&n).await.expect("best effort never fails");
        assert_eq!(memory.sent().len(), 1);
    }

    #[test]
    fn the_shipped_config_parses_and_a_missing_one_means_off() {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/notify.toml");
        NotifyConfig::load(&shipped).expect("config/notify.toml");
        let none = NotifyConfig::load(Path::new("/nonexistent/notify.toml")).expect("defaults");
        assert!(!none.phone.enabled);
    }
}
