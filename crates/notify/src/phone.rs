//! Phone alerts through ntfy : a notification becomes a push on the operator's phone.
//!
//! ntfy (ntfy.sh or a self-hosted server) has free iOS/Android apps; publishing is one HTTPS
//! POST. The **topic is a secret** — whoever knows it can read the alerts — so it lives in the
//! keychain (`quorum secrets set ntfy-topic`), never in config or logs, and travels only in the
//! request body (the URL is the server's root), so an error can never print it. An access token
//! for a protected server is optional (`ntfy-token`). Only notifications at or above
//! `min_severity` are sent. Best effort, like every notifier: a failure is logged by the fan-out
//! and never stops trading.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use quorum_core::{Notification, Notifier, PortError, Severity};
use serde::{Deserialize, Serialize};

/// `[phone]` in `config/notify.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhoneSettings {
    pub enabled: bool,
    /// The ntfy server (its root URL).
    pub server: String,
    /// Send only notifications at or above this.
    pub min_severity: Severity,
}

impl Default for PhoneSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            server: "https://ntfy.sh".into(),
            min_severity: Severity::Warning,
        }
    }
}

/// The topic and optional token, read when an alert is sent.
#[derive(Clone, PartialEq, Eq)]
pub struct PhoneKeys {
    pub topic: String,
    pub token: Option<String>,
}

impl fmt::Debug for PhoneKeys {
    // Never print them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PhoneKeys(<secret>)")
    }
}

type KeySource = Box<dyn Fn() -> Option<PhoneKeys> + Send + Sync>;

/// Publishes notifications to an ntfy topic.
pub struct PhoneNotifier {
    client: reqwest::Client,
    server: String,
    min: Severity,
    keys: KeySource,
}

impl fmt::Debug for PhoneNotifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhoneNotifier")
            .field("server", &self.server)
            .field("min", &self.min)
            .finish_non_exhaustive()
    }
}

/// A topic ntfy accepts: 1–64 of letters, digits, `-` and `_`.
pub fn valid_topic(topic: &str) -> bool {
    (1..=64).contains(&topic.len())
        && topic
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl PhoneNotifier {
    /// `keys` is asked for the topic and token each time an alert goes out.
    pub fn new(settings: &PhoneSettings, keys: KeySource) -> Result<Self, PortError> {
        let client = reqwest::Client::builder()
            .user_agent("quorum/0.1")
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| PortError::adapter("phone notifier", e))?;
        Ok(Self {
            client,
            server: settings.server.trim_end_matches('/').to_owned(),
            min: settings.min_severity,
            keys,
        })
    }

    /// Topic and token from `read` (`ntfy-topic`, `ntfy-token`), e.g. the keychain.
    pub fn from_secrets(
        settings: &PhoneSettings,
        read: fn(&str) -> Option<String>,
    ) -> Result<Self, PortError> {
        Self::new(
            settings,
            Box::new(move || {
                Some(PhoneKeys {
                    topic: read("ntfy-topic")?,
                    token: read("ntfy-token"),
                })
            }),
        )
    }
}

/// The JSON ntfy takes on its root URL.
pub fn message(topic: &str, n: &Notification) -> serde_json::Value {
    let (priority, tag) = match n.severity {
        Severity::Info => (3, "chart_with_upwards_trend"),
        Severity::Warning => (4, "warning"),
        Severity::Critical => (5, "rotating_light"),
    };
    serde_json::json!({
        "topic": topic,
        "title": n.title,
        "message": n.body,
        "priority": priority,
        "tags": [tag],
    })
}

#[async_trait]
impl Notifier for PhoneNotifier {
    fn name(&self) -> &'static str {
        "phone"
    }

    async fn notify(&self, n: &Notification) -> Result<(), PortError> {
        if n.severity < self.min {
            return Ok(());
        }
        let keys = (self.keys)().ok_or_else(|| {
            PortError::Invalid(
                "no ntfy topic: pick a long random one and run `quorum secrets set ntfy-topic`"
                    .into(),
            )
        })?;
        if !valid_topic(&keys.topic) {
            return Err(PortError::Invalid(
                "the ntfy topic must be 1–64 letters, digits, - or _".into(),
            ));
        }
        let mut request = self
            .client
            .post(format!("{}/", self.server))
            .json(&message(&keys.topic, n));
        if let Some(token) = &keys.token {
            request = request.bearer_auth(token);
        }
        request
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map(drop)
            .map_err(|e| PortError::adapter("ntfy publish", e.without_url()))
    }
}

#[cfg(test)]
mod tests;
