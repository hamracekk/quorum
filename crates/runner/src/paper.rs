//! The paper run: preflight, open or resume the session, rebuild by replay, then live bars.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, DurationRound, Utc};
use futures_util::StreamExt;
use quorum_bus::Bus;
use quorum_core::{Bar, Engine, EngineError, Event, InstrumentId, RunInfo, RunKind, Series};
use quorum_envelope::{Envelope, Mode};
use quorum_slots::{Evidence, Gates, SlotConfig, SlotState, Transition, Universe};
use serde::Serialize;

use crate::build::{backtest_config, interval_duration, strategy};
use crate::session::{Session, SlotFiles, StateFile};
use crate::{Ports, Result, RunnerError};

/// How a run behaves. `now` is the run's notion of the current time (tests pin it).
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Root of quorum's data directory (`data/`).
    pub data: PathBuf,
    /// End an open session and start a new one.
    pub fresh: bool,
    pub now: DateTime<Utc>,
    /// How often the stop marker is checked and `state.json` refreshed.
    pub heartbeat: StdDuration,
    /// How often closed bars the stream may have missed are fetched.
    pub catch_up_every: StdDuration,
}

impl RunOptions {
    #[must_use]
    pub fn new(data: PathBuf) -> Self {
        Self {
            data,
            fresh: false,
            now: Utc::now(),
            heartbeat: StdDuration::from_secs(10),
            catch_up_every: StdDuration::from_mins(15),
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Stopped on request (`quorum paper stop`, Ctrl+C). The session resumes on the next run.
    Stopped,
    /// The live stream ended. The session resumes on the next run.
    StreamEnded,
}

impl Status {
    /// Whether a supervisor should start the run again: a stream that ended is a transient
    /// failure; a requested stop is not.
    #[must_use]
    pub const fn restart(self) -> bool {
        matches!(self, Self::StreamEnded)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::StreamEnded => "stream-ended",
        })
    }
}

/// The result of a run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub session_id: String,
    pub resumed: bool,
    pub status: Status,
    /// Live bars processed by this run (replayed history not counted).
    pub bars: usize,
    pub equity: f64,
    pub journaled_until: Option<DateTime<Utc>>,
}

