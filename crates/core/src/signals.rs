//! Tradable signals: candlestick patterns plus indicator events, detected bar by bar.
//!
//! Indicator signals fire on the *crossing* into a condition (RSI dropping below 30, MACD
//! crossing its signal line, a close first breaking a Bollinger band), not on every bar that
//! stays there — so each occurrence is one distinct event for scoring and trading.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::indicators::{Bollinger, Macd, MacdValue, Rsi};
use crate::patterns::{Bias, Pattern, detect_at};
use crate::strategies::Sma;
use crate::types::Bar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Signal {
    Pattern(Pattern),
    RsiOversold,
    RsiOverbought,
    MacdBullishCross,
    MacdBearishCross,
    BollingerBelowLower,
    BollingerAboveUpper,
    GoldenCross,
    DeathCross,
}

const INDICATORS: [Signal; 8] = [
    Signal::RsiOversold,
    Signal::RsiOverbought,
    Signal::MacdBullishCross,
    Signal::MacdBearishCross,
    Signal::BollingerBelowLower,
    Signal::BollingerAboveUpper,
    Signal::GoldenCross,
    Signal::DeathCross,
];

impl Signal {
    /// Every signal: the 13 patterns, then the 8 indicator events.
    pub fn all() -> Vec<Self> {
        Pattern::ALL
            .into_iter()
            .map(Self::Pattern)
            .chain(INDICATORS)
            .collect()
    }

    pub const fn bias(self) -> Bias {
        match self {
            Self::Pattern(p) => p.bias(),
            // Oversold / lower-band breaks are read as mean-reversion buys, and vice versa.
            Self::RsiOversold
            | Self::MacdBullishCross
            | Self::BollingerBelowLower
            | Self::GoldenCross => Bias::Bullish,
            Self::RsiOverbought
            | Self::MacdBearishCross
            | Self::BollingerAboveUpper
            | Self::DeathCross => Bias::Bearish,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Pattern(p) => p.name(),
            Self::RsiOversold => "rsi_oversold",
            Self::RsiOverbought => "rsi_overbought",
            Self::MacdBullishCross => "macd_bullish_cross",
            Self::MacdBearishCross => "macd_bearish_cross",
            Self::BollingerBelowLower => "bollinger_below_lower",
            Self::BollingerAboveUpper => "bollinger_above_upper",
            Self::GoldenCross => "golden_cross",
            Self::DeathCross => "death_cross",
        }
    }

    /// `pattern` or `indicator`.
    pub const fn family(self) -> &'static str {
        match self {
            Self::Pattern(_) => "pattern",
            _ => "indicator",
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for Signal {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Signal {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        name.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown signal `{0}`")]
pub struct UnknownSignal(pub String);

impl FromStr for Signal {
    type Err = UnknownSignal;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::all()
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| UnknownSignal(name.to_owned()))
    }
}

/// Indicator parameters for signal detection.
#[derive(Debug, Clone, Copy)]
pub struct SignalConfig {
    pub rsi_period: usize,
    pub rsi_oversold: f64,
    pub rsi_overbought: f64,
    pub bollinger_period: usize,
    pub bollinger_k: f64,
    pub sma_fast: usize,
    pub sma_slow: usize,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            rsi_period: 14,
            rsi_oversold: 30.0,
            rsi_overbought: 70.0,
            bollinger_period: 20,
            bollinger_k: 2.0,
            sma_fast: 20,
            sma_slow: 50,
        }
    }
}

/// Bars kept for pattern detection (trend context + a three-bar pattern).
const WINDOW: usize = 9;

/// Streaming detector: push each closed bar, get the signals it completes.
#[derive(Debug, Clone)]
pub struct SignalDetector {
    cfg: SignalConfig,
    window: Vec<Bar>,
    rsi: Rsi,
    macd: Macd,
    bollinger: Bollinger,
    sma_fast: Sma,
    sma_slow: Sma,
    last_rsi: Option<f64>,
    last_macd: Option<MacdValue>,
    last_band_side: Option<i8>,
    last_smas: Option<(f64, f64)>,
}

impl SignalDetector {
    pub fn new(cfg: SignalConfig) -> Self {
        Self {
            cfg,
            window: Vec::with_capacity(WINDOW + 1),
            rsi: Rsi::new(cfg.rsi_period),
            macd: Macd::default(),
            bollinger: Bollinger::new(cfg.bollinger_period, cfg.bollinger_k),
            sma_fast: Sma::new(cfg.sma_fast),
            sma_slow: Sma::new(cfg.sma_slow),
            last_rsi: None,
            last_macd: None,
            last_band_side: None,
            last_smas: None,
        }
    }

