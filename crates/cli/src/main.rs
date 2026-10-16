//! `quorum`: the command line, and the composition root — the only place that names concrete
//! adapters (Binance, Parquet, SQLite, JSONL). Logic lives in the library crates.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quorum_bus::JsonlEventLog;
use quorum_core::BarStore as _;
use quorum_data::ParquetStore;
use quorum_data::binance::BinanceSpot;
use quorum_data::binance_futures::BinanceFutures;
use quorum_data::binance_liquidations::BinanceLiquidations;
use quorum_data::binance_ws::BinanceStream;
use quorum_envelope::Envelope;
use quorum_journal::SqliteJournal;
use quorum_recorder::{Feeds, RecordStore, RecorderConfig, record};
use quorum_runner::{Ports, RunOptions, SlotFiles, request_stop, run};
use quorum_slots::{SlotConfig, SlotId};

mod secrets;

#[derive(Debug, Parser)]
#[command(
    name = "quorum",
    version,
    about = "Autonomous multi-market trading (paper only)"
)]
struct Cli {
    /// Config directory: `envelope.toml` and `slots/<id>.toml`.
    #[arg(long, global = true, default_value = "config")]
    config: PathBuf,
    /// Data directory: bars, journal, event log, slot files.
    #[arg(long, global = true, default_value = "data")]
    data: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Paper trading of one slot.
    Paper {
        #[command(subcommand)]
        command: Paper,
    },
    /// Forward-record liquidations, book depth and derivatives (`config/recorder.toml`) until
    /// Ctrl+C or SIGTERM.
    Record,
    /// Study the price & trend factors across a slot's instruments (stored bars), in-sample
    /// vs out-of-sample, after costs. Writes the report to `<data>/factors/`.
    Factors {
        #[arg(long)]
        slot: String,
        /// Bars between rebalance dates and the forward window.
        #[arg(long, default_value_t = 7)]
        horizon: usize,
    },
    /// Label a slot's market (trending, choppy, risk-off, crisis) and study whether the label
    /// separates risk and pays, in-sample vs out-of-sample. Report to `<data>/reports/`.
    Regime {
        #[arg(long)]
        slot: String,
        /// Bars ahead the forward volatility and return are measured over.
        #[arg(long, default_value_t = 20)]
        horizon: usize,
    },
    /// Walk-forward each slot's strategy on its research history and study allocating across
    /// them: each alone, equal capital, risk parity, risk parity tilted by regime.
    Allocate {
        /// Comma-separated slot ids.
        #[arg(long, value_delimiter = ',', default_value = "crypto-spot,multi-asset")]
        slots: Vec<String>,
    },
    /// Research data, kept apart from what paper sessions replay.
    Research {
        #[command(subcommand)]
        command: Research,
    },
    /// API keys in the macOS keychain (never printed).
    Secrets {
        #[command(subcommand)]
        command: Secrets,
    },
}

#[derive(Debug, Subcommand)]
enum Secrets {
    /// Store a key; `security` prompts for it, so it never appears on a command line.
    Set { account: String },
    /// Whether each known key is stored (never the key itself).
    Status,
}

#[derive(Debug, Subcommand)]
enum Research {
    /// Download a slot's instruments' bars back `days` days into `<data>/research/bars`
    /// (extends older history and brings it up to date; the paper store is never touched).
    Sync {
        #[arg(long)]
        slot: String,
        #[arg(long, default_value_t = 3000)]
        days: u32,
    },
}

#[derive(Debug, Subcommand)]
enum Paper {
    /// Paper-trade a slot until Ctrl+C or `quorum paper stop` (resumes an open session).
    Run {
        #[arg(long)]
        slot: String,
        /// End the open session and start a new one.
        #[arg(long)]
        fresh: bool,
    },
    /// Show a slot's session and its latest state.
    Status {
        #[arg(long)]
        slot: String,
    },
    /// Ask a slot's running session to stop (it resumes on the next run).
    Stop {
        #[arg(long)]
        slot: String,
    },
}

/// Exit code for a run a supervisor should start again (the live stream ended):
/// `EX_TEMPFAIL`. A requested stop exits 0, so launchd leaves the service down.
const RESTART: u8 = 75;

