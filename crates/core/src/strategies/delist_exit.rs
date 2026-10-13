//! Exit before a delisting: once an instrument's delisting is announced it is held flat for
//! good, whatever the inner strategy wants.
//!
//! [`DelistExit`] wraps any strategy with announcement times per instrument (known in
//! advance in backtests, from the stored announcements) and takes new ones live through
//! [`Strategy::inform`] as calls from source [`DELISTING`]. From the first bar at or after an
//! instrument's announcement its target is 0 for [`HOLD_FLAT_DAYS`] — long past the delisting
//! itself, so a ticker Binance lists again years later can trade again. It can only ever
//! reduce exposure.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::strategy::{ExternalCall, Strategy};
use crate::types::Bar;

/// Source id of a delisting announcement call.
pub const DELISTING: &str = "delisting";
/// Days an announced instrument is held flat.
pub const HOLD_FLAT_DAYS: i64 = 90;

/// A strategy that drops every instrument whose delisting was announced.
#[derive(Debug)]
pub struct DelistExit {
    inner: Box<dyn Strategy>,
    /// Announcement times per instrument.
    announced: BTreeMap<InstrumentId, Vec<DateTime<Utc>>>,
    desired: BTreeMap<InstrumentId, f64>,
    sent: BTreeMap<InstrumentId, f64>,
}

impl DelistExit {
    pub fn new(
        inner: Box<dyn Strategy>,
        announced: BTreeMap<InstrumentId, Vec<DateTime<Utc>>>,
    ) -> Self {
        Self {
            inner,
            announced,
            desired: BTreeMap::new(),
            sent: BTreeMap::new(),
        }
    }

    fn exited(&self, instrument: &InstrumentId, ts: DateTime<Utc>) -> bool {
        self.announced.get(instrument).is_some_and(|times| {
            times
                .iter()
                .any(|at| *at <= ts && ts < *at + chrono::Duration::days(HOLD_FLAT_DAYS))
        })
    }
}

impl Strategy for DelistExit {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if let Some(target) = self.inner.on_bar(instrument, bar) {
            self.desired.insert(instrument.clone(), target);
        }
        let want = if self.exited(instrument, bar.ts) {
            0.0
        } else {
            *self.desired.get(instrument)?
        };
        if self
            .sent
            .get(instrument)
            .is_some_and(|last| (last - want).abs() <= f64::EPSILON)
        {
            return None;
        }
        self.sent.insert(instrument.clone(), want);
        Some(want)
    }

    fn explain(&self, instrument: &InstrumentId) -> Option<serde_json::Value> {
        let mut reason = self
            .inner
            .explain(instrument)
            .unwrap_or_else(|| serde_json::json!({}));
        if let (Some(map), Some(at)) = (
            reason.as_object_mut(),
            self.announced.get(instrument).and_then(|t| t.last()),
        ) {
            map.insert("delisting_announced".into(), at.to_rfc3339().into());
        }
        Some(reason)
    }

    fn inform(&mut self, calls: &[ExternalCall]) {
        for call in calls
            .iter()
            .filter(|c| c.source == DELISTING && c.direction < 0.0)
        {
            let times = self.announced.entry(call.instrument.clone()).or_default();
            if !times.contains(&call.ts) {
                times.push(call.ts);
            }
        }
        self.inner.inform(calls);
    }

    fn set_universe(&mut self, members: &BTreeSet<InstrumentId>) {
        self.inner.set_universe(members);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::strategies::BuyAndHold;

    fn day(n: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("ts") + Duration::days(n)
    }

    fn bar(n: i64) -> Bar {
        Bar {
            ts: day(n),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }
    }

    #[test]
    fn an_announced_instrument_is_held_flat_from_then_on() {
        let a = InstrumentId::spot("test", "A").expect("instrument");
        let mut s = DelistExit::new(
            Box::new(BuyAndHold::default()),
            BTreeMap::from([(a.clone(), vec![day(3)])]),
        );
        let targets: Vec<Option<f64>> = (0..6).map(|n| s.on_bar(&a, &bar(n))).collect();
        assert_eq!(targets[0], Some(1.0));
        assert_eq!(targets[3], Some(0.0), "flat from the announcement");
        assert_eq!(targets[4..], [None, None], "and stays flat");
        assert_eq!(
            s.on_bar(&a, &bar(3 + HOLD_FLAT_DAYS)),
            Some(1.0),
            "listed again long after: tradable again"
        );
    }

    #[test]
    fn live_announcements_arrive_as_calls() {
        let a = InstrumentId::spot("test", "A").expect("instrument");
        let mut s = DelistExit::new(Box::new(BuyAndHold::default()), BTreeMap::new());
        assert_eq!(s.on_bar(&a, &bar(0)), Some(1.0));
        s.inform(&[ExternalCall {
            source: DELISTING.into(),
            instrument: a.clone(),
            ts: day(1),
            direction: -1.0,
            strength: 1.0,
        }]);
        assert_eq!(s.on_bar(&a, &bar(1)), Some(0.0));
    }
}
