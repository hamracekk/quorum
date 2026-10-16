//! The long-term book on paper (DESIGN §20): the operator's policy run live on end-of-day prices,
//! with its paper contributions, apart from the slots.
//!
//! Each run is a whole replay: from the session's first day, every stored bar of the policy's
//! holdings goes through the one engine with the start amount and a contribution each month;
//! every order goes through the risk process as the book `longterm`, and its verdicts are
//! recorded so the next replay repeats them. Bars that arrived since the last run therefore
//! decide new orders and nothing else changes. The policy is pinned in the session: a changed
//! policy needs a fresh session (`--fresh`), the old one is archived.
//!
//! Files in `<data>/longterm/`: `session.json`, `state.json` (holdings, lots, value, for
//! `quorum longterm status` and the review), `verdicts.jsonl`, `sessions/` (archived), `LOCK`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Duration, DurationRound, NaiveDate, Utc};
use quorum_core::{
    BarStore, Engine, Fill, InstrumentId, MarketData, Notification, Notifier, PaperBroker, Series,
    Severity, Side,
};
use quorum_envelope::{Envelope, LONGTERM_BOOK, Mode};
use quorum_longterm::review::{
    BookFacts, BookNow, CostFacts, EarlierReview, Facts, FundFacts, HoldingFacts, PolicyFacts,
    Position, TaxFacts,
};
use quorum_longterm::{Holding, IfSold, LongTermConfig, Lot, Policy, PolicyStrategy, SimCosts};
use quorum_risk::RiskGate;
use serde::{Deserialize, Serialize};

use crate::gate::{GateBook, GateSession, GatedBroker};
use crate::session::{SlotFiles, read_json, write_json};
use crate::{Result, RunnerError};

/// Tax on a year's non-exempt net gain (§10 ZDP).
pub const TAX_RATE: f64 = 0.15;
const INTERVAL: &str = "1d";

/// The adapters a long-term run talks to.
#[derive(Debug, Clone)]
pub struct BookPorts {
    pub market: Arc<dyn MarketData>,
    pub bars: Arc<dyn BarStore>,
    pub risk: Arc<dyn RiskGate>,
    pub notifier: Arc<dyn Notifier>,
}

/// How a run behaves.
#[derive(Debug, Clone)]
pub struct BookOptions {
    pub data: PathBuf,
    /// Archive the open session and start a new one (needed after a policy change).
    pub fresh: bool,
    pub now: DateTime<Utc>,
}

/// Where the book keeps its files: `<data>/longterm/`.
#[must_use]
pub fn book_dir(data: &Path) -> PathBuf {
    data.join("longterm")
}

/// Costs the session runs with (pinned with the policy).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BookCosts {
    pub fee_rate: f64,
    pub slippage_bps: f64,
    pub fx_cost: f64,
}

/// The paper session of the long-term book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookSession {
    pub id: String,
    pub started_at: DateTime<Utc>,
    /// The first day whose close counts: the start amount goes in on it.
    pub trade_from: DateTime<Utc>,
    pub policy: Policy,
    pub costs: BookCosts,
    /// The latest bar an earlier run processed (new fills after it are announced).
    pub processed_until: Option<DateTime<Utc>>,
}

/// One holding now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HoldingState {
    pub instrument: InstrumentId,
    pub qty: f64,
    pub price_usd: f64,
    pub value_czk: f64,
    /// Share of the book's equity.
    pub weight: f64,
    /// The policy's weight for it.
    pub target: f64,
}

/// One tax lot now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LotState {
    pub instrument: InstrumentId,
    pub qty: f64,
    pub bought: DateTime<Utc>,
    pub cost_czk: f64,
    /// When selling it becomes tax-free (the three-year test).
    pub exempt_from: DateTime<Utc>,
}

/// The book as of the latest close: `state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookState {
    pub session_id: String,
    pub policy: String,
    pub trade_from: DateTime<Utc>,
    /// When this run finished.
    pub updated: DateTime<Utc>,
    /// The latest close processed; `None` before the first.
    pub last_bar: Option<DateTime<Utc>>,
    pub czk_per_usd: f64,
    pub contributed_czk: f64,
    /// Equity in CZK after converting back (the conversion cost included).
    pub value_czk: f64,
    pub cash_czk: f64,
    pub holdings: Vec<HoldingState>,
    pub lots: Vec<LotState>,
    /// Selling everything now.
    pub if_sold: IfSold,
    /// Tax owed on gains realised so far.
    pub tax_paid_czk: f64,
    pub fills: usize,
    pub sells: usize,
    /// Orders the risk process refused.
    pub refused: usize,
    /// The latest fills (at most ten), newest last.
    pub recent_fills: Vec<Fill>,
}

