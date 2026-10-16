//! `quorum longterm`: the long-term book (composition only; logic in `quorum_longterm`).

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Datelike as _;
use clap::Subcommand;
use quorum_longterm::{LongTermConfig, SimCosts, simulate};

#[derive(Debug, Subcommand)]
pub(crate) enum LongTermCommand {
    /// Run every policy in config/longterm.toml over history in CZK: contributions, band
    /// rebalancing, tax lots; money- and time-weighted results.
    Backtest,
}

/// Tax on a year's non-exempt net gain (§10 ZDP).
const TAX_RATE: f64 = 0.15;

pub(crate) async fn run(config: &Path, data: &Path, command: LongTermCommand) -> Result<()> {
    let LongTermCommand::Backtest = command;
    let cfg = LongTermConfig::load(&config.join("longterm.toml")).context("long-term config")?;
    let (_, series, _) = crate::research_series(config, data, &cfg.prices_from)?;
    let first_year = series
        .iter()
        .filter_map(|s| s.bars.first().map(|b| b.ts.year()))
        .min()
        .unwrap_or_else(|| chrono::Utc::now().year());
    let rates = crate::benchmark::czk_per_usd(data, first_year).await?;
    let costs = SimCosts {
        fee_rate: cfg.fee_rate,
        slippage_bps: cfg.slippage_bps,
        fx_cost: cfg.fx_cost,
        tax_rate: TAX_RATE,
    };
    let mut reports = Vec::new();
    for policy in &cfg.policies {
        reports
            .push(simulate(policy, &series, &rates, costs).with_context(|| policy.name.clone())?);
    }
    println!(
        "long-term policies in CZK, after costs (example values in config/longterm.toml, not advice)"
    );
    println!(
        "{:<22} {:>12} {:>12} {:>9} {:>12} {:>8} {:>8} {:>8} {:>7} {:>6}",
        "policy",
        "paid in",
        "value",
        "tax paid",
        "after sale",
        "money/yr",
        "unit/yr",
        "max DD",
        "trades",
        "sells"
    );
    for r in &reports {
        println!(
            "{:<22} {:>12.0} {:>12.0} {:>9.0} {:>12.0} {:>7.1}% {:>7.1}% {:>7.0}% {:>7} {:>6}",
            r.policy,
            r.contributed_czk,
            r.value_czk,
            r.tax_paid_czk,
            r.after_sale_tax_czk,
            r.money_weighted * 100.0,
            r.time_weighted * 100.0,
            r.max_drawdown * 100.0,
            r.trades,
            r.sells
        );
    }
    if let Some(r) = reports.first() {
        println!(
            "{}: {} → {}; selling everything at the end: taxable gain {:.0} CZK, tax-free gain {:.0} CZK (lots held over 3 years)",
            r.policy,
            r.from
                .map_or_else(String::new, |d| d.date_naive().to_string()),
            r.to.map_or_else(String::new, |d| d.date_naive().to_string()),
            r.if_sold.taxable_gain,
            r.if_sold.exempt_gain
        );
    }
    println!(
        "money/yr: what each crown earned from when it went in; unit/yr: the book's own return, contributions removed."
    );
    println!(
        "report: {}",
        crate::save_report(data, "longterm", &reports)?.display()
    );
    Ok(())
}
