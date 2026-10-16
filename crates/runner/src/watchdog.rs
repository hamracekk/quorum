//! The watchdog's decisions (`quorum watchdog`, run by launchd every few minutes).
//!
//! What is wrong now, what to alert, what recovered. Pure, so it is tested without clocks or
//! files; the CLI gathers the observations and sends the alerts.
//!
//! An incident alerts once when it opens and once when it clears. After a gap since the last
//! check longer than [`SLEEP_GAP_SECS`] (the laptop slept), the check only records the time:
//! the services need a moment after waking to write a fresh heartbeat.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use quorum_core::Severity;
use serde::{Deserialize, Serialize};

/// A paper heartbeat older than this, while the session says it is running, is an incident.
pub const PAPER_STALE_SECS: i64 = 300;
/// Recorder files untouched for this long (book snapshots come every 5 minutes) is an incident.
pub const RECORDER_STALE_SECS: i64 = 900;
/// A longer gap since the last check means the machine slept: skip this check.
pub const SLEEP_GAP_SECS: i64 = 600;

/// A slot's `state.json` as seen: its status and heartbeat.
pub type SlotSeen = (String, DateTime<Utc>);

/// What the watchdog saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub now: DateTime<Utc>,
    /// Per slot: `(status in state.json, heartbeat)`; `None` when there is no state yet.
    pub slots: Vec<(String, Option<SlotSeen>)>,
    /// When the recorder last wrote a file; `None` when it has not recorded yet.
    pub recorder_last_write: Option<DateTime<Utc>>,
    /// Whether the risk process answered; `None` when it was not checked.
    pub risk_answered: Option<bool>,
}

/// What the watchdog remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchState {
    pub last_run: Option<DateTime<Utc>>,
    /// Open incidents: key → since when.
    pub open: BTreeMap<String, DateTime<Utc>>,
}

/// One alert to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub severity: Severity,
    pub title: String,
    pub body: String,
}

fn incidents(o: &Observation) -> BTreeMap<String, (Severity, String)> {
    let mut found = BTreeMap::new();
    for (slot, state) in &o.slots {
        let Some((status, heartbeat)) = state else {
            continue;
        };
        let age = (o.now - *heartbeat).num_seconds();
        if status == "failed" {
            found.insert(
                format!("paper {slot} failed"),
                (
                    Severity::Critical,
                    "the session stopped on an error; `quorum paper status` shows it".to_owned(),
                ),
            );
        } else if status == "running" && age > PAPER_STALE_SECS {
            found.insert(
                format!("paper {slot} not responding"),
                (
                    Severity::Critical,
                    format!(
                        "last heartbeat {} min ago: the process is gone or hung",
                        age / 60
                    ),
                ),
            );
        }
    }
    if o.risk_answered == Some(false) {
        found.insert(
            "risk process down".to_owned(),
            (
                Severity::Critical,
                "it does not answer: paper slots refuse every order that adds risk until it is back \
                 (`launchctl kickstart gui/$(id -u)/com.quorum.risk`)"
                    .to_owned(),
            ),
        );
    }
    if let Some(last) = o.recorder_last_write {
        let age = (o.now - last).num_seconds();
        if age > RECORDER_STALE_SECS {
            found.insert(
                "recorder stale".to_owned(),
                (
                    Severity::Warning,
                    format!("no recorder write for {} min: data is being lost", age / 60),
                ),
            );
        }
    }
    found
}

