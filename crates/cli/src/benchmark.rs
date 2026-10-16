//! The passive benchmarks (`config/benchmark.toml`): compared with every allocation variant, and
//! priced live for the daily paper summary (composition only; logic in `quorum_lab::benchmark`).

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use quorum_core::{MarketData as _, PassiveBenchmark, PortError, Series};
use quorum_data::tiingo::TiingoDaily;
use quorum_lab::allocation::Variant;
use quorum_lab::benchmark::{BenchmarkConfig, Portfolio, Verdict, compare, curve};
use quorum_lab::overfit::normal_quantile;

use crate::secrets;

/// Trading days in a year on the benchmarks' (exchange) calendar.
const YEAR: usize = 252;

/// One side's cost of the slot that prices the benchmarks.
fn side_cost(slot: &quorum_slots::SlotConfig) -> f64 {
    slot.costs.fee_rate + slot.costs.slippage_bps / 10_000.0
}

/// Compares every variant with every benchmark on the dates both cover, prints the table and
/// saves the report.
pub(crate) fn compare_variants(config: &Path, data: &Path, variants: &[Variant]) -> Result<()> {
    let cfg = BenchmarkConfig::load(&config.join("benchmark.toml")).context("benchmark config")?;
    let (slot, series, _) = crate::research_series(config, data, &cfg.slot)?;
    let cost = side_cost(&slot);
    let trials = (variants.len() * cfg.portfolios.len()).max(1);
    // Two-sided, Bonferroni across every comparison made here.
    let t_threshold = normal_quantile(1.0 - 0.025 / trials as f64);
    let mut rows = Vec::new();
    for portfolio in &cfg.portfolios {
        let bench = curve(portfolio, &series, 10_000.0, cost)
            .with_context(|| format!("benchmark `{}`", portfolio.name))?;
        for v in variants {
            if let Some(c) = compare(
                (&v.name, &v.curve),
                (&portfolio.name, &bench),
                0.3,
                YEAR,
                t_threshold,
            ) {
                rows.push(c);
            }
        }
    }
    println!(
        "\n== vs passive benchmarks (after costs, before tax; |t| ≥ {t_threshold:.2} across {trials} comparisons) =="
    );
    println!(
        "{:<24} {:<24} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>8} {:>6}  verdict",
        "variant",
        "benchmark",
        "CAGR",
        "bench",
        "Sharpe",
        "bench",
        "max DD",
        "bench",
        "1y wins",
        "t"
    );
    for c in &rows {
        println!(
            "{:<24} {:<24} {:>6.1}% {:>6.1}% {:>7.2} {:>7.2} {:>6.0}% {:>6.0}% {:>8} {:>6.2}  {}",
            c.subject,
            c.benchmark,
            c.subject_perf.cagr * 100.0,
            c.benchmark_perf.cagr * 100.0,
            c.subject_perf.sharpe,
            c.benchmark_perf.sharpe,
            c.subject_perf.max_drawdown * 100.0,
            c.benchmark_perf.max_drawdown * 100.0,
            c.year_wins
                .map_or_else(|| "-".to_owned(), |w| format!("{:.0}%", w * 100.0)),
            c.t,
            match c.verdict {
                Verdict::Beats => "beats",
                Verdict::Trails => "trails",
                Verdict::NotSignificant => "not significant",
            }
        );
    }
    println!(
        "Before tax: in CZ a passive fund held over 3 years is tax-free; quorum's trading is not."
    );
    println!(
        "report: {}",
        crate::save_report(data, "benchmark", &rows)?.display()
    );
    Ok(())
}

/// The summary benchmark priced from Tiingo end-of-day data.
#[derive(Debug)]
pub(crate) struct TiingoBenchmark {
    market: TiingoDaily,
    portfolio: Portfolio,
    cost: f64,
}

#[async_trait]
impl PassiveBenchmark for TiingoBenchmark {
    fn name(&self) -> &str {
        &self.portfolio.name
    }

    async fn growth(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> std::result::Result<Option<f64>, PortError> {
        let mut series = Vec::new();
        for h in &self.portfolio.holdings {
            // A week back: the last close before `from` is the starting value.
            let bars = self
                .market
                .bars(&h.instrument, "1d", from - Duration::days(7), to)
                .await?;
            series.push(Series::new(h.instrument.clone(), bars));
        }
        let points = curve(&self.portfolio, &series, 1.0, self.cost)
            .map_err(|e| PortError::Invalid(e.to_string()))?;
        let base = points.iter().rev().find(|p| p.ts < from);
        let end = points.iter().rev().find(|p| p.ts < to);
        Ok(match (base, end) {
            (Some(b), Some(e)) if e.ts > b.ts => Some(e.equity / b.equity - 1.0),
            _ => None,
        })
    }
}

/// The paper summary's benchmark, if the config and a Tiingo key are there; otherwise the
/// summary goes without it (logged).
pub(crate) fn paper_benchmark(config: &Path) -> Option<std::sync::Arc<dyn PassiveBenchmark>> {
    let build = || -> Result<Option<TiingoBenchmark>> {
        let cfg =
            BenchmarkConfig::load(&config.join("benchmark.toml")).context("benchmark config")?;
        let Some(key) = secrets::get(secrets::TIINGO) else {
            return Ok(None);
        };
        let (slot, _) = crate::load(config, &cfg.slot)?;
        let portfolio = cfg
            .portfolio(&cfg.summary)
            .cloned()
            .context("summary portfolio")?;
        Ok(Some(TiingoBenchmark {
            market: TiingoDaily::new(Some(key)).context("tiingo")?,
            portfolio,
            cost: side_cost(&slot),
        }))
    };
    match build() {
        Ok(Some(b)) => Some(std::sync::Arc::new(b)),
        Ok(None) => {
            tracing::info!("no Tiingo key: the daily summary goes without the benchmark");
            None
        }
        Err(err) => {
            tracing::warn!(error = %err, "no benchmark for the daily summary");
            None
        }
    }
}