/// Paper-trades `slot` until `shutdown` resolves, a stop is requested, or the stream ends.
pub async fn run(
    ports: &Ports,
    slot: &SlotConfig,
    envelope: &Envelope,
    options: &RunOptions,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<Outcome> {
    let step = interval_duration(&slot.interval)?;
    let files = SlotFiles::new(&options.data, &slot.id);
    files.create()?;
    let _lock = files.lock()?; // held until the run returns
    preflight(ports, slot, envelope, &files, options.now)?;
    let instruments = instruments(slot)?;
    let (session, resumed) = open_session(&files, slot, &instruments, options, step)?;
    if !resumed {
        ports
            .journal
            .start_run(&RunInfo {
                id: session.id.clone(),
                kind: RunKind::Paper,
                label: session.label.clone(),
                started_at: session.started_at,
            })
            .await?;
    }
    files.clear_stop()?;
    files.save_session(&session)?;
    tracing::info!(session = %session.id, resumed, trade_from = %session.trade_from, "paper session");

    let history = load_history(ports, slot, &instruments, options.now).await?;
    // The engine borrows the strategy for the whole run.
    let mut book = strategy(slot, instruments.len())?;
    let engine = Engine::new(
        book.as_mut(),
        backtest_config(slot, envelope),
        &instruments,
        Some(session.trade_from),
    );
    let mut live = Live {
        ports,
        files,
        session,
        engine,
        series: BTreeMap::new(),
        bus: Bus::new(Arc::clone(&ports.log), Bus::DEFAULT_CAPACITY),
        step,
        bars: 0,
        clock: (options.now, Utc::now()),
        last_catch_up: None,
        catch_up_every: options.catch_up_every,
    };
    live.rebuild(&instruments, history).await?;
    let status = live.trade(&instruments, options.heartbeat, shutdown).await;
    live.finish(status, resumed)
}

/// Checks the slot may paper-trade: venue allowed, ports on the slot's venue, and a ladder
/// rung of at least paper. Applies a lowered envelope to the slot's rung first.
fn preflight(
    ports: &Ports,
    slot: &SlotConfig,
    envelope: &Envelope,
    files: &SlotFiles,
    now: DateTime<Utc>,
) -> Result<SlotState> {
    slot.check_envelope(envelope)?;
    for venue in [ports.market.venue(), ports.stream.venue()] {
        if venue != &slot.venue {
            return Err(RunnerError::Config(format!(
                "{}: slot venue is `{}` but a port serves `{}`",
                slot.id,
                slot.venue.as_str(),
                venue.as_str()
            )));
        }
    }
    let mut state = files
        .load_slot()?
        .unwrap_or_else(|| SlotState::new(slot, envelope, now));
    if state.id() != &slot.id {
        return Err(RunnerError::Config(format!(
            "{} holds the state of slot {}",
            files.slot().display(),
            state.id()
        )));
    }
    // No evidence is measured yet (P1+), so this review can only apply a lowered envelope;
    // every promotion gate fails on empty evidence.
    let transition = state.review(&Evidence::default(), &Gates::default(), envelope, now);
    if matches!(
        transition,
        Transition::Demoted { .. } | Transition::Halted { .. }
    ) {
        tracing::warn!(slot = %slot.id, ?transition, "slot moved down the ladder");
    }
    files.save_slot(&state)?;
    if state.is_halted() {
        return Err(RunnerError::Refused(format!(
            "slot {} is halted; the operator resumes it",
            slot.id
        )));
    }
    if state.mode() < Mode::Paper {
        return Err(RunnerError::Refused(format!(
            "slot {} is in {}; it reaches paper through the ladder",
            slot.id,
            state.mode()
        )));
    }
    Ok(state)
}

fn instruments(slot: &SlotConfig) -> Result<Vec<InstrumentId>> {
    match &slot.universe {
        Universe::Fixed { instruments } => Ok(instruments.clone()),
        Universe::TopVolume { .. } => Err(RunnerError::Refused(format!(
            "{}: top-volume universes need the candidate sync, which is not built yet",
            slot.id
        ))),
    }
}

fn label(slot: &SlotConfig, instruments: usize) -> String {
    format!(
        "paper {} {} {} instruments {}",
        slot.id,
        slot.strategy.name(),
        instruments,
        slot.interval
    )
}

/// Resumes the open session of the same configuration, or starts a new one.
fn open_session(
    files: &SlotFiles,
    slot: &SlotConfig,
    instruments: &[InstrumentId],
    options: &RunOptions,
    step: Duration,
) -> Result<(Session, bool)> {
    if let Some(existing) = files.load_session()?
        && existing.ended_at.is_none()
        && !options.fresh
    {
        if existing.config != *slot || existing.instruments != instruments {
            return Err(RunnerError::Refused(format!(
                "open session {} of slot {} was started with a different configuration; \
                 restore it or start over with --fresh",
                existing.id, slot.id
            )));
        }
        return Ok((existing, true));
    }
    let trade_from = options
        .now
        .duration_trunc(step)
        .map_err(|e| RunnerError::Config(format!("session start: {e}")))?;
    let session = Session {
        id: uuid::Uuid::now_v7().to_string(),
        slot: slot.id.clone(),
        label: label(slot, instruments.len()),
        started_at: options.now,
        trade_from,
        instruments: instruments.to_vec(),
        config: slot.clone(),
        journaled_until: None,
        ended_at: None,
        end_reason: None,
    };
    Ok((session, false))
}

/// Brings every instrument's stored bars up to date, then loads them.
async fn load_history(
    ports: &Ports,
    slot: &SlotConfig,
    instruments: &[InstrumentId],
    now: DateTime<Utc>,
) -> Result<BTreeMap<InstrumentId, Series>> {
    let start = now - Duration::days(i64::from(slot.history_days));
    let mut series = BTreeMap::new();
    for instrument in instruments {
        quorum_data::sync(
            ports.market.as_ref(),
            ports.bars.as_ref(),
            instrument,
            &slot.interval,
            start,
            now,
        )
        .await?;
        let loaded = ports
            .bars
            .load(instrument, &slot.interval)?
            .unwrap_or_else(|| Series::new(instrument.clone(), Vec::new()));
        series.insert(instrument.clone(), loaded);
    }
    Ok(series)
}

/// Everything the live loop mutates.
struct Live<'s> {
    ports: &'s Ports,
    files: SlotFiles,
    session: Session,
    engine: Engine<'s>,
    series: BTreeMap<InstrumentId, Series>,
    bus: Bus,
    step: Duration,
    bars: usize,
    /// `RunOptions::now` and the wall clock at the start. Wall clock, not a monotonic timer:
    /// on macOS the monotonic clock stops while the machine sleeps.
    clock: (DateTime<Utc>, DateTime<Utc>),
    last_catch_up: Option<tokio::time::Instant>,
    catch_up_every: StdDuration,
}

