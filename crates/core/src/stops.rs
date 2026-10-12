//! Per-trade exits (T-2, ADR 0006): stop-loss, take-profit and trailing stop, plus sizing by
//! risk per trade, for every strategy.
//!
//! A trade opens when a position goes from flat (or the other side) to long or short. Its stop
//! sits `distance` from the entry — a fixed fraction of the price, or a multiple of the average
//! true range when the trade opened — and its take-profit `take_profit_r` stop distances on the
//! other side. Each bar is first checked against the levels set *before* it (low/high against
//! stop and target; a gap through a level exits at the open), then a trailing stop follows the
//! best price for the next bar — so a bar never moves a level it is then judged by. After an
//! exit the instrument stays flat until the strategy changes its target (or `cooldown_bars`
//! have passed), so a stop is not undone by re-entering on the same signal.
//!
//! Everything is off by default; with the defaults the engine behaves exactly as without stops.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::types::Bar;

/// The exit rules (all off by default).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StopRules {
    /// Stop distance as a fraction of the entry price (`0.05` = 5%); `0` = use `stop_atr`.
    pub stop_pct: f64,
    /// Stop distance in average true ranges at entry; `0` (with `stop_pct` 0) = no stop.
    pub stop_atr: f64,
    /// Bars behind the average true range.
    pub atr_bars: usize,
    /// Take-profit in stop distances (`3` = 1:3 risk/reward); `0` = none.
    pub take_profit_r: f64,
    /// The stop follows the best price at the stop distance.
    pub trailing: bool,
    /// Size so a stopped trade loses at most this fraction of equity; `0` = off.
    pub risk_per_trade: f64,
    /// Bars an instrument stays flat after an exit; `0` = until the strategy changes its target.
    pub cooldown_bars: usize,
    /// Crash stop: exit a long whose price falls this fraction below the previous close (a short
    /// whose price rises as much), filled at that level — or at the open on a gap — like a stop
    /// order resting at the exchange. `0` = off. Independent of the other rules.
    pub crash_pct: f64,
    /// A held instrument with no bar for this many timestamps is treated as delisted and sold at
    /// its last close less `stale_haircut` (R-1). `0` = off.
    pub stale_bars: usize,
    /// Fraction knocked off the last close when a delisted position is sold.
    pub stale_haircut: f64,
}

impl Default for StopRules {
    fn default() -> Self {
        Self {
            stop_pct: 0.0,
            stop_atr: 0.0,
            atr_bars: 14,
            take_profit_r: 0.0,
            trailing: false,
            risk_per_trade: 0.0,
            cooldown_bars: 0,
            crash_pct: 0.0,
            stale_bars: 0,
            stale_haircut: 0.0,
        }
    }
}

impl StopRules {
    /// Whether any rule is on.
    pub fn active(&self) -> bool {
        self.stop_pct > 0.0 || self.stop_atr > 0.0
    }

    /// Stop distance in price for a trade entered at `price`, given the current ATR.
    pub fn distance(&self, price: f64, atr: Option<f64>) -> Option<f64> {
        if self.stop_pct > 0.0 {
            Some(price * self.stop_pct)
        } else if self.stop_atr > 0.0 {
            atr.map(|a| a * self.stop_atr).filter(|d| *d > 0.0)
        } else {
            None
        }
    }

    /// `target` limited so a stop at `distance` loses at most `risk_per_trade` of equity.
    pub fn size(&self, target: f64, price: f64, distance: Option<f64>) -> f64 {
        match distance {
            Some(d) if self.risk_per_trade > 0.0 && d > 0.0 && price > 0.0 => {
                let cap = self.risk_per_trade / (d / price);
                target.signum() * target.abs().min(cap)
            }
            _ => target,
        }
    }
}

/// Average true range over the last `bars` bars (simple mean).
#[derive(Debug, Clone)]
pub struct Atr {
    bars: usize,
    ranges: VecDeque<f64>,
    prev_close: Option<f64>,
}

impl Atr {
    pub fn new(bars: usize) -> Self {
        Self {
            bars: bars.max(1),
            ranges: VecDeque::new(),
            prev_close: None,
        }
    }

    pub fn push(&mut self, bar: &Bar) {
        let range = self.prev_close.map_or(bar.high - bar.low, |c| {
            (bar.high - bar.low)
                .max((bar.high - c).abs())
                .max((bar.low - c).abs())
        });
        self.ranges.push_back(range);
        if self.ranges.len() > self.bars {
            self.ranges.pop_front();
        }
        self.prev_close = Some(bar.close);
    }

    /// The average once `bars` ranges are known.
    pub fn value(&self) -> Option<f64> {
        (self.ranges.len() == self.bars).then(|| self.ranges.iter().sum::<f64>() / self.bars as f64)
    }
}

/// Why a trade ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitKind {
    StopLoss,
    TakeProfit,
    TrailingStop,
}

/// An open trade's levels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OpenTrade {
    /// `1.0` long, `-1.0` short.
    pub direction: f64,
    pub entry: f64,
    pub distance: f64,
    pub stop: f64,
    pub take_profit: Option<f64>,
    /// Best price seen since entry (highest for longs, lowest for shorts).
    pub best: f64,
    /// The stop has moved off its initial level.
    pub trailed: bool,
}

impl OpenTrade {
    pub fn open(rules: &StopRules, direction: f64, entry: f64, distance: f64) -> Self {
        let d = direction.signum();
        Self {
            direction: d,
            entry,
            distance,
            stop: d.mul_add(-distance, entry),
            take_profit: (rules.take_profit_r > 0.0)
                .then(|| d.mul_add(distance * rules.take_profit_r, entry)),
            best: entry,
            trailed: false,
        }
    }

