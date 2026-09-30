//! Streaming technical indicators: push one close at a time, get a value once warmed up.
//! Every indicator uses only data already pushed, so they are safe for live trading.

use std::collections::VecDeque;

use serde::Serialize;

/// Exponential moving average, seeded with the simple average of the first `period` values.
#[derive(Debug, Clone)]
pub struct Ema {
    period: usize,
    alpha: f64,
    seed: Vec<f64>,
    value: Option<f64>,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "EMA period must be positive");
        Self {
            period,
            alpha: 2.0 / (period as f64 + 1.0),
            seed: Vec::with_capacity(period),
            value: None,
        }
    }

    pub fn push(&mut self, x: f64) -> Option<f64> {
        if let Some(previous) = self.value {
            let next = self.alpha.mul_add(x - previous, previous);
            self.value = Some(next);
            return self.value;
        }
        self.seed.push(x);
        if self.seed.len() == self.period {
            self.value = Some(self.seed.iter().sum::<f64>() / self.period as f64);
        }
        self.value
    }
}

/// Relative Strength Index with Wilder smoothing (0..=100).
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    previous: Option<f64>,
    gains: f64,
    losses: f64,
    changes: usize,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "RSI period must be positive");
        Self {
            period,
            previous: None,
            gains: 0.0,
            losses: 0.0,
            changes: 0,
        }
    }

    pub fn push(&mut self, close: f64) -> Option<f64> {
        let previous = self.previous.replace(close)?;
        let change = close - previous;
        let (gain, loss) = (change.max(0.0), (-change).max(0.0));
        let n = self.period as f64;
        if self.changes < self.period {
            self.gains += gain;
            self.losses += loss;
            self.changes += 1;
            if self.changes < self.period {
                return None;
            }
            self.gains /= n;
            self.losses /= n;
        } else {
            self.gains = self.gains.mul_add(n - 1.0, gain) / n;
            self.losses = self.losses.mul_add(n - 1.0, loss) / n;
        }
        Some(if self.losses == 0.0 {
            if self.gains == 0.0 { 50.0 } else { 100.0 }
        } else {
            100.0 - 100.0 / (1.0 + self.gains / self.losses)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MacdValue {
    pub macd: f64,
    pub signal: f64,
    pub histogram: f64,
}

/// MACD: EMA(fast) − EMA(slow), with an EMA(signal) of that line.
#[derive(Debug, Clone)]
pub struct Macd {
    fast: Ema,
    slow: Ema,
    signal: Ema,
}

impl Default for Macd {
    fn default() -> Self {
        Self::new(12, 26, 9)
    }
}

impl Macd {
    pub fn new(fast: usize, slow: usize, signal: usize) -> Self {
        Self {
            fast: Ema::new(fast),
            slow: Ema::new(slow),
            signal: Ema::new(signal),
        }
    }

    pub fn push(&mut self, close: f64) -> Option<MacdValue> {
        let fast = self.fast.push(close);
        let slow = self.slow.push(close);
        let macd = fast? - slow?;
        let signal = self.signal.push(macd)?;
        Some(MacdValue {
            macd,
            signal,
            histogram: macd - signal,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Band {
    pub upper: f64,
    pub middle: f64,
    pub lower: f64,
}

/// Bollinger Bands: SMA ± `k` population standard deviations over `period` closes.
#[derive(Debug, Clone)]
pub struct Bollinger {
    period: usize,
    k: f64,
    window: VecDeque<f64>,
}

impl Bollinger {
    pub fn new(period: usize, k: f64) -> Self {
        assert!(period > 1, "Bollinger period must exceed 1");
        Self {
            period,
            k,
            window: VecDeque::with_capacity(period),
        }
    }

    pub fn push(&mut self, close: f64) -> Option<Band> {
        self.window.push_back(close);
        if self.window.len() > self.period {
            self.window.pop_front();
        }
        if self.window.len() < self.period {
            return None;
        }
        let n = self.period as f64;
        let mean = self.window.iter().sum::<f64>() / n;
        let variance = self.window.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        let width = self.k * variance.sqrt();
        Some(Band {
            upper: mean + width,
            middle: mean,
            lower: mean - width,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn ema_is_seeded_with_the_sma_and_tracks_a_constant() {
        let mut ema = Ema::new(3);
        assert_eq!(ema.push(1.0), None);
        assert_eq!(ema.push(2.0), None);
        assert!(ema.push(3.0).is_some_and(|v| close(v, 2.0)));
        assert!(
            ema.push(4.0).is_some_and(|v| close(v, 3.0)),
            "alpha 0.5: 2 + 0.5·(4−2)"
        );
    }

    #[test]
    fn rsi_is_100_on_pure_gains_0_on_pure_losses_and_50_when_flat() {
        let feed = |values: &[f64]| {
            let mut rsi = Rsi::new(3);
            values.iter().filter_map(|&v| rsi.push(v)).last()
        };
        assert!(feed(&[1.0, 2.0, 3.0, 4.0, 5.0]).is_some_and(|v| close(v, 100.0)));
        assert!(feed(&[5.0, 4.0, 3.0, 2.0, 1.0]).is_some_and(|v| close(v, 0.0)));
        assert!(feed(&[3.0; 6]).is_some_and(|v| close(v, 50.0)));
        assert_eq!(feed(&[1.0, 2.0, 3.0]), None, "needs `period` changes");
    }

    #[test]
    fn rsi_matches_a_hand_computed_value() {
        // Changes +1, −1, +2 → avg gain 1, avg loss 1/3 → RS 3 → RSI 75.
        let mut rsi = Rsi::new(3);
        let last = [10.0, 11.0, 10.0, 12.0]
            .iter()
            .filter_map(|&v| rsi.push(v))
            .last();
        assert!(last.is_some_and(|v| close(v, 75.0)), "{last:?}");
    }

    #[test]
    fn macd_is_zero_on_a_constant_series() {
        let mut macd = Macd::new(2, 4, 2);
        let last = (0..10)
            .filter_map(|_| macd.push(5.0))
            .last()
            .expect("warmed up");
        assert!(close(last.macd, 0.0) && close(last.signal, 0.0) && close(last.histogram, 0.0));
    }

    #[test]
    fn bollinger_collapses_on_a_constant_and_widens_with_variance() {
        let mut flat = Bollinger::new(3, 2.0);
        let band = [5.0, 5.0, 5.0]
            .iter()
            .filter_map(|&v| flat.push(v))
            .last()
            .expect("band");
        assert!(close(band.upper, 5.0) && close(band.lower, 5.0));

        let mut moving = Bollinger::new(2, 2.0);
        let band = [4.0, 6.0]
            .iter()
            .filter_map(|&v| moving.push(v))
            .last()
            .expect("band");
        assert!(close(band.middle, 5.0) && close(band.upper, 7.0) && close(band.lower, 3.0));
    }
}
