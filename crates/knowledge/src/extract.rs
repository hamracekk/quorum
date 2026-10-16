//! News → typed events with two tiers of Claude (port of trader's M5b orchestration).
//!
//! 1. **First pass** (a cheap model): every new item, newest first, `batch` per run.
//! 2. **Escalation** (a stronger model): items whose first-pass reading is relevant with at
//!    least the `escalate` magnitude — or every item when the first pass is off.
//! 3. **Impact themes** (the first-pass model): unread items classified into the operator's
//!    themes, expanded through the map's links into implied moves across markets.
//!
//! Each tier stops for the UTC day once its token budget is spent. Every attempt is recorded
//! (ok, rejected for breaking the schema, or failed), which is also the cache. An escalation
//! reading supersedes the first-pass one for the same item.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveTime, Utc};
use quorum_ai::extract::{self, PROMPT_VERSION};
use quorum_ai::themes::{self, THEMES_VERSION};
use quorum_core::impact::ImpactMap;
use quorum_core::news::{Extraction, ExtractionStatus, Magnitude, NewsEvent, NewsItem};
use quorum_core::{InstrumentId, LlmProvider, NewsStore};
use serde::Serialize;

use crate::Result;
use crate::config::{NewsConfig, Tier};
use crate::fetch::base_asset;

/// The tiers' models; `None` for a tier that is off (or has no key).
#[derive(Debug, Clone, Default)]
pub struct LLMs {
    pub first_pass: Option<Arc<dyn LlmProvider>>,
    pub escalation: Option<Arc<dyn LlmProvider>>,
}

/// What one tier did in a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct TierReport {
    pub attempted: usize,
    pub ok: usize,
    pub rejected: usize,
    pub failed: usize,
    pub tokens: u64,
    /// Items not sent because today's budget for the tier is spent.
    pub skipped_for_budget: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtractReport {
    pub first_pass: TierReport,
    pub escalation: TierReport,
    pub themes: TierReport,
    /// Events from readings made in this run (escalation readings supersede first-pass ones).
    pub events: Vec<NewsEvent>,
    /// Implied moves from theme readings made in this run.
    pub implications: Vec<NewsEvent>,
    /// Why a tier could not run, if any.
    pub warnings: Vec<String>,
}

/// Base assets (`BTC`, `SPY`) and the instrument each maps to (first instrument wins).
#[must_use]
pub fn assets(instruments: &[InstrumentId]) -> Vec<(String, InstrumentId)> {
    let mut map: BTreeMap<String, InstrumentId> = BTreeMap::new();
    for instrument in instruments {
        let base = base_asset(instrument.symbol()).to_ascii_uppercase();
        map.entry(base).or_insert_with(|| instrument.clone());
    }
    map.into_iter().collect()
}

/// Whether a first-pass reading goes to the escalation tier under `rule`.
#[must_use]
pub fn escalates(rule: &str, first: &Extraction) -> bool {
    if first.status != ExtractionStatus::Ok || !first.relevant {
        return false;
    }
    let threshold = match rule {
        "never" => return false,
        "all" => return true,
        "high" => Magnitude::High,
        "low" => Magnitude::Low,
        _ => Magnitude::Medium,
    };
    first.events.iter().any(|e| e.magnitude >= threshold)
}

fn start_of_day(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive().and_time(NaiveTime::MIN).and_utc()
}

/// One attempt's empty record.
fn record(
    item: &NewsItem,
    version: &str,
    tier: Tier,
    model: &str,
    now: DateTime<Utc>,
) -> Extraction {
    Extraction {
        url: item.url.clone(),
        prompt_version: version.to_owned(),
        tier: tier.journal_name().to_owned(),
        model: model.to_owned(),
        ts: now,
        status: ExtractionStatus::Failed,
        relevant: false,
        events: Vec::new(),
        error: None,
        input_tokens: 0,
        output_tokens: 0,
    }
}

