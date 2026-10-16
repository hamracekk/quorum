use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::instrument::InstrumentId;

/// Portfolio-level brakes (E-5): shrink every target when the portfolio is in a drawdown or the
/// market turns unusually volatile. All off by default.
///
/// The scale moves in steps of [`PortfolioRisk::STEP`] so a slowly changing drawdown does not
/// turn into a trade on every bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PortfolioRisk {
    /// Drawdown from the equity peak where de-risking starts (`0` = off).
    pub drawdown_start: f64,
    /// Drawdown where exposure reaches `drawdown_min_scale`.
    pub drawdown_floor: f64,
    /// Smallest share of the targets kept in a deep drawdown.
    pub drawdown_min_scale: f64,
    /// Bars behind the equity peak (`0` = since the start).
    pub drawdown_window: usize,
    /// Market volatility over `vol_short_bars` above this multiple of its `vol_long_bars` level
    /// counts as a high-volatility regime (`0` = off). The market is the equal-weight universe.
    pub vol_regime_ratio: f64,
    /// Share of the targets kept in a high-volatility regime.
    pub vol_regime_scale: f64,
    pub vol_short_bars: usize,
    pub vol_long_bars: usize,
}

impl PortfolioRisk {
    /// Every brake off.
    pub const OFF: Self = Self {
        drawdown_start: 0.0,
        drawdown_floor: 0.0,
        drawdown_min_scale: 1.0,
        drawdown_window: 0,
        vol_regime_ratio: 0.0,
        vol_regime_scale: 1.0,
        vol_short_bars: 20,
        vol_long_bars: 365,
    };
    /// Granularity of the scale.
    pub const STEP: f64 = 0.25;

    /// Whether the drawdown brake is configured.
    pub const fn drawdown_on(&self) -> bool {
        self.drawdown_start > 0.0 && self.drawdown_floor > self.drawdown_start
    }

    /// Whether the volatility-regime brake is configured.
    pub const fn vol_on(&self) -> bool {
        self.vol_regime_ratio > 0.0
            && self.vol_short_bars >= 2
            && self.vol_long_bars > self.vol_short_bars
    }

    /// The share of targets kept at drawdown `dd` (a fraction) and volatility ratio `ratio`.
    pub fn scale(&self, dd: f64, ratio: Option<f64>) -> f64 {
        let mut scale = 1.0;
        if self.drawdown_on() && dd > self.drawdown_start {
            let min = self.drawdown_min_scale.clamp(0.0, 1.0);
            let t = ((dd - self.drawdown_start) / (self.drawdown_floor - self.drawdown_start))
                .clamp(0.0, 1.0);
            scale *= t.mul_add(min - 1.0, 1.0);
        }
        if self.vol_on() && ratio.is_some_and(|r| r > self.vol_regime_ratio) {
            scale *= self.vol_regime_scale.clamp(0.0, 1.0);
        }
        if scale >= 1.0 {
            return 1.0;
        }
        // Down to the step below, never under the configured minimums.
        let floor = (self.drawdown_min_scale * self.vol_regime_scale).clamp(0.0, 1.0);
        ((scale / Self::STEP).floor() * Self::STEP).max(floor)
    }
}

impl Default for PortfolioRisk {
    fn default() -> Self {
        Self::OFF
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RiskLimits {
    /// Maximum absolute exposure to any single instrument, as a fraction of equity.
    pub max_exposure: f64,
    /// Maximum sum of absolute exposures across all instruments.
    pub max_gross_exposure: f64,
    pub allow_short: bool,
    /// Flatten and stop trading for the rest of the UTC day once equity falls
    /// this fraction below the day's opening equity.
    pub max_daily_loss: f64,
    /// Drawdown and volatility-regime brakes (E-5).
    pub portfolio: PortfolioRisk,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_exposure: 1.0,
            max_gross_exposure: 1.0,
            allow_short: false,
            max_daily_loss: 0.05,
            portfolio: PortfolioRisk::OFF,
        }
    }
}

