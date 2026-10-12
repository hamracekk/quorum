//! A regime filter (R-6): scale every target of a strategy by a schedule known in advance.
//!
//! [`ExposureSchedule`] wraps any strategy; at each bar the scale is the latest schedule entry
//! at or before the bar's timestamp (`1` before the first), so a regime signal computed from
//! data up to a time only acts from that time on. A scale change re-sends each instrument's
//! scaled target at its next bar.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::strategy::{ExternalCall, Strategy};
use crate::types::Bar;

/// A strategy whose targets are multiplied by a time schedule of scales.
#[derive(Debug)]
pub struct ExposureSchedule {
    inner: Box<dyn Strategy>,
    /// `(from, scale)`, oldest first.
    schedule: Vec<(DateTime<Utc>, f64)>,
    next: usize,
    scale: f64,
    desired: BTreeMap<InstrumentId, f64>,
    sent: BTreeMap<InstrumentId, f64>,
}

impl ExposureSchedule {
    pub fn new(inner: Box<dyn Strategy>, mut schedule: Vec<(DateTime<Utc>, f64)>) -> Self {
        schedule.sort_by_key(|(ts, _)| *ts);
        Self {
            inner,
            schedule,
            next: 0,
            scale: 1.0,
            desired: BTreeMap::new(),
            sent: BTreeMap::new(),
        }
    }
}

impl Strategy for ExposureSchedule {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        while let Some(&(from, scale)) = self.schedule.get(self.next) {
            if from > bar.ts {
                break;
            }
            self.scale = scale.clamp(0.0, 1.0);
            self.next += 1;
        }
        if let Some(target) = self.inner.on_bar(instrument, bar) {
            self.desired.insert(instrument.clone(), target);
        }
        let want = self.desired.get(instrument)? * self.scale;
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
        if let Some(map) = reason.as_object_mut() {
            map.insert("regime_scale".into(), self.scale.into());
        }
        Some(reason)
    }

    fn inform(&mut self, calls: &[ExternalCall]) {
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
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1.0,
        }
    }

    #[test]
    fn targets_follow_the_schedule_from_its_time_on() {
        let a = InstrumentId::spot("test", "A").expect("instrument");
        let mut s = ExposureSchedule::new(
            Box::new(BuyAndHold::default()),
            vec![(day(3), 0.5), (day(6), 1.0)],
        );
        let targets: Vec<Option<f64>> = (0..8).map(|n| s.on_bar(&a, &bar(n))).collect();
        assert_eq!(targets[0], Some(1.0), "before the first entry: full");
        assert_eq!(targets[1], None, "unchanged is not repeated");
        assert_eq!(targets[3], Some(0.5), "halved from day 3");
        assert_eq!(targets[4], None);
        assert_eq!(targets[6], Some(1.0), "back to full from day 6");
    }
}
