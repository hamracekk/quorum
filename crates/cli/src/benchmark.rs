//! The passive benchmarks (`config/benchmark.toml`): compared with every allocation variant, and
//! priced live for the daily paper summary (composition only; logic in `quorum_lab::benchmark`).

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike as _, Duration, Utc};
use quorum_core::{EquityPoint, MarketData as _, PassiveBenchmark, PortError, Series};
use quorum_data::tiingo::TiingoDaily;
use quorum_lab::allocation::Variant;
use quorum_lab::benchmark::{
    BenchmarkConfig, Portfolio, Verdict, after_tax, compare, curve, in_currency,
};
use quorum_lab::overfit::normal_quantile;

use crate::secrets;

/// Trading days in a year on the benchmarks' (exchange) calendar.
const YEAR: usize = 252;

/// One side's cost of the slot that prices the benchmarks.
fn side_cost(slot: &quorum_slots::SlotConfig) -> f64 {
    slot.costs.fee_rate + slot.costs.slippage_bps / 10_000.0
}

/// Compares every variant with every benchmark on the dates both cover, before and (with a
/// `trading_tax`) after the trading book's tax, prints the tables and saves the report.
pub(crate) async fn compare_variants(
    config: &Path,
    data: &Path,
    variants: &[Variant],
) -> Result<()> {
    let cfg = BenchmarkConfig::load(&config.join("benchmark.toml")).context("benchmark config")?;
    let (slot, series, _) = crate::research_series(config, data, &cfg.slot)?;
    let cost = side_cost(&slot);
    // In the investor's currency: tax is owed on gains in CZK, exchange moves included.
    let rates = if cfg.currency == "CZK" {
        let first_year = variants
            .iter()
            .filter_map(|v| v.curve.first())
            .map(|p| p.ts.year())
            .min()
            .unwrap_or_else(|| Utc::now().year());
        Some(czk_per_usd(data, first_year).await?)
    } else {
        None
    };
    let convert = |c: &[EquityPoint]| match &rates {
        Some(r) => in_currency(c, r, cfg.fx_cost),
        None => c.to_vec(),
    };
    let mut views = vec![("before tax".to_owned(), 0.0)];
    if cfg.trading_tax > 0.0 {
        views.push((
            format!(
                "after {:.0}% tax on each year's trading gain; passive held 3+ years is tax-free",
                cfg.trading_tax * 100.0
            ),
            cfg.trading_tax,
        ));
    }
    let trials = (variants.len() * cfg.portfolios.len() * views.len()).max(1);
    // Two-sided, Bonferroni across every comparison made here.
    let t_threshold = normal_quantile(1.0 - 0.025 / trials as f64);
    let benches = cfg
        .portfolios
        .iter()
        .map(|p| {
            curve(p, &series, 10_000.0, cost)
                .map(|c| (p.name.as_str(), convert(&c)))
                .with_context(|| format!("benchmark `{}`", p.name))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut report = Vec::new();
    for (label, tax) in &views {
        let mut rows = Vec::new();
        for (name, bench) in &benches {
            for v in variants {
                let subject = after_tax(&convert(&v.curve), *tax);
                if let Some(c) = compare((&v.name, &subject), (name, bench), 0.3, YEAR, t_threshold)
                {
                    rows.push(c);
                }
            }
        }
        print_rows(
            &format!(
                "vs passive benchmarks in {}, after costs, {label} (|t| ≥ {t_threshold:.2} across {trials} comparisons)",
                cfg.currency
            ),
            &rows,
        );
        report.push(serde_json::json!({ "view": label, "tax": tax, "rows": rows }));
    }
    println!(
        "report: {}",
        crate::save_report(data, "benchmark", &report)?.display()
    );
    Ok(())
}

fn print_rows(title: &str, rows: &[quorum_lab::benchmark::Comparison]) {
    println!("\n== {title} ==");
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
    for c in rows {
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
}

/// CZK per USD at ČNB fixings from `first_year` on, cached in `data/research/fx/CZK-USD.json`
/// and refreshed when the cache is more than a few days old.
async fn czk_per_usd(
    data: &Path,
    first_year: i32,
) -> Result<std::collections::BTreeMap<chrono::NaiveDate, f64>> {
    let dir = data.join("research").join("fx");
    let path = dir.join("CZK-USD.json");
    let today = Utc::now().date_naive();
    let cached: Option<std::collections::BTreeMap<chrono::NaiveDate, f64>> =
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(rates) = cached
        && rates
            .first_key_value()
            .is_some_and(|(d, _)| d.year() <= first_year)
        && rates
            .last_key_value()
            .is_some_and(|(d, _)| (today - *d).num_days() <= 4)
    {
        return Ok(rates);
    }
    let rates = quorum_data::cnb::rates("USD", first_year, today.year())
        .await
        .context("ČNB exchange rates")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    std::fs::write(&path, serde_json::to_string(&rates)?)
        .with_context(|| format!("{}", path.display()))?;
    Ok(rates)
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
