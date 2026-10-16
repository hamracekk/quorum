//! `quorum news`: the knowledge plane's commands (composition only; logic in `quorum_knowledge`).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use quorum_core::NewsStore as _;
use quorum_core::news::NewsQuery;
use quorum_core::{InstrumentId, LlmProvider, NewsSource};
use quorum_journal::SqliteJournal;
use quorum_knowledge::{LLMs, NewsConfig, TierConfig, extract, fetch, final_readings, load_impact};
use quorum_slots::SlotConfig;

use crate::secrets;

#[derive(Debug, Subcommand)]
pub(crate) enum NewsCommand {
    /// Fetch every feed once, tag and store new items.
    Fetch,
    /// Read pending items with the Claude tiers once (within today's budgets).
    Extract,
    /// Fetch and read every `poll_minutes` until Ctrl+C or SIGTERM (the launchd service).
    Run,
    /// Latest items with the reading that counts.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Sources, today's tokens per tier, configured models.
    Status,
}

struct Setup {
    cfg: NewsConfig,
    instruments: Vec<InstrumentId>,
    store: Arc<SqliteJournal>,
}

async fn setup(config: &Path, data: &Path) -> Result<Setup> {
    let cfg = NewsConfig::load(&config.join("news.toml")).context("news config")?;
    let slots = cfg
        .slots
        .iter()
        .map(|id| {
            SlotConfig::load(&config.join("slots").join(format!("{}.toml", id.as_str())))
                .with_context(|| format!("slot `{id}`"))
        })
        .collect::<Result<Vec<_>>>()?;
    let instruments = cfg.instruments(&slots).map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(data).with_context(|| format!("{}", data.display()))?;
    let store = Arc::new(
        SqliteJournal::open(&data.join("quorum.db"))
            .await
            .context("journal")?,
    );
    Ok(Setup {
        cfg,
        instruments,
        store,
    })
}

fn sources(cfg: &NewsConfig) -> Result<Vec<Arc<dyn NewsSource>>> {
    cfg.sources
        .iter()
        .map(|s| {
            let source = quorum_news::RssSource::new(&s.name, &s.url, s.user_agent.as_deref())
                .with_context(|| format!("source `{}`", s.name))?;
            Ok(Arc::new(source) as Arc<dyn NewsSource>)
        })
        .collect()
}

fn tier(tier: &TierConfig, key: Option<&String>) -> Result<Option<Arc<dyn LlmProvider>>> {
    let (true, Some(key)) = (tier.enabled, key) else {
        return Ok(None);
    };
    let provider =
        quorum_ai::ClaudeProvider::new(key.clone(), &tier.model, &tier.effort, tier.fallbacks)
            .context("Claude client")?;
    Ok(Some(Arc::new(provider)))
}

fn llms(cfg: &NewsConfig) -> Result<LLMs> {
    let key = secrets::get(secrets::ANTHROPIC);
    if key.is_none() {
        tracing::warn!("no Anthropic key: `quorum secrets set anthropic`; nothing is read");
    }
    Ok(LLMs {
        first_pass: tier(&cfg.first_pass, key.as_ref())?,
        escalation: tier(&cfg.escalation, key.as_ref())?,
    })
}