impl Live<'_> {
    fn now(&self) -> DateTime<Utc> {
        self.clock.0 + (Utc::now() - self.clock.1).max(Duration::zero())
    }

    fn last_ts(&self, instrument: &InstrumentId) -> Option<DateTime<Utc>> {
        self.series
            .get(instrument)
            .and_then(|s| s.bars.last())
            .map(|b| b.ts)
    }

    fn publish(&self, events: &[Event]) {
        for event in events {
            if let Err(err) = self.bus.publish(event) {
                tracing::error!(error = %err, "could not append to the event log");
            }
        }
    }

    fn write_state(&self, status: &str, error: Option<String>) -> Result<()> {
        self.files.save_state(&StateFile {
            status: status.to_owned(),
            bars_processed: self.bars,
            error,
            ..StateFile::new(&self.session, &self.engine.snapshot())
        })
    }

    /// Replays stored bars through the fresh engine in time order (instruments in config
    /// order within a timestamp). Decisions past the watermark, and from `trade_from` on, are
    /// published and journaled; everything earlier was journaled by an earlier run or only
    /// warms the strategy up.
    async fn rebuild(
        &mut self,
        instruments: &[InstrumentId],
        loaded: BTreeMap<InstrumentId, Series>,
    ) -> Result<()> {
        let rank: BTreeMap<&InstrumentId, usize> = instruments
            .iter()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();
        let mut bars: Vec<(usize, &InstrumentId, &Bar)> = loaded
            .iter()
            .flat_map(|(id, s)| {
                let r = rank.get(id).copied().unwrap_or(usize::MAX);
                s.bars.iter().map(move |bar| (r, id, bar))
            })
            .collect();
        bars.sort_by_key(|&(r, _, bar)| (bar.ts, r));

        let watermark = self.session.journaled_until;
        let mut fresh = Vec::new();
        for (_, instrument, bar) in bars {
            let events = self.engine.on_bar(instrument, bar)?;
            if watermark.is_none_or(|w| bar.ts > w) && bar.ts >= self.session.trade_from {
                fresh.push(Event::Bar {
                    instrument: instrument.clone(),
                    bar: *bar,
                });
                fresh.extend(events);
                self.session.journaled_until = Some(bar.ts);
            }
        }
        self.publish(&fresh);
        let decisions: Vec<Event> = fresh
            .into_iter()
            .filter(|e| !matches!(e, Event::Bar { .. }))
            .collect();
        self.ports
            .journal
            .record(&self.session.id, &decisions)
            .await?;
        self.series = loaded;
        self.files.save_session(&self.session)?;
        self.write_state("running", None)
    }

    /// The live loop. Returns how it ended; errors end it as a failure.
    async fn trade(
        &mut self,
        instruments: &[InstrumentId],
        heartbeat: StdDuration,
        shutdown: impl Future<Output = ()> + Send,
    ) -> Result<Status> {
        let interval = self.session.config.interval.clone();
        let mut stream = self.ports.stream.subscribe(instruments, &interval).await?;
        let mut ticks = tokio::time::interval(heartbeat);
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => return Ok(Status::Stopped),
                _ = ticks.tick() => {
                    if self.files.take_stop() {
                        tracing::info!("stop requested");
                        return Ok(Status::Stopped);
                    }
                    self.catch_up().await?;
                    self.write_state("running", None)?;
                }
                next = stream.next() => match next {
                    None => return Ok(Status::StreamEnded),
                    Some(Err(err)) => tracing::warn!(error = %err, "live stream error"),
                    Some(Ok((instrument, bar))) => {
                        self.on_live_bar(&instrument, bar).await?;
                        self.files.save_session(&self.session)?;
                        self.write_state("running", None)?;
                    }
                },
            }
        }
    }

    /// One closed live bar. A gap before it means the stream missed bars: every instrument is
    /// backfilled up to it first, in time order — backfilling only this instrument would move
    /// the engine past bars the others still miss, and those would arrive out of order.
    async fn on_live_bar(&mut self, instrument: &InstrumentId, bar: Bar) -> Result<()> {
        let Some(last) = self
            .series
            .get(instrument)
            .map(|s| s.bars.last().map(|b| b.ts))
        else {
            tracing::warn!(%instrument, "bar for an instrument outside the slot; skipped");
            return Ok(());
        };
        if last.is_some_and(|ts| bar.ts <= ts) {
            tracing::debug!(%instrument, ts = %bar.ts, "duplicate bar skipped");
            return Ok(());
        }
        if last.is_some_and(|ts| bar.ts > ts + self.step) {
            let filled = self.fill_until(bar.ts).await?;
            tracing::info!(%instrument, filled, "backfilled a gap in the live stream");
        }
        self.process(instrument, bar).await
    }

    async fn process(&mut self, instrument: &InstrumentId, bar: Bar) -> Result<()> {
        let decisions = match self.engine.on_bar(instrument, &bar) {
            Ok(decisions) => decisions,
            Err(EngineError::OutOfOrder { .. }) => {
                tracing::warn!(%instrument, ts = %bar.ts, "bar arrived out of order; skipped");
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        self.publish(&[Event::Bar {
            instrument: instrument.clone(),
            bar,
        }]);
        self.publish(&decisions);
        self.ports
            .journal
            .record(&self.session.id, &decisions)
            .await?;
        self.session.journaled_until = Some(bar.ts);
        if let Some(series) = self.series.get_mut(instrument) {
            series.bars.push(bar);
            self.ports
                .bars
                .save(series, &self.session.config.interval)?;
        }
        self.bars += 1;
        Ok(())
    }

    /// Fetches closed bars the stream missed (a reconnect, a sleeping laptop), at most every
    /// `catch_up_every`, and trades them in time order.
    async fn catch_up(&mut self) -> Result<()> {
        if self
            .last_catch_up
            .is_some_and(|t| t.elapsed() < self.catch_up_every)
        {
            return Ok(());
        }
        self.last_catch_up = Some(tokio::time::Instant::now());
        let filled = self.fill_until(self.now()).await?;
        if filled > 0 {
            tracing::info!(bars = filled, "caught up on closed bars the stream missed");
            self.files.save_session(&self.session)?;
        }
        Ok(())
    }

    /// Trades every instrument's missing closed bars with `ts < until`, in time order
    /// (instruments in slot order within a timestamp). Returns how many were traded.
    async fn fill_until(&mut self, until: DateTime<Utc>) -> Result<usize> {
        let instruments = self.session.instruments.clone();
        let mut missing: Vec<(InstrumentId, Bar)> = Vec::new();
        for instrument in &instruments {
            let Some(last) = self.last_ts(instrument) else {
                continue;
            };
            // Nothing new can have closed: the bar after `last` closes at `last + 2 steps`.
            if until <= last + self.step {
                continue;
            }
            let start = last + Duration::milliseconds(1);
            match self
                .ports
                .market
                .bars(instrument, &self.session.config.interval, start, until)
                .await
            {
                Ok(bars) => missing.extend(
                    bars.into_iter()
                        .filter(|b| b.ts > last && b.ts < until)
                        .map(|b| (instrument.clone(), b)),
                ),
                Err(err) => tracing::warn!(%instrument, error = %err, "backfill failed"),
            }
        }
        missing.sort_by_key(|(_, bar)| bar.ts); // stable: instruments keep their order
        let filled = missing.len();
        for (instrument, bar) in missing {
            self.process(&instrument, bar).await?;
        }
        Ok(filled)
    }

    /// Records how the run ended. The session stays open: the next run resumes it.
    fn finish(&self, status: Result<Status>, resumed: bool) -> Result<Outcome> {
        let status = match status {
            Ok(status) => status,
            Err(err) => {
                tracing::error!(error = %err, "paper run stopped on an error");
                if let Err(write) = self.write_state("failed", Some(err.to_string())) {
                    tracing::error!(error = %write, "could not record the failure in state.json");
                }
                return Err(err);
            }
        };
        self.files.save_session(&self.session)?;
        self.write_state(&status.to_string(), None)?;
        Ok(Outcome {
            session_id: self.session.id.clone(),
            resumed,
            status,
            bars: self.bars,
            equity: self.engine.snapshot().equity,
            journaled_until: self.session.journaled_until,
        })
    }
}
