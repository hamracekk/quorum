//! A slot's configuration, parsed from the operator's TOML with unknown keys rejected.

use std::fmt;
use std::path::Path;

use quorum_core::backtest::BacktestConfig;
use quorum_core::instrument::{InstrumentId, VenueId};
use quorum_core::strategies::UniverseRule;
use quorum_envelope::{Envelope, Mode};
use serde::{Deserialize, Serialize};

/// Slot identifier, e.g. `crypto-spot`: lowercase letters, digits and `-`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SlotId(String);

impl SlotId {
    pub fn new(id: impl Into<String>) -> Result<Self, SlotConfigError> {
        let id = id.into();
        let valid = !id.is_empty()
            && !id.starts_with('-')
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if valid {
            Ok(Self(id))
        } else {
            Err(SlotConfigError::Invalid(format!("slot id `{id}`")))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SlotId {
    type Error = SlotConfigError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl From<SlotId> for String {
    fn from(id: SlotId) -> Self {
        id.0
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SlotConfigError {
    #[error("cannot read slot config `{path}`: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid slot config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid slot config: {0}")]
    Invalid(String),
}

/// Which instruments the slot may trade.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Universe {
    /// A fixed list (survivorship-biased: judge results against buy & hold of the same list).
    Fixed { instruments: Vec<InstrumentId> },
    /// Point-in-time top by dollar volume (trader ADR 0007 step 3), in bars of the slot's
    /// interval.
    TopVolume {
        size: usize,
        window: usize,
        refresh: usize,
        min_history: usize,
    },
}

impl Universe {
    /// The rule for `quorum_core::strategies::TopVolume`, for a top-volume universe.
    #[must_use]
    pub const fn top_volume_rule(&self) -> Option<UniverseRule> {
        match *self {
            Self::Fixed { .. } => None,
            Self::TopVolume {
                size,
                window,
                refresh,
                min_history,
            } => Some(UniverseRule {
                size,
                window,
                refresh,
                min_history,
            }),
        }
    }
}

/// When the slot's market trades.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum TradingCalendar {
    /// 24/7 (crypto).
    #[default]
    AlwaysOpen,
}

/// Trading costs the slot is judged with — in backtests, paper fills and the ladder's slippage
/// gate.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostModel {
    /// Commission per trade as a fraction of notional (0.001 = 0.1%).
    pub fee_rate: f64,
    pub slippage_bps: f64,
    /// Square-root market impact coefficient (`core::backtest`); `0` = off.
    #[serde(default)]
    pub impact: f64,
}

impl CostModel {
    /// Sets the cost fields of `cfg` from this model.
    pub const fn apply(&self, cfg: &mut BacktestConfig) {
        cfg.fee_rate = self.fee_rate;
        cfg.slippage_bps = self.slippage_bps;
        cfg.impact = self.impact;
    }
}

const INTERVALS: [&str; 3] = ["1h", "4h", "1d"];

/// The operator's description of a slot (`config/slots/<id>.toml`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotConfig {
    pub id: SlotId,
    pub venue: VenueId,
    /// Bar interval the strategies run on: `1h`, `4h` or `1d`.
    pub interval: String,
    pub universe: Universe,
    /// Strategy kinds run in this slot, e.g. `trend-momentum`.
    pub strategies: Vec<String>,
    #[serde(default)]
    pub calendar: TradingCalendar,
    pub costs: CostModel,
    /// Where a new slot starts: `research` or `paper`. Live modes are reached only through the
    /// ladder.
    #[serde(default = "research")]
    pub start_mode: Mode,
}

const fn research() -> Mode {
    Mode::Research
}

impl SlotConfig {
    /// Reads and validates a slot config file.
    pub fn load(path: &Path) -> Result<Self, SlotConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| SlotConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses and validates slot config TOML.
    pub fn parse(text: &str) -> Result<Self, SlotConfigError> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), SlotConfigError> {
        let invalid = |msg: String| Err(SlotConfigError::Invalid(format!("{}: {msg}", self.id)));
        if !INTERVALS.contains(&self.interval.as_str()) {
            return invalid(format!(
                "interval `{}` (one of {INTERVALS:?})",
                self.interval
            ));
        }
        if self.strategies.is_empty() || self.strategies.iter().any(String::is_empty) {
            return invalid("needs at least one strategy".to_owned());
        }
        let mut sorted = self.strategies.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != self.strategies.len() {
            return invalid("a strategy is listed twice".to_owned());
        }
        match &self.universe {
            Universe::Fixed { instruments } => {
                if instruments.is_empty() {
                    return invalid("fixed universe is empty".to_owned());
                }
                if let Some(other) = instruments.iter().find(|i| i.venue() != &self.venue) {
                    return invalid(format!("{other} is not on venue {}", self.venue.as_str()));
                }
            }
            Universe::TopVolume {
                size,
                window,
                refresh,
                ..
            } => {
                if *size == 0 || *window == 0 || *refresh == 0 {
                    return invalid("top-volume size, window and refresh must be > 0".to_owned());
                }
            }
        }
        let c = &self.costs;
        if [c.fee_rate, c.slippage_bps, c.impact]
            .iter()
            .any(|x| !x.is_finite() || *x < 0.0)
        {
            return invalid("costs must be finite and ≥ 0".to_owned());
        }
        if self.start_mode.is_live() {
            return invalid(format!(
                "start_mode `{}`: live modes are reached only through the ladder",
                self.start_mode
            ));
        }
        Ok(())
    }

    /// Checks the slot against the envelope: its venue must be allowed.
    pub fn check_envelope(&self, envelope: &Envelope) -> Result<(), SlotConfigError> {
        if envelope.allows_venue(&self.venue) {
            Ok(())
        } else {
            Err(SlotConfigError::Invalid(format!(
                "{}: venue `{}` is not in the envelope's venues_allowed",
                self.id,
                self.venue.as_str()
            )))
        }
    }
}

#[cfg(test)]
mod tests;