impl BookState {
    /// Value over what was paid in, less one: the plain gain so far (not annualised).
    #[must_use]
    pub fn gain(&self) -> f64 {
        if self.contributed_czk > 0.0 {
            self.value_czk / self.contributed_czk - 1.0
        } else {
            0.0
        }
    }
}

/// The book's latest state, if a run has written one.
pub fn load_state(data: &Path) -> Result<Option<BookState>> {
    read_json(&book_dir(data).join("state.json"))
}

/// Runs the long-term book once: syncs the holdings' bars, replays the session, sends the new
/// orders through the risk process, writes `state.json` and announces new fills.
///
/// `rates` are CZK per USD by day (ČNB), covering the session.
pub async fn run_book(
    ports: &BookPorts,
    cfg: &LongTermConfig,
    envelope: &Envelope,
    rates: &BTreeMap<NaiveDate, f64>,
    options: &BookOptions,
) -> Result<BookState> {
    let policy = cfg
        .policies
        .first()
        .ok_or_else(|| RunnerError::Config("config/longterm.toml has no policy".to_owned()))?;
    preflight(policy, envelope)?;
    if rates.is_empty() {
        return Err(RunnerError::Config("no CZK/USD rates".to_owned()));
    }
    let files = SlotFiles {
        dir: book_dir(&options.data),
    };
    files.create()?;
    let _lock = files.lock()?;
    let costs = BookCosts {
        fee_rate: cfg.fee_rate,
        slippage_bps: cfg.slippage_bps,
        fx_cost: cfg.fx_cost,
    };
    let mut session = open_session(&files, policy, costs, options)?;
    write_json(&files.session(), &session)?;

    let instruments = quorum_longterm::instruments(&session.policy);
    let series = load_bars(ports, &instruments, session.trade_from, options.now).await?;

    let mut strategy = PolicyStrategy::new(session.policy.clone());
    let broker = GatedBroker::new(
        Box::new(PaperBroker::new(costs.fee_rate, costs.slippage_bps)),
        Arc::clone(&ports.risk),
        GateSession {
            book: LONGTERM_BOOK.to_owned(),
            session: session.id.clone(),
            replay_until: None,
            log: files.verdicts(),
        },
        GateBook {
            mode: Mode::Paper,
            capital: 0.0,
            interval_secs: Duration::days(1).num_seconds(),
        },
    )?;
    let mut engine = Engine::with_broker(
        &mut strategy,
        quorum_longterm::engine_config(costs.fee_rate, costs.slippage_bps),
        &instruments,
        Some(session.trade_from),
        Box::new(broker),
    );
    let driven = quorum_longterm::drive(
        &mut engine,
        &session.policy,
        &series,
        rates,
        costs.fx_cost,
        Some(session.trade_from),
    )
    .map_err(|e| RunnerError::Config(e.to_string()))?;
    let snapshot = engine.snapshot();
    drop(engine);

    let report = quorum_longterm::report(
        &session.policy,
        &driven,
        snapshot.equity,
        SimCosts {
            fee_rate: costs.fee_rate,
            slippage_bps: costs.slippage_bps,
            fx_cost: costs.fx_cost,
            tax_rate: TAX_RATE,
        },
    );
    let refused = refused(&files.verdicts(), &session.id)?;
    let state = book_state(&session, &driven, &snapshot, &report, refused);
    write_json(&files.state(), &state)?;

    let new: Vec<&Fill> = driven
        .fills
        .iter()
        .filter(|f| session.processed_until.is_none_or(|t| f.ts > t))
        .collect();
    if !new.is_empty() {
        announce(ports.notifier.as_ref(), &state, &new).await;
    }
    session.processed_until = driven.to.or(session.processed_until);
    write_json(&files.session(), &session)?;
    Ok(state)
}

