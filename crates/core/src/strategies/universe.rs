//! Point-in-time universe (ADR 0007, step 3): trade only the instruments that were among the
//! most traded **at the time**, not today's survivors.
//!
//! [`TopVolume`] wraps any strategy. Every `refresh` timestamps it ranks the instruments by
//! dollar volume (close × volume) over their last `window` bars — using only completed
//! timestamps, so the ranking never depends on feed order — and admits the top `size` among those
//! with at least `min_history` bars. The inner strategy sees every bar (its indicators stay warm)
//! and should size for `size` instruments; a target for a non-member is replaced by 0, and an
//! instrument that joins gets the inner strategy's current target.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// How the universe is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniverseRule {
    /// Members at a time.
    pub size: usize,
    /// Bars of dollar volume behind the ranking.
    pub window: usize,
    /// Timestamps between re-rankings.
    pub refresh: usize,
    /// Bars an instrument needs before it can join (new listings are skipped).
    pub min_history: usize,
}

/// A strategy restricted to a point-in-time top-volume universe.
#[derive(Debug)]
pub struct TopVolume {
    inner: Box<dyn Strategy>,
    rule: UniverseRule,
    volumes: BTreeMap<InstrumentId, VecDeque<f64>>,
    bars: BTreeMap<InstrumentId, usize>,
    /// The timestamp index each instrument last traded at (a stopped one cannot rank).
    seen: BTreeMap<InstrumentId, usize>,
    current: Option<DateTime<Utc>>,
    stamps: usize,
    members: BTreeSet<InstrumentId>,
    /// The inner strategy's latest target per instrument.
    desired: BTreeMap<InstrumentId, f64>,
    /// What this wrapper last returned per instrument.
    sent: BTreeMap<InstrumentId, f64>,
}

impl TopVolume {
    pub fn new(inner: Box<dyn Strategy>, rule: UniverseRule) -> Self {
        Self {
            inner,
            rule: UniverseRule {
                size: rule.size.max(1),
                window: rule.window.max(1),
                refresh: rule.refresh.max(1),
                min_history: rule.min_history,
            },
            volumes: BTreeMap::new(),
            bars: BTreeMap::new(),
            seen: BTreeMap::new(),
            current: None,
            stamps: 0,
            members: BTreeSet::new(),
            desired: BTreeMap::new(),
            sent: BTreeMap::new(),
        }
    }

    /// The current members.
    pub const fn members(&self) -> &BTreeSet<InstrumentId> {
        &self.members
    }

    fn rank(&mut self) {
        let mut scored: Vec<(&InstrumentId, f64)> = self
            .volumes
            .iter()
            .filter(|(id, _)| self.bars.get(*id).copied().unwrap_or(0) >= self.rule.min_history)
            // Traded in the timestamp just completed: delisted coins drop out.
            .filter(|(id, _)| self.seen.get(*id).is_some_and(|s| s + 1 >= self.stamps))
            .map(|(id, v)| (id, v.iter().sum::<f64>() / v.len().max(1) as f64))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        self.members = scored
            .into_iter()
            .take(self.rule.size)
            .map(|(id, _)| id.clone())
            .collect();
    }
}

impl Strategy for TopVolume {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if self.current.is_none_or(|ts| bar.ts > ts) {
            // A new timestamp: every instrument's volume is complete up to the previous one.
            if self.current.is_some() {
                if self.stamps.is_multiple_of(self.rule.refresh) {
                    self.rank();
                }
                self.stamps += 1;
            }
            self.current = Some(bar.ts);
        }
        let volume = self.volumes.entry(instrument.clone()).or_default();
        volume.push_back(bar.close * bar.volume);
        while volume.len() > self.rule.window {
            volume.pop_front();
        }
        *self.bars.entry(instrument.clone()).or_default() += 1;
        self.seen.insert(instrument.clone(), self.stamps);
        if let Some(target) = self.inner.on_bar(instrument, bar) {
            self.desired.insert(instrument.clone(), target);
        }
        let want = if self.members.contains(instrument) {
            self.desired.get(instrument).copied().unwrap_or(0.0)
        } else {
            0.0
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
        if let Some(map) = reason.as_object_mut() {
            map.insert(
                "universe_member".into(),
                self.members.contains(instrument).into(),
            );
        }
        Some(reason)
    }
}

#[cfg(test)]
mod tests;
