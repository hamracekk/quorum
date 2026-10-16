//! `quorum longterm`: the long-term book (composition only; logic in `quorum_longterm` and
//! `quorum_runner::longterm`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike as _, Utc};
use clap::Subcommand;
use quorum_core::{InstrumentId, Notification, Series, Severity};
use quorum_data::ParquetStore;
use quorum_envelope::Envelope;
use quorum_longterm::review::{
    Costed, Decision, EarlierReview, FundFacts, Proposal, ReviewJournal, ReviewRecord, due, score,
    switch_cost,
};
use quorum_longterm::{Holding, LongTermConfig, SimCosts, simulate};
use quorum_runner::longterm::{
    BookOptions, BookPorts, BookSession, BookState, TAX_RATE, book_dir, book_now, book_weights,
    load_session, load_state, review_facts, run_book,
};

use crate::secrets;

#[derive(Debug, Subcommand)]
pub(crate) enum LongTermCommand {
    /// Run every policy in config/longterm.toml over history in CZK: contributions, band
    /// rebalancing, tax lots; money- and time-weighted results.
    Backtest,
    /// Run the paper book once: new closes, contributions, orders through the risk process,
    /// then the AI review when one is due. The launchd job runs this every 6 hours.
    Paper {
        /// Archive the open session and start again (after changing the policy).
        #[arg(long)]
        fresh: bool,
    },
    /// The paper book now: holdings against targets, lots and their tax clocks.
    Status,
    /// Ask the reviewer about the paper book now, due or not.
    Review,
    /// Every review: its proposals, what each would cost, your decisions, and each
    /// proposal's edge over keeping the book unchanged since.
    Reviews,
    /// Record your decision on a proposal. Accepting does not change the book: you apply it by
    /// editing config/longterm.toml (`paper --fresh`).
    Decide {
        /// The review's id (`quorum longterm reviews`).
        #[arg(long)]
        review: String,
        /// The proposal's number in that review (1, 2, 3).
        #[arg(long)]
        proposal: usize,
        #[arg(long, conflicts_with = "decline", required_unless_present = "decline")]
        accept: bool,
        #[arg(long)]
        decline: bool,
        #[arg(long)]
        note: Option<String>,
    },
}

pub(crate) async fn run(config: &Path, data: &Path, command: LongTermCommand) -> Result<()> {
    match command {
        LongTermCommand::Backtest => backtest(config, data).await,
        LongTermCommand::Paper { fresh } => paper(config, data, fresh).await,
        LongTermCommand::Status => {
            let state = load_state(data)?.context("no paper book yet: `quorum longterm paper`")?;
            print_state(&state);
            Ok(())
        }
        LongTermCommand::Review => {
            let cfg = config_of(config)?;
            let ports = book_ports(config, data)?;
            let state = load_state(data)?.context("no paper book yet: `quorum longterm paper`")?;
            review(config, data, &cfg, &ports, &state).await
        }
        LongTermCommand::Reviews => reviews(config, data).await,
        LongTermCommand::Decide {
            review,
            proposal,
            accept,
            decline: _,
            note,
        } => {
            let journal = ReviewJournal::new(&book_dir(data));
            let decision = Decision {
                review,
                proposal: proposal.checked_sub(1).context("proposals count from 1")?,
                accepted: accept,
                ts: Utc::now(),
                note,
            };
            journal.decide(&decision)?;
            println!(
                "recorded: proposal {proposal} of review {} {}",
                decision.review,
                if accept { "accepted" } else { "declined" }
            );
            if accept {
                println!(
                    "apply it by editing config/longterm.toml, then `quorum longterm paper --fresh`"
                );
            }
            Ok(())
        }
    }
}

fn config_of(config: &Path) -> Result<LongTermConfig> {
    LongTermConfig::load(&config.join("longterm.toml")).context("long-term config")
}

