//! `quorum`: the command line, and the composition root — the only place that names concrete
//! adapters (Binance, Parquet, SQLite, JSONL). Logic lives in the library crates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quorum_bus::JsonlEventLog;
use quorum_data::ParquetStore;
use quorum_data::binance::BinanceSpot;
use quorum_data::binance_ws::BinanceStream;
use quorum_envelope::Envelope;
use quorum_journal::SqliteJournal;
use quorum_runner::{Ports, RunOptions, SlotFiles, request_stop, run};
use quorum_slots::{SlotConfig, SlotId};

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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Paper { command } => match command {
            Paper::Run { slot, fresh } => paper_run(&cli.config, &cli.data, &slot, fresh).await,
            Paper::Status { slot } => paper_status(&cli.data, &slot),
            Paper::Stop { slot } => {
                let path = request_stop(&SlotFiles::new(&cli.data, &slot_id(&slot)?))?;
                println!("stop requested ({})", path.display());
                Ok(())
            }
        },
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

async fn paper_run(config: &Path, data: &Path, slot: &str, fresh: bool) -> Result<()> {
    let (slot, envelope) = load(config, slot)?;
    let ports = ports(&slot, data).await?;
    let options = RunOptions {
        fresh,
        ..RunOptions::new(data.to_path_buf())
    };
    let shutdown = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "Ctrl+C handler failed; stop with `quorum paper stop`");
            std::future::pending::<()>().await;
        }
    };
    let outcome = run(&ports, &slot, &envelope, &options, shutdown).await?;
    println!(
        "{} session {} ({}): {} live bars, equity {:.2}, resume with `quorum paper run --slot {}`",
        outcome.status,
        outcome.session_id,
        if outcome.resumed { "resumed" } else { "new" },
        outcome.bars,
        outcome.equity,
        slot.id
    );
    Ok(())
}

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
        println!(
            "state     {} at {}  equity {:.2}  cash {:.2}  positions {}  live bars {}{}",
            state.status,
            state.heartbeat,
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