/// Applies exposure limits to strategy targets and enforces the daily-loss kill switch.
#[derive(Debug, Clone)]
pub struct RiskManager {
    limits: RiskLimits,
    day: Option<NaiveDate>,
    /// The day's reference: equity at the previous close (the last update before the day
    /// changed); the first update ever when there is none.
    day_open_equity: f64,
    /// Equity at the latest update.
    last_equity: Option<f64>,
    halted: bool,
    /// Equity at closed timestamps (for the drawdown peak).
    equity: VecDeque<f64>,
    peak: f64,
    drawdown: f64,
    /// The market's return per timestamp (mean log return of the instruments that traded),
    /// warm-up included, for the volatility regime.
    returns: VecDeque<f64>,
    scale: f64,
    /// A temporary cap on every target (`reduce_exposure`): the scale and until when.
    cap: Option<(f64, DateTime<Utc>)>,
    /// The latest closed timestamp.
    now: Option<DateTime<Utc>>,
}

impl RiskManager {
    pub const fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            day: None,
            day_open_equity: 0.0,
            last_equity: None,
            halted: false,
            equity: VecDeque::new(),
            peak: 0.0,
            drawdown: 0.0,
            returns: VecDeque::new(),
            scale: 1.0,
            cap: None,
            now: None,
        }
    }

    /// Record a measured timestamp's equity (drawdown brake).
    pub fn observe_equity(&mut self, equity: f64) {
        let p = self.limits.portfolio;
        if !p.drawdown_on() {
            return;
        }
        if p.drawdown_window > 0 {
            self.equity.push_back(equity);
            while self.equity.len() > p.drawdown_window {
                self.equity.pop_front();
            }
            self.peak = self.equity.iter().copied().fold(0.0, f64::max);
        } else {
            self.peak = self.peak.max(equity);
        }
        self.drawdown = if self.peak > 0.0 {
            (1.0 - equity / self.peak).max(0.0)
        } else {
            0.0
        };
        self.scale = p.scale(self.drawdown, self.vol_ratio());
    }

    /// Record the market's return over a closed timestamp (volatility brake; warm-up too).
    pub fn observe_market(&mut self, ret: f64) {
        let p = self.limits.portfolio;
        if !p.vol_on() || !ret.is_finite() {
            return;
        }
        self.returns.push_back(ret);
        while self.returns.len() > p.vol_long_bars {
            self.returns.pop_front();
        }
        self.scale = p.scale(self.drawdown, self.vol_ratio());
    }

    /// Recent market volatility over its long-run level, once `vol_long_bars` returns are known.
    fn vol_ratio(&self) -> Option<f64> {
        let p = self.limits.portfolio;
        if self.returns.len() < p.vol_long_bars {
            return None;
        }
        let all: Vec<f64> = self.returns.iter().copied().collect();
        let short = stdev(&all[all.len().saturating_sub(p.vol_short_bars)..])?;
        let long = stdev(&all)?;
        (long > 0.0).then(|| short / long)
    }

    /// The share of targets the portfolio brakes keep (`1` = no brake).
    pub const fn scale(&self) -> f64 {
        self.scale
    }

    /// Cap every target at `scale` until `until` (lower only: a looser cap never replaces a
    /// tighter one that is still running).
    pub fn cap(&mut self, scale: f64, until: DateTime<Utc>) {
        let scale = scale.clamp(0.0, 1.0);
        let running = self
            .cap
            .filter(|(_, end)| self.now.is_none_or(|now| now < *end));
        self.cap = Some(match running {
            Some((current, end)) if current <= scale => (current, end.max(until)),
            _ => (scale, until),
        });
    }

    /// The cap in force at the latest closed timestamp (`1` = none).
    pub fn cap_scale(&self) -> f64 {
        match (self.cap, self.now) {
            (Some((scale, until)), Some(now)) if now < until => scale,
            (Some((scale, _)), None) => scale,
            _ => 1.0,
        }
    }

    /// Records a closed timestamp's equity and checks the daily loss against the previous
    /// close. With daily bars every update starts a new day, so the reference must be the last
    /// equity before it, not this one (which made the loss always 0).
    pub fn update(&mut self, ts: DateTime<Utc>, equity: f64) {
        self.now = Some(ts);
        let day = ts.date_naive();
        if self.day != Some(day) {
            self.day = Some(day);
            self.day_open_equity = self.last_equity.unwrap_or(equity);
            self.halted = false;
        }
        self.last_equity = Some(equity);
        if self.day_open_equity > 0.0
            && (self.day_open_equity - equity) / self.day_open_equity >= self.limits.max_daily_loss
        {
            self.halted = true;
        }
    }

    pub const fn halted(&self) -> bool {
        self.halted
    }

    /// Risk-adjusted targets: all zero when halted; otherwise each target clamped to
    /// the per-instrument limit, then all scaled down together if gross exposure is too high.
    pub fn apply(&self, desired: &BTreeMap<InstrumentId, f64>) -> BTreeMap<InstrumentId, f64> {
        if self.halted {
            return desired.keys().map(|s| (s.clone(), 0.0)).collect();
        }
        let floor = if self.limits.allow_short {
            -self.limits.max_exposure
        } else {
            0.0
        };
        let mut targets: BTreeMap<InstrumentId, f64> = desired
            .iter()
            .map(|(s, t)| (s.clone(), t.clamp(floor, self.limits.max_exposure)))
            .collect();

        let gross: f64 = targets.values().map(|t| t.abs()).sum();
        if gross > self.limits.max_gross_exposure && gross > 0.0 {
            let scale = self.limits.max_gross_exposure / gross;
            for target in targets.values_mut() {
                *target *= scale;
            }
        }
        let scale = self.scale * self.cap_scale();
        if scale < 1.0 {
            for target in targets.values_mut() {
                *target *= scale;
            }
        }
        targets
    }
}

