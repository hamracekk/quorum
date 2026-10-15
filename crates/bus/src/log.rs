use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, NaiveDate, Utc};
use quorum_core::{Envelope, Event, EventLog, PortError};

use crate::BusError;

/// `EventLog` on JSON Lines files: `<dir>/<YYYY-MM-DD>.jsonl`, one envelope per line, the
/// file chosen by the event's UTC date. Sequence numbers continue across files and restarts.
///
/// Each append is a single `write` of a complete line to a file opened in append mode, so a
/// reader never sees a partial event from a live process. Appends are serialized by a lock.
#[derive(Debug)]
pub struct JsonlEventLog {
    dir: PathBuf,
    next_seq: Mutex<u64>,
}

impl JsonlEventLog {
    /// Open (creating if needed) the log in `dir`, resuming after the highest stored sequence.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, BusError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|source| BusError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut last = 0;
        for (_, path) in day_files(&dir)? {
            for envelope in read_file(&path)? {
                last = last.max(envelope.seq);
            }
        }
        Ok(Self {
            dir,
            next_seq: Mutex::new(last + 1),
        })
    }

    /// File holding events dated `date`.
    pub fn file_for(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!("{date}.jsonl"))
    }

    fn append_line(&self, event: &Event) -> Result<Envelope, BusError> {
        let mut next_seq = self.next_seq.lock().unwrap_or_else(PoisonError::into_inner);
        let envelope = Envelope {
            seq: *next_seq,
            event: event.clone(),
        };
        let mut line = serde_json::to_string(&envelope).map_err(BusError::Serialize)?;
        line.push('\n');

        let path = self.file_for(event.ts().date_naive());
        let io = |source| BusError::Io {
            path: path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        file.write_all(line.as_bytes()).map_err(io)?;
        *next_seq += 1;
        drop(next_seq);
        Ok(envelope)
    }

    fn read_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<Envelope>, BusError> {
        let (first, last) = (start.date_naive(), end.date_naive());
        let mut events = Vec::new();
        for (date, path) in day_files(&self.dir)? {
            if date < first || date > last {
                continue;
            }
            events.extend(
                read_file(&path)?
                    .into_iter()
                    .filter(|e| e.event.ts() >= start && e.event.ts() < end),
            );
        }
        events.sort_by_key(|e| e.seq);
        Ok(events)
    }
}

impl EventLog for JsonlEventLog {
    fn append(&self, event: &Event) -> Result<Envelope, PortError> {
        self.append_line(event)
            .map_err(|e| PortError::adapter("jsonl event log", e))
    }

    fn read(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Envelope>, PortError> {
        self.read_range(start, end)
            .map_err(|e| PortError::adapter("jsonl event log", e))
    }
}

/// `(date, path)` of every `YYYY-MM-DD.jsonl` file in `dir`, oldest first. Other files are ignored.
fn day_files(dir: &Path) -> Result<Vec<(NaiveDate, PathBuf)>, BusError> {
    let io = |source| BusError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).map_err(io)? {
        let path = entry.map_err(io)?.path();
        let is_jsonl = path.extension().is_some_and(|ext| ext == "jsonl");
        let date = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok());
        if let (true, Some(date)) = (is_jsonl, date) {
            files.push((date, path));
        }
    }
    files.sort();
    Ok(files)
}

fn read_file(path: &Path) -> Result<Vec<Envelope>, BusError> {
    let io = |source| BusError::Io {
        path: path.to_path_buf(),
        source,
    };
    let reader = BufReader::new(File::open(path).map_err(io)?);
    let mut events = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(io)?;
        if line.trim().is_empty() {
            continue;
        }
        let envelope = serde_json::from_str(&line).map_err(|source| BusError::Corrupt {
            path: path.to_path_buf(),
            line: index + 1,
            source,
        })?;
        events.push(envelope);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quorum-log-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir); // stale leftovers from an earlier run, if any
        dir
    }

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn halt(hours: i64) -> Event {
        Event::RiskHalt {
            ts: at(hours),
            equity: 1.0,
        }
    }

    #[test]
    fn sequence_numbers_increase_and_survive_reopening() {
        let dir = temp_dir("reopen");
        let log = JsonlEventLog::open(&dir).expect("open");
        assert_eq!(log.append(&halt(0)).expect("append").seq, 1);
        assert_eq!(log.append(&halt(1)).expect("append").seq, 2);
        drop(log);

        let reopened = JsonlEventLog::open(&dir).expect("reopen");
        assert_eq!(reopened.append(&halt(2)).expect("append").seq, 3);
    }

    #[test]
    fn events_rotate_into_one_file_per_utc_day() {
        let dir = temp_dir("rotate");
        let log = JsonlEventLog::open(&dir).expect("open");
        for hour in [1, 23, 25, 49] {
            log.append(&halt(hour)).expect("append");
        }
        let days: Vec<String> = day_files(&dir)
            .expect("list")
            .iter()
            .map(|(date, _)| date.to_string())
            .collect();
        assert_eq!(days, vec!["1970-01-01", "1970-01-02", "1970-01-03"]);
    }

    #[test]
    fn read_returns_a_half_open_range_across_days_in_sequence_order() {
        let dir = temp_dir("range");
        let log = JsonlEventLog::open(&dir).expect("open");
        for hour in [30, 5, 20, 40] {
            log.append(&halt(hour)).expect("append");
        }
        let read = log.read(at(5), at(40)).expect("read");
        let seqs: Vec<u64> = read.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn corrupt_lines_are_reported_with_file_and_line() {
        let dir = temp_dir("corrupt");
        let log = JsonlEventLog::open(&dir).expect("open");
        log.append(&halt(1)).expect("append");
        let path = log.file_for(at(1).date_naive());
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open file");
        file.write_all(b"{not json}\n").expect("write");

        let err = JsonlEventLog::open(&dir).expect_err("corrupt");
        assert!(matches!(err, BusError::Corrupt { line: 2, .. }), "{err}");
    }

    #[test]
    fn unrelated_files_are_ignored() {
        let dir = temp_dir("unrelated");
        fs::create_dir_all(&dir).expect("create dir");
        fs::write(dir.join("notes.txt"), "hello").expect("write");
        fs::write(dir.join("latest.jsonl"), "garbage").expect("write");
        let log = JsonlEventLog::open(&dir).expect("open");
        assert_eq!(log.append(&halt(0)).expect("append").seq, 1);
    }
}
