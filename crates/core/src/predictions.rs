//! Prediction markets (SRC-2): market-implied probabilities as an information source.
//!
//! A prediction market prices a yes/no question ("Will Bitcoin be above $110k by Dec 31?"), so
//! its price is the crowd's probability. trader only *reads* them: a market is followed for one
//! instrument with a stated meaning of "yes" (bullish or bearish for it), and a large move in the
//! probability becomes a directional call that the scoreboard judges like any other source.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// What a "yes" outcome would mean for the followed instrument's price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YesMeans {
    Bullish,
    Bearish,
}

impl YesMeans {
    /// `1.0` when a rising probability is bullish, `-1.0` when it is bearish.
    pub const fn sign(self) -> f64 {
        match self {
            Self::Bullish => 1.0,
            Self::Bearish => -1.0,
        }
    }
}

/// A market's current state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PredictionMarket {
    /// Exchange id, e.g. `kalshi`.
    pub venue: String,
    pub ticker: String,
    /// The question, as the exchange words it (untrusted text, shown as data).
    pub question: String,
    /// Implied probability of "yes" (last trade, else the bid/ask mid), when it trades.
    pub probability: Option<f64>,
    /// Open for trading (closed, settled and unopened markets are not).
    pub active: bool,
    pub close_time: Option<DateTime<Utc>>,
    /// Link to the market on the exchange's site.
    pub url: String,
}

/// The implied probability at the end of a period (hourly).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ProbabilityPoint {
    pub ts: DateTime<Utc>,
    pub probability: f64,
}

/// When a probability change counts as news.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveRule {
    /// Smallest absolute change, in probability (0.05 = 5 points).
    pub threshold: f64,
    /// Compared with the probability this long before.
    pub window: Duration,
    /// After a move, the next one counts only this much later (one story, one call).
    pub cooldown: Duration,
}

impl Default for MoveRule {
    fn default() -> Self {
        Self {
            threshold: 0.05,
            window: Duration::hours(24),
            cooldown: Duration::hours(24),
        }
    }
}

/// A probability move: known at `ts`, by `change` (signed) to `probability`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ProbabilityMove {
    pub ts: DateTime<Utc>,
    pub change: f64,
    pub probability: f64,
}

/// Moves in time-ordered `points`, using only points up to the one judged (no look-ahead).
///
/// Each point is compared with the latest one at least `window` older; a change of at least
/// `threshold` is a move unless the previous move is less than `cooldown` ago.
pub fn moves(points: &[ProbabilityPoint], rule: MoveRule) -> Vec<ProbabilityMove> {
    let mut found: Vec<ProbabilityMove> = Vec::new();
    for (i, point) in points.iter().enumerate() {
        let earlier = points
            .get(..i)
            .unwrap_or_default()
            .iter()
            .rev()
            .find(|p| point.ts - p.ts >= rule.window);
        let Some(earlier) = earlier else { continue };
        let change = point.probability - earlier.probability;
        let cooled = found
            .last()
            .is_none_or(|m| point.ts - m.ts >= rule.cooldown);
        if change.abs() >= rule.threshold && cooled {
            found.push(ProbabilityMove {
                ts: point.ts,
                change,
                probability: point.probability,
            });
        }
    }
    found
}

/// Change over the last `window` of `points` (newest minus the latest point at least `window`
/// older), for displays.
pub fn change_over(points: &[ProbabilityPoint], window: Duration) -> Option<f64> {
    let last = points.last()?;
    let earlier = points.iter().rev().find(|p| last.ts - p.ts >= window)?;
    Some(last.probability - earlier.probability)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000 + hours * 3_600, 0).expect("valid timestamp")
    }

    fn series(probabilities: &[f64]) -> Vec<ProbabilityPoint> {
        probabilities
            .iter()
            .zip(0..)
            .map(|(p, hour)| ProbabilityPoint {
                ts: at(hour),
                probability: *p,
            })
            .collect()
    }

    fn rule() -> MoveRule {
        MoveRule {
            threshold: 0.05,
            window: Duration::hours(2),
            cooldown: Duration::hours(3),
        }
    }

    #[test]
    fn moves_need_the_threshold_over_the_window_and_respect_the_cooldown() {
        // Hours: 0.20, 0.21, 0.24 (+4 over 2h: no), 0.28 (+7: move), 0.35 (cooldown),
        // 0.36, 0.36 (+1), 0.20 (−16 at h7, 4h after the first move: move).
        let points = series(&[0.20, 0.21, 0.24, 0.28, 0.35, 0.36, 0.36, 0.20]);
        let found = moves(&points, rule());
        let times: Vec<DateTime<Utc>> = found.iter().map(|m| m.ts).collect();
        assert_eq!(times, [at(3), at(7)]);
        assert!((found[0].change - 0.07).abs() < 1e-9);
        assert!((found[1].change + 0.16).abs() < 1e-9);
        assert!((found[1].probability - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn short_histories_have_no_moves_and_change_needs_a_full_window() {
        assert!(moves(&series(&[0.1, 0.9]), rule()).is_empty());
        assert_eq!(change_over(&series(&[0.1, 0.9]), Duration::hours(2)), None);
        let change = change_over(&series(&[0.1, 0.2, 0.4]), Duration::hours(2)).expect("change");
        assert!((change - 0.3).abs() < 1e-12);
        assert!(YesMeans::Bearish.sign() < 0.0);
    }
}
