use crate::instrument::InstrumentId;
use crate::patterns::Bias;
use crate::signals::{Signal, SignalConfig, SignalDetector};
use crate::strategy::Strategy;
use crate::types::Bar;

/// Trades a chosen set of signals for a fixed holding period.
///
/// A bullish signal goes long for `hold` bars; a bearish signal exits (or goes short for
/// `hold` bars when shorting is allowed). Neutral signals are ignored. A new entry signal
/// while holding restarts the holding period.
#[derive(Debug, Clone)]
pub struct SignalFollower {
    signals: Vec<Signal>,
    hold: usize,
    allow_short: bool,
    detector: SignalDetector,
    target: f64,
    bars_held: usize,
}

impl SignalFollower {
    pub fn new(signals: Vec<Signal>, hold: usize, allow_short: bool, cfg: SignalConfig) -> Self {
        assert!(hold > 0, "holding period must be at least one bar");
        Self {
            signals,
            hold,
            allow_short,
            detector: SignalDetector::new(cfg),
            target: 0.0,
            bars_held: 0,
        }
    }
}

impl Strategy for SignalFollower {
    fn name(&self) -> &'static str {
        "signal-follower"
    }

    fn on_bar(&mut self, _instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let fired = self.detector.push(bar);
        let wanted = |bias: Bias| {
            fired
                .iter()
                .any(|s| s.bias() == bias && self.signals.contains(s))
        };
        if wanted(Bias::Bullish) {
            self.target = 1.0;
            self.bars_held = 0;
        } else if wanted(Bias::Bearish) {
            self.target = if self.allow_short { -1.0 } else { 0.0 };
            self.bars_held = 0;
        } else if self.target.abs() > f64::EPSILON {
            self.bars_held += 1;
            if self.bars_held >= self.hold {
                self.target = 0.0;
            }
        }
        Some(self.target)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    fn bar(i: i64, close: f64) -> Bar {
        Bar {
            ts: DateTime::from_timestamp(i * 3_600, 0).expect("valid timestamp"),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1.0,
        }
    }

    fn cfg() -> SignalConfig {
        SignalConfig {
            sma_fast: 2,
            sma_slow: 4,
            ..SignalConfig::default()
        }
    }

    fn targets(strategy: &mut SignalFollower, closes: &[f64]) -> Vec<f64> {
        let id = InstrumentId::spot("test", "A").expect("valid instrument");
        closes
            .iter()
            .zip(0_i64..)
            .filter_map(|(&c, i)| strategy.on_bar(&id, &bar(i, c)))
            .collect()
    }

    #[test]
    fn enters_on_a_chosen_signal_and_exits_after_the_holding_period() {
        let mut strategy = SignalFollower::new(vec![Signal::GoldenCross], 2, false, cfg());
        let out = targets(
            &mut strategy,
            &[5.0, 4.0, 3.0, 2.0, 1.0, 2.0, 4.0, 6.0, 7.0, 8.0, 9.0],
        );
        let entered = out
            .iter()
            .position(|t| (*t - 1.0).abs() < 1e-12)
            .expect("entered");
        assert!((out[entered + 1] - 1.0).abs() < 1e-12, "held one more bar");
        assert!(out[entered + 2].abs() < 1e-12, "flat after two bars");
    }

    #[test]
    fn ignores_signals_it_was_not_asked_to_trade() {
        let mut strategy = SignalFollower::new(vec![Signal::RsiOversold], 5, false, cfg());
        let out = targets(
            &mut strategy,
            &[5.0, 4.0, 3.0, 2.0, 1.0, 2.0, 4.0, 6.0, 7.0],
        );
        assert!(out.iter().all(|t| t.abs() < 1e-12), "{out:?}");
    }
}
