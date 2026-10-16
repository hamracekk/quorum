//! From a slot's configuration to the engine's inputs: the strategy stack and its config.

use chrono::Duration;
use quorum_core::Strategy;
use quorum_core::backtest::BacktestConfig;
use quorum_core::risk::{PortfolioRisk, RiskLimits};
use quorum_core::stops::StopRules;
use quorum_core::strategies::{
    CrisisGate, PerInstrument, PortfolioVolRule, PortfolioVolTarget, TopVolume, TrendMomentum,
    VolTarget, XsMomentum,
};
use quorum_envelope::Envelope;
use quorum_slots::{SlotConfig, StrategySpec};

use crate::{Result, RunnerError};

/// Length of one bar of `interval`.
pub fn interval_duration(interval: &str) -> Result<Duration> {
    match interval {
        "1h" => Ok(Duration::hours(1)),
        "4h" => Ok(Duration::hours(4)),
        "1d" => Ok(Duration::days(1)),
        other => Err(RunnerError::Config(format!(
            "unsupported interval `{other}`"
        ))),
    }
}

/// The slot's strategy: the configured kind, sized per holding, restricted to a top-volume
/// universe when configured, and scaled to the book's volatility target when set.
pub fn strategy(slot: &SlotConfig, instruments: usize) -> Result<Box<dyn Strategy>> {
    let step = interval_duration(&slot.interval)?;
    let periods_per_year = slot.calendar.bars_per_year(step.num_seconds());
    let z = &slot.sizing;
    let sizing = VolTarget {
        target_vol: z.target_vol,
        vol_bars: z.vol_bars,
        periods_per_year,
        band: z.band,
    };
    let rule = slot.universe.top_volume_rule();
    let members = rule.map_or(instruments, |r| r.size);
    let inner: Box<dyn Strategy> = match slot.strategy {
        StrategySpec::TrendMomentum { lookback } => Box::new(PerInstrument::new(
            TrendMomentum::new(lookback, sizing),
            members,
        )),
        StrategySpec::XsMomentum {
            lookback,
            top,
            rebalance,
        } => Box::new(XsMomentum::new(lookback, top, rebalance, sizing)),
    };
    let universe: Box<dyn Strategy> = match rule {
        Some(rule) => Box::new(TopVolume::new(inner, rule)),
        None => inner,
    };
    let universe: Box<dyn Strategy> = if slot.brakes.crisis_entries {
        Box::new(CrisisGate::new(universe, whole_year(periods_per_year)))
    } else {
        universe
    };
    if z.portfolio_vol <= 0.0 {
        return Ok(universe);
    }
    Ok(Box::new(PortfolioVolTarget::new(
        universe,
        PortfolioVolRule {
            target_vol: z.portfolio_vol,
            window: z.portfolio_vol_bars,
            periods_per_year,
            band: z.band.max(0.1),
        },
    )))
}

/// A year of bars, rounded.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a positive bar count, rounded"
)]
const fn whole_year(periods_per_year: f64) -> usize {
    periods_per_year.round().max(2.0) as usize
}

/// The engine's configuration: the slot's costs, brakes and exits, and the **envelope's** daily
/// loss limit. Long-only spot, at most fully invested: paper never borrows.
#[must_use]
pub fn backtest_config(slot: &SlotConfig, envelope: &Envelope) -> BacktestConfig {
    let portfolio = slot
        .brakes
        .vol_regime
        .map_or(PortfolioRisk::OFF, |b| PortfolioRisk {
            vol_regime_ratio: b.ratio,
            vol_regime_scale: b.scale,
            vol_short_bars: b.short_bars,
            vol_long_bars: b.long_bars,
            ..PortfolioRisk::OFF
        });
    let mut cfg = BacktestConfig {
        initial_cash: slot.paper_capital,
        risk: RiskLimits {
            max_exposure: 1.0,
            max_gross_exposure: 1.0,
            allow_short: false,
            max_daily_loss: envelope.loss().daily_max,
            portfolio,
        },
        stops: StopRules {
            stale_bars: slot.exits.stale_bars,
            ..StopRules::default()
        },
        perp_longs: false,
        ..BacktestConfig::default()
    };
    slot.costs.apply(&mut cfg);
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENVELOPE: &str = r#"
currency = "EUR"
capital_total = 0
leverage_max = 1.0
venues_allowed = ["binance"]
loss = { daily_max = 0.02, total_max = 0.15 }
slot_default = { capital_max = 0, max_mode = "paper" }
"#;

    fn shipped() -> SlotConfig {
        SlotConfig::parse(include_str!("../../../config/slots/crypto-spot.toml")).expect("valid")
    }

    #[test]
    fn the_engine_gets_the_slot_costs_and_the_envelope_loss_limit() {
        let envelope = Envelope::parse(ENVELOPE).expect("valid");
        let cfg = backtest_config(&shipped(), &envelope);
        assert!((cfg.initial_cash - 10_000.0).abs() < 1e-9);
        assert!((cfg.fee_rate - 0.001).abs() < 1e-12);
        assert!((cfg.impact - 0.05).abs() < 1e-12);
        assert!(
            (cfg.risk.max_daily_loss - 0.02).abs() < 1e-12,
            "from the envelope"
        );
        assert!(!cfg.risk.allow_short && !cfg.perp_longs, "long-only spot");
        assert!((cfg.risk.portfolio.vol_regime_ratio - 1.5).abs() < 1e-12);
        assert!((cfg.risk.portfolio.vol_regime_scale - 0.5).abs() < 1e-12);
        assert_eq!(cfg.risk.portfolio.vol_long_bars, 365);
        assert_eq!(cfg.stops.stale_bars, 5);
    }

    #[test]
    fn brakes_left_out_stay_off() {
        let text = include_str!("../../../config/slots/crypto-spot.toml");
        let start = text.find("[brakes.vol_regime]").expect("section");
        let end = text.find("[exits]").expect("section");
        let slot =
            SlotConfig::parse(&format!("{}{}", &text[..start], &text[end..])).expect("valid");
        let cfg = backtest_config(&slot, &Envelope::parse(ENVELOPE).expect("valid"));
        assert!(cfg.risk.portfolio.vol_regime_ratio.abs() < f64::EPSILON);
        assert!((cfg.risk.portfolio.vol_regime_scale - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn intervals_map_to_durations() {
        assert_eq!(interval_duration("4h").expect("known"), Duration::hours(4));
        assert!(interval_duration("5m").is_err());
    }

    #[test]
    fn every_strategy_kind_builds() {
        let xs = SlotConfig::parse(
            &include_str!("../../../config/slots/crypto-spot.toml").replace(
                "kind = \"trend-momentum\"\nlookback = 60",
                "kind = \"xs-momentum\"\nlookback = 60\ntop = 5\nrebalance = 7",
            ),
        )
        .expect("valid");
        assert_eq!(xs.strategy.name(), "xs-momentum");
        strategy(&xs, 25).expect("xs-momentum builds");
        strategy(&shipped(), 25).expect("trend-momentum builds");
    }
}
