//! The review of the long-term book (DESIGN §20).
//!
//! What the reviewer is shown, what a proposal would cost now, how it did against keeping the
//! book unchanged, and the journal of reviews and the operator's decisions. A review only
//! proposes. Each proposal is a full set of target weights; it is costed when it
//! is made (trading costs and the tax on selling lots younger than three years) and scored
//! afterwards: the proposed weights bought and held from the review, less that cost, against
//! the book as it stood. Proposing nothing is a valid answer and is what the score favours
//! unless a change pays for itself.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use quorum_core::{InstrumentId, Series};
use serde::{Deserialize, Serialize};

use crate::LongTermError;
use crate::ledger::Lot;
use crate::policy::Holding;

/// How often the book is reviewed, how, and by which model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    pub enabled: bool,
    pub model: String,
    pub effort: String,
    /// Days between reviews.
    pub every_days: i64,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "claude-opus-5-5".to_owned(),
            effort: "high".to_owned(),
            every_days: 91,
        }
    }
}

/// A change the reviewer proposes: the whole book's target weights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub title: String,
    pub rationale: String,
    pub targets: Vec<Holding>,
}

/// What switching the book to a proposal's targets would cost now, in CZK.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SwitchCost {
    /// Value bought and sold.
    pub turnover_czk: f64,
    /// Commissions and slippage on the turnover.
    pub trading_czk: f64,
    /// Gain on the lots sold that are younger than three years.
    pub taxable_gain_czk: f64,
    /// Tax on that gain (no other gains or losses of the year counted).
    pub tax_czk: f64,
    /// Trading and tax as a fraction of the book.
    pub fraction: f64,
}

/// One holding as the switch sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub instrument: InstrumentId,
    pub value_czk: f64,
    /// Price of one unit in CZK now.
    pub price_czk: f64,
}

/// The book as a switch sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BookNow {
    pub positions: Vec<Position>,
    /// Every lot held, in FIFO order per instrument (as the ledger lists them).
    pub lots: Vec<(InstrumentId, Lot)>,
    pub equity_czk: f64,
}

/// The cost of moving `book` to `targets` now.
///
/// Sales take the oldest lots first, as the ledger does; `cost_rate` is commission plus
/// slippage per unit of turnover.
#[must_use]
pub fn switch_cost(
    book: &BookNow,
    targets: &[Holding],
    cost_rate: f64,
    tax_rate: f64,
    now: DateTime<Utc>,
) -> SwitchCost {
    let (positions, lots, equity_czk) = (&book.positions, &book.lots, book.equity_czk);
    let mut out = SwitchCost::default();
    if equity_czk <= 0.0 {
        return out;
    }
    let target = |id: &InstrumentId| {
        targets
            .iter()
            .find(|h| &h.instrument == id)
            .map_or(0.0, |h| h.weight * equity_czk)
    };
    for p in positions {
        let delta = target(&p.instrument) - p.value_czk;
        out.turnover_czk += delta.abs();
        if delta >= 0.0 || p.price_czk <= 0.0 {
            continue;
        }
        let mut left = -delta / p.price_czk;
        for (_, lot) in lots.iter().filter(|(id, _)| id == &p.instrument) {
            if left <= 1e-12 {
                break;
            }
            let take = left.min(lot.qty);
            if now < lot.exempt_from() {
                out.taxable_gain_czk =
                    (p.price_czk - lot.cost_czk).mul_add(take, out.taxable_gain_czk);
            }
            left -= take;
        }
    }
    // New holdings are bought in full.
    out.turnover_czk += targets
        .iter()
        .filter(|h| !positions.iter().any(|p| p.instrument == h.instrument))
        .map(|h| h.weight * equity_czk)
        .sum::<f64>();
    out.trading_czk = out.turnover_czk * cost_rate;
    out.tax_czk = out.taxable_gain_czk.max(0.0) * tax_rate;
    out.fraction = (out.trading_czk + out.tax_czk) / equity_czk;
    out
}

/// How a proposal did against the book left as it was.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Growth of the proposed weights bought and held, less the switch cost.
    pub proposal: f64,
    /// Growth of the book as it stood.
    pub unchanged: f64,
}

impl Score {
    /// What the proposal would have added (negative: it would have cost).
    #[must_use]
    pub fn edge(&self) -> f64 {
        self.proposal - self.unchanged
    }
}

/// The latest close at or before `ts`.
fn close_at(series: &Series, ts: DateTime<Utc>) -> Option<f64> {
    series
        .bars
        .iter()
        .take_while(|b| b.ts <= ts)
        .last()
        .map(|b| b.close)
}

