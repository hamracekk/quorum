//! A slot's files and its paper session record.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use quorum_core::{EngineSnapshot, InstrumentId};
use quorum_slots::{SlotConfig, SlotId, SlotState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Result, RunnerError};

/// Where one slot keeps its files: `<data>/slots/<id>/`.
///
/// - `slot.json`: the slot's [`SlotState`] (ladder rung), outliving sessions.
/// - `session.json`: the current or last [`Session`].
/// - `state.json`: the latest [`StateFile`] for status displays.
/// - `verdicts.jsonl`: the risk process's verdict on every order, so replay repeats them.
/// - `risk-caps.jsonl`: exposure caps applied for a risk halt, so replay applies them again.
/// - `STOP`: a request to stop the running session (resumable).
/// - `LOCK`: held by the one process running the slot (released when it exits, crash included).
#[derive(Debug, Clone)]
pub struct SlotFiles {
    pub dir: PathBuf,
}

impl SlotFiles {
    #[must_use]
    pub fn new(data: &Path, slot: &SlotId) -> Self {
        Self {
            dir: data.join("slots").join(slot.as_str()),
        }
    }

    #[must_use]
    pub fn slot(&self) -> PathBuf {
        self.dir.join("slot.json")
    }

    #[must_use]
    pub fn session(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    #[must_use]
    pub fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    #[must_use]
    pub fn verdicts(&self) -> PathBuf {
        self.dir.join("verdicts.jsonl")
    }

    #[must_use]
    pub fn risk_caps(&self) -> PathBuf {
        self.dir.join("risk-caps.jsonl")
    }

    #[must_use]
    pub fn stop(&self) -> PathBuf {
        self.dir.join("STOP")
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("LOCK")
    }

    /// Takes the slot's exclusive lock: one writer per slot (a launchd service and a manual run
    /// cannot both trade it). The OS releases it when the returned file is dropped or the
    /// process dies.
    pub(crate) fn lock(&self) -> Result<fs::File> {
        let path = self.lock_path();
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io(&path))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => Err(RunnerError::Refused(format!(
                "another quorum process is running this slot ({} is locked)",
                path.display()
            ))),
            Err(fs::TryLockError::Error(source)) => Err(RunnerError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    pub(crate) fn create(&self) -> Result<()> {
        fs::create_dir_all(&self.dir).map_err(io(&self.dir))
    }

    pub(crate) fn load_slot(&self) -> Result<Option<SlotState>> {
        read_json(&self.slot())
    }

    pub(crate) fn save_slot(&self, state: &SlotState) -> Result<()> {
        write_json(&self.slot(), state)
    }

    pub fn load_session(&self) -> Result<Option<Session>> {
        read_json(&self.session())
    }

    pub(crate) fn save_session(&self, session: &Session) -> Result<()> {
        write_json(&self.session(), session)
    }

    pub fn load_state(&self) -> Result<Option<StateFile>> {
        read_json(&self.state())
    }

    pub(crate) fn save_state(&self, state: &StateFile) -> Result<()> {
        write_json(&self.state(), state)
    }

    /// Takes a pending stop request, removing the marker.
    pub(crate) fn take_stop(&self) -> bool {
        let path = self.stop();
        if !path.exists() {
            return false;
        }
        if let Err(err) = fs::remove_file(&path) {
            tracing::warn!(error = %err, "could not remove the stop marker");
        }
        true
    }

    /// Drops a stop request left from an earlier run: it was meant for that run only.
    pub(crate) fn clear_stop(&self) -> Result<()> {
        let path = self.stop();
        if path.exists() {
            fs::remove_file(&path).map_err(io(&path))?;
        }
        Ok(())
    }
}

/// Asks the slot's running session to stop at its next heartbeat. It resumes on the next run.
pub fn request_stop(files: &SlotFiles) -> Result<PathBuf> {
    files.create()?;
    let path = files.stop();
    fs::write(&path, b"stop\n").map_err(io(&path))?;
    Ok(path)
}

/// One paper session of a slot. A run resumes the open session of the same configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub slot: SlotId,
    pub label: String,
    pub started_at: DateTime<Utc>,
    /// Bars before this only warm the strategy up; the session trades from here.
    pub trade_from: DateTime<Utc>,
    pub instruments: Vec<InstrumentId>,
    /// The slot configuration the session started with; a changed one needs a fresh session.
    pub config: SlotConfig,
    /// The latest bar whose decisions are journaled. Replay journals only past it, so restarts
    /// never lose or duplicate a decision.
    pub journaled_until: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<String>,
    /// The latest complete timestamp a daily summary was sent for, and the equity then.
    #[serde(default)]
    pub summarized_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub summary_equity: Option<f64>,
}

/// The latest state of a running (or ended) session, for `quorum paper status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateFile {
    pub session_id: String,
    pub slot: SlotId,
    pub status: String,
    pub heartbeat: DateTime<Utc>,
    pub bars_processed: usize,
    pub equity: f64,
    pub cash: f64,
    pub positions: usize,
    pub last_bar: Option<DateTime<Utc>>,
    pub halted: bool,
    pub error: Option<String>,
}

impl StateFile {
    pub(crate) fn new(session: &Session, snapshot: &EngineSnapshot) -> Self {
        Self {
            session_id: session.id.clone(),
            slot: session.slot.clone(),
            status: "running".into(),
            heartbeat: Utc::now(),
            bars_processed: 0,
            equity: snapshot.equity,
            cash: snapshot.cash,
            positions: snapshot.positions.len(),
            last_bar: snapshot.ts,
            halted: snapshot.halted,
            error: None,
        }
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> RunnerError + '_ {
    move |source| RunnerError::Io {
        path: path.display().to_string(),
        source,
    }
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).map_err(io(path))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| RunnerError::Json {
            path: path.display().to_string(),
            source,
        })
}

/// Writes through a temporary file and a rename, so a reader never sees half a file.
pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|source| RunnerError::Json {
        path: path.display().to_string(),
        source,
    })?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(io(&tmp))?;
    fs::rename(&tmp, path).map_err(io(path))
}