#[tokio::main]
async fn main() -> Result<ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        // Colour codes only on a terminal; log files (launchd) stay plain text.
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Paper { command } => match command {
            Paper::Run { slot, fresh } => paper_run(&cli.config, &cli.data, &slot, fresh).await,
            Paper::Status { slot } => paper_status(&cli.data, &slot).map(|()| ExitCode::SUCCESS),
            Paper::Stop { slot } => {
                let path = request_stop(&SlotFiles::new(&cli.data, &slot_id(&slot)?))?;
                println!("stop requested ({})", path.display());
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Record => record_run(&cli.config, &cli.data).await,
        Command::Research {
            command: Research::Sync { slot, days },
        } => research_sync(&cli.config, &cli.data, &slot, days)
            .await
            .map(|()| ExitCode::SUCCESS),
        Command::Secrets { command } => {
            match command {
                Secrets::Set { account } => {
                    secrets::store_interactive(&account)?;
                    println!("stored `{account}` in the keychain (service quorum)");
                }
                Secrets::Status => {
                    for account in secrets::KNOWN {
                        let state = if secrets::get(account).is_some() {
                            "set"
                        } else {
                            "missing"
                        };
                        println!("{account:<10} {state}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Allocate { slots } => {
            allocate(&cli.config, &cli.data, &slots).map(|()| ExitCode::SUCCESS)
        }
        Command::Regime { slot, horizon } => {
            regime(&cli.config, &cli.data, &slot, horizon).map(|()| ExitCode::SUCCESS)
        }
        Command::Factors { slot, horizon } => {
            factors(&cli.config, &cli.data, &slot, horizon).map(|()| ExitCode::SUCCESS)
        }
    }
}

/// Resolves on Ctrl+C or SIGTERM (`launchctl bootout`, `kill`), whichever comes first.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "Ctrl+C handler failed");
            std::future::pending::<()>().await;
        }
    };
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "SIGTERM handler failed");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => tracing::info!("Ctrl+C: stopping"),
        () = term => tracing::info!("SIGTERM: stopping"),
    }
}

fn slot_id(slot: &str) -> Result<SlotId> {
    SlotId::new(slot).with_context(|| format!("slot id `{slot}`"))
}

fn load(config: &Path, slot: &str) -> Result<(SlotConfig, Envelope)> {
    let id = slot_id(slot)?;
    let path = config.join("slots").join(format!("{}.toml", id.as_str()));
    let slot = SlotConfig::load(&path).with_context(|| format!("slot `{id}`"))?;
    if slot.id != id {
        bail!("{} declares id `{}`", path.display(), slot.id);
    }
    let envelope = Envelope::load(&config.join("envelope.toml")).context("envelope")?;
    Ok((slot, envelope))
}

/// The adapters for the slot's venue.
async fn ports(slot: &SlotConfig, data: &Path) -> Result<Ports> {
    if slot.venue.as_str() != "binance" {
        bail!(
            "no adapters for venue `{}` yet (binance only)",
            slot.venue.as_str()
        );
    }
    std::fs::create_dir_all(data).with_context(|| format!("data dir {}", data.display()))?;
    let journal = SqliteJournal::open(&data.join("quorum.db"))
        .await
        .context("journal")?;
    let log = JsonlEventLog::open(data.join("events")).context("event log")?;
    Ok(Ports {
        market: Arc::new(BinanceSpot::new().context("binance market data")?),
        stream: Arc::new(BinanceStream::new().context("binance stream")?),
        bars: Arc::new(ParquetStore::new(data.join("bars"))),
        journal: Arc::new(journal),
        log: Arc::new(log),
    })
}

async fn paper_run(config: &Path, data: &Path, slot: &str, fresh: bool) -> Result<ExitCode> {
    let (slot, envelope) = load(config, slot)?;
    let ports = ports(&slot, data).await?;
    let options = RunOptions {
        fresh,
        ..RunOptions::new(data.to_path_buf())
    };
    let outcome = run(&ports, &slot, &envelope, &options, shutdown_signal()).await?;
    println!(
        "{} session {} ({}): {} live bars, equity {:.2}, resume with `quorum paper run --slot {}`",
        outcome.status,
        outcome.session_id,
        if outcome.resumed { "resumed" } else { "new" },
        outcome.bars,
        outcome.equity,
        slot.id
    );
    Ok(if outcome.status.restart() {
        ExitCode::from(RESTART)
    } else {
        ExitCode::SUCCESS
    })
}