/// Growth of `weights` (fractions of the book; the rest is cash) bought at `from` and held to
/// `to`; `None` when a price is missing.
fn growth(
    weights: &[Holding],
    series: &BTreeMap<InstrumentId, Series>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<f64> {
    let invested: f64 = weights.iter().map(|h| h.weight).sum();
    let mut out = 1.0 - invested;
    for h in weights {
        let s = series.get(&h.instrument)?;
        let (a, b) = (close_at(s, from)?, close_at(s, to)?);
        if a <= 0.0 {
            return None;
        }
        out += h.weight * b / a;
    }
    Some(out)
}

/// Scores a proposal made at `from` up to `to` against the book's weights then.
#[must_use]
pub fn score(
    proposal: &[Holding],
    cost_fraction: f64,
    book: &[Holding],
    series: &BTreeMap<InstrumentId, Series>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<Score> {
    Some(Score {
        from,
        to,
        proposal: growth(proposal, series, from, to)? * (1.0 - cost_fraction),
        unchanged: growth(book, series, from, to)?,
    })
}

/// A proposal with its cost when it was made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Costed {
    pub proposal: Proposal,
    pub cost: SwitchCost,
}

/// One review, as journaled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub id: String,
    pub ts: DateTime<Utc>,
    /// The paper session reviewed.
    pub session: String,
    pub model: String,
    /// The prompt and schema version.
    pub version: String,
    pub summary: String,
    pub observations: Vec<String>,
    /// The book's weights at the review: what "unchanged" holds.
    pub book: Vec<Holding>,
    pub proposals: Vec<Costed>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The operator's answer to one proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub review: String,
    /// Index into the review's proposals.
    pub proposal: usize,
    pub accepted: bool,
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub note: Option<String>,
}

/// `reviews.jsonl` and `decisions.jsonl` in the book's directory: append-only.
#[derive(Debug, Clone)]
pub struct ReviewJournal {
    dir: PathBuf,
}

impl ReviewJournal {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn append(&self, file: &str, value: &impl Serialize) -> Result<(), LongTermError> {
        let path = self.dir.join(file);
        let line = serde_json::to_string(value).map_err(|e| LongTermError::Data(e.to_string()))?;
        fs::create_dir_all(&self.dir)
            .and_then(|()| {
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                writeln!(f, "{line}")
            })
            .map_err(|e| LongTermError::Data(format!("{}: {e}", path.display())))
    }

    fn read<T: serde::de::DeserializeOwned>(&self, file: &str) -> Vec<T> {
        fs::read_to_string(self.dir.join(file))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn add_review(&self, record: &ReviewRecord) -> Result<(), LongTermError> {
        self.append("reviews.jsonl", record)
    }

    /// Every review, oldest first (unreadable lines skipped).
    #[must_use]
    pub fn reviews(&self) -> Vec<ReviewRecord> {
        self.read("reviews.jsonl")
    }

    /// Records a decision on an existing proposal.
    pub fn decide(&self, decision: &Decision) -> Result<(), LongTermError> {
        let known = self
            .reviews()
            .iter()
            .any(|r| r.id == decision.review && decision.proposal < r.proposals.len());
        if !known {
            return Err(LongTermError::Data(format!(
                "no proposal {} in review {}",
                decision.proposal + 1,
                decision.review
            )));
        }
        self.append("decisions.jsonl", decision)
    }

    /// Every decision, oldest first; a later one on the same proposal replaces an earlier one.
    #[must_use]
    pub fn decisions(&self) -> Vec<Decision> {
        self.read("decisions.jsonl")
    }
}

/// Whether a review of `session` is due at `now`: none yet, or the last is `every_days` old.
#[must_use]
pub fn due(reviews: &[ReviewRecord], session: &str, now: DateTime<Utc>, every_days: i64) -> bool {
    reviews
        .iter()
        .filter(|r| r.session == session)
        .map(|r| r.ts)
        .max()
        .is_none_or(|last| now - last >= Duration::days(every_days))
}

/// What the reviewer is shown: the book, the policy, the fund universe and earlier reviews.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Facts {
    pub as_of: Option<DateTime<Utc>>,
    pub currency: &'static str,
    pub policy: PolicyFacts,
    pub book: BookFacts,
    pub tax: TaxFacts,
    pub costs: CostFacts,
    /// Funds the operator allows (UCITS, by ISIN), with the instrument that prices each here.
    pub funds: Vec<FundFacts>,
    /// The only instruments a proposal may name.
    pub allowed: Vec<InstrumentId>,
    pub earlier_reviews: Vec<EarlierReview>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PolicyFacts {
    pub name: String,
    pub monthly_czk: f64,
    /// Rebalance when a holding is this far from its target (absolute).
    pub band: f64,
    pub targets: Vec<Holding>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookFacts {
    pub started: DateTime<Utc>,
    pub contributed_czk: f64,
    pub value_czk: f64,
    pub cash_czk: f64,
    pub holdings: Vec<HoldingFacts>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HoldingFacts {
    pub instrument: InstrumentId,
    pub value_czk: f64,
    pub weight: f64,
    pub target: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaxFacts {
    pub rate: f64,
    /// Selling everything now: gain on lots younger than three years (taxed) and older (free).
    pub taxable_gain_czk: f64,
    pub exempt_gain_czk: f64,
    /// Value of lots turning tax-free within a year.
    pub exempt_within_year_czk: f64,
    /// The earliest date a lot becomes tax-free.
    pub next_exempt: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CostFacts {
    pub fee_rate: f64,
    pub slippage_bps: f64,
    pub fx_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FundFacts {
    pub isin: String,
    pub name: String,
    pub exposure: String,
    pub accumulating: bool,
    pub ter: Option<f64>,
    /// The instrument that prices it in quorum until the broker's own prices exist.
    pub priced_by: Option<InstrumentId>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EarlierReview {
    pub ts: DateTime<Utc>,
    pub summary: String,
    /// Each proposal's title, the operator's decision and its edge so far over unchanged.
    pub proposals: Vec<(String, Option<bool>, Option<f64>)>,
}