/// Reads one item with one model and records the attempt.
async fn attempt(
    store: &dyn NewsStore,
    llm: &dyn LlmProvider,
    tier: Tier,
    item: &NewsItem,
    assets: &[(String, InstrumentId)],
    report: &mut TierReport,
) -> Result<Extraction> {
    let names: Vec<String> = assets.iter().map(|(a, _)| a.clone()).collect();
    let model = format!("{}/{}", llm.name(), llm.model());
    let now = Utc::now();
    let mut extraction = record(item, PROMPT_VERSION, tier, &model, now);
    report.attempted += 1;
    match llm.complete(&extract::request(item, &names)).await {
        Ok(response) => {
            extraction.input_tokens = response.input_tokens;
            extraction.output_tokens = response.output_tokens;
            report.tokens += response.input_tokens + response.output_tokens;
            match extract::parse(&response.text, item, assets, &model, now) {
                Ok(reading) => {
                    extraction.status = ExtractionStatus::Ok;
                    extraction.relevant = reading.relevant;
                    extraction.events = reading.events;
                    report.ok += 1;
                }
                Err(reason) => {
                    tracing::warn!(url = %item.url, %model, %reason, "model output rejected");
                    extraction.status = ExtractionStatus::Rejected;
                    extraction.error = Some(reason);
                    report.rejected += 1;
                }
            }
        }
        Err(err) => {
            tracing::warn!(url = %item.url, %model, error = %err, "extraction failed");
            extraction.error = Some(err.to_string());
            report.failed += 1;
        }
    }
    store.record_extraction(&extraction).await?;
    Ok(extraction)
}

/// Today's tokens spent by `tier`.
async fn spent_today(store: &dyn NewsStore, tier: Tier) -> Result<u64> {
    let usage = store.token_usage(start_of_day(Utc::now())).await?;
    Ok(match tier {
        Tier::FirstPass => usage.local,
        Tier::Escalation => usage.remote,
    })
}

/// Runs the tiers once over pending items.
pub async fn extract(
    cfg: &NewsConfig,
    store: &dyn NewsStore,
    llms: &LLMs,
    instruments: &[InstrumentId],
    map: &ImpactMap,
) -> Result<ExtractReport> {
    let mut report = ExtractReport::default();
    if llms.first_pass.is_none() && llms.escalation.is_none() {
        report
            .warnings
            .push("no model tier enabled (config/news.toml, and an Anthropic key)".into());
        return Ok(report);
    }
    let assets = assets(instruments);
    let mut readings: BTreeMap<String, Extraction> = BTreeMap::new();

    if let Some(first) = &llms.first_pass {
        let mut spent = spent_today(store, Tier::FirstPass).await?;
        let pending = store
            .unextracted(PROMPT_VERSION, Tier::FirstPass.journal_name(), cfg.batch)
            .await?;
        for (index, item) in pending.iter().enumerate() {
            if spent >= cfg.first_pass.daily_tokens {
                report.first_pass.skipped_for_budget = pending.len() - index;
                break;
            }
            let reading = attempt(
                store,
                first.as_ref(),
                Tier::FirstPass,
                item,
                &assets,
                &mut report.first_pass,
            )
            .await?;
            spent += reading.input_tokens + reading.output_tokens;
            readings.insert(item.url.clone(), reading);
        }
    }

    if let Some(escalation) = &llms.escalation {
        let candidates = escalation_candidates(cfg, store, llms.first_pass.is_some()).await?;
        let mut spent = spent_today(store, Tier::Escalation).await?;
        for (index, item) in candidates.iter().enumerate() {
            if spent >= cfg.escalation.daily_tokens {
                report.escalation.skipped_for_budget = candidates.len() - index;
                break;
            }
            let reading = attempt(
                store,
                escalation.as_ref(),
                Tier::Escalation,
                item,
                &assets,
                &mut report.escalation,
            )
            .await?;
            spent += reading.input_tokens + reading.output_tokens;
            if reading.status == ExtractionStatus::Ok {
                readings.insert(item.url.clone(), reading);
            }
        }
    }

    if !map.is_empty() {
        match &llms.first_pass {
            Some(first) => classify_themes(cfg, store, first.as_ref(), map, &mut report).await?,
            None => report
                .warnings
                .push("impact themes run on the first-pass tier, which is off".into()),
        }
    }

    report.events = readings
        .into_values()
        .filter(|r| r.status == ExtractionStatus::Ok)
        .flat_map(|r| r.events)
        .collect();
    tracing::info!(
        first_pass = report.first_pass.attempted,
        escalation = report.escalation.attempted,
        themes = report.themes.attempted,
        events = report.events.len(),
        implications = report.implications.len(),
        "news extraction run"
    );
    Ok(report)
}

