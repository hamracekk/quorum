//! The impact map (ADR 0008): operator-written themes and the instruments each one moves.
//!
//! A news item about a theme ("GPU prices rise") implies moves in several markets at once,
//! often opposite ones (the GPU maker up, crypto miners down). A model only *classifies* an item
//! into the map's themes, with the theme's direction; [`ImpactMap::implications`] expands that
//! through the links written here. Models never edit the map. Each link is scored on the
//! scoreboard from its `since` date — readings before it are why it was added, not its record.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;
use crate::news::{Direction, Horizon};

/// Most themes in a map (kept small on purpose: mechanisms, not guesses).
pub const MAX_THEMES: usize = 40;
/// Most links per theme.
pub const MAX_LINKS: usize = 20;
/// Longest description or why (characters).
pub const MAX_TEXT: usize = 300;

/// One instrument a theme moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactLink {
    pub instrument: InstrumentId,
    /// `1`: moves with the theme; `-1`: against it.
    pub sign: i8,
    /// The mechanism, one plain sentence.
    pub why: String,
    /// When the link was added: its scoreboard record starts here.
    pub since: NaiveDate,
    #[serde(default = "default_horizon")]
    pub horizon: Horizon,
}

const fn default_horizon() -> Horizon {
    Horizon::Days
}

/// A theme news can be about, and what it moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactTheme {
    /// Lower-case id, e.g. `gpu-prices`.
    pub id: String,
    /// What counts as this theme, for the classifier ("GPU and AI accelerator prices or
    /// shortages"); its direction is "up" when the described quantity rises.
    pub description: String,
    pub links: Vec<ImpactLink>,
}

/// The operator's themes (`[impact]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactMap {
    pub themes: Vec<ImpactTheme>,
}

/// The direction a theme moved in a news item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMove {
    Up,
    Down,
}

/// One instrument move implied by a theme reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implication {
    pub theme: String,
    pub instrument: InstrumentId,
    pub direction: Direction,
    pub horizon: Horizon,
    pub why: String,
    pub since: NaiveDate,
}

fn plain(text: &str) -> bool {
    let n = text.chars().count();
    n > 0 && n <= MAX_TEXT && !text.chars().any(char::is_control)
}

impl ImpactMap {
    pub const fn is_empty(&self) -> bool {
        self.themes.is_empty()
    }

    /// Ids, sizes, signs and text bounds; the first problem found, readable.
    pub fn validate(&self) -> Result<(), String> {
        if self.themes.len() > MAX_THEMES {
            return Err(format!("impact: at most {MAX_THEMES} themes"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for theme in &self.themes {
            let id_ok = !theme.id.is_empty()
                && theme.id.len() <= 40
                && theme
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !id_ok {
                return Err(format!(
                    "impact: theme id `{}` must be 1–40 of a-z, 0-9 and -",
                    theme.id
                ));
            }
            if !seen.insert(theme.id.as_str()) {
                return Err(format!("impact: theme `{}` appears twice", theme.id));
            }
            if !plain(&theme.description) {
                return Err(format!(
                    "impact: theme `{}` needs a description of 1–{MAX_TEXT} plain characters",
                    theme.id
                ));
            }
            if theme.links.is_empty() || theme.links.len() > MAX_LINKS {
                return Err(format!(
                    "impact: theme `{}` needs 1–{MAX_LINKS} links",
                    theme.id
                ));
            }
            let mut instruments = std::collections::BTreeSet::new();
            for link in &theme.links {
                if link.sign != 1 && link.sign != -1 {
                    return Err(format!(
                        "impact: `{}` → {}: sign must be 1 or -1",
                        theme.id, link.instrument
                    ));
                }
                if !plain(&link.why) {
                    return Err(format!(
                        "impact: `{}` → {}: why must be 1–{MAX_TEXT} plain characters",
                        theme.id, link.instrument
                    ));
                }
                if !instruments.insert(&link.instrument) {
                    return Err(format!(
                        "impact: `{}` links {} twice",
                        theme.id, link.instrument
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn theme(&self, id: &str) -> Option<&ImpactTheme> {
        self.themes.iter().find(|t| t.id == id)
    }

    /// What `theme` moving `way` implies for each linked instrument (empty for an unknown id).
    pub fn implications(&self, theme: &str, way: ThemeMove) -> Vec<Implication> {
        let Some(found) = self.theme(theme) else {
            return Vec::new();
        };
        found
            .links
            .iter()
            .map(|link| {
                let up = (way == ThemeMove::Up) == (link.sign > 0);
                Implication {
                    theme: found.id.clone(),
                    instrument: link.instrument.clone(),
                    direction: if up {
                        Direction::Bullish
                    } else {
                        Direction::Bearish
                    },
                    horizon: link.horizon,
                    why: link.why.clone(),
                    since: link.since,
                }
            })
            .collect()
    }

    /// Short fingerprint-friendly text of the themes the classifier sees (ids + descriptions).
    pub fn classifier_text(&self) -> String {
        self.themes
            .iter()
            .map(|t| format!("{}: {}", t.id, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests;
