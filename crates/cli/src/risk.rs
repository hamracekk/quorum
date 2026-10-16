//! `quorum risk`: the risk process and its kill switch (composition only; logic in
//! `quorum_risk`).

use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quorum_risk::{Halt, Request, Response, RiskPaths, SocketGate, serve};

#[derive(Debug, Subcommand)]
pub(crate) enum RiskCommand {
    /// Run the risk process until Ctrl+C or SIGTERM (the launchd service). Paper slots send it
    /// every order; without it they refuse every order that adds risk.
    Run,
    /// Halts, loss references and order counts per slot.
    Status,
    /// Kill switch: no new risk for one slot, or every slot. Risk-reducing orders still pass.
    Halt {
        /// Halt only this slot.
        #[arg(long)]
        slot: Option<String>,
        /// Why, for the record and the alert.
        #[arg(long)]
        reason: String,
    },
    /// Lift the halts of one slot, or the kill switch and every slot's halts (operator only).
    Resume {
        #[arg(long)]
        slot: Option<String>,
    },
}

/// The risk process's files under `data`.
pub(crate) fn paths(config: &Path, data: &Path) -> RiskPaths {
    RiskPaths::new(data, &config.join("envelope.toml"))
}

pub(crate) async fn run(
    config: &Path,
    data: &Path,
    command: RiskCommand,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<()> {
    let paths = paths(config, data);
    let request = match command {
        RiskCommand::Run => {
            let notifier = crate::notifier(config)?;
            return serve(paths, notifier, shutdown)
                .await
                .context("risk process");
        }
        RiskCommand::Status => Request::Status,
        RiskCommand::Halt { slot, reason } => Request::Halt { slot, reason },
        RiskCommand::Resume { slot } => Request::Resume { slot },
    };
    let gate = SocketGate::new(paths.socket());
    let response = tokio::task::spawn_blocking(move || gate.request(&request))
        .await
        .context("risk request")?
        .context("is the risk process running? (`quorum risk run`)")?;
    match response {
        Response::Done => println!("done"),
        Response::Status(report) => {
            let halt =
                |h: Option<&Halt>, none: &str| h.map_or_else(|| none.to_owned(), Halt::describe);
            println!("kill switch: {}", halt(report.kill.as_ref(), "off"));
            println!(
                "envelope: {}",
                if report.envelope_ok {
                    "ok"
                } else {
                    "UNREADABLE (only risk-reducing orders pass)"
                }
            );
            let money = |x: Option<f64>| x.map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"));
            for (slot, risk) in &report.slots {
                let seen = risk.last_seen.map_or_else(
                    || "never".to_owned(),
                    |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
                );
                println!(
                    "{slot:<14} {} · equity {} · day ref {} · peak {} · {} approved, {} rejected · last seen {seen}",
                    halt(risk.halt.as_ref(), "trading"),
                    money(risk.last_equity),
                    money(risk.day_ref),
                    money(risk.peak),
                    risk.approved,
                    risk.rejected,
                );
            }
        }
        other => bail!("unexpected answer from the risk process: {other:?}"),
    }
    Ok(())
}
