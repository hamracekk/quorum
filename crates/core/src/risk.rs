use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

#[derive(Debug, Clone, Copy)]
pub struct RiskLimits {
    /// Maximum absolute exposure to any single symbol, as a fraction of equity.
    pub max_exposure: f64,
    /// Maximum sum of absolute exposures across all symbols.
    pub max_gross_exposure: f64,
    pub allow_short: bool,
    /// Flatten and stop trading for the rest of the UTC day once equity falls
    /// this fraction below the day's opening equity.
    pub max_daily_loss: f64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_exposure: 1.0,
            max_gross_exposure: 1.0,
            allow_short: false,
            max_daily_loss: 0.05,
        }
    }
}

/// Applies exposure limits to strategy targets and enforces the daily-loss kill switch.
#[derive(Debug, Clone)]
pub struct RiskManager {
    limits: RiskLimits,
    day: Option<NaiveDate>,
    day_open_equity: f64,
    halted: bool,
}

impl RiskManager {
    pub const fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            day: None,
            day_open_equity: 0.0,
            halted: false,
        }
    }

    pub fn update(&mut self, ts: DateTime<Utc>, equity: f64) {
        let day = ts.date_naive();
        if self.day != Some(day) {
            self.day = Some(day);
            self.day_open_equity = equity;
            self.halted = false;
        }
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
    /// the per-symbol limit, then all scaled down together if gross exposure is too high.
    pub fn apply(&self, desired: &BTreeMap<String, f64>) -> BTreeMap<String, f64> {
        if self.halted {
            return desired.keys().map(|s| (s.clone(), 0.0)).collect();
        }
        let floor = if self.limits.allow_short {
            -self.limits.max_exposure
        } else {
            0.0
        };
        let mut targets: BTreeMap<String, f64> = desired
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
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn targets(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|&(s, t)| (s.to_owned(), t)).collect()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
    }

    #[test]
    fn clamps_each_symbol() {
        let risk = RiskManager::new(RiskLimits {
            max_exposure: 0.5,
            max_gross_exposure: 10.0,
            ..RiskLimits::default()
        });
        let out = risk.apply(&targets(&[("A", 1.0), ("B", -1.0)]));
        assert_close(out["A"], 0.5);
        assert_close(out["B"], 0.0);
    }

    #[test]
    fn scales_down_gross_exposure() {
        let risk = RiskManager::new(RiskLimits::default());
        let out = risk.apply(&targets(&[("A", 1.0), ("B", 1.0)]));
        assert_close(out["A"], 0.5);
        assert_close(out["B"], 0.5);
    }

    #[test]
    fn daily_loss_halts_until_next_day() {
        let mut risk = RiskManager::new(RiskLimits::default());
        let want = targets(&[("A", 1.0)]);
        risk.update(at(0), 1_000.0);
        risk.update(at(5), 940.0);
        assert!(risk.halted());
        assert_close(risk.apply(&want)["A"], 0.0);

        risk.update(at(24), 940.0);
        assert!(!risk.halted());
        assert_close(risk.apply(&want)["A"], 1.0);
    }
}
