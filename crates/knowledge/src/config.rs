//! `config/news.toml`: feeds, which slots' instruments to read news for, the model tiers and
//! their daily budgets. The impact map is separate (`config/impact.toml`, operator-written).

use std::collections::BTreeMap;
use std::path::Path;

use quorum_core::InstrumentId;
use quorum_core::impact::ImpactMap;
use quorum_slots::{SlotConfig, SlotId, Universe};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum NewsConfigError {
    #[error("cannot read `{path}`: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid `{path}`: {message}")]
    Invalid { path: String, message: String },
}

/// One news feed (RSS, Atom or JSON Feed).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub url: String,
    /// Required by some publishers (SEC EDGAR: a name and a contact address).
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// One model tier.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierConfig {
    pub enabled: bool,
    /// Claude model id, e.g. `claude-haiku-5-5`.
    pub model: String,
    /// `output_config.effort`: `low` keeps a simple extraction cheap.
    pub effort: String,
    /// Server-side fallback on a refusal (`fallbacks: "default"`); not every model offers it
    /// (Claude Haiku 5.5 does not).
    pub fallbacks: bool,
    /// Tokens (input incl. cached + output) this tier may spend per UTC day; 0 = never runs.
    pub daily_tokens: u64,
}

/// Which tier, as journaled. The names predate the cloud first pass (it was a local model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    FirstPass,
    Escalation,
}

impl Tier {
    #[must_use]
    pub const fn journal_name(self) -> &'static str {
        match self {
            Self::FirstPass => "local",
            Self::Escalation => "remote",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    poll_minutes: u64,
    slots: Vec<SlotId>,
    #[serde(default)]
    aliases: BTreeMap<String, Vec<String>>,
    #[serde(rename = "source")]
    sources: Vec<Source>,
    batch: usize,
    escalate: String,
    first_pass: TierConfig,
    escalation: TierConfig,
}

/// The news configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsConfig {
    /// Minutes between polls of the feeds (the `quorum news run` service).
    pub poll_minutes: u64,
    /// Read news for the instruments of these slots.
    pub slots: Vec<SlotId>,
    /// Extra words that tag an item with an asset, by base asset (`GLD = ["gold"]`).
    pub aliases: BTreeMap<String, Vec<String>>,
    pub sources: Vec<Source>,
    /// Items read per tier per run.
    pub batch: usize,
    /// First-pass readings at or above this magnitude go to the escalation tier:
    /// `low`, `medium`, `high`, `all` or `never`.
    pub escalate: String,
    pub first_pass: TierConfig,
    pub escalation: TierConfig,
}

const ESCALATE: [&str; 5] = ["low", "medium", "high", "all", "never"];
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

impl NewsConfig {
    pub fn load(path: &Path) -> Result<Self, NewsConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| NewsConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text).map_err(|message| NewsConfigError::Invalid {
            path: path.display().to_string(),
            message,
        })
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.to_string())?;
        if !ESCALATE.contains(&raw.escalate.as_str()) {
            return Err(format!("escalate `{}`: one of {ESCALATE:?}", raw.escalate));
        }
        for (name, tier) in [
            ("first_pass", &raw.first_pass),
            ("escalation", &raw.escalation),
        ] {
            if !EFFORTS.contains(&tier.effort.as_str()) {
                return Err(format!(
                    "{name}.effort `{}`: one of {EFFORTS:?}",
                    tier.effort
                ));
            }
            if tier.model.trim().is_empty() {
                return Err(format!("{name}.model is empty"));
            }
        }
        if raw.sources.is_empty() {
            return Err("no [[source]]".to_owned());
        }
        if raw.batch == 0 || raw.batch > 500 {
            return Err("batch must be 1..=500".to_owned());
        }
        Ok(Self {
            poll_minutes: raw.poll_minutes,
            slots: raw.slots,
            aliases: raw.aliases,
            sources: raw.sources,
            batch: raw.batch,
            escalate: raw.escalate,
            first_pass: raw.first_pass,
            escalation: raw.escalation,
        })
    }

    /// The instruments of the configured slots' fixed universes, deduplicated in slot order.
    pub fn instruments(&self, slots: &[SlotConfig]) -> Result<Vec<InstrumentId>, String> {
        let mut out: Vec<InstrumentId> = Vec::new();
        for id in &self.slots {
            let slot = slots
                .iter()
                .find(|s| &s.id == id)
                .ok_or_else(|| format!("slot `{id}` not loaded"))?;
            let Universe::Fixed { instruments } = &slot.universe else {
                return Err(format!("slot `{id}` has no fixed universe"));
            };
            for i in instruments {
                if !out.contains(i) {
                    out.push(i.clone());
                }
            }
        }
        Ok(out)
    }
}

/// Reads the operator's impact map (`config/impact.toml`: `[[themes]]`); missing = empty.
pub fn load_impact(path: &Path) -> Result<ImpactMap, NewsConfigError> {
    if !path.exists() {
        return Ok(ImpactMap::default());
    }
    let invalid = |message: String| NewsConfigError::Invalid {
        path: path.display().to_string(),
        message,
    };
    let text = std::fs::read_to_string(path).map_err(|source| NewsConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let map: ImpactMap = toml::from_str(&text).map_err(|e| invalid(e.to_string()))?;
    map.validate().map_err(invalid)?;
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config")
    }

    #[test]
    fn the_shipped_configs_are_valid() {
        let cfg = NewsConfig::load(&root().join("news.toml")).expect("news.toml");
        assert!(cfg.sources.len() >= 5);
        let slots: Vec<SlotConfig> = cfg
            .slots
            .iter()
            .map(|id| {
                SlotConfig::load(&root().join("slots").join(format!("{}.toml", id.as_str())))
                    .expect("slot")
            })
            .collect();
        let instruments = cfg.instruments(&slots).expect("instruments");
        assert_eq!(instruments.len(), 50, "25 coins + 25 ETFs");
        let map = load_impact(&root().join("impact.toml")).expect("impact.toml");
        assert!(!map.is_empty());
    }

    #[test]
    fn bad_values_are_rejected() {
        let text = std::fs::read_to_string(root().join("news.toml")).expect("read");
        assert!(
            NewsConfig::parse(&text.replace("escalate = \"medium\"", "escalate = \"loud\""))
                .is_err()
        );
        assert!(NewsConfig::parse(&text.replace("batch = 25", "batch = 0")).is_err());
        assert!(NewsConfig::parse(&format!("{text}\nextra = 1")).is_err());
    }
}
