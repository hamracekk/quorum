use std::collections::VecDeque;

use crate::instrument::InstrumentId;
use crate::strategy::Strategy;
use crate::types::Bar;

/// Rolling simple moving average.
#[derive(Debug, Clone)]
pub struct Sma {
    period: usize,
    window: VecDeque<f64>,
    sum: f64,
}

impl Sma {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "SMA period must be positive");
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
        }
    }

    /// Add a value; returns the average once the window is full.
    pub fn push(&mut self, value: f64) -> Option<f64> {
        self.window.push_back(value);
        self.sum += value;
        if self.window.len() > self.period {
            self.sum -= self.window.pop_front().unwrap_or_default();
        }
        (self.window.len() == self.period).then(|| self.sum / self.period as f64)
    }
}

/// Classic trend follower: long when the fast SMA is above the slow SMA,
/// otherwise flat (or short when `allow_short`).
#[derive(Debug, Clone)]
pub struct SmaCross {
    fast: Sma,
    slow: Sma,
    allow_short: bool,
}

impl SmaCross {
    pub fn new(fast: usize, slow: usize, allow_short: bool) -> Self {
        assert!(fast < slow, "fast period must be shorter than slow period");
        Self {
            fast: Sma::new(fast),
            slow: Sma::new(slow),
            allow_short,
        }
    }
}

impl Strategy for SmaCross {
    fn name(&self) -> &'static str {
        "sma-cross"
    }

    fn on_bar(&mut self, _instrument: &InstrumentId, bar: &Bar) -> Option<f64> {
        let fast = self.fast.push(bar.close);
        let slow = self.slow.push(bar.close);
        let (fast, slow) = (fast?, slow?);
        Some(match (fast > slow, self.allow_short) {
            (true, _) => 1.0,
            (false, true) => -1.0,
            (false, false) => 0.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sma_waits_for_full_window() {
        let mut sma = Sma::new(3);
        assert_eq!(sma.push(1.0), None);
        assert_eq!(sma.push(2.0), None);
        assert_eq!(sma.push(3.0), Some(2.0));
        assert_eq!(sma.push(4.0), Some(3.0));
    }

    fn id() -> InstrumentId {
        InstrumentId::spot("test", "A").expect("valid instrument")
    }

    fn signals(strategy: &mut SmaCross, closes: &[f64]) -> Vec<Option<f64>> {
        closes
            .iter()
            .zip(0_i64..)
            .map(|(&close, hour)| {
                let bar = Bar {
                    ts: chrono::DateTime::from_timestamp(hour * 3_600, 0).expect("valid timestamp"),
                    open: close,
                    high: close,
                    low: close,
                    close,
                    volume: 1.0,
                };
                strategy.on_bar(&id(), &bar)
            })
            .collect()
    }

    fn is(signal: Option<f64>, expected: f64) -> bool {
        signal.is_some_and(|t| (t - expected).abs() < 1e-12)
    }

    #[test]
    fn silent_until_the_slow_window_is_full() {
        let out = signals(&mut SmaCross::new(2, 4, false), &[1.0, 2.0, 3.0, 4.0]);
        assert!(out[..3].iter().all(Option::is_none));
        assert!(out[3].is_some());
    }

    #[test]
    fn long_in_an_uptrend() {
        let out = signals(&mut SmaCross::new(2, 4, false), &[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert!(is(out[4], 1.0));
    }

    #[test]
    fn flat_in_a_downtrend_without_shorting() {
        let out = signals(&mut SmaCross::new(2, 4, false), &[5.0, 4.0, 3.0, 2.0, 1.0]);
        assert!(is(out[4], 0.0));
    }

    #[test]
    fn short_in_a_downtrend_when_allowed() {
        let out = signals(&mut SmaCross::new(2, 4, true), &[5.0, 4.0, 3.0, 2.0, 1.0]);
        assert!(is(out[4], -1.0));
    }

    #[test]
    #[should_panic(expected = "fast period must be shorter")]
    fn rejects_fast_not_below_slow() {
        let _ = SmaCross::new(5, 5, false);
    }

    #[test]
    #[should_panic(expected = "SMA period must be positive")]
    fn rejects_zero_period() {
        let _ = Sma::new(0);
    }
}