    /// Whether `bar` hits a level set before it, and the exit price (a gap exits at the open).
    /// The stop is checked first: within one bar we cannot know which came first, so the
    /// cautious assumption is taken.
    pub fn check(&self, bar: &Bar) -> Option<(ExitKind, f64)> {
        let stop_kind = if self.trailed {
            ExitKind::TrailingStop
        } else {
            ExitKind::StopLoss
        };
        if self.direction > 0.0 {
            if bar.low <= self.stop {
                return Some((stop_kind, bar.open.min(self.stop)));
            }
            if let Some(tp) = self.take_profit.filter(|tp| bar.high >= *tp) {
                return Some((ExitKind::TakeProfit, bar.open.max(tp)));
            }
        } else {
            if bar.high >= self.stop {
                return Some((stop_kind, bar.open.max(self.stop)));
            }
            if let Some(tp) = self.take_profit.filter(|tp| bar.low <= *tp) {
                return Some((ExitKind::TakeProfit, bar.open.min(tp)));
            }
        }
        None
    }

    /// After a bar survived: follow the best price with a trailing stop (for the next bar).
    pub fn trail(&mut self, rules: &StopRules, bar: &Bar) {
        if self.direction > 0.0 {
            self.best = self.best.max(bar.high);
        } else {
            self.best = self.best.min(bar.low);
        }
        if !rules.trailing {
            return;
        }
        let candidate = self.direction.mul_add(-self.distance, self.best);
        let better = if self.direction > 0.0 {
            candidate > self.stop
        } else {
            candidate < self.stop
        };
        if better {
            self.stop = candidate;
            self.trailed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(open: f64, high: f64, low: f64, close: f64) -> Bar {
        Bar {
            ts: chrono::DateTime::from_timestamp(0, 0).expect("ts"),
            open,
            high,
            low,
            close,
            volume: 1.0,
        }
    }

    fn rules() -> StopRules {
        StopRules {
            stop_pct: 0.05,
            take_profit_r: 2.0,
            ..StopRules::default()
        }
    }

    #[test]
    fn off_by_default() {
        let r = StopRules::default();
        assert!(!r.active());
        assert_eq!(r.distance(100.0, Some(3.0)), None);
        assert!((r.size(0.8, 100.0, Some(5.0)) - 0.8).abs() < 1e-12);
    }

    #[test]
    fn a_long_is_stopped_below_and_takes_profit_above() {
        let trade = OpenTrade::open(&rules(), 1.0, 100.0, 5.0);
        assert!((trade.stop - 95.0).abs() < 1e-12);
        assert_eq!(trade.take_profit, Some(110.0));
        assert_eq!(trade.check(&bar(100.0, 104.0, 96.0, 101.0)), None);
        assert_eq!(
            trade.check(&bar(99.0, 100.0, 94.0, 96.0)),
            Some((ExitKind::StopLoss, 95.0))
        );
        assert_eq!(
            trade.check(&bar(90.0, 92.0, 89.0, 91.0)),
            Some((ExitKind::StopLoss, 90.0)),
            "a gap through the stop exits at the open"
        );
        assert_eq!(
            trade.check(&bar(105.0, 111.0, 104.0, 110.0)),
            Some((ExitKind::TakeProfit, 110.0))
        );
    }

    #[test]
    fn a_short_mirrors_the_levels() {
        let trade = OpenTrade::open(&rules(), -1.0, 100.0, 5.0);
        assert!((trade.stop - 105.0).abs() < 1e-12);
        assert_eq!(trade.take_profit, Some(90.0));
        assert_eq!(
            trade.check(&bar(101.0, 106.0, 100.0, 104.0)),
            Some((ExitKind::StopLoss, 105.0))
        );
        assert_eq!(
            trade.check(&bar(95.0, 96.0, 89.0, 90.0)),
            Some((ExitKind::TakeProfit, 90.0))
        );
    }

    #[test]
    fn the_trailing_stop_follows_the_best_price_only_for_the_next_bar() {
        let r = StopRules {
            trailing: true,
            ..rules()
        };
        let mut trade = OpenTrade::open(&r, 1.0, 100.0, 5.0);
        let up = bar(100.0, 108.0, 99.0, 107.0);
        assert_eq!(trade.check(&up), None, "judged by the old stop (95)");
        trade.trail(&r, &up);
        assert!((trade.stop - 103.0).abs() < 1e-12);
        assert_eq!(
            trade.check(&bar(106.0, 106.0, 102.0, 103.0)),
            Some((ExitKind::TrailingStop, 103.0))
        );
        trade.trail(&r, &bar(100.0, 101.0, 99.0, 100.0));
        assert!((trade.stop - 103.0).abs() < 1e-12, "never moves back");
    }

    #[test]
    fn atr_distance_and_risk_sizing() {
        let mut atr = Atr::new(2);
        atr.push(&bar(100.0, 102.0, 98.0, 101.0)); // range 4
        assert_eq!(atr.value(), None);
        atr.push(&bar(101.0, 103.0, 100.0, 102.0)); // range 3
        assert_eq!(atr.value(), Some(3.5));
        let r = StopRules {
            stop_atr: 2.0,
            risk_per_trade: 0.01,
            ..StopRules::default()
        };
        assert_eq!(r.distance(100.0, atr.value()), Some(7.0));
        // A 7% stop risking 1% of equity allows ~14% of equity.
        let sized = r.size(1.0, 100.0, Some(7.0));
        assert!((sized - 0.01 / 0.07).abs() < 1e-12);
        assert!((r.size(-1.0, 100.0, Some(7.0)) + 0.01 / 0.07).abs() < 1e-12);
    }
}
