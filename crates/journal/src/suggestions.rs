//! Position suggestions (T-3, migrations `0011_suggestions.sql`, `0013_suggestion_decided_by.sql`).

use std::str::FromStr;

use chrono::{DateTime, Utc};
use quorum_core::InstrumentId;
use quorum_core::suggestions::{NewSuggestion, Side, Status, Suggestion};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::{JournalError, SqliteJournal, format_ts, parse_ts};

macro_rules! columns {
    () => {
        "id, author, created_at, expires_at, instrument, side, size, stop_pct, take_profit_pct, \
         price, rationale, evidence, confidence, status, decided_at, decision_reason, \
         session_id, applied_after, executed_at, decided_by"
    };
}

fn invalid(what: &str, err: impl std::fmt::Display) -> JournalError {
    JournalError::Invalid(format!("suggestion {what}: {err}"))
}

fn suggestion_from(row: &SqliteRow) -> Result<Suggestion, JournalError> {
    let ts = |name: &str| -> Result<Option<DateTime<Utc>>, JournalError> {
        let text: Option<String> = row.try_get(name)?;
        text.as_deref().map(parse_ts).transpose()
    };
    let created: String = row.try_get("created_at")?;
    let expires: String = row.try_get("expires_at")?;
    let instrument: String = row.try_get("instrument")?;
    let side: String = row.try_get("side")?;
    let status: String = row.try_get("status")?;
    let evidence: String = row.try_get("evidence")?;
    Ok(Suggestion {
        id: row.try_get("id")?,
        author: row.try_get("author")?,
        created_at: parse_ts(&created)?,
        expires_at: parse_ts(&expires)?,
        instrument: InstrumentId::from_str(&instrument).map_err(|e| invalid("instrument", e))?,
        side: side.parse::<Side>().map_err(|e| invalid("side", e))?,
        size: row.try_get("size")?,
        stop_pct: row.try_get("stop_pct")?,
        take_profit_pct: row.try_get("take_profit_pct")?,
        price: row.try_get("price")?,
        rationale: row.try_get("rationale")?,
        evidence: serde_json::from_str(&evidence).map_err(|e| invalid("evidence", e))?,
        confidence: row.try_get("confidence")?,
        status: status.parse::<Status>().map_err(|e| invalid("status", e))?,
        decided_at: ts("decided_at")?,
        decision_reason: row.try_get("decision_reason")?,
        session_id: row.try_get("session_id")?,
        applied_after: ts("applied_after")?,
        executed_at: ts("executed_at")?,
        decided_by: row.try_get("decided_by")?,
    })
}

impl SqliteJournal {
    pub(crate) async fn insert_suggestion(
        &self,
        author: &str,
        new: &NewSuggestion,
        price: Option<f64>,
        ts: DateTime<Utc>,
    ) -> Result<Suggestion, JournalError> {
        let evidence = serde_json::to_string(&new.evidence).map_err(|e| invalid("evidence", e))?;
        let size = if new.side == Side::Close {
            0.0
        } else {
            new.size
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO suggestions (author, created_at, expires_at, instrument, side, size, \
             stop_pct, take_profit_pct, price, rationale, evidence, confidence, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending') RETURNING id",
        )
        .bind(author)
        .bind(format_ts(ts))
        .bind(format_ts(ts + new.expires_in))
        .bind(new.instrument.to_string())
        .bind(new.side.as_str())
        .bind(size)
        .bind(new.stop_pct)
        .bind(new.take_profit_pct)
        .bind(price)
        .bind(new.rationale.trim())
        .bind(evidence)
        .bind(new.confidence)
        .fetch_one(&self.pool)
        .await?;
        self.suggestion_by_id(id)
            .await?
            .ok_or_else(|| invalid("insert", "row vanished"))
    }

    pub(crate) async fn suggestion_by_id(
        &self,
        id: i64,
    ) -> Result<Option<Suggestion>, JournalError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM suggestions WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(suggestion_from).transpose()
    }

    pub(crate) async fn select_suggestions(
        &self,
        status: Option<Status>,
        max: usize,
    ) -> Result<Vec<Suggestion>, JournalError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM suggestions WHERE ?1 IS NULL OR status = ?1 ORDER BY id DESC LIMIT ?2"
        ))
        .bind(status.map(Status::as_str))
        .bind(i64::try_from(max).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(suggestion_from).collect()
    }

    pub(crate) async fn select_session_suggestions(
        &self,
        session: &str,
    ) -> Result<Vec<Suggestion>, JournalError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM suggestions WHERE session_id = ? AND status = 'executed' ORDER BY id"
        ))
        .bind(session)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(suggestion_from).collect()
    }

    pub(crate) async fn update_suggestion_row(&self, s: &Suggestion) -> Result<(), JournalError> {
        let updated = sqlx::query(
            "UPDATE suggestions SET status = ?, decided_at = ?, decision_reason = ?, \
             session_id = ?, applied_after = ?, executed_at = ?, decided_by = ? WHERE id = ?",
        )
        .bind(s.status.as_str())
        .bind(s.decided_at.map(format_ts))
        .bind(s.decision_reason.as_deref())
        .bind(s.session_id.as_deref())
        .bind(s.applied_after.map(format_ts))
        .bind(s.executed_at.map(format_ts))
        .bind(s.decided_by.as_deref())
        .bind(s.id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(invalid("update", format!("no suggestion {}", s.id)));
        }
        Ok(())
    }
}
