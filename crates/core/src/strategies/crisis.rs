//! No new entries while the market is in crisis (the regime model's rule, applied bar by bar).
//!
//! [`CrisisGate`] wraps any strategy. It keeps an equal-weighted index of the instruments it sees
//! and, at each new timestamp, labels the completed one: crisis when the index's 20-bar
//! volatility is at least 1.5× its one-year level **and** the index is at least 20% below its
//! one-year peak (`regime::RegimeRule::default`, the fixed thresholds). While in crisis, a target
//! that would open a position from flat (or flip it) is held back as flat; existing positions
//! keep following the strategy, so exits and reductions still happen. Once the crisis ends, the
//! strategy's held-back target is sent. Point in time: the label uses completed timestamps only.
//!
//! Evidence (evaluator, survivorship-free crypto): entries opened in crisis lost −6.5% on average
//! in sample and −4.7% out of sample (t −4.1).

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};

use crate::instrument::InstrumentId;
use crate::regime::RegimeRule;
use crate::strategy::Strategy;
use crate::types::Bar;

const FLAT: f64 = 1e-12;

/// A strategy that opens no new positions while the market is in crisis.
#[derive(Debug)]
pub struct CrisisGate {
    inner: Box<dyn Strategy>,
    rule: RegimeRule,
    year: usize,
    current: Option<DateTime<Utc>>,
    /// Closes at the timestamp being completed, and at the previous one.
    now_closes: BTreeMap<InstrumentId, f64>,
    prev_closes: BTreeMap<InstrumentId, f64>,
    returns: VecDeque<f64>,
    levels: VecDeque<f64>,
    level: f64,
    crisis: bool,
    desired: BTreeMap<InstrumentId, f64>,
    sent: BTreeMap<InstrumentId, f64>,
}

impl CrisisGate {
    /// `year` is a year of the slot's bars (365 always-open daily, ~252 weekdays).
    pub fn new(inner: Box<dyn Strategy>, year: usize) -> Self {
        Self {
            inner,
            rule: RegimeRule::default(),
            year: year.max(2),
            current: None,
            now_closes: BTreeMap::new(),
            prev_closes: BTreeMap::new(),
            returns: VecDeque::new(),
            levels: VecDeque::new(),
            level: 1.0,
            crisis: false,
            desired: BTreeMap::new(),
            sent: BTreeMap::new(),
        }
    }

    /// Whether the latest completed timestamp was a crisis.
    #[must_use]
    pub const fn in_crisis(&self) -> bool {
        self.crisis
    }

    /// Closes the timestamp just completed: its market return, level and label.
    fn complete(&mut self) {
        let moves: Vec<f64> = self
            .now_closes
            .iter()
            .filter_map(|(id, now)| {
                let prev = *self.prev_closes.get(id)?;
                (prev > 0.0 && *now > 0.0).then(|| now / prev - 1.0)
            })
            .collect();
        for (id, close) in std::mem::take(&mut self.now_closes) {
            self.prev_closes.insert(id, close);
        }
        if moves.is_empty() {
            return;
        }
        let r = moves.iter().sum::<f64>() / moves.len() as f64;
        self.level *= 1.0 + r;
        self.returns.push_back(r);
        self.levels.push_back(self.level);
        while self.returns.len() > self.year {
            self.returns.pop_front();
            self.levels.pop_front();
        }
        self.crisis = self.returns.len() >= self.year && {
            let long = std_dev(self.returns.iter().copied());
            let short_n = self.rule.vol_short_bars.min(self.year);
            let short = std_dev(self.returns.iter().skip(self.year - short_n).copied());
            let peak = self.levels.iter().copied().fold(f64::MIN, f64::max);
            long > 0.0
                && short / long >= self.rule.crisis_vol_ratio
                && 1.0 - self.level / peak >= self.rule.crisis_drawdown
        };
    }
}

