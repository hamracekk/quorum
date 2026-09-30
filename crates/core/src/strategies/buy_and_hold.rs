use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// Baseline: go fully long on the first bar and never trade again.
#[derive(Debug, Clone, Default)]
pub struct BuyAndHold {
    entered: bool,
}

impl Strategy for BuyAndHold {
    fn name(&self) -> &'static str {
        "buy-and-hold"
    }

    fn on_bar(&mut self, _instrument: &InstrumentId, _bar: &Bar) -> Option<f64> {
        if self.entered {
            return None;
        }
        self.entered = true;
        Some(1.0)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    #[test]
    fn enters_fully_once_then_holds() {
        let id = InstrumentId::spot("test", "A").expect("valid instrument");
        let bar = Bar {
            ts: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
        };
        let mut strategy = BuyAndHold::default();
        assert!(
            strategy
                .on_bar(&id, &bar)
                .is_some_and(|t| (t - 1.0).abs() < 1e-12)
        );
        assert!(strategy.on_bar(&id, &bar).is_none());
        assert!(strategy.on_bar(&id, &bar).is_none());
    }
}