async fn record_run(config: &Path, data: &Path) -> Result<ExitCode> {
    let recorder = RecorderConfig::load(&config.join("recorder.toml")).context("recorder")?;
    let slots = recorder
        .slots
        .iter()
        .map(|id| {
            let path = config.join("slots").join(format!("{}.toml", id.as_str()));
            SlotConfig::load(&path).with_context(|| format!("slot `{id}`"))
        })
        .collect::<Result<Vec<_>>>()?;
    let symbols = recorder.resolve(&slots)?;
    let feeds = Feeds {
        book: Arc::new(BinanceSpot::new().context("binance spot")?),
        derivs: Arc::new(BinanceFutures::new().context("binance futures")?),
        liquidations: Arc::new(BinanceLiquidations::new().context("binance liquidations")?),
    };
    let store = RecordStore::new(data.join("recorder"));
    tracing::info!(symbols = symbols.len(), dir = %store.root().display(), "recording");
    let (status, outcome) = record(
        &feeds,
        &symbols,
        &recorder,
        &store,
        &chrono::Utc::now,
        shutdown_signal(),
    )
    .await?;
    println!(
        "recorder {}: {} liquidations, {} books, {} derivs snapshots, {} failed polls",
        match status {
            quorum_recorder::Status::Stopped => "stopped",
            quorum_recorder::Status::StreamEnded => "stream ended",
        },
        outcome.liquidations,
        outcome.books,
        outcome.derivs,
        outcome.errors
    );
    Ok(if status.restart() {
        ExitCode::from(RESTART)
    } else {
        ExitCode::SUCCESS
    })
}

/// Where research bars live: apart from the paper store, because a paper session replays
/// everything in its store and a longer history there would change how it warms up.
fn research_dir(data: &Path) -> PathBuf {
    data.join("research").join("bars")
}

fn research_store(data: &Path) -> ParquetStore {
    ParquetStore::new(research_dir(data))
}

/// Market data for a research slot's venue.
fn research_market(slot: &SlotConfig) -> Result<Box<dyn quorum_core::MarketData>> {
    match slot.venue.as_str() {
        "binance" => Ok(Box::new(BinanceSpot::new().context("binance market data")?)),
        "tiingo" => {
            let key = secrets::get(secrets::TIINGO);
            if key.is_none() {
                bail!("no Tiingo key: `quorum secrets set tiingo` (free key at tiingo.com)");
            }
            Ok(Box::new(
                quorum_data::tiingo::TiingoDaily::new(key).context("tiingo market data")?,
            ))
        }
        other => bail!("no research data adapter for venue `{other}` yet (binance, tiingo)"),
    }
}

async fn research_sync(config: &Path, data: &Path, slot: &str, days: u32) -> Result<()> {
    let (slot, _) = load(config, slot)?;
    let quorum_slots::Universe::Fixed { instruments } = &slot.universe else {
        bail!("{}: research sync needs a fixed universe for now", slot.id);
    };
    let market = research_market(&slot)?;
    let market = market.as_ref();
    let store = research_store(data);
    let now = chrono::Utc::now();
    let since = now - chrono::Duration::days(i64::from(days));
    for instrument in instruments {
        quorum_data::sync(market, &store, instrument, &slot.interval, since, now)
            .await
            .with_context(|| format!("sync {instrument}"))?;
        quorum_data::backfill(market, &store, instrument, &slot.interval, since)
            .await
            .with_context(|| format!("backfill {instrument}"))?;
    }
    let mut firsts = Vec::new();
    for instrument in instruments {
        if let Some(s) = store.load(instrument, &slot.interval)?
            && let Some(first) = s.bars.first()
        {
            firsts.push((
                first.ts.date_naive(),
                s.bars.len(),
                instrument.symbol().to_owned(),
            ));
        }
    }
    firsts.sort();
    println!(
        "{} instruments in {}",
        firsts.len(),
        research_dir(data).display()
    );
    for (first, bars, symbol) in firsts {
        println!("  {symbol:<10} from {first}  {bars} bars");
    }
    Ok(())
}

