use chrono::{DateTime, NaiveDate, Utc};

#[derive(Debug, Clone, Copy)]
pub struct RiskLimits {
    /// Maximum absolute exposure as a fraction of equity.
    pub max_exposure: f64,
    pub allow_short: bool,
    /// Flatten and stop trading for the rest of the UTC day once equity falls
    /// this fraction below the day's opening equity.
    pub max_daily_loss: f64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_exposure: 1.0,
            allow_short: false,
            max_daily_loss: 0.05,
        }
    }
}

/// Clamps strategy targets and enforces the daily-loss kill switch.
#[derive(Debug, Clone)]
pub struct RiskManager {
    limits: RiskLimits,
    day: Option<NaiveDate>,
    day_open_equity: f64,
    halted: bool,
}

impl RiskManager {
    pub fn new(limits: RiskLimits) -> Self {
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

    pub fn halted(&self) -> bool {
        self.halted
    }

    pub fn clamp(&self, target: f64) -> f64 {
        if self.halted {
            return 0.0;
        }
        let floor = if self.limits.allow_short {
            -self.limits.max_exposure
        } else {
            0.0
        };
        target.clamp(floor, self.limits.max_exposure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    #[test]
    fn clamps_to_limits() {
        let risk = RiskManager::new(RiskLimits {
            max_exposure: 0.5,
            ..RiskLimits::default()
        });
        assert_eq!(risk.clamp(1.0), 0.5);
        assert_eq!(risk.clamp(-1.0), 0.0);
    }

    #[test]
    fn daily_loss_halts_until_next_day() {
        let mut risk = RiskManager::new(RiskLimits::default());
        risk.update(at(0), 1_000.0);
        risk.update(at(5), 940.0);
        assert!(risk.halted());
        assert_eq!(risk.clamp(1.0), 0.0);

        risk.update(at(24), 940.0);
        assert!(!risk.halted());
        assert_eq!(risk.clamp(1.0), 1.0);
    }
}
