use crate::strategy::Strategy;
use crate::types::Bar;

/// Baseline: go fully long on the first bar and never trade again.
#[derive(Debug, Default)]
pub struct BuyAndHold {
    entered: bool,
}

impl Strategy for BuyAndHold {
    fn name(&self) -> &'static str {
        "buy-and-hold"
    }

    fn on_bar(&mut self, _bar: &Bar) -> Option<f64> {
        if self.entered {
            return None;
        }
        self.entered = true;
        Some(1.0)
    }
}