/// The book as the replay left it.
fn book_state(
    session: &BookSession,
    driven: &quorum_longterm::Driven,
    snapshot: &quorum_core::EngineSnapshot,
    report: &quorum_longterm::SimReport,
    refused: usize,
) -> BookState {
    let rate = driven.rate;
    let targets: BTreeMap<&InstrumentId, f64> = session
        .policy
        .holdings
        .iter()
        .map(|h| (&h.instrument, h.weight))
        .collect();
    let holdings = snapshot
        .positions
        .iter()
        .filter(|p| p.qty.abs() > 1e-12)
        .map(|p| HoldingState {
            instrument: p.instrument.clone(),
            qty: p.qty,
            price_usd: p.mark,
            value_czk: p.value * rate,
            weight: if snapshot.equity > 0.0 {
                p.value / snapshot.equity
            } else {
                0.0
            },
            target: targets.get(&p.instrument).copied().unwrap_or(0.0),
        })
        .collect();
    let lots = driven
        .ledger
        .lots()
        .map(|(id, lot)| LotState {
            instrument: id.clone(),
            qty: lot.qty,
            bought: lot.bought,
            cost_czk: lot.cost_czk,
            exempt_from: lot.exempt_from(),
        })
        .collect();
    BookState {
        session_id: session.id.clone(),
        policy: session.policy.name.clone(),
        trade_from: session.trade_from,
        updated: Utc::now(),
        last_bar: driven.to,
        czk_per_usd: rate,
        contributed_czk: driven.contributed_czk(),
        value_czk: report.value_czk,
        cash_czk: snapshot.cash * rate,
        holdings,
        lots,
        if_sold: report.if_sold,
        tax_paid_czk: report.tax_paid_czk,
        fills: report.trades,
        sells: report.sells,
        refused,
        recent_fills: driven.fills.iter().rev().take(10).rev().cloned().collect(),
    }
}

/// The envelope must bound the book, allow paper and every holding's venue.
fn preflight(policy: &Policy, envelope: &Envelope) -> Result<()> {
    let Some(bounds) = envelope.longterm() else {
        return Err(RunnerError::Refused(
            "the envelope has no [longterm] section: the long-term book may not trade".to_owned(),
        ));
    };
    if bounds.max_mode < Mode::Paper {
        return Err(RunnerError::Refused(format!(
            "the envelope keeps the long-term book in {}",
            bounds.max_mode
        )));
    }
    for h in &policy.holdings {
        if !envelope.allows_venue(h.instrument.venue()) {
            return Err(RunnerError::Refused(format!(
                "{}: venue `{}` is not in the envelope",
                h.instrument,
                h.instrument.venue().as_str()
            )));
        }
        if h.weight > bounds.position_max {
            return Err(RunnerError::Refused(format!(
                "{}: target {:.0}% is over the envelope's longterm.position_max {:.0}%",
                h.instrument,
                h.weight * 100.0,
                bounds.position_max * 100.0
            )));
        }
    }
    Ok(())
}

/// Resumes the open session of the same policy and costs, or starts one (archiving the old one
/// on `fresh`).
fn open_session(
    files: &SlotFiles,
    policy: &Policy,
    costs: BookCosts,
    options: &BookOptions,
) -> Result<BookSession> {
    if let Some(existing) = read_json::<BookSession>(&files.session())? {
        if !options.fresh {
            if existing.policy != *policy || existing.costs != costs {
                return Err(RunnerError::Refused(format!(
                    "the long-term session {} started with a different policy or costs; \
                     restore config/longterm.toml or start over with --fresh",
                    existing.id
                )));
            }
            return Ok(existing);
        }
        let archive = files.dir.join("sessions");
        fs::create_dir_all(&archive).map_err(|source| RunnerError::Io {
            path: archive.display().to_string(),
            source,
        })?;
        write_json(&archive.join(format!("{}.json", existing.id)), &existing)?;
    }
    let trade_from = options
        .now
        .duration_trunc(Duration::days(1))
        .map_err(|e| RunnerError::Config(format!("session start: {e}")))?;
    Ok(BookSession {
        id: uuid::Uuid::now_v7().to_string(),
        started_at: options.now,
        trade_from,
        policy: policy.clone(),
        costs,
        processed_until: None,
    })
}

