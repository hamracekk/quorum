use std::collections::VecDeque;

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
    fn name(&self) -> &str {
        "sma-cross"
    }

    fn on_bar(&mut self, bar: &Bar) -> Option<f64> {
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
}
