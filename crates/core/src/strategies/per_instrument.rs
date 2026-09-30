use std::collections::BTreeMap;

use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// Runs an independent copy of a single-asset strategy for every instrument and gives
/// each an equal share of equity (`1 / instruments`).
#[derive(Debug, Clone)]
pub struct PerInstrument<S> {
    template: S,
    weight: f64,
    instances: BTreeMap<InstrumentId, S>,
}

impl<S: Strategy + Clone> PerInstrument<S> {
    pub fn new(template: S, instruments: usize) -> Self {
        Self {
            template,
            weight: if instruments == 0 {
                0.0
            } else {
                1.0 / instruments as f64
            },
            instances: BTreeMap::new(),
        }
    }
}

impl<S: Strategy + Clone> Strategy for PerInstrument<S> {
    fn name(&self) -> &str {
        self.template.name()
    }

    fn on_bar(&mut self, instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let weight = self.weight;
        let template = &self.template;
        let instance = self
            .instances
            .entry(instrument.clone())
            .or_insert_with(|| template.clone());
        instance
            .on_bar(instrument, bar)
            .map(|target| target * weight)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;
    use crate::strategies::BuyAndHold;

    fn id(symbol: &str) -> InstrumentId {
        InstrumentId::spot("test", symbol).expect("valid instrument")
    }

    fn bar() -> Bar {
        Bar {
            ts: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        }
    }

    #[test]
    fn each_instrument_has_its_own_state() {
        let mut strategy = PerInstrument::new(BuyAndHold::default(), 4);
        let a = strategy.on_bar(&id("A"), &bar()).expect("A enters");
        let b = strategy.on_bar(&id("B"), &bar()).expect("B enters");
        assert!((a - 0.25).abs() < 1e-12);
        assert!((b - 0.25).abs() < 1e-12);
        assert!(strategy.on_bar(&id("A"), &bar()).is_none());
    }
}