/// A research slot's stored bars and the bars a year holds on its calendar: the research store,
/// else the paper store with a note.
fn research_series(
    config: &Path,
    data: &Path,
    slot: &str,
) -> Result<(SlotConfig, Vec<quorum_core::Series>, f64)> {
    let (slot, _) = load(config, slot)?;
    let quorum_slots::Universe::Fixed { instruments } = &slot.universe else {
        bail!("{}: studies need a fixed universe for now", slot.id);
    };
    let load = |store: &ParquetStore| -> Result<Vec<quorum_core::Series>> {
        let mut series = Vec::new();
        for instrument in instruments {
            if let Some(s) = store.load(instrument, &slot.interval)?
                && !s.bars.is_empty()
            {
                series.push(s);
            }
        }
        Ok(series)
    };
    let mut series = load(&research_store(data))?;
    if series.is_empty() {
        println!(
            "no research history yet (`quorum research sync --slot {}`); using the paper store",
            slot.id
        );
        series = load(&ParquetStore::new(data.join("bars")))?;
    }
    if series.is_empty() {
        bail!(
            "no stored bars for {}: run `quorum research sync --slot {}`",
            slot.id,
            slot.id
        );
    }
    let step = quorum_runner::interval_duration(&slot.interval)?;
    let per_year = slot.calendar.bars_per_year(step.num_seconds());
    Ok((slot, series, per_year))
}

/// The slot's round-trip cost as a fraction.
fn round_trip(slot: &SlotConfig) -> f64 {
    2.0 * (slot.costs.fee_rate + slot.costs.slippage_bps / 10_000.0)
}

fn save_report(data: &Path, name: &str, report: &impl serde::Serialize) -> Result<PathBuf> {
    let dir = data.join("reports");
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    let path = dir.join(format!("{name}-{}.json", chrono::Utc::now().date_naive()));
    std::fs::write(&path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("{}", path.display()))?;
    Ok(path)
}

/// The slot with its strategy's lookback replaced.
fn with_lookback(slot: &SlotConfig, lookback: usize) -> SlotConfig {
    let mut slot = slot.clone();
    slot.strategy = match slot.strategy {
        quorum_slots::StrategySpec::TrendMomentum { .. } => {
            quorum_slots::StrategySpec::TrendMomentum { lookback }
        }
        quorum_slots::StrategySpec::XsMomentum { top, rebalance, .. } => {
            quorum_slots::StrategySpec::XsMomentum {
                lookback,
                top,
                rebalance,
            }
        }
    };
    slot
}

const fn lookback_of(slot: &SlotConfig) -> usize {
    match slot.strategy {
        quorum_slots::StrategySpec::TrendMomentum { lookback }
        | quorum_slots::StrategySpec::XsMomentum { lookback, .. } => lookback,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "bars per year is a small positive number"
)]
const fn whole_bars(bars: f64) -> usize {
    bars.round().max(1.0) as usize
}

/// One slot's walk-forward curve (lookback chosen per window on the year before) and its
/// market's regime exposure per day, with or without the envelope's daily loss limit.
fn slot_stream(
    slot: &SlotConfig,
    series: &[quorum_core::Series],
    per_year: f64,
    envelope: &Envelope,
    daily_limit: bool,
) -> Result<quorum_lab::allocation::SlotStream> {
    use quorum_core::regime::{Regime, RegimeRule, regimes};
    use quorum_lab::{WalkForwardConfig, walk_forward};

    let n = series.len();
    quorum_runner::strategy(slot, n)?; // the slot's own config builds: the grid's will too
    let base = lookback_of(slot);
    let mut grid: Vec<usize> = [base / 2, base, base * 3 / 2, base * 2]
        .into_iter()
        .filter(|l| *l > 0)
        .collect();
    grid.dedup();
    let factory = |lookback: &usize| {
        quorum_runner::strategy(&with_lookback(slot, *lookback), n)
            .expect("same interval and kind as the slot config, which built")
    };
    let mut cfg = quorum_runner::backtest_config(slot, envelope);
    if !daily_limit {
        cfg.risk.max_daily_loss = 1.0;
    }
    let year = whole_bars(per_year);
    let wf = WalkForwardConfig {
        train_bars: year,
        test_bars: (year / 4).max(1),
    };
    let report = walk_forward(&grid, &factory, series, &cfg, wf)
        .with_context(|| format!("walk-forward {}", slot.id))?;
    let exposure = |r: Regime| match r {
        Regime::Trending => 1.0,
        Regime::Choppy => 0.6,
        Regime::RiskOff => 0.3,
        Regime::Crisis => 0.0,
    };
    let regime_exposure = regimes(series, &RegimeRule::default(), year)
        .into_iter()
        .map(|p| (p.ts.date_naive(), exposure(p.regime)))
        .collect();
    Ok(quorum_lab::allocation::SlotStream {
        name: slot.id.to_string(),
        curve: report.curve,
        regime_exposure,
    })
}

