//! The operator's envelope (DESIGN §5.1): the hard bounds the whole system runs inside.
//!
//! The envelope is **read-only to the system**. It is parsed from the operator's TOML file and
//! exposes queries only: no setters, no `Serialize`, no way to build a looser copy. Inside it the
//! system promotes, demotes and trades on its own; outside it nothing can act.
//!
//! Unknown keys are rejected, so a typo (`max_mod = "live"`) fails loudly instead of silently
//! falling back to a default.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use quorum_core::instrument::VenueId;
use serde::{Deserialize, Serialize};

/// A slot's rung on the autonomy ladder (DESIGN §5.3), lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Backtests and walk-forward only; no live data trading.
    Research,
    /// Live data, simulated fills.
    Paper,
    /// Real orders at 1–5% of the slot's capital cap.
    LiveMicro,
    /// Real orders up to the slot's capital cap.
    Live,
}

impl Mode {
    /// The rung above, if any.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self {
            Self::Research => Some(Self::Paper),
            Self::Paper => Some(Self::LiveMicro),
            Self::LiveMicro => Some(Self::Live),
            Self::Live => None,
        }
    }

    /// The rung below, if any.
    #[must_use]
    pub const fn prev(self) -> Option<Self> {
        match self {
            Self::Research => None,
            Self::Paper => Some(Self::Research),
            Self::LiveMicro => Some(Self::Paper),
            Self::Live => Some(Self::LiveMicro),
        }
    }

    /// Whether this mode trades real money.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::LiveMicro | Self::Live)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Research => "research",
            Self::Paper => "paper",
            Self::LiveMicro => "live-micro",
            Self::Live => "live",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("cannot read envelope `{path}`: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid envelope: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid envelope: {0}")]
    Invalid(String),
}

/// Loss limits as fractions of equity (0.02 = 2%).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LossLimits {
    /// Breach: flatten everything, halt until the next UTC day.
    pub daily_max: f64,
    /// Drawdown from the equity peak. Breach: flatten, halt, require the operator.
    pub total_max: f64,
}

/// Bounds for one slot.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotBounds {
    /// Most capital the slot may hold, in the envelope's currency.
    pub capital_max: f64,
    /// Highest rung the slot may reach on its own.
    pub max_mode: Mode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    currency: String,
    capital_total: f64,
    leverage_max: f64,
    venues_allowed: BTreeSet<VenueId>,
    loss: LossLimits,
    slot_default: SlotBounds,
    #[serde(default)]
    slots: BTreeMap<String, SlotBounds>,
}

/// The operator's envelope. Build it with [`Envelope::load`] or [`Envelope::parse`].
#[derive(Debug, Clone)]
pub struct Envelope {
    currency: String,
    capital_total: f64,
    leverage_max: f64,
    venues_allowed: BTreeSet<VenueId>,
    loss: LossLimits,
    slot_default: SlotBounds,
    slots: BTreeMap<String, SlotBounds>,
}

impl Envelope {
    /// Reads and validates the envelope file.
    pub fn load(path: &Path) -> Result<Self, EnvelopeError> {
        let text = std::fs::read_to_string(path).map_err(|source| EnvelopeError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses and validates envelope TOML.
    pub fn parse(text: &str) -> Result<Self, EnvelopeError> {
        let raw: Raw = toml::from_str(text)?;
        let envelope = Self {
            currency: raw.currency,
            capital_total: raw.capital_total,
            leverage_max: raw.leverage_max,
            venues_allowed: raw.venues_allowed,
            loss: raw.loss,
            slot_default: raw.slot_default,
            slots: raw.slots,
        };
        envelope.validate()?;
        Ok(envelope)
    }

    fn validate(&self) -> Result<(), EnvelopeError> {
        let invalid = |msg: String| Err(EnvelopeError::Invalid(msg));
        if self.currency.len() != 3 || !self.currency.chars().all(|c| c.is_ascii_uppercase()) {
            return invalid(format!(
                "currency `{}` is not an ISO code like EUR",
                self.currency
            ));
        }
        if !self.capital_total.is_finite() || self.capital_total < 0.0 {
            return invalid(format!("capital_total {} must be ≥ 0", self.capital_total));
        }
        if !self.leverage_max.is_finite() || self.leverage_max <= 0.0 {
            return invalid(format!("leverage_max {} must be > 0", self.leverage_max));
        }
        let fraction = |x: f64| x.is_finite() && x > 0.0 && x <= 1.0;
        if !fraction(self.loss.daily_max) || !fraction(self.loss.total_max) {
            return invalid("loss limits must be fractions in (0, 1]".to_owned());
        }
        if self.loss.daily_max > self.loss.total_max {
            return invalid("loss.daily_max exceeds loss.total_max".to_owned());
        }
        let named = self.slots.iter().map(|(name, b)| (name.as_str(), b));
        for (name, bounds) in std::iter::once(("slot_default", &self.slot_default)).chain(named) {
            if !bounds.capital_max.is_finite() || bounds.capital_max < 0.0 {
                return invalid(format!("{name}: capital_max must be ≥ 0"));
            }
            if bounds.capital_max > self.capital_total {
                return invalid(format!(
                    "{name}: capital_max {} exceeds capital_total {}",
                    bounds.capital_max, self.capital_total
                ));
            }
            // Real money with no capital is a contradiction, not a default to resolve quietly.
            if bounds.max_mode.is_live() && bounds.capital_max <= 0.0 {
                return invalid(format!(
                    "{name}: max_mode `{}` needs capital_max > 0",
                    bounds.max_mode
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn currency(&self) -> &str {
        &self.currency
    }

    #[must_use]
    pub const fn capital_total(&self) -> f64 {
        self.capital_total
    }

    #[must_use]
    pub const fn leverage_max(&self) -> f64 {
        self.leverage_max
    }

    #[must_use]
    pub const fn loss(&self) -> LossLimits {
        self.loss
    }

    /// Whether `venue` may be traded at all (paper included).
    #[must_use]
    pub fn allows_venue(&self, venue: &VenueId) -> bool {
        self.venues_allowed.contains(venue)
    }

    /// Bounds for `slot`: its own entry, else `slot_default`.
    #[must_use]
    pub fn slot(&self, slot: &str) -> SlotBounds {
        self.slots.get(slot).copied().unwrap_or(self.slot_default)
    }

    /// Highest rung `slot` may reach.
    #[must_use]
    pub fn max_mode(&self, slot: &str) -> Mode {
        self.slot(slot).max_mode
    }

    /// No slot can reach a live mode.
    #[must_use]
    pub fn is_paper_only(&self) -> bool {
        std::iter::once(&self.slot_default)
            .chain(self.slots.values())
            .all(|b| !b.max_mode.is_live())
    }
}

#[cfg(test)]
mod tests;