fn std_dev(xs: impl Iterator<Item = f64> + Clone) -> f64 {
    let n = xs.clone().count();
    if n < 2 {
        return 0.0;
    }
    let mean = xs.clone().sum::<f64>() / n as f64;
    (xs.map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
}

impl Strategy for CrisisGate {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        if self.current.is_none_or(|ts| bar.ts > ts) {
            if self.current.is_some() {
                self.complete();
            }
            self.current = Some(bar.ts);
        }
        self.now_closes.insert(instrument.clone(), bar.close);
        if let Some(target) = self.inner.on_bar(instrument, bar) {
            self.desired.insert(instrument.clone(), target);
        }
        let held = self.sent.get(instrument).copied().unwrap_or(0.0);
        let wanted = self.desired.get(instrument).copied().unwrap_or(0.0);
        let opens = held.abs() < FLAT && wanted.abs() >= FLAT;
        let flips = held * wanted < 0.0;
        let want = if self.crisis && (opens || flips) {
            0.0
        } else {
            wanted
        };
        if (held - want).abs() <= f64::EPSILON && self.sent.contains_key(instrument) {
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
            map.insert("market_crisis".into(), self.crisis.into());
        }
        Some(reason)
    }

    fn inform(&mut self, calls: &[crate::strategy::ExternalCall]) {
        self.inner.inform(calls);
    }

    fn set_universe(&mut self, members: &std::collections::BTreeSet<InstrumentId>) {
        self.inner.set_universe(members);
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    /// Wants a full position from the first bar on.
    #[derive(Debug)]
    struct AlwaysIn;

    impl Strategy for AlwaysIn {
        fn name(&self) -> &'static str {
            "always-in"
        }
        fn on_bar(&mut self, _: &InstrumentId, _: &Bar) -> Option<f64> {
            Some(1.0)
        }
    }

    fn bar(day: i64, close: f64) -> Bar {
        Bar {
            ts: Utc
                .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
                .single()
                .expect("valid")
                + Duration::days(day),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    /// Calm drift for `calm` days, then a violent fall.
    fn path(calm: i64, crash: i64) -> Vec<f64> {
        let mut out = vec![100.0];
        for d in 1..calm + crash {
            let last = out[out.len() - 1];
            let r = if d < calm {
                if d % 2 == 0 { 0.002 } else { -0.001 }
            } else if d % 2 == 0 {
                -0.08
            } else {
                0.03
            };
            out.push(last * (1.0 + r));
        }
        out
    }

    /// Wants in only from day `from`.
    #[derive(Debug)]
    struct InFrom(i64, i64);

    impl Strategy for InFrom {
        fn name(&self) -> &'static str {
            "in-from"
        }
        fn on_bar(&mut self, _: &InstrumentId, _: &Bar) -> Option<f64> {
            self.1 += 1;
            Some(if self.1 > self.0 { 1.0 } else { 0.0 })
        }
    }

    #[test]
    fn no_entry_during_a_crisis_but_a_position_held_before_it_is_kept() {
        let id = InstrumentId::spot("binance", "AUSDT").expect("valid");
        let mut late = CrisisGate::new(Box::new(InFrom(135, 0)), 100);
        let mut held = CrisisGate::new(Box::new(AlwaysIn), 100);
        let mut late_entered_in_crisis = false;
        let mut held_dropped = false;
        for (d, p) in path(120, 30).iter().enumerate() {
            let b = bar(i64::try_from(d).expect("small"), *p);
            let entry = late.on_bar(&id, &b);
            if late.in_crisis() && entry.is_some_and(|t| t > 0.0) {
                late_entered_in_crisis = true;
            }
            if held.on_bar(&id, &b).is_some_and(|t| t < 1.0) {
                held_dropped = true;
            }
        }
        assert!(!late_entered_in_crisis, "an entry went through in a crisis");
        assert!(!held_dropped, "a position from before the crisis was cut");
    }

    #[test]
    fn a_crash_after_a_calm_year_is_a_crisis_and_calm_is_not() {
        let id = InstrumentId::spot("binance", "AUSDT").expect("valid");
        let mut gate = CrisisGate::new(Box::new(AlwaysIn), 100);
        let prices = path(120, 30);
        let mut labels = Vec::new();
        for (d, p) in prices.iter().enumerate() {
            gate.on_bar(&id, &bar(i64::try_from(d).expect("small"), *p));
            labels.push(gate.in_crisis());
        }
        assert!(!labels[..120].iter().any(|c| *c), "calm is not a crisis");
        assert!(labels[120..].iter().any(|c| *c), "the crash is");
    }
}