fn allocate(config: &Path, data: &Path, slots: &[String]) -> Result<()> {
    use quorum_lab::allocation::{AllocationConfig, study};

    let envelope = Envelope::load(&config.join("envelope.toml")).context("envelope")?;
    let mut inputs = Vec::new();
    for id in slots {
        inputs.push(research_series(config, data, id)?);
    }
    for daily_limit in [true, false] {
        let streams = inputs
            .iter()
            .map(|(slot, series, per_year)| {
                slot_stream(slot, series, *per_year, &envelope, daily_limit)
            })
            .collect::<Result<Vec<_>>>()?;
        let report = study(&streams, &AllocationConfig::default());
        println!(
            "\n== daily loss limit: {} · {} → {} (later part from {}) ==",
            if daily_limit {
                format!("envelope {:.0}%", envelope.loss().daily_max * 100.0)
            } else {
                "off".to_owned()
            },
            report.from.map_or_else(String::new, |d| d.to_string()),
            report.to.map_or_else(String::new, |d| d.to_string()),
            report
                .later_from
                .map_or_else(String::new, |d| d.to_string()),
        );
        println!(
            "{:<24} {:>8} {:>7} {:>7}   {:>8} {:>7} {:>7}   weights",
            "variant", "CAGR", "Sharpe", "max DD", "CAGR", "Sharpe", "max DD"
        );
        println!("{:<24} {:^24}   {:^24}", "", "earlier", "later");
        for v in &report.variants {
            let w: Vec<String> = v
                .avg_weights
                .iter()
                .map(|(n, w)| format!("{n} {:.0}%", w * 100.0))
                .collect();
            println!(
                "{:<24} {:>7.1}% {:>7.2} {:>6.0}%   {:>7.1}% {:>7.2} {:>6.0}%   {}",
                v.name,
                v.earlier.cagr * 100.0,
                v.earlier.sharpe,
                v.earlier.max_drawdown * 100.0,
                v.later.cagr * 100.0,
                v.later.sharpe,
                v.later.max_drawdown * 100.0,
                w.join(", ")
            );
        }
        if let Some(c) = report.correlation.first().and_then(|r| r.get(1)) {
            println!("correlation of daily returns: {c:.2}");
        }
        println!(
            "regime-tilted risk parity beats every slot alone (Sharpe and drawdown, both parts): {}",
            if report.beats_each_alone { "yes" } else { "no" }
        );
        let name = if daily_limit {
            "allocation-envelope-limit"
        } else {
            "allocation-no-limit"
        };
        println!("report: {}", save_report(data, name, &report)?.display());
    }
    Ok(())
}

fn regime(config: &Path, data: &Path, slot: &str, horizon: usize) -> Result<()> {
    use quorum_lab::regime::{RegimeStudyConfig, Sample, study};

    let (slot, series, periods_per_year) = research_series(config, data, slot)?;
    let cfg = RegimeStudyConfig {
        horizon,
        cost: round_trip(&slot),
        periods_per_year,
        ..RegimeStudyConfig::default()
    };
    let report = study(&series, &cfg);
    if let Some(now) = report.current {
        let f = now.features;
        println!(
            "{} now ({}): {}  · vol ratio {:.2}, drawdown {:.0}%, breadth {:.0}%, trend {:+.0}%",
            slot.id,
            now.ts.date_naive(),
            now.regime.as_str().to_uppercase(),
            f.vol_ratio,
            f.drawdown * 100.0,
            f.breadth * 100.0,
            f.trend * 100.0
        );
    }
    let show = |name: &str, s: &Sample| {
        println!(
            "\n{name} {} → {} · forward {horizon} bars",
            s.from
                .map_or_else(String::new, |d| d.date_naive().to_string()),
            s.to.map_or_else(String::new, |d| d.date_naive().to_string()),
        );
        println!(
            "{:<10} {:>6} {:>12} {:>12}",
            "regime", "share", "fwd return", "fwd vol/yr"
        );
        for r in &s.regimes {
            println!(
                "{:<10} {:>5.0}% {:>11.2}% {:>11.1}%",
                r.regime.as_str(),
                r.share * 100.0,
                r.forward_return * 100.0,
                r.forward_vol * 100.0
            );
        }
        println!(
            "separates risk: {}  ·  regime exposure: {:+.1}%/yr, Sharpe {:.2}, max DD {:.0}%  vs hold: {:+.1}%/yr, Sharpe {:.2}, max DD {:.0}%  → helps: {}",
            if s.separates_risk { "yes" } else { "no" },
            s.regime_exposure.annual_return * 100.0,
            s.regime_exposure.sharpe,
            s.regime_exposure.max_drawdown * 100.0,
            s.hold.annual_return * 100.0,
            s.hold.sharpe,
            s.hold.max_drawdown * 100.0,
            if s.exposure_helps { "yes" } else { "no" },
        );
    };
    show("in-sample", &report.in_sample);
    show("out-of-sample", &report.out_of_sample);
    println!(
        "\nexposure per regime (trending, choppy, risk-off, crisis): {:?}, fixed in advance, one bar late, after costs",
        report.exposure
    );
    let path = save_report(data, &format!("regime-{}-h{horizon}", slot.id), &report)?;
    println!("report: {}", path.display());
    Ok(())
}

