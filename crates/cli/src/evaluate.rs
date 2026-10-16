//! `quorum evaluate`: the trade evaluator in shadow (composition only; logic in
//! `quorum_evaluator`).

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use quorum_core::{BarStore as _, Event, Journal as _, Series};
use quorum_data::ParquetStore;
use quorum_evaluator::{CaseConfig, EXIT, EvaluatorStudy, StudyConfig, cases, study};
use quorum_journal::SqliteJournal;
use quorum_runner::SlotFiles;

/// Where the decisions come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Source {
    /// The slot's strategy over its research history (`quorum research sync`): many cases now.
    History,
    /// The slot's open paper session: what it proposed live, with its real fills.
    Paper,
}

impl Source {
    const fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Paper => "paper",
        }
    }
}

/// The bars, the engine's decisions on them, and the fills that happened.
struct Decisions {
    series: Vec<Series>,
    /// A paper session's first traded bar; its warm-up bars only prepare the strategy.
    measure_from: Option<chrono::DateTime<chrono::Utc>>,
    events: Vec<Event>,
    fills: BTreeSet<(quorum_core::InstrumentId, chrono::DateTime<chrono::Utc>)>,
}

async fn decisions(
    config: &Path,
    data: &Path,
    slot: &quorum_slots::SlotConfig,
    envelope: &quorum_envelope::Envelope,
    source: Source,
) -> Result<Decisions> {
    let backtest = quorum_runner::backtest_config(slot, envelope);
    let (series, measure_from, journal_fills) = match source {
        Source::History => {
            let (_, series, _) = crate::research_series(config, data, slot.id.as_str())?;
            (series, None, None)
        }
        Source::Paper => {
            let Some(session) = SlotFiles::new(data, &slot.id).load_session()? else {
                bail!("{}: no paper session yet", slot.id);
            };
            let store = ParquetStore::new(data.join("bars"));
            let mut series = Vec::new();
            for instrument in &session.instruments {
                series.push(
                    store
                        .load(instrument, &slot.interval)?
                        .unwrap_or_else(|| Series::new(instrument.clone(), Vec::new())),
                );
            }
            let journal = SqliteJournal::open(&data.join("quorum.db"))
                .await
                .context("journal")?;
            let real = journal.fills(&session.id).await.context("session fills")?;
            (series, Some(session.trade_from), Some(real))
        }
    };
    let mut strategy = quorum_runner::strategy(slot, series.len())?;
    let (_, events) =
        quorum_core::run_with_events(strategy.as_mut(), &series, &backtest, measure_from);
    // A paper session's real fills decide `taken` (the risk process may have refused some);
    // history has only the replay's own.
    let fills = journal_fills.map_or_else(
        || {
            events
                .iter()
                .filter_map(|e| match e {
                    Event::Fill(f) => Some((f.instrument.clone(), f.ts)),
                    _ => None,
                })
                .collect()
        },
        |real| real.into_iter().map(|f| (f.instrument, f.ts)).collect(),
    );
    Ok(Decisions {
        series,
        measure_from,
        events,
        fills,
    })
}

pub(crate) async fn run(config: &Path, data: &Path, slot: &str, source: Source) -> Result<()> {
    let (slot, envelope) = crate::load(config, slot)?;
    let step = quorum_runner::interval_duration(&slot.interval)?;
    let per_year = slot.calendar.bars_per_year(step.num_seconds());
    let d = decisions(config, data, &slot, &envelope, source).await?;
    let cost = crate::round_trip(&slot);
    let found = cases(
        &d.series,
        &d.events,
        &d.fills,
        d.measure_from,
        &CaseConfig::new(cost, per_year),
    );
    let report = study(&found, &StudyConfig::default());

    let dir = data.join("evaluator");
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    let path = dir.join(format!("{}-{}.jsonl", slot.id, source.as_str()));
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(&path).with_context(|| format!("{}", path.display()))?,
    );
    for case in &found {
        writeln!(file, "{}", serde_json::to_string(case)?)?;
    }
    file.flush()?;
    let saved = crate::save_report(
        data,
        &format!("evaluator-{}-{}", slot.id, source.as_str()),
        &report,
    )?;

    print(&report, slot.id.as_str(), source, cost);
    if source == Source::History {
        println!(
            "History cases come from the slot's own strategy, whose settings were chosen on this \
             history, over today's surviving instruments: base rates are flattered."
        );
    }
    println!("cases: {}", path.display());
    println!("report: {}", saved.display());
    Ok(())
}