    pub fn push(&mut self, bar: &Bar) -> Vec<Signal> {
        self.window.push(*bar);
        if self.window.len() > WINDOW {
            self.window.remove(0);
        }
        let mut signals: Vec<Signal> = detect_at(&self.window, self.window.len() - 1)
            .into_iter()
            .map(Signal::Pattern)
            .collect();

        if let Some(rsi) = self.rsi.push(bar.close) {
            if let Some(previous) = self.last_rsi {
                if previous >= self.cfg.rsi_oversold && rsi < self.cfg.rsi_oversold {
                    signals.push(Signal::RsiOversold);
                }
                if previous <= self.cfg.rsi_overbought && rsi > self.cfg.rsi_overbought {
                    signals.push(Signal::RsiOverbought);
                }
            }
            self.last_rsi = Some(rsi);
        }

        if let Some(macd) = self.macd.push(bar.close) {
            if let Some(previous) = self.last_macd {
                if previous.macd <= previous.signal && macd.macd > macd.signal {
                    signals.push(Signal::MacdBullishCross);
                }
                if previous.macd >= previous.signal && macd.macd < macd.signal {
                    signals.push(Signal::MacdBearishCross);
                }
            }
            self.last_macd = Some(macd);
        }

        if let Some(band) = self.bollinger.push(bar.close) {
            let side = i8::from(bar.close > band.upper) - i8::from(bar.close < band.lower);
            if self.last_band_side.is_some_and(|previous| previous != side) {
                match side {
                    -1 => signals.push(Signal::BollingerBelowLower),
                    1 => signals.push(Signal::BollingerAboveUpper),
                    _ => {}
                }
            }
            self.last_band_side = Some(side);
        }

        let fast = self.sma_fast.push(bar.close);
        let slow = self.sma_slow.push(bar.close);
        if let (Some(fast), Some(slow)) = (fast, slow) {
            if let Some((previous_fast, previous_slow)) = self.last_smas {
                if previous_fast <= previous_slow && fast > slow {
                    signals.push(Signal::GoldenCross);
                }
                if previous_fast >= previous_slow && fast < slow {
                    signals.push(Signal::DeathCross);
                }
            }
            self.last_smas = Some((fast, slow));
        }
        signals
    }
}

/// Every signal in `bars`: `(index of the bar that completes it, signal)`.
pub fn occurrences(bars: &[Bar], cfg: SignalConfig) -> Vec<(usize, Signal)> {
    let mut detector = SignalDetector::new(cfg);
    bars.iter()
        .enumerate()
        .flat_map(|(i, bar)| detector.push(bar).into_iter().map(move |s| (i, s)))
        .collect()
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

    fn small() -> SignalConfig {
        SignalConfig {
            rsi_period: 3,
            bollinger_period: 5,
            sma_fast: 2,
            sma_slow: 4,
            ..SignalConfig::default()
        }
    }

    fn found(closes: &[f64], cfg: SignalConfig) -> Vec<Signal> {
        let bars: Vec<Bar> = closes
            .iter()
            .zip(0_i64..)
            .map(|(&c, i)| bar(i, c))
            .collect();
        occurrences(&bars, cfg)
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    }

    #[test]
    fn names_round_trip_and_are_unique() {
        let all = Signal::all();
        assert_eq!(all.len(), 21);
        for signal in &all {
            assert_eq!(signal.name().parse::<Signal>(), Ok(*signal));
        }
        assert!("nonsense".parse::<Signal>().is_err());
        assert_eq!(
            serde_json::to_string(&Signal::RsiOversold).expect("json"),
            "\"rsi_oversold\""
        );
    }

    #[test]
    fn rsi_oversold_fires_once_on_entering_the_zone() {
        let signals = found(
            &[10.0, 11.0, 12.0, 13.0, 12.0, 10.0, 8.0, 6.0, 4.0],
            small(),
        );
        assert_eq!(
            signals
                .iter()
                .filter(|s| **s == Signal::RsiOversold)
                .count(),
            1
        );
    }

    #[test]
    fn sma_crosses_fire_on_the_cross() {
        let signals = found(
            &[5.0, 4.0, 3.0, 2.0, 1.0, 2.0, 4.0, 6.0, 8.0, 6.0, 3.0, 1.0],
            small(),
        );
        assert!(signals.contains(&Signal::GoldenCross), "{signals:?}");
        assert!(signals.contains(&Signal::DeathCross), "{signals:?}");
    }

    #[test]
    fn bollinger_break_fires_on_the_first_close_outside() {
        // The band includes the current close, so one outlier can sit at most (n−1)/√n
        // deviations out: a 5-bar band could never be broken at k = 2, a 10-bar band can.
        let cfg = SignalConfig {
            bollinger_period: 10,
            ..small()
        };
        let closes = [
            10.0, 10.1, 9.9, 10.0, 10.1, 9.9, 10.0, 10.1, 9.9, 10.0, 5.0, 4.0,
        ];
        let signals = found(&closes, cfg);
        assert_eq!(
            signals
                .iter()
                .filter(|s| **s == Signal::BollingerBelowLower)
                .count(),
            1
        );
    }

    #[test]
    fn biases_and_families_are_consistent() {
        assert_eq!(Signal::RsiOversold.bias(), Bias::Bullish);
        assert_eq!(Signal::DeathCross.bias(), Bias::Bearish);
        assert_eq!(Signal::Pattern(Pattern::Doji).family(), "pattern");
        assert_eq!(Signal::MacdBullishCross.family(), "indicator");
    }
}