/// Classifies unread items into the impact map's themes with the first-pass model.
async fn classify_themes(
    cfg: &NewsConfig,
    store: &dyn NewsStore,
    llm: &dyn LlmProvider,
    map: &ImpactMap,
    report: &mut ExtractReport,
) -> Result<()> {
    let model = format!("{}/{}", llm.name(), llm.model());
    let mut spent = spent_today(store, Tier::FirstPass).await?;
    let pending = store.unthemed(THEMES_VERSION, cfg.batch).await?;
    for (index, item) in pending.iter().enumerate() {
        if spent >= cfg.first_pass.daily_tokens {
            report.themes.skipped_for_budget = pending.len() - index;
            break;
        }
        let now = Utc::now();
        let mut reading = record(item, THEMES_VERSION, Tier::FirstPass, &model, now);
        report.themes.attempted += 1;
        match llm.complete(&themes::request(item, map)).await {
            Ok(response) => {
                reading.input_tokens = response.input_tokens;
                reading.output_tokens = response.output_tokens;
                spent += response.input_tokens + response.output_tokens;
                report.themes.tokens += response.input_tokens + response.output_tokens;
                match themes::parse(&response.text, item, map, &model, now) {
                    Ok(parsed) => {
                        reading.status = ExtractionStatus::Ok;
                        reading.relevant = parsed.relevant;
                        reading.events = parsed.events;
                        report.themes.ok += 1;
                    }
                    Err(reason) => {
                        tracing::warn!(url = %item.url, %model, %reason, "theme reading rejected");
                        reading.status = ExtractionStatus::Rejected;
                        reading.error = Some(reason);
                        report.themes.rejected += 1;
                    }
                }
            }
            Err(err) => {
                tracing::warn!(url = %item.url, %model, error = %err, "theme reading failed");
                reading.error = Some(err.to_string());
                report.themes.failed += 1;
            }
        }
        store.record_themes(&reading).await?;
        report.implications.extend(reading.events);
    }
    Ok(())
}

/// Items for the escalation tier: every unread item without a first pass; otherwise the ones
/// whose first-pass reading escalates.
async fn escalation_candidates(
    cfg: &NewsConfig,
    store: &dyn NewsStore,
    has_first_pass: bool,
) -> Result<Vec<NewsItem>> {
    let tier = Tier::Escalation.journal_name();
    if !has_first_pass {
        return Ok(store.unextracted(PROMPT_VERSION, tier, cfg.batch).await?);
    }
    let pending = store
        .unextracted(PROMPT_VERSION, tier, cfg.batch.saturating_mul(8))
        .await?;
    let urls: Vec<String> = pending.iter().map(|i| i.url.clone()).collect();
    let first: BTreeMap<String, Extraction> = store
        .extractions(&urls, PROMPT_VERSION)
        .await?
        .into_iter()
        .filter(|e| e.tier == Tier::FirstPass.journal_name())
        .map(|e| (e.url.clone(), e))
        .collect();
    Ok(pending
        .into_iter()
        .filter(|item| {
            first
                .get(&item.url)
                .is_some_and(|r| escalates(&cfg.escalate, r))
        })
        .take(cfg.batch)
        .collect())
}

/// The reading that counts for each item: a valid escalation one, else a valid first-pass one.
#[must_use]
pub fn final_readings(extractions: Vec<Extraction>) -> BTreeMap<String, Extraction> {
    let (first, escalation) = (
        Tier::FirstPass.journal_name(),
        Tier::Escalation.journal_name(),
    );
    let mut chosen: BTreeMap<String, Extraction> = BTreeMap::new();
    for extraction in extractions
        .into_iter()
        .filter(|e| e.status == ExtractionStatus::Ok)
    {
        let replace = chosen
            .get(&extraction.url)
            .is_none_or(|current| current.tier == first && extraction.tier == escalation);
        if replace {
            chosen.insert(extraction.url.clone(), extraction);
        }
    }
    chosen
}
