//! Reports to the operator (AG-3c, migration `0010_agent_reports.sql`).

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use trader_core::agents::{AgentReport, NewReport};

use crate::{JournalError, SqliteJournal, format_ts, parse_ts};

fn report_from(row: &SqliteRow) -> Result<AgentReport, JournalError> {
    let ts = |name: &str| -> Result<DateTime<Utc>, JournalError> {
        let text: String = row.try_get(name)?;
        parse_ts(&text)
    };
    let facts: String = row.try_get("facts")?;
    Ok(AgentReport {
        id: row.try_get("id")?,
        agent: row.try_get("agent")?,
        created_at: ts("created_at")?,
        period_from: ts("period_from")?,
        period_to: ts("period_to")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        facts: serde_json::from_str(&facts)
            .map_err(|e| JournalError::Invalid(format!("report facts: {e}")))?,
    })
}

impl SqliteJournal {
    pub(crate) async fn insert_report(
        &self,
        agent: &str,
        report: &NewReport,
        ts: DateTime<Utc>,
    ) -> Result<AgentReport, JournalError> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO agent_reports (agent, created_at, period_from, period_to, title, body, \
             facts) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(agent)
        .bind(format_ts(ts))
        .bind(format_ts(report.period_from))
        .bind(format_ts(report.period_to))
        .bind(report.title.as_str())
        .bind(report.body.as_str())
        .bind(report.facts.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(AgentReport {
            id,
            agent: agent.into(),
            created_at: ts,
            period_from: report.period_from,
            period_to: report.period_to,
            title: report.title.clone(),
            body: report.body.clone(),
            facts: report.facts.clone(),
        })
    }

    pub(crate) async fn select_reports(
        &self,
        agent: Option<&str>,
        max: usize,
    ) -> Result<Vec<AgentReport>, JournalError> {
        let rows = sqlx::query(
            "SELECT id, agent, created_at, period_from, period_to, title, body, facts \
             FROM agent_reports WHERE ?1 IS NULL OR agent = ?1 ORDER BY id DESC LIMIT ?2",
        )
        .bind(agent)
        .bind(i64::try_from(max).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(report_from).collect()
    }
}