/// End-of-day prices from Tiingo into the book's own bar store (apart from the slots').
fn book_ports(config: &Path, data: &Path) -> Result<BookPorts> {
    let key = secrets::get(secrets::TIINGO);
    if key.is_none() {
        bail!("no Tiingo key: `quorum secrets set tiingo`");
    }
    Ok(BookPorts {
        market: Arc::new(quorum_data::tiingo::TiingoDaily::new(key).context("tiingo prices")?),
        bars: Arc::new(ParquetStore::new(book_dir(data).join("bars"))),
        risk: Arc::new(quorum_risk::SocketGate::new(
            crate::risk::paths(config, data).socket(),
        )),
        notifier: crate::notifier(config)?,
    })
}

async fn paper(config: &Path, data: &Path, fresh: bool) -> Result<()> {
    let cfg = config_of(config)?;
    let envelope = Envelope::load(&config.join("envelope.toml")).context("envelope")?;
    let rates = crate::benchmark::czk_per_usd(data, Utc::now().year() - 1).await?;
    let ports = book_ports(config, data)?;
    let options = BookOptions {
        data: data.to_path_buf(),
        fresh,
        now: Utc::now(),
    };
    let state = run_book(&ports, &cfg, &envelope, &rates, &options).await?;
    print_state(&state);
    let journal = ReviewJournal::new(&book_dir(data));
    if cfg.review.enabled
        && !state.holdings.is_empty()
        && due(
            &journal.reviews(),
            &state.session_id,
            Utc::now(),
            cfg.review.every_days,
        )
        && let Err(err) = review(config, data, &cfg, &ports, &state).await
    {
        // The review is advice: a failure never fails the book's run.
        tracing::warn!(error = %err, "long-term review failed; the next run tries again");
    }
    Ok(())
}

fn print_state(s: &BookState) {
    println!(
        "long-term book `{}` (paper) since {} · last close {}",
        s.policy,
        s.trade_from.date_naive(),
        s.last_bar
            .map_or_else(|| "none yet".to_owned(), |t| t.date_naive().to_string())
    );
    println!(
        "paid in {:.0} CZK · value {:.0} CZK ({:+.2}%) · cash {:.0} CZK · {} fills, {} sells, {} refused",
        s.contributed_czk,
        s.value_czk,
        s.gain() * 100.0,
        s.cash_czk,
        s.fills,
        s.sells,
        s.refused
    );
    for h in &s.holdings {
        println!(
            "  {:<22} {:>10.0} CZK  {:>5.1}% (target {:>4.1}%)",
            h.instrument.to_string(),
            h.value_czk,
            h.weight * 100.0,
            h.target * 100.0
        );
    }
    let next = s.lots.iter().map(|l| l.exempt_from).min();
    println!(
        "{} lots · selling all now: taxable gain {:.0} CZK, tax-free {:.0} CZK{}",
        s.lots.len(),
        s.if_sold.taxable_gain,
        s.if_sold.exempt_gain,
        next.map_or_else(String::new, |t| format!(
            " · first lot tax-free on {}",
            t.date_naive()
        ))
    );
}

/// Brings `instruments`' bars in the book's store up to date and loads them.
async fn series_for(
    ports: &BookPorts,
    instruments: &[InstrumentId],
    since: chrono::DateTime<Utc>,
) -> Result<BTreeMap<InstrumentId, Series>> {
    let mut out = BTreeMap::new();
    for id in instruments {
        quorum_data::sync(
            ports.market.as_ref(),
            ports.bars.as_ref(),
            id,
            "1d",
            since,
            Utc::now(),
        )
        .await?;
        if let Some(s) = ports.bars.load(id, "1d")? {
            out.insert(id.clone(), s);
        }
    }
    Ok(out)
}

fn targets_of(p: &Proposal) -> &[Holding] {
    &p.targets
}

/// Each earlier proposal's edge over the book unchanged, from its review to the latest close.
fn edges(record: &ReviewRecord, series: &BTreeMap<InstrumentId, Series>) -> Vec<Option<f64>> {
    let now = Utc::now();
    record
        .proposals
        .iter()
        .map(|c| {
            score(
                targets_of(&c.proposal),
                c.cost.fraction,
                &record.book,
                series,
                record.ts,
                now,
            )
            .map(|s| s.edge())
        })
        .collect()
}

