//! Risk-reducing position controls by watcher agents (T-5, migration
//! `0012_position_controls.sql`).

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use trader_core::InstrumentId;
use trader_core::suggestions::{ControlAction, ControlStatus, PositionControl};

use crate::{JournalError, SqliteJournal, format_ts, parse_ts};

macro_rules! columns {
    () => {
        "id, author, created_at, instrument, action, stop, reason, status, note, session_id, \
         applied_after, applied_at"
    };
}

fn invalid(what: &str, err: impl std::fmt::Display) -> JournalError {
    JournalError::Invalid(format!("position control {what}: {err}"))
}

fn control_from(row: &SqliteRow) -> Result<PositionControl, JournalError> {
    let ts = |name: &str| -> Result<Option<DateTime<Utc>>, JournalError> {
        let text: Option<String> = row.try_get(name)?;
        text.as_deref().map(parse_ts).transpose()
    };
    let created: String = row.try_get("created_at")?;
    let instrument: String = row.try_get("instrument")?;
    let action: String = row.try_get("action")?;
    let stop: Option<f64> = row.try_get("stop")?;
    let status: String = row.try_get("status")?;
    Ok(PositionControl {
        id: row.try_get("id")?,
        author: row.try_get("author")?,
        created_at: parse_ts(&created)?,
        instrument: InstrumentId::from_str(&instrument).map_err(|e| invalid("instrument", e))?,
        action: match (action.as_str(), stop) {
            ("tighten_stop", Some(stop)) => ControlAction::TightenStop { stop },
            ("close", _) => ControlAction::Close,
            ("reduce_exposure", Some(scale)) => ControlAction::ReduceExposure { scale },
            (other, _) => return Err(invalid("action", other)),
        },
        reason: row.try_get("reason")?,
        status: status
            .parse::<ControlStatus>()
            .map_err(|e| invalid("status", e))?,
        note: row.try_get("note")?,
        session_id: row.try_get("session_id")?,
        applied_after: ts("applied_after")?,
        applied_at: ts("applied_at")?,
    })
}

impl SqliteJournal {
    pub(crate) async fn insert_control(
        &self,
        author: &str,
        instrument: &InstrumentId,
        action: ControlAction,
        reason: &str,
        ts: DateTime<Utc>,
    ) -> Result<PositionControl, JournalError> {
        let stop = match action {
            ControlAction::TightenStop { stop } => Some(stop),
            ControlAction::ReduceExposure { scale } => Some(scale),
            ControlAction::Close => None,
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO position_controls (author, created_at, instrument, action, stop, reason, \
             status) VALUES (?, ?, ?, ?, ?, ?, 'pending') RETURNING id",
        )
        .bind(author)
        .bind(format_ts(ts))
        .bind(instrument.to_string())
        .bind(action.as_str())
        .bind(stop)
        .bind(reason)
        .fetch_one(&self.pool)
        .await?;
        Ok(PositionControl {
            id,
            author: author.into(),
            created_at: ts,
            instrument: instrument.clone(),
            action,
            reason: reason.into(),
            status: ControlStatus::Pending,
            note: None,
            session_id: None,
            applied_after: None,
            applied_at: None,
        })
    }

    pub(crate) async fn select_controls_by(
        &self,
        status: Option<ControlStatus>,
        max: usize,
    ) -> Result<Vec<PositionControl>, JournalError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM position_controls WHERE ?1 IS NULL OR status = ?1 ORDER BY id DESC LIMIT ?2"
        ))
        .bind(status.map(ControlStatus::as_str))
        .bind(i64::try_from(max).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(control_from).collect()
    }

    pub(crate) async fn select_session_controls(
        &self,
        session: &str,
    ) -> Result<Vec<PositionControl>, JournalError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM position_controls WHERE session_id = ? AND status = 'applied' ORDER BY id"
        ))
        .bind(session)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(control_from).collect()
    }

    pub(crate) async fn update_control_row(&self, c: &PositionControl) -> Result<(), JournalError> {
        let updated = sqlx::query(
            "UPDATE position_controls SET status = ?, note = ?, session_id = ?, applied_after = ?, \
             applied_at = ? WHERE id = ?",
        )
        .bind(c.status.as_str())
        .bind(c.note.as_deref())
        .bind(c.session_id.as_deref())
        .bind(c.applied_after.map(format_ts))
        .bind(c.applied_at.map(format_ts))
        .bind(c.id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(invalid("update", format!("no control {}", c.id)));
        }
        Ok(())
    }
}