/// Decides this run's alerts and the state to keep.
#[must_use]
pub fn evaluate(o: &Observation, state: &WatchState) -> (Vec<Alert>, WatchState) {
    let slept = state
        .last_run
        .is_some_and(|last| (o.now - last).num_seconds() > SLEEP_GAP_SECS);
    if slept {
        let kept = WatchState {
            last_run: Some(o.now),
            open: state.open.clone(),
        };
        return (Vec::new(), kept);
    }
    let now_open = incidents(o);
    let mut alerts = Vec::new();
    let mut open = BTreeMap::new();
    for (key, (severity, body)) in &now_open {
        let since = state.open.get(key).copied();
        if since.is_none() {
            alerts.push(Alert {
                severity: *severity,
                title: format!("watchdog · {key}"),
                body: body.clone(),
            });
        }
        open.insert(key.clone(), since.unwrap_or(o.now));
    }
    for (key, since) in &state.open {
        if !now_open.contains_key(key) {
            alerts.push(Alert {
                severity: Severity::Info,
                title: format!("watchdog · recovered: {key}"),
                body: format!("after {} min", (o.now - *since).num_minutes()),
            });
        }
    }
    (
        alerts,
        WatchState {
            last_run: Some(o.now),
            open,
        },
    )
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn t(min: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0)
            .single()
            .expect("valid")
            + Duration::minutes(min)
    }

    fn seen(now: i64, heartbeat: i64, status: &str, recorder: i64) -> Observation {
        Observation {
            now: t(now),
            slots: vec![("crypto-spot".into(), Some((status.into(), t(heartbeat))))],
            recorder_last_write: Some(t(recorder)),
            risk_answered: Some(true),
        }
    }

    #[test]
    fn healthy_services_raise_nothing() {
        let (alerts, state) = evaluate(&seen(10, 10, "running", 8), &WatchState::default());
        assert!(alerts.is_empty());
        assert!(state.open.is_empty());
        assert_eq!(state.last_run, Some(t(10)));
    }

    #[test]
    fn a_stale_heartbeat_alerts_once_then_recovers_once() {
        let start = WatchState {
            last_run: Some(t(5)),
            open: BTreeMap::new(),
        };
        let (first, state) = evaluate(&seen(10, 2, "running", 9), &start);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].severity, Severity::Critical);
        assert!(first[0].title.contains("not responding"));
        let (second, state) = evaluate(&seen(15, 2, "running", 14), &state);
        assert!(second.is_empty(), "still open: no repeat");
        let (third, state) = evaluate(&seen(20, 20, "running", 19), &state);
        assert_eq!(third.len(), 1);
        assert!(third[0].title.contains("recovered"));
        assert!(state.open.is_empty());
    }

    #[test]
    fn a_failed_session_and_a_stale_recorder_are_incidents() {
        let start = WatchState {
            last_run: Some(t(55)),
            open: BTreeMap::new(),
        };
        let (alerts, _) = evaluate(&seen(60, 59, "failed", 30), &start);
        let titles: Vec<&str> = alerts.iter().map(|a| a.title.as_str()).collect();
        assert!(titles.iter().any(|t| t.contains("failed")), "{titles:?}");
        assert!(
            titles.iter().any(|t| t.contains("recorder stale")),
            "{titles:?}"
        );
    }

    #[test]
    fn a_silent_risk_process_alerts_once_and_recovers_once() {
        let start = WatchState {
            last_run: Some(t(5)),
            open: BTreeMap::new(),
        };
        let down = Observation {
            risk_answered: Some(false),
            ..seen(10, 10, "running", 9)
        };
        let (first, state) = evaluate(&down, &start);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].severity, Severity::Critical);
        assert!(first[0].title.contains("risk process down"));
        let (second, state) = evaluate(&seen(15, 15, "running", 14), &state);
        assert!(second[0].title.contains("recovered"));
        assert!(state.open.is_empty());
    }

    #[test]
    fn a_stopped_session_is_intentional_and_quiet() {
        let (alerts, _) = evaluate(&seen(60, 0, "stopped", 59), &WatchState::default());
        assert!(alerts.is_empty());
    }

    #[test]
    fn right_after_a_sleep_the_check_only_records_the_time() {
        let before = WatchState {
            last_run: Some(t(0)),
            open: BTreeMap::new(),
        };
        let (alerts, state) = evaluate(&seen(120, 1, "running", 1), &before);
        assert!(alerts.is_empty(), "stale only because the machine slept");
        assert_eq!(state.last_run, Some(t(120)));
    }
}