/// Brings each holding's daily bars up to date from a week before the session, then loads them.
async fn load_bars(
    ports: &BookPorts,
    instruments: &[InstrumentId],
    from: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<Series>> {
    let mut out = Vec::new();
    for instrument in instruments {
        quorum_data::sync(
            ports.market.as_ref(),
            ports.bars.as_ref(),
            instrument,
            INTERVAL,
            from - Duration::days(7),
            now,
        )
        .await?;
        out.push(
            ports
                .bars
                .load(instrument, INTERVAL)?
                .unwrap_or_else(|| Series::new(instrument.clone(), Vec::new())),
        );
    }
    Ok(out)
}

/// Orders of the session the risk process refused.
fn refused(log: &Path, session: &str) -> Result<usize> {
    Ok(crate::gate::load(log, session)?
        .values()
        .filter(|r| !r.approved)
        .count())
}

/// One notification for the fills since the last run (best effort).
async fn announce(notifier: &dyn Notifier, state: &BookState, fills: &[&Fill]) {
    let trades: Vec<String> = fills
        .iter()
        .map(|f| {
            format!(
                "{} {} {:.4} @ {:.2}",
                if f.side == Side::Buy {
                    "bought"
                } else {
                    "sold"
                },
                f.instrument.symbol(),
                f.qty,
                f.price
            )
        })
        .collect();
    let note = Notification {
        ts: Utc::now(),
        severity: Severity::Info,
        title: format!("long-term · {} trades", fills.len()),
        body: format!(
            "{} · value {:.0} CZK, paid in {:.0} CZK ({:+.2}%)",
            trades.join(", "),
            state.value_czk,
            state.contributed_czk,
            state.gain() * 100.0
        ),
    };
    if let Err(err) = notifier.notify(&note).await {
        tracing::warn!(error = %err, "notification not delivered");
    }
}

/// The book's open session, if any.
pub fn load_session(data: &Path) -> Result<Option<BookSession>> {
    read_json(&book_dir(data).join("session.json"))
}

/// The book's weights now (cash is the rest): what a review's "unchanged" holds.
#[must_use]
pub fn book_weights(state: &BookState) -> Vec<Holding> {
    state
        .holdings
        .iter()
        .map(|h| Holding {
            instrument: h.instrument.clone(),
            weight: h.weight,
        })
        .collect()
}

/// The book as a switch to new weights sees it.
#[must_use]
pub fn book_now(state: &BookState) -> BookNow {
    BookNow {
        positions: state
            .holdings
            .iter()
            .map(|h| Position {
                instrument: h.instrument.clone(),
                value_czk: h.value_czk,
                price_czk: h.price_usd * state.czk_per_usd,
            })
            .collect(),
        lots: state
            .lots
            .iter()
            .map(|l| {
                (
                    l.instrument.clone(),
                    Lot {
                        qty: l.qty,
                        bought: l.bought,
                        cost_czk: l.cost_czk,
                    },
                )
            })
            .collect(),
        equity_czk: state.value_czk,
    }
}

/// What the reviewer is shown. `funds` are the funds the operator allows; a proposal may name
/// the policy's instruments and the instruments that price those funds.
#[must_use]
pub fn review_facts(
    state: &BookState,
    session: &BookSession,
    funds: Vec<FundFacts>,
    earlier_reviews: Vec<EarlierReview>,
) -> Facts {
    let mut allowed: Vec<InstrumentId> = session
        .policy
        .holdings
        .iter()
        .map(|h| h.instrument.clone())
        .chain(funds.iter().filter_map(|f| f.priced_by.clone()))
        .collect();
    allowed.sort();
    allowed.dedup();
    let next_exempt = state
        .lots
        .iter()
        .map(|l| l.exempt_from)
        .filter(|t| state.last_bar.is_none_or(|now| *t > now))
        .min();
    Facts {
        as_of: state.last_bar,
        currency: "CZK",
        policy: PolicyFacts {
            name: session.policy.name.clone(),
            monthly_czk: session.policy.monthly_czk,
            band: session.policy.band,
            targets: session.policy.holdings.clone(),
        },
        book: BookFacts {
            started: session.trade_from,
            contributed_czk: state.contributed_czk,
            value_czk: state.value_czk,
            cash_czk: state.cash_czk,
            holdings: state
                .holdings
                .iter()
                .map(|h| HoldingFacts {
                    instrument: h.instrument.clone(),
                    value_czk: h.value_czk,
                    weight: h.weight,
                    target: h.target,
                })
                .collect(),
        },
        tax: TaxFacts {
            rate: TAX_RATE,
            taxable_gain_czk: state.if_sold.taxable_gain,
            exempt_gain_czk: state.if_sold.exempt_gain,
            exempt_within_year_czk: state.if_sold.exempt_within_year,
            next_exempt,
        },
        costs: CostFacts {
            fee_rate: session.costs.fee_rate,
            slippage_bps: session.costs.slippage_bps,
            fx_cost: session.costs.fx_cost,
        },
        funds,
        allowed,
        earlier_reviews,
    }
}