fn decision_of(decisions: &[Decision], review: &str, proposal: usize) -> Option<bool> {
    decisions
        .iter()
        .rev()
        .find(|d| d.review == review && d.proposal == proposal)
        .map(|d| d.accepted)
}

/// The funds the operator allows (`config/universe/ucits.toml`), as the reviewer sees them.
fn fund_facts(config: &Path) -> Result<Vec<FundFacts>> {
    let universe = quorum_slots::ucits::UcitsUniverse::load(&config.join("universe/ucits.toml"))
        .map_err(|e| anyhow!(e))?;
    Ok(universe
        .funds
        .iter()
        .map(|f| FundFacts {
            isin: f.isin.clone(),
            name: f.name.clone(),
            exposure: f.exposure.clone(),
            accumulating: f.accumulating,
            ter: f.ter,
            priced_by: f.stand_in.clone(),
        })
        .collect())
}

/// Each proposal with what switching the book to it would cost now.
fn costed(
    proposals: Vec<quorum_ai::holdings::ProposedChange>,
    state: &BookState,
    session: &BookSession,
) -> Result<Vec<Costed>> {
    let book = book_now(state);
    let now = state.last_bar.unwrap_or_else(Utc::now);
    let cost_rate = session.costs.fee_rate + session.costs.slippage_bps / 10_000.0;
    proposals
        .into_iter()
        .map(|p| {
            let targets = p
                .targets
                .iter()
                .map(|t| {
                    Ok(Holding {
                        instrument: t.instrument.parse().map_err(|e| anyhow!("{e}"))?,
                        weight: t.weight,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let cost = switch_cost(&book, &targets, cost_rate, TAX_RATE, now);
            Ok(Costed {
                proposal: Proposal {
                    title: p.title,
                    rationale: p.rationale,
                    targets,
                },
                cost,
            })
        })
        .collect()
}

async fn review(
    config: &Path,
    data: &Path,
    cfg: &LongTermConfig,
    ports: &BookPorts,
    state: &BookState,
) -> Result<()> {
    use quorum_core::ports::LlmProvider as _;

    let session: BookSession = load_session(data)?.context("no paper session")?;
    let key = secrets::get(secrets::ANTHROPIC)
        .context("no Anthropic key: `quorum secrets set anthropic`")?;
    let llm = quorum_ai::ClaudeProvider::new(key, &cfg.review.model, &cfg.review.effort, false)
        .context("Claude client")?;
    let funds = fund_facts(config)?;
    let journal = ReviewJournal::new(&book_dir(data));
    let earlier_records: Vec<ReviewRecord> = journal
        .reviews()
        .into_iter()
        .filter(|r| r.session == session.id)
        .collect();
    let draft = review_facts(state, &session, funds.clone(), Vec::new());
    let series = series_for(ports, &draft.allowed, session.trade_from).await?;
    let decisions = journal.decisions();
    let earlier = earlier_records
        .iter()
        .map(|r| EarlierReview {
            ts: r.ts,
            summary: r.summary.clone(),
            proposals: r
                .proposals
                .iter()
                .zip(edges(r, &series))
                .enumerate()
                .map(|(i, (c, edge))| {
                    (
                        c.proposal.title.clone(),
                        decision_of(&decisions, &r.id, i),
                        edge,
                    )
                })
                .collect(),
        })
        .collect();
    let facts = review_facts(state, &session, funds, earlier);
    let allowed: BTreeSet<String> = facts.allowed.iter().map(ToString::to_string).collect();
    let response = llm
        .complete(&quorum_ai::holdings::request(&serde_json::to_string(
            &facts,
        )?))
        .await
        .context("the reviewer")?;
    let answer =
        quorum_ai::holdings::parse(&response.text, &allowed).map_err(|e| anyhow!("answer: {e}"))?;

    let proposals = costed(answer.proposals, state, &session)?;
    let record = ReviewRecord {
        id: Utc::now().format("%Y%m%d-%H%M%S").to_string(),
        ts: Utc::now(),
        session: session.id.clone(),
        model: cfg.review.model.clone(),
        version: quorum_ai::holdings::HOLDINGS_VERSION.to_owned(),
        summary: answer.summary,
        observations: answer.observations,
        book: book_weights(state),
        proposals,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
    };
    journal.add_review(&record)?;
    print_review(&record, &[], &[]);
    let note = Notification {
        ts: Utc::now(),
        severity: Severity::Info,
        title: "long-term · review".to_owned(),
        body: format!(
            "{} proposals · `quorum longterm reviews` to read and decide",
            record.proposals.len()
        ),
    };
    if let Err(err) = ports.notifier.notify(&note).await {
        tracing::warn!(error = %err, "notification not delivered");
    }
    Ok(())
}

fn print_review(r: &ReviewRecord, decisions: &[Decision], edges: &[Option<f64>]) {
    println!(
        "review {} ({}, {} in / {} out tokens)",
        r.id, r.model, r.input_tokens, r.output_tokens
    );
    println!("  {}", r.summary);
    for o in &r.observations {
        println!("  · {o}");
    }
    if r.proposals.is_empty() {
        println!("  no change proposed");
    }
    for (i, c) in r.proposals.iter().enumerate() {
        let weights: Vec<String> = c
            .proposal
            .targets
            .iter()
            .map(|h| format!("{} {:.0}%", h.instrument.symbol(), h.weight * 100.0))
            .collect();
        println!("  {}. {} — {}", i + 1, c.proposal.title, weights.join(", "));
        println!("     {}", c.proposal.rationale);
        println!(
            "     switching now: turnover {:.0} CZK, costs {:.0} CZK + tax {:.0} CZK ({:.2}% of the book)",
            c.cost.turnover_czk,
            c.cost.trading_czk,
            c.cost.tax_czk,
            c.cost.fraction * 100.0
        );
        let decided = match decision_of(decisions, &r.id, i) {
            Some(true) => "accepted",
            Some(false) => "declined",
            None => "undecided",
        };
        let edge = edges
            .get(i)
            .copied()
            .flatten()
            .map_or_else(String::new, |e| {
                format!(" · {:+.2}% vs unchanged since", e * 100.0)
            });
        println!("     {decided}{edge}");
    }
}

async fn reviews(config: &Path, data: &Path) -> Result<()> {
    let journal = ReviewJournal::new(&book_dir(data));
    let records = journal.reviews();
    if records.is_empty() {
        println!("no reviews yet (`quorum longterm review`, or wait for the paper book's first)");
        return Ok(());
    }
    let mut instruments: Vec<InstrumentId> = records
        .iter()
        .flat_map(|r| {
            r.book
                .iter()
                .chain(r.proposals.iter().flat_map(|c| c.proposal.targets.iter()))
                .map(|h| h.instrument.clone())
        })
        .collect();
    instruments.sort();
    instruments.dedup();
    let since = records.iter().map(|r| r.ts).min().unwrap_or_else(Utc::now);
    let ports = book_ports(config, data)?;
    let series = series_for(&ports, &instruments, since).await?;
    let decisions = journal.decisions();
    for r in &records {
        print_review(r, &decisions, &edges(r, &series));
    }
    println!(
        "decide with `quorum longterm decide --review <id> --proposal <n> --accept|--decline`"
    );
    Ok(())
}

async fn backtest(config: &Path, data: &Path) -> Result<()> {
    let cfg = config_of(config)?;
    let (_, series, _) = crate::research_series(config, data, &cfg.prices_from)?;
    let first_year = series
        .iter()
        .filter_map(|s| s.bars.first().map(|b| b.ts.year()))
        .min()
        .unwrap_or_else(|| Utc::now().year());
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
