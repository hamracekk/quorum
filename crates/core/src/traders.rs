//! Followed traders (SRC-3, M7): public positions and fills of accounts the operator chooses.
//!
//! On-chain venues (Hyperliquid) publish every account's positions and fills. trader only
//! *reads* them: a burst of position-opening fills on a coin becomes a long or short call that
//! the scoreboard judges like any other source, so a trader earns weight only by a record.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::Serialize;

/// One open position.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraderPosition {
    /// Venue's coin name, e.g. `BTC`.
    pub coin: String,
    /// Signed size in coins: positive long, negative short.
    pub size: f64,
    pub entry_price: Option<f64>,
    pub value: f64,
    pub unrealized_pnl: f64,
    pub leverage: Option<f64>,
}

/// An account's current state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraderAccount {
    pub venue: String,
    pub address: String,
    pub account_value: f64,
    pub positions: Vec<TraderPosition>,
    /// Link to the account on a public explorer.
    pub url: String,
}

/// One fill. `opens` is the direction of the position it opened or grew (`1.0` long,
/// `-1.0` short), `None` for closes and anything else.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraderFill {
    pub ts: DateTime<Utc>,
    pub coin: String,
    pub price: f64,
    pub size: f64,
    pub opens: Option<f64>,
}

/// When opening fills count as a call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CallRule {
    /// Smallest notional (USD) opened on one coin in one direction within an hour.
    pub min_notional: f64,
    /// After a call on a coin in one direction, the next one counts only this much later.
    pub cooldown: Duration,
}

impl Default for CallRule {
    fn default() -> Self {
        Self {
            min_notional: 10_000.0,
            cooldown: Duration::hours(24),
        }
    }
}

/// A trader's call: known at `ts` (the end of the hour its fills fell in).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraderCall {
    pub ts: DateTime<Utc>,
    pub coin: String,
    pub direction: f64,
    /// USD opened in that hour.
    pub notional: f64,
}

/// Calls in `fills`, oldest first.
///
/// Per coin and direction, opening fills are summed by hour; an hour with at least
/// `min_notional` is a call, known when the hour ends, unless the previous call on that coin in
/// that direction is less than `cooldown` earlier.
pub fn calls(fills: &[TraderFill], rule: CallRule) -> Vec<TraderCall> {
    // (coin, direction as ±1) → hour → notional.
    let mut hours: BTreeMap<(String, i8), BTreeMap<DateTime<Utc>, f64>> = BTreeMap::new();
    for fill in fills {
        let Some(direction) = fill.opens else {
            continue;
        };
        let Ok(hour) = fill.ts.duration_trunc(Duration::hours(1)) else {
            continue;
        };
        let side = if direction > 0.0 { 1 } else { -1 };
        *hours
            .entry((fill.coin.clone(), side))
            .or_default()
            .entry(hour)
            .or_default() += (fill.price * fill.size).abs();
    }
    let mut found = Vec::new();
    for ((coin, side), by_hour) in hours {
        let mut last: Option<DateTime<Utc>> = None;
        for (hour, notional) in by_hour {
            let known = hour + Duration::hours(1);
            let cooled = last.is_none_or(|t| known - t >= rule.cooldown);
            if notional >= rule.min_notional && cooled {
                found.push(TraderCall {
                    ts: known,
                    coin: coin.clone(),
                    direction: f64::from(side),
                    notional,
                });
                last = Some(known);
            }
        }
    }
    found.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.coin.cmp(&b.coin)));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_789_999_200 + minutes * 60, 0).expect("valid timestamp")
    }

    fn fill(minutes: i64, coin: &str, price: f64, size: f64, opens: Option<f64>) -> TraderFill {
        TraderFill {
            ts: at(minutes),
            coin: coin.into(),
            price,
            size,
            opens,
        }
    }

    #[test]
    fn opening_fills_add_up_by_hour_and_respect_the_cooldown() {
        let rule = CallRule {
            min_notional: 10_000.0,
            cooldown: Duration::hours(24),
        };
        let fills = [
            // Hour 0: two BTC long opens adding to $12k → a call at the end of hour 0.
            fill(5, "BTC", 60_000.0, 0.1, Some(1.0)),
            fill(40, "BTC", 60_000.0, 0.1, Some(1.0)),
            // A close never counts, however large.
            fill(50, "BTC", 60_000.0, 5.0, None),
            // Hour 3: another BTC long, inside the cooldown → no call.
            fill(185, "BTC", 60_000.0, 1.0, Some(1.0)),
            // Hour 3: an ETH short of $9k → under the threshold.
            fill(190, "ETH", 3_000.0, 3.0, Some(-1.0)),
            // Hour 4: a BTC short is the other direction → its own call.
            fill(250, "BTC", 60_000.0, 0.5, Some(-1.0)),
        ];
        let found = calls(&fills, rule);
        let summary: Vec<(DateTime<Utc>, &str, f64)> = found
            .iter()
            .map(|c| (c.ts, c.coin.as_str(), c.direction))
            .collect();
        assert_eq!(summary, [(at(60), "BTC", 1.0), (at(300), "BTC", -1.0)]);
        assert!((found[0].notional - 12_000.0).abs() < 1e-6);
    }

    #[test]
    fn calls_resume_after_the_cooldown() {
        let rule = CallRule {
            min_notional: 1.0,
            cooldown: Duration::hours(2),
        };
        let fills = [
            fill(0, "SOL", 100.0, 1.0, Some(1.0)),
            fill(70, "SOL", 100.0, 1.0, Some(1.0)),
            fill(130, "SOL", 100.0, 1.0, Some(1.0)),
        ];
        let times: Vec<DateTime<Utc>> = calls(&fills, rule).iter().map(|c| c.ts).collect();
        assert_eq!(times, [at(60), at(180)]);
    }
}