async fn cycle(
    setup: &Setup,
    sources: &[Arc<dyn NewsSource>],
    llms: &LLMs,
    config: &Path,
) -> Result<()> {
    let fetched = fetch(
        sources,
        setup.store.as_ref(),
        &setup.instruments,
        &setup.cfg.aliases,
    )
    .await?;
    let new: usize = fetched.iter().map(|r| r.inserted).sum();
    let failed: Vec<&str> = fetched
        .iter()
        .filter(|r| r.error.is_some())
        .map(|r| r.source.as_str())
        .collect();
    let map = load_impact(&config.join("impact.toml"))?;
    let report = extract(
        &setup.cfg,
        setup.store.as_ref(),
        llms,
        &setup.instruments,
        &map,
    )
    .await?;
    println!(
        "news: {new} new items{} · first pass {} read ({} tokens, {} rejected, {} failed, {} over budget) · escalation {} read ({} tokens) · themes {} read · {} events, {} implied moves",
        if failed.is_empty() {
            String::new()
        } else {
            format!(" (failed: {})", failed.join(", "))
        },
        report.first_pass.attempted,
        report.first_pass.tokens,
        report.first_pass.rejected,
        report.first_pass.failed,
        report.first_pass.skipped_for_budget,
        report.escalation.attempted,
        report.escalation.tokens,
        report.themes.attempted,
        report.events.len(),
        report.implications.len(),
    );
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

pub(crate) async fn run(
    config: &Path,
    data: &Path,
    command: NewsCommand,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<()> {
    let setup = setup(config, data).await?;
    match command {
        NewsCommand::Fetch => {
            let reports = fetch(
                &sources(&setup.cfg)?,
                setup.store.as_ref(),
                &setup.instruments,
                &setup.cfg.aliases,
            )
            .await?;
            for r in reports {
                match r.error {
                    Some(err) => println!("{:<16} FAILED: {err}", r.source),
                    None => println!(
                        "{:<16} {:>3} fetched, {:>3} new, {:>3} duplicates",
                        r.source, r.fetched, r.inserted, r.duplicates
                    ),
                }
            }
        }
        NewsCommand::Extract => {
            let map = load_impact(&config.join("impact.toml"))?;
            let report = extract(
                &setup.cfg,
                setup.store.as_ref(),
                &llms(&setup.cfg)?,
                &setup.instruments,
                &map,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        NewsCommand::Run => {
            let sources = sources(&setup.cfg)?;
            let llms = llms(&setup.cfg)?;
            let every = std::time::Duration::from_secs(setup.cfg.poll_minutes.max(1) * 60);
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut shutdown = std::pin::pin!(shutdown);
            loop {
                tokio::select! {
                    () = &mut shutdown => break,
                    _ = ticks.tick() => {
                        if let Err(err) = cycle(&setup, &sources, &llms, config).await {
                            tracing::error!(error = %err, "news cycle failed; next poll retries");
                        }
                    }
                }
            }
        }
        NewsCommand::List { limit } => list(&setup, limit).await?,
        NewsCommand::Status => status(&setup).await?,
    }
    Ok(())
}

async fn list(setup: &Setup, limit: usize) -> Result<()> {
    let items = setup
        .store
        .news(&NewsQuery {
            limit,
            ..NewsQuery::default()
        })
        .await?;
    let urls: Vec<String> = items.iter().map(|i| i.url.clone()).collect();
    let readings = final_readings(
        setup
            .store
            .extractions(&urls, quorum_ai::extract::PROMPT_VERSION)
            .await?,
    );
    for item in items {
        let reading = readings.get(&item.url).map_or_else(
            || "not read yet".to_owned(),
            |r| {
                if r.events.is_empty() {
                    "not relevant".to_owned()
                } else {
                    r.events
                        .iter()
                        .map(|e| {
                            format!(
                                "{} {:?} {:?}",
                                e.instrument.symbol(),
                                e.direction,
                                e.magnitude
                            )
                            .to_lowercase()
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            },
        );
        println!(
            "{} {:<14} {}",
            item.published_at.format("%m-%d %H:%M"),
            item.source,
            item.title
        );
        println!("{:>27} → {reading}", "");
    }
    Ok(())
}

async fn status(setup: &Setup) -> Result<()> {
    let start = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .context("midnight")?
        .and_utc();
    let usage = setup.store.token_usage(start).await?;
    let (f, e) = (&setup.cfg.first_pass, &setup.cfg.escalation);
    println!(
        "first pass  {:<18} {} · today {} of {} tokens",
        f.model,
        if f.enabled { "on" } else { "off" },
        usage.local,
        f.daily_tokens
    );
    println!(
        "escalation  {:<18} {} · today {} of {} tokens ({} calls)",
        e.model,
        if e.enabled { "on" } else { "off" },
        usage.remote,
        e.daily_tokens,
        usage.remote_calls
    );
    println!(
        "anthropic key: {}",
        if secrets::get(secrets::ANTHROPIC).is_some() {
            "set"
        } else {
            "missing: quorum secrets set anthropic"
        }
    );
    for s in setup.store.news_stats().await? {
        println!(
            "{:<16} {:>5} items, latest {}",
            s.source,
            s.items,
            s.latest
                .map_or_else(String::new, |d| d.format("%Y-%m-%d %H:%M").to_string())
        );
    }
    Ok(())
}