fn print(report: &EvaluatorStudy, slot: &str, source: Source, cost: f64) {
    println!(
        "{} · {} · {} proposed entries, {} closed · round-trip cost {:.2}% · shadow: nothing here changes a trade",
        slot,
        source.as_str(),
        report.cases,
        report.labelled,
        cost * 100.0
    );
    println!(
        "{:<6} {:>6} {:>6} {:>6} {:>9}",
        "label", "n", "taken", "win", "mean net"
    );
    for h in &report.horizons {
        println!(
            "{:<6} {:>6} {:>6} {:>5.0}% {:>8.2}%",
            h.label,
            h.n,
            h.taken,
            h.win_rate * 100.0,
            h.mean_net * 100.0
        );
    }
    let b = &report.baseline;
    println!(
        "always take ({EXIT}): in sample {} cases, {:.2}% mean, {:.0}% win · out of sample {} cases{}, {:.2}% mean, {:.0}% win",
        b.n_in,
        b.mean_in * 100.0,
        b.win_in * 100.0,
        b.n_out,
        b.split
            .map_or_else(String::new, |d| format!(" from {}", d.date_naive())),
        b.mean_out * 100.0,
        b.win_out * 100.0
    );
    for r in &report.regimes {
        println!(
            "  regime {:<10} {:>5} cases  {:>4.0}% win  {:>6.2}% mean",
            r.regime.map_or("unlabelled", |g| g.as_str()),
            r.n,
            r.win_rate * 100.0,
            r.mean_net * 100.0
        );
    }
    println!(
        "regime skips (skip a regime's entries if they lost in sample; judged on the later part):"
    );
    for r in &report.regime_skips {
        println!(
            "  skip {:<10} in {:>4} cases {:>6.2}% · out {:>4} cases {:>6.2}% (t {:>5.2}) · always take {:>5.2}% → with the skip {:>5.2}%  {:?}",
            r.regime.map_or("unlabelled", |g| g.as_str()),
            r.n_in,
            r.mean_in * 100.0,
            r.n_out,
            r.mean_out * 100.0,
            r.t_out,
            r.always_out * 100.0,
            r.skipping_out * 100.0,
            r.verdict
        );
    }
    println!(
        "features ({} tested; out of sample z ≥ {:.2} to separate winners at the exit):",
        report.features_tested, report.z_threshold
    );
    println!(
        "{:<24} {:>6} {:>7} {:>8} {:>6}  verdict",
        "feature", "n out", "AUC in", "AUC out", "z out"
    );
    for f in &report.features {
        println!(
            "{:<24} {:>6} {:>7.3} {:>8.3} {:>6.2}  {:?}",
            f.feature, f.n_out, f.auc_in, f.auc_out, f.z_out, f.verdict
        );
    }
}

/// The shadow evaluator's record on the open paper session, for the daily summary.
#[derive(Debug)]
pub(crate) struct EvaluatorNote {
    pub(crate) config: std::path::PathBuf,
    pub(crate) data: std::path::PathBuf,
    pub(crate) slot: String,
}

#[async_trait::async_trait]
impl quorum_core::DailyNote for EvaluatorNote {
    async fn line(&self) -> std::result::Result<Option<String>, quorum_core::PortError> {
        let fail = |e: anyhow::Error| quorum_core::PortError::Invalid(format!("{e:#}"));
        let (slot, envelope) = crate::load(&self.config, &self.slot).map_err(fail)?;
        let step = quorum_runner::interval_duration(&slot.interval).map_err(|e| fail(e.into()))?;
        let per_year = slot.calendar.bars_per_year(step.num_seconds());
        let d = decisions(&self.config, &self.data, &slot, &envelope, Source::Paper)
            .await
            .map_err(fail)?;
        let cost = crate::round_trip(&slot);
        let found = cases(
            &d.series,
            &d.events,
            &d.fills,
            d.measure_from,
            &CaseConfig::new(cost, per_year),
        );
        if found.is_empty() {
            return Ok(None);
        }
        let report = study(&found, &StudyConfig::default());
        let taken = found.iter().filter(|c| c.taken).count();
        let labels: Vec<String> = report
            .horizons
            .iter()
            .filter(|h| h.n > 0)
            .map(|h| {
                format!(
                    "{} {:.0}% won, {:+.2}% mean (n {})",
                    h.label,
                    h.win_rate * 100.0,
                    h.mean_net * 100.0,
                    h.n
                )
            })
            .collect();
        Ok(Some(format!(
            "shadow evaluator: {} entries since start ({taken} taken) · {}",
            found.len(),
            if labels.is_empty() {
                "none labelled yet".to_owned()
            } else {
                labels.join(" · ")
            }
        )))
    }
}