fn factors(config: &Path, data: &Path, slot: &str, horizon: usize) -> Result<()> {
    use quorum_lab::factors::{FactorStudyConfig, study};

    let (slot, series, periods_per_year) = research_series(config, data, slot)?;
    let cfg = FactorStudyConfig {
        horizon,
        cost: round_trip(&slot),
        periods_per_year,
        ..FactorStudyConfig::default()
    };
    let family = quorum_core::factors::price_family(cfg.periods_per_year);
    let report = study(&family, &series, &cfg);

    println!(
        "{} factors · {} instruments · {} → {} · horizon {} bars · round-trip cost {:.2}% · in-sample |t| ≥ {}",
        report.factors_tested,
        report.instruments,
        report
            .first_date
            .map_or_else(String::new, |d| d.date_naive().to_string()),
        report
            .last_date
            .map_or_else(String::new, |d| d.date_naive().to_string()),
        report.horizon,
        report.cost * 100.0,
        report.t_threshold,
    );
    println!(
        "{:<22} {:>4} {:>7} {:>7} {:>7} {:>7} {:>10} {:>9}  verdict",
        "factor", "dir", "IC in", "t in", "IC out", "t out", "excess/yr", "turnover"
    );
    for row in &report.rows {
        println!(
            "{:<22} {:>4} {:>7.3} {:>7.2} {:>7.3} {:>7.2} {:>9.1}% {:>8.0}%  {:?}",
            row.factor,
            if row.direction > 0 { "+" } else { "−" },
            row.in_sample.ic,
            row.in_sample.ic_t,
            row.out_of_sample.ic,
            row.out_of_sample.ic_t,
            row.out_of_sample.long_excess_net_annual * 100.0,
            row.out_of_sample.turnover * 100.0,
            row.verdict,
        );
    }
    println!(
        "IC and t are signed by the in-sample direction; excess/yr: the top quintile over the \
         universe out-of-sample, net of turnover costs. Today's survivors: results are flattered."
    );
    let dir = data.join("factors");
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    let path = dir.join(format!(
        "{}-{}-h{}.json",
        slot.id,
        chrono::Utc::now().date_naive(),
        horizon
    ));
    std::fs::write(&path, serde_json::to_string_pretty(&report)?)
        .with_context(|| format!("{}", path.display()))?;
    println!("report: {}", path.display());
    Ok(())
}

/// A running session writes `state.json` at least every heartbeat (10 s); older than this means
/// the process is gone, hung or asleep.
const STALE_SECS: i64 = 120;

fn paper_status(data: &Path, slot: &str) -> Result<()> {
    let files = SlotFiles::new(data, &slot_id(slot)?);
    let Some(session) = files.load_session()? else {
        println!("slot {slot}: no paper session yet");
        return Ok(());
    };
    println!("session   {} ({})", session.id, session.label);
    println!(
        "started   {}  trades from {}",
        session.started_at, session.trade_from
    );
    match session.journaled_until {
        Some(ts) => println!("journaled {ts}"),
        None => println!("journaled nothing yet"),
    }
    if let Some(state) = files.load_state()? {
        let age = (chrono::Utc::now() - state.heartbeat).num_seconds();
        let stale = if state.status == "running" && age > STALE_SECS {
            "  STALE (not running, hung or asleep)"
        } else {
            ""
        };
        println!(
            "state     {}{stale} {age}s ago  equity {:.2}  cash {:.2}  positions {}  live bars {}{}",
            state.status,
            state.equity,
            state.cash,
            state.positions,
            state.bars_processed,
            if state.halted {
                "  HALTED (daily loss)"
            } else {
                ""
            }
        );
        if let Some(error) = state.error {
            println!("error     {error}");
        }
    }
    Ok(())
}
