//! What the risk process remembers across restarts: halts and each slot's loss references.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::RiskError;

/// Why new risk is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Halt {
    /// The day lost `loss.daily_max` or more; lifts at the next UTC day.
    Daily { day: NaiveDate },
    /// Drawdown from the equity peak reached `loss.total_max`; the operator lifts it.
    Total,
    /// The kill switch; the operator lifts it.
    Operator { reason: String },
}

impl Halt {
    /// One line for status displays and alerts.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Daily { day } => format!("daily loss limit reached on {day}"),
            Self::Total => "total drawdown limit reached".to_owned(),
            Self::Operator { reason } => format!("kill switch: {reason}"),
        }
    }
}

/// One slot as the risk process sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotRisk {
    pub halt: Option<Halt>,
    /// The UTC day `day_ref` belongs to.
    pub day: Option<NaiveDate>,
    /// Equity the day's loss is measured against: the last equity seen before the day began.
    pub day_ref: Option<f64>,
    /// Highest equity seen; the total drawdown is measured from it.
    pub peak: Option<f64>,
    pub last_equity: Option<f64>,
    /// Last request from the slot (wall clock).
    pub last_seen: Option<DateTime<Utc>>,
    pub approved: u64,
    pub rejected: u64,
}

/// The risk process's whole memory, kept in `<data>/risk/state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    /// The kill switch for every slot.
    pub kill: Option<Halt>,
    pub slots: BTreeMap<String, SlotRisk>,
}

impl RiskState {
    /// Reads the state file; a missing file is a fresh state.
    pub fn load(path: &Path) -> Result<Self, RiskError> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| RiskError::Json {
                path: path.display().to_string(),
                source,
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(RiskError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Writes the state file atomically (a temporary file, then a rename).
    pub fn save(&self, path: &Path) -> Result<(), RiskError> {
        let io = |source| RiskError::Io {
            path: path.display().to_string(),
            source,
        };
        let text = serde_json::to_string_pretty(self).map_err(|source| RiskError::Json {
            path: path.display().to_string(),
            source,
        })?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(io)?;
        fs::rename(&tmp, path).map_err(io)
    }

    /// The halt that applies to `slot`: the kill switch first, then the slot's own.
    #[must_use]
    pub fn halt_for(&self, slot: &str) -> Option<&Halt> {
        self.kill
            .as_ref()
            .or_else(|| self.slots.get(slot).and_then(|s| s.halt.as_ref()))
    }
}
