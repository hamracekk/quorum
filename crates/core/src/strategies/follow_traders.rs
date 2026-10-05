//! Follow traders (T-4): hold each instrument on the side the followed traders hold it.
//!
//! Inputs are position states — a followed trader's side on an instrument (`1` long, `-1`
//! short, `0` flat), known at the end of the hour it changed — passed as [`ExternalCall`]s with
//! `follow:` source ids. On each closed bar the target for an instrument is the average side of
//! the traders that have a state there (a trader who never traded it does not count), floored
//! at 0 without shorting, times an equal share per instrument. So the strategy enters when they
//! enter and exits when they exit; per-trade stops (T-2) still apply in the engine. Whether it
//! deserves to trade is the tournament's call.

use std::collections::BTreeMap;

use chrono::Duration;
use serde_json::{Value, json};

use crate::instrument::InstrumentId;
use crate::strategy::{ExternalCall, Strategy};
use crate::types::Bar;

/// Source id prefix of position states.
pub const PREFIX: &str = "follow:";

#[derive(Debug, Clone)]
pub struct FollowTraders {
    states: Vec<ExternalCall>,
    cursor: usize,
    /// Latest side per (instrument, trader).
    sides: BTreeMap<InstrumentId, BTreeMap<String, f64>>,
    step: Duration,
    share: f64,
    allow_short: bool,
}

impl FollowTraders {
    pub fn new(
        mut states: Vec<ExternalCall>,
        instruments: usize,
        step: Duration,
        allow_short: bool,
    ) -> Self {
        states.retain(|s| s.source.starts_with(PREFIX));
        states.sort_by_key(|s| s.ts);
        Self {
            states,
            cursor: 0,
            sides: BTreeMap::new(),
            step,
            share: if instruments == 0 {
                0.0
            } else {
                1.0 / instruments as f64
            },
            allow_short,
        }
    }

    fn conviction(&self, instrument: &InstrumentId) -> f64 {
        let Some(sides) = self.sides.get(instrument).filter(|s| !s.is_empty()) else {
            return 0.0;
        };
        let mean = sides.values().sum::<f64>() / sides.len() as f64;
        if self.allow_short {
            mean.clamp(-1.0, 1.0)
        } else {
            mean.clamp(0.0, 1.0)
        }
    }
}

impl Strategy for FollowTraders {
    fn name(&self) -> &'static str {
        "follow-traders"
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        // Decided at the bar's close: states known by then count.
        let now = bar.ts + self.step;
        while let Some(state) = self.states.get(self.cursor).filter(|s| s.ts <= now) {
            self.sides
                .entry(state.instrument.clone())
                .or_default()
                .insert(state.source.clone(), state.direction.clamp(-1.0, 1.0));
            self.cursor += 1;
        }
        Some(self.conviction(instrument) * self.share)
    }

    fn explain(&self, instrument: &InstrumentId) -> Option<Value> {
        let sides = self.sides.get(instrument)?;
        Some(json!({
            "traders": sides.iter().map(|(t, s)| json!({ "trader": t, "side": s })).collect::<Vec<_>>(),
            "conviction": self.conviction(instrument),
        }))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("ts")
    }

    fn btc() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("id")
    }

    fn bar(hour: i64) -> Bar {
        Bar {
            ts: at(hour),
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1.0,
        }
    }

    fn state(trader: &str, hour: i64, side: f64) -> ExternalCall {
        ExternalCall {
            source: format!("{PREFIX}hyperliquid:{trader}"),
            instrument: btc(),
            ts: at(hour),
            direction: side,
            strength: 1.0,
        }
    }

    #[test]
    fn enters_with_the_traders_and_exits_when_they_exit() {
        let states = vec![
            state("a", 2, 1.0),
            state("b", 3, 1.0),
            state("a", 5, 0.0),
            state("b", 6, 0.0),
        ];
        let mut s = FollowTraders::new(states, 1, Duration::hours(1), false);
        let targets: Vec<f64> = (0..8)
            .map(|h| s.on_bar(&btc(), &bar(h)).expect("target"))
            .collect();
        // Known at the bar's close (ts + 1h): a at 2 → bar 1; b at 3 → bar 2; a out at 5 → bar 4.
        assert_eq!(targets, [0.0, 1.0, 1.0, 1.0, 0.5, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn shorts_count_only_where_shorting_is_allowed() {
        let states = vec![state("a", 1, -1.0), state("b", 1, 1.0), state("c", 1, -1.0)];
        let mut long_only = FollowTraders::new(states.clone(), 2, Duration::hours(1), false);
        assert_eq!(long_only.on_bar(&btc(), &bar(1)), Some(0.0));
        let mut both = FollowTraders::new(states, 2, Duration::hours(1), true);
        let t = both.on_bar(&btc(), &bar(1)).expect("target");
        assert!(
            (t + 1.0 / 6.0).abs() < 1e-12,
            "a third short of a half share: {t}"
        );
    }

    #[test]
    fn other_sources_are_ignored() {
        let mut news = state("a", 1, 1.0);
        news.source = "news:final".into();
        let mut s = FollowTraders::new(vec![news], 1, Duration::hours(1), false);
        assert_eq!(s.on_bar(&btc(), &bar(2)), Some(0.0));
    }
}
