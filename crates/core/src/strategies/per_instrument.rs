use std::collections::BTreeMap;

use crate::strategy::Strategy;
use crate::types::Bar;

/// Runs an independent copy of a single-asset strategy for every symbol and gives
/// each an equal share of equity (`1 / symbols`).
#[derive(Debug, Clone)]
pub struct PerSymbol<S> {
    template: S,
    weight: f64,
    instances: BTreeMap<String, S>,
}

impl<S: Strategy + Clone> PerSymbol<S> {
    pub fn new(template: S, symbols: usize) -> Self {
        Self {
            template,
            weight: if symbols == 0 {
                0.0
            } else {
                1.0 / symbols as f64
            },
            instances: BTreeMap::new(),
        }
    }
}

impl<S: Strategy + Clone> Strategy for PerSymbol<S> {
    fn name(&self) -> &str {
        self.template.name()
    }

    fn on_bar(&mut self, symbol: &str, bar: &Bar) -> Option<f64> {
        let weight = self.weight;
        let template = &self.template;
        let instance = self
            .instances
            .entry(symbol.to_owned())
            .or_insert_with(|| template.clone());
        instance.on_bar(symbol, bar).map(|target| target * weight)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;
    use crate::strategies::BuyAndHold;

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
    fn each_symbol_has_its_own_state() {
        let mut strategy = PerSymbol::new(BuyAndHold::default(), 4);
        let a = strategy.on_bar("A", &bar()).expect("A enters");
        let b = strategy.on_bar("B", &bar()).expect("B enters");
        assert!((a - 0.25).abs() < 1e-12);
        assert!((b - 0.25).abs() < 1e-12);
        assert!(strategy.on_bar("A", &bar()).is_none());
    }
}
