//! `config/recorder.toml`: what to record and how often.

use std::collections::BTreeSet;
use std::path::Path;

use quorum_slots::{SlotConfig, SlotId, Universe};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum RecorderConfigError {
    #[error("cannot read recorder config `{path}`: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid recorder config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid recorder config: {0}")]
    Invalid(String),
}

/// Shortest allowed poll interval: stays far inside Binance's request-weight limits.
const MIN_EVERY_SECS: u64 = 60;

/// What the recorder records. Unknown keys are errors.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderConfig {
    /// Record the instruments of these slots (their fixed universes).
    #[serde(default)]
    pub slots: Vec<SlotId>,
    /// More Binance symbols to record, e.g. `BTCUSDT`.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Seconds between order-book snapshots (0 = off).
    pub book_every_secs: u64,
    /// Seconds between derivatives snapshots (0 = off).
    pub derivs_every_secs: u64,
    /// Record liquidations (every perpetual: the stream carries them all).
    pub liquidations: bool,
}

impl RecorderConfig {
    pub fn load(path: &Path) -> Result<Self, RecorderConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| RecorderConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, RecorderConfigError> {
        let config: Self = toml::from_str(text)?;
        for (name, every) in [
            ("book_every_secs", config.book_every_secs),
            ("derivs_every_secs", config.derivs_every_secs),
        ] {
            if every != 0 && every < MIN_EVERY_SECS {
                return Err(RecorderConfigError::Invalid(format!(
                    "{name} = {every}: at least {MIN_EVERY_SECS} (or 0 for off)"
                )));
            }
        }
        let valid = |s: &String| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
        if let Some(bad) = config.symbols.iter().find(|s| !valid(s)) {
            return Err(RecorderConfigError::Invalid(format!("symbol `{bad}`")));
        }
        Ok(config)
    }

    /// The Binance symbols to record: the listed slots' fixed universes plus `symbols`,
    /// deduplicated and sorted. `slots` must hold the config of every listed slot.
    pub fn resolve(&self, slots: &[SlotConfig]) -> Result<Vec<String>, RecorderConfigError> {
        let mut symbols: BTreeSet<String> = self.symbols.iter().cloned().collect();
        for id in &self.slots {
            let slot = slots
                .iter()
                .find(|s| &s.id == id)
                .ok_or_else(|| RecorderConfigError::Invalid(format!("slot `{id}` not loaded")))?;
            if slot.venue.as_str() != "binance" {
                return Err(RecorderConfigError::Invalid(format!(
                    "slot `{id}` is on `{}`; the recorder reads binance only",
                    slot.venue.as_str()
                )));
            }
            match &slot.universe {
                Universe::Fixed { instruments } => {
                    symbols.extend(instruments.iter().map(|i| i.symbol().to_owned()));
                }
                Universe::TopVolume { .. } => {
                    return Err(RecorderConfigError::Invalid(format!(
                        "slot `{id}` has a top-volume universe; list its symbols instead"
                    )));
                }
            }
        }
        Ok(symbols.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
slots = ["crypto-spot"]
symbols = ["BTCUSDT", "PEPEUSDT"]
book_every_secs = 300
derivs_every_secs = 300
liquidations = true
"#;

    #[test]
    fn symbols_come_from_slots_and_the_list_deduplicated() {
        let config = RecorderConfig::parse(CONFIG).expect("valid");
        let slot = SlotConfig::parse(include_str!("../../../config/slots/crypto-spot.toml"))
            .expect("shipped slot");
        let symbols = config.resolve(&[slot]).expect("resolved");
        assert_eq!(symbols.len(), 26, "25 slot coins + PEPEUSDT; BTCUSDT once");
        assert!(symbols.windows(2).all(|w| w[0] < w[1]), "sorted, unique");
    }

    #[test]
    fn the_shipped_config_is_valid() {
        RecorderConfig::parse(include_str!("../../../config/recorder.toml")).expect("valid");
    }

    #[test]
    fn too_frequent_polls_and_bad_symbols_are_rejected() {
        let fast = CONFIG.replace("book_every_secs = 300", "book_every_secs = 5");
        assert!(RecorderConfig::parse(&fast).is_err());
        let off = CONFIG.replace("book_every_secs = 300", "book_every_secs = 0");
        assert!(RecorderConfig::parse(&off).is_ok(), "0 switches a feed off");
        let bad = CONFIG.replace("PEPEUSDT", "../x");
        assert!(RecorderConfig::parse(&bad).is_err());
        assert!(RecorderConfig::parse(&format!("{CONFIG}\nextra = 1")).is_err());
    }

    #[test]
    fn unloaded_slots_are_an_error() {
        let config = RecorderConfig::parse(CONFIG).expect("valid");
        assert!(config.resolve(&[]).is_err());
    }
}