fn stdev(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    Some(var.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn id(symbol: &str) -> InstrumentId {
        InstrumentId::spot("test", symbol).expect("valid instrument")
    }

    fn targets(pairs: &[(&str, f64)]) -> BTreeMap<InstrumentId, f64> {
        pairs.iter().map(|&(s, t)| (id(s), t)).collect()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    #[test]
    fn clamps_each_instrument() {
        let risk = RiskManager::new(RiskLimits {
            max_exposure: 0.5,
            max_gross_exposure: 10.0,
            ..RiskLimits::default()
        });
        let out = risk.apply(&targets(&[("A", 1.0), ("B", -1.0)]));
        assert_close(out[&id("A")], 0.5);
        assert_close(out[&id("B")], 0.0);
    }

    #[test]
    fn a_cap_lowers_every_target_until_it_expires_and_never_loosens() {
        let mut risk = RiskManager::new(RiskLimits::default());
        let want = targets(&[("A", 0.4)]);
        risk.update(at(0), 1_000.0);
        risk.cap(0.5, at(24));
        assert_close(risk.apply(&want)[&id("A")], 0.2);
        risk.cap(0.8, at(48)); // looser: keeps 0.5, runs to the later end
        risk.update(at(30), 1_000.0);
        assert_close(risk.apply(&want)[&id("A")], 0.2);
        risk.update(at(48), 1_000.0);
        assert_close(risk.apply(&want)[&id("A")], 0.4);
    }

    #[test]
    fn scales_down_gross_exposure() {
        let risk = RiskManager::new(RiskLimits::default());
        let out = risk.apply(&targets(&[("A", 1.0), ("B", 1.0)]));
        assert_close(out[&id("A")], 0.5);
        assert_close(out[&id("B")], 0.5);
    }

    #[test]
    fn daily_loss_halts_until_next_day() {
        let mut risk = RiskManager::new(RiskLimits::default());
        let want = targets(&[("A", 1.0)]);
        risk.update(at(0), 1_000.0);
        risk.update(at(5), 940.0);
        assert!(risk.halted());
        assert_close(risk.apply(&want)[&id("A")], 0.0);

        risk.update(at(24), 940.0);
        assert!(!risk.halted());
        assert_close(risk.apply(&want)[&id("A")], 1.0);
    }

    /// Regression: with one update per day (daily bars) the day's reference used to be that
    /// same update's equity, so the loss was always 0 and the kill switch never fired.
    #[test]
    fn daily_bars_halt_on_a_loss_against_the_previous_close() {
        let limits = RiskLimits {
            max_daily_loss: 0.02,
            ..RiskLimits::default()
        };
        let mut risk = RiskManager::new(limits);
        risk.update(at(0), 1_000.0);
        assert!(!risk.halted());
        risk.update(at(24), 975.0); // −2.5% from yesterday's close
        assert!(risk.halted(), "a 2.5% daily loss must halt at a 2% limit");
        risk.update(at(48), 975.0); // flat day: a new day, the halt lifts
        assert!(!risk.halted());
        risk.update(at(72), 960.0); // −1.5%: below the limit
        assert!(!risk.halted());
    }

    #[test]
    fn the_first_hour_of_a_day_counts_against_the_previous_close() {
        let limits = RiskLimits {
            max_daily_loss: 0.05,
            ..RiskLimits::default()
        };
        let mut risk = RiskManager::new(limits);
        risk.update(at(23), 1_000.0); // yesterday's last hour
        risk.update(at(24), 940.0); // today's first hour: −6% from yesterday's close
        assert!(risk.halted());
    }

    #[test]
    fn shorts_are_clamped_symmetrically_when_allowed() {
        let risk = RiskManager::new(RiskLimits {
            max_exposure: 0.5,
            max_gross_exposure: 10.0,
            allow_short: true,
            ..RiskLimits::default()
        });
        assert_close(risk.apply(&targets(&[("A", -2.0)]))[&id("A")], -0.5);
    }

    #[test]
    fn loss_just_below_the_limit_does_not_halt() {
        let mut risk = RiskManager::new(RiskLimits::default());
        risk.update(at(0), 1_000.0);
        risk.update(at(1), 951.0);
        assert!(!risk.halted());
    }

    #[test]
    fn halting_zeroes_every_target() {
        let mut risk = RiskManager::new(RiskLimits::default());
        risk.update(at(0), 1_000.0);
        risk.update(at(1), 900.0);
        let out = risk.apply(&targets(&[("A", 0.5), ("B", 0.5)]));
        assert_close(out[&id("A")], 0.0);
        assert_close(out[&id("B")], 0.0);
    }

    const BRAKES: PortfolioRisk = PortfolioRisk {
        drawdown_start: 0.2,
        drawdown_floor: 0.4,
        drawdown_min_scale: 0.25,
        drawdown_window: 0,
        vol_regime_ratio: 1.5,
        vol_regime_scale: 0.5,
        vol_short_bars: 5,
        vol_long_bars: 40,
    };

    #[test]
    fn the_drawdown_brake_steps_down_between_start_and_floor() {
        assert_close(BRAKES.scale(0.1, None), 1.0);
        assert_close(BRAKES.scale(0.25, None), 0.75);
        assert_close(BRAKES.scale(0.3, None), 0.5);
        assert_close(BRAKES.scale(0.5, None), 0.25);
        assert_close(BRAKES.scale(0.1, Some(2.0)), 0.5);
        assert_close(BRAKES.scale(0.1, Some(1.2)), 1.0);
        assert_close(PortfolioRisk::OFF.scale(0.9, Some(9.0)), 1.0);
    }

    #[test]
    fn a_deep_drawdown_shrinks_every_target_and_recovers_with_equity() {
        let mut risk = RiskManager::new(RiskLimits {
            portfolio: PortfolioRisk {
                vol_regime_ratio: 0.0,
                ..BRAKES
            },
            ..RiskLimits::default()
        });
        for equity in [100.0, 110.0, 120.0] {
            risk.observe_equity(equity);
        }
        assert_close(risk.scale(), 1.0);
        risk.observe_equity(84.0);
        assert_close(risk.scale(), 0.5);
        let out = risk.apply(&targets(&[("A", 0.5), ("B", 0.5)]));
        assert_close(out[&id("A")], 0.25);
        risk.observe_equity(118.0);
        assert_close(risk.scale(), 1.0);
    }

    #[test]
    fn a_volatile_market_halves_exposure() {
        let mut risk = RiskManager::new(RiskLimits {
            portfolio: PortfolioRisk {
                drawdown_start: 0.0,
                drawdown_floor: 0.0,
                ..BRAKES
            },
            ..RiskLimits::default()
        });
        for i in 0..40 {
            risk.observe_market(if i % 2 == 0 { 0.01 } else { -0.01 });
        }
        assert_close(risk.scale(), 1.0);
        for i in 0..5 {
            risk.observe_market(if i % 2 == 0 { 0.10 } else { -0.10 });
        }
        assert_close(risk.scale(), 0.5);
    }
}
