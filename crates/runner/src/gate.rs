//! The paper order path through the risk process: every order the engine sends is approved by
//! it first, and every verdict is recorded so replay makes the same decisions.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use quorum_core::{Broker, ExecutionReport, InstrumentId, Order, OrderId, PortError, Side};
use quorum_envelope::Mode;
use quorum_risk::{CLUSTER_CORRELATION, OrderCheck, RiskGate, Verdict};
use serde::{Deserialize, Serialize};

use crate::session::{Session, SlotFiles};
use crate::{Result, RunnerError};

/// One verdict, as recorded in `verdicts.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Recorded {
    session: String,
    order: u64,
    ts: DateTime<Utc>,
    approved: bool,
    reason: String,
}

#[derive(Debug, thiserror::Error)]
#[error("refused: {0}")]
struct Refused(String);

/// A [`Broker`] that asks the risk process before handing an order to the venue.
///
/// Replay must repeat what earlier runs decided, so verdicts come from three places, in order:
/// the session's recorded verdict for the order; for orders at or before the session's journal
/// watermark with no record (sent before the gate existed), approval; otherwise the risk
/// process, whose answer is recorded before the order goes out. When the process cannot be
/// reached, orders that only reduce risk pass and all others are refused.
///
/// The book it sends with each order (cash, positions, marks) is kept from the fills and quotes
/// that pass through it, starting from the slot's paper capital.
#[derive(Debug)]
pub(crate) struct GatedBroker {
    inner: Box<dyn Broker>,
    gate: Arc<dyn RiskGate>,
    slot: String,
    mode: Mode,
    session: String,
    replay_until: Option<DateTime<Utc>>,
    recorded: BTreeMap<u64, Recorded>,
    log: PathBuf,
    cash: f64,
    positions: BTreeMap<InstrumentId, f64>,
    marks: BTreeMap<InstrumentId, f64>,
    /// Recent quotes per instrument, for the correlation behind `OrderCheck::cluster`.
    closes: BTreeMap<InstrumentId, VecDeque<f64>>,
    interval_secs: i64,
}

/// What the gate tells the risk process about the slot besides its orders.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GateBook {
    pub(crate) mode: Mode,
    /// The paper capital the slot's book starts from.
    pub(crate) capital: f64,
    pub(crate) interval_secs: i64,
}

/// Quotes behind a correlation (60 returns).
const CORRELATION_QUOTES: usize = 61;

impl GatedBroker {
    pub(crate) fn new(
        inner: Box<dyn Broker>,
        gate: Arc<dyn RiskGate>,
        session: &Session,
        book: GateBook,
        files: &SlotFiles,
    ) -> Result<Self> {
        let log = files.verdicts();
        Ok(Self {
            inner,
            gate,
            slot: session.slot.as_str().to_owned(),
            mode: book.mode,
            session: session.id.clone(),
            replay_until: session.journaled_until,
            recorded: load(&log, &session.id)?,
            log,
            cash: book.capital,
            positions: BTreeMap::new(),
            marks: BTreeMap::new(),
            closes: BTreeMap::new(),
            interval_secs: book.interval_secs,
        })
    }

    fn check(&self, order: &Order) -> OrderCheck {
        let value =
            |(id, qty): (&InstrumentId, &f64)| qty * self.marks.get(id).copied().unwrap_or(0.0);
        let price = self.marks.get(&order.instrument).copied().unwrap_or(0.0);
        let held = self
            .positions
            .get(&order.instrument)
            .copied()
            .unwrap_or(0.0);
        let after = match order.side {
            Side::Buy => held + order.qty,
            Side::Sell => held - order.qty,
        };
        let cluster = (after * price).abs()
            + self
                .positions
                .iter()
                .filter(|(id, _)| *id != &order.instrument)
                .filter(|(id, _)| {
                    self.correlation(id, &order.instrument)
                        .is_some_and(|c| c >= CLUSTER_CORRELATION)
                })
                .map(|p| value(p).abs())
                .sum::<f64>();
        OrderCheck {
            slot: self.slot.clone(),
            mode: self.mode,
            instrument: order.instrument.clone(),
            side: order.side,
            qty: order.qty,
            price: self.marks.get(&order.instrument).copied().unwrap_or(0.0),
            position: self
                .positions
                .get(&order.instrument)
                .copied()
                .unwrap_or(0.0),
            gross: self.positions.iter().map(|p| value(p).abs()).sum(),
            equity: self.cash + self.positions.iter().map(value).sum::<f64>(),
            cluster,
            ts: order.created_at,
            interval_secs: self.interval_secs,
        }
    }

