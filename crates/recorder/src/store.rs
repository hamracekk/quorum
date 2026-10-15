//! Append-only daily JSONL files: `<root>/<feed>/<YYYY-MM-DD>.jsonl`, one record per line, the
//! day being the UTC date of `known_time`.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use quorum_core::recorder::{Feed, Record};

use crate::{RecorderError, Result};

/// Where recorded data lives (`<data>/recorder/`).
#[derive(Debug, Clone)]
pub struct RecordStore {
    root: PathBuf,
}

impl RecordStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn file(&self, feed: Feed, day: NaiveDate) -> PathBuf {
        self.root
            .join(feed.as_str())
            .join(format!("{}.jsonl", day.format("%Y-%m-%d")))
    }

    /// Appends `record` as one line, in one write, to its feed's file for its day.
    pub fn append(&self, record: &Record) -> Result<()> {
        let path = self.file(record.feed(), record.known_time().date_naive());
        let io = |source| RecorderError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io)?;
        }
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        file.write_all(&line).map_err(io)
    }

    /// Every record of `feed` on `day`, in file order (empty when nothing was recorded).
    pub fn read(&self, feed: Feed, day: NaiveDate) -> Result<Vec<Record>> {
        let path = self.file(feed, day);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&path).map_err(|source| RecorderError::Io {
            path: path.display().to_string(),
            source,
        })?;
        text.lines()
            .map(|line| serde_json::from_str(line).map_err(RecorderError::from))
            .collect()
    }

    /// Takes the recorder's exclusive lock (`<root>/LOCK`): one recorder per data directory.
    /// The OS releases it when the file is dropped or the process dies.
    pub fn lock(&self) -> Result<fs::File> {
        let path = self.root.join("LOCK");
        let io = |source| RecorderError::Io {
            path: path.display().to_string(),
            source,
        };
        fs::create_dir_all(&self.root).map_err(io)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => Err(RecorderError::Refused(format!(
                "another recorder is running ({} is locked)",
                path.display()
            ))),
            Err(fs::TryLockError::Error(source)) => Err(io(source)),
        }
    }
}