    /// Correlation of the two instruments' latest quote-to-quote returns; `None` with fewer
    /// than 20 in common.
    fn correlation(&self, a: &InstrumentId, b: &InstrumentId) -> Option<f64> {
        let returns = |id: &InstrumentId| -> Option<Vec<f64>> {
            let q = self.closes.get(id)?;
            Some(
                q.iter()
                    .zip(q.iter().skip(1))
                    .filter(|(x, _)| **x > 0.0)
                    .map(|(x, y)| y / x - 1.0)
                    .collect(),
            )
        };
        let (ra, rb) = (returns(a)?, returns(b)?);
        let n = ra.len().min(rb.len());
        if n < 20 {
            return None;
        }
        let (ra, rb) = (&ra[ra.len() - n..], &rb[rb.len() - n..]);
        let mean = |xs: &[f64]| xs.iter().sum::<f64>() / xs.len() as f64;
        let (ma, mb) = (mean(ra), mean(rb));
        let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
        for (x, y) in ra.iter().zip(rb) {
            sab = (x - ma).mul_add(y - mb, sab);
            saa = (x - ma).mul_add(x - ma, saa);
            sbb = (y - mb).mul_add(y - mb, sbb);
        }
        let r = sab / (saa * sbb).sqrt();
        r.is_finite().then_some(r)
    }

    fn verdict(&mut self, order: &Order) -> Verdict {
        if let Some(recorded) = self.recorded.get(&order.id.0) {
            return Verdict {
                approved: recorded.approved,
                reason: recorded.reason.clone(),
            };
        }
        if self.replay_until.is_some_and(|w| order.created_at <= w) {
            return Verdict::approve("sent before the risk gate");
        }
        let check = self.check(order);
        let verdict = match self.gate.check(&check) {
            Ok(verdict) => verdict,
            Err(err) if check.reduces_risk() => {
                tracing::warn!(error = %err, order = %order.id, "risk process unreachable; the order only reduces risk");
                Verdict::approve(format!("reduces risk; {err}"))
            }
            Err(err) => {
                tracing::warn!(error = %err, order = %order.id, "risk process unreachable; order refused");
                Verdict::reject(err.to_string())
            }
        };
        self.record(order, &verdict);
        verdict
    }

    /// Appends the verdict before the order goes out. A record that cannot be written is
    /// logged: replay then treats a refused order as sent before the gate.
    fn record(&mut self, order: &Order, verdict: &Verdict) {
        let recorded = Recorded {
            session: self.session.clone(),
            order: order.id.0,
            ts: order.created_at,
            approved: verdict.approved,
            reason: verdict.reason.clone(),
        };
        let written = serde_json::to_string(&recorded)
            .map_err(std::io::Error::other)
            .and_then(|line| {
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.log)?;
                writeln!(file, "{line}")
            });
        if let Err(err) = written {
            tracing::error!(error = %err, path = %self.log.display(), "risk verdict not recorded");
        }
        self.recorded.insert(order.id.0, recorded);
    }
}

impl Broker for GatedBroker {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn quote(&mut self, instrument: &InstrumentId, ts: DateTime<Utc>, price: f64) {
        self.marks.insert(instrument.clone(), price);
        let closes = self.closes.entry(instrument.clone()).or_default();
        closes.push_back(price);
        if closes.len() > CORRELATION_QUOTES {
            closes.pop_front();
        }
        self.inner.quote(instrument, ts, price);
    }

    fn liquidity(&mut self, instrument: &InstrumentId, dollar_volume: f64) {
        self.inner.liquidity(instrument, dollar_volume);
    }

    fn spread(&mut self, instrument: &InstrumentId, half_spread: f64) {
        self.inner.spread(instrument, half_spread);
    }

    fn submit(&mut self, order: &Order) -> std::result::Result<(), PortError> {
        let verdict = self.verdict(order);
        if verdict.approved {
            self.inner.submit(order)
        } else {
            Err(PortError::adapter("risk process", Refused(verdict.reason)))
        }
    }

    fn cancel(&mut self, id: OrderId, ts: DateTime<Utc>) -> std::result::Result<(), PortError> {
        self.inner.cancel(id, ts)
    }

    fn poll(&mut self) -> Vec<ExecutionReport> {
        let reports = self.inner.poll();
        for report in &reports {
            if let ExecutionReport::Filled { fill, .. } = report {
                let signed = match fill.side {
                    Side::Buy => fill.qty,
                    Side::Sell => -fill.qty,
                };
                *self.positions.entry(fill.instrument.clone()).or_default() += signed;
                self.cash -= signed.mul_add(fill.price, fill.fee);
            }
        }
        reports
    }
}

/// The session's recorded verdicts. A torn last line (a crash mid-write) is skipped.
fn load(path: &Path, session: &str) -> Result<BTreeMap<u64, Recorded>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(RunnerError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Recorded>(line) {
            Ok(r) if r.session == session => {
                out.insert(r.order, r);
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(error = %err, path = %path.display(), "unreadable verdict line skipped");
            }
        }
    }
    Ok(out)
}
