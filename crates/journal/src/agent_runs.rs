//! Runs of internal agents (AG-3b, migration `0009_agent_runs.sql`).

use chrono::{DateTime, Utc};
use quorum_core::agents::AgentRun;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::{JournalError, SqliteJournal, format_ts, parse_ts};

fn tokens(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn run_from(row: &SqliteRow) -> Result<AgentRun, JournalError> {
    let started: String = row.try_get("started_at")?;
    let finished: Option<String> = row.try_get("finished_at")?;
    let count = |name: &str| -> Result<u64, JournalError> {
        let value: i64 = row.try_get(name)?;
        Ok(u64::try_from(value).unwrap_or(0))
    };
    Ok(AgentRun {
        id: row.try_get("id")?,
        agent: row.try_get("agent")?,
        task: row.try_get("task")?,
        trigger: row.try_get("trigger")?,
        started_at: parse_ts(&started)?,
        finished_at: finished.as_deref().map(parse_ts).transpose()?,
        status: row.try_get("status")?,
        steps: u32::try_from(count("steps")?).unwrap_or(u32::MAX),
        input_tokens: count("input_tokens")?,
        output_tokens: count("output_tokens")?,
        summary: row.try_get("summary")?,
        error: row.try_get("error")?,
    })
}

impl SqliteJournal {
    pub(crate) async fn insert_agent_run(
        &self,
        agent: &str,
        task: &str,
        trigger: &str,
        ts: DateTime<Utc>,
    ) -> Result<AgentRun, JournalError> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO agent_runs (agent, task, trigger, started_at, status) \
             VALUES (?, ?, ?, ?, 'running') RETURNING id",
        )
        .bind(agent)
        .bind(task)
        .bind(trigger)
        .bind(format_ts(ts))
        .fetch_one(&self.pool)
        .await?;
        Ok(AgentRun {
            id,
            agent: agent.into(),
            task: task.into(),
            trigger: trigger.into(),
            started_at: ts,
            finished_at: None,
            status: "running".into(),
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            summary: None,
            error: None,
        })
    }

    pub(crate) async fn update_agent_run(&self, run: &AgentRun) -> Result<(), JournalError> {
        let updated = sqlx::query(
            "UPDATE agent_runs SET finished_at = ?, status = ?, steps = ?, input_tokens = ?, \
             output_tokens = ?, summary = ?, error = ? WHERE id = ?",
        )
        .bind(run.finished_at.map(format_ts))
        .bind(run.status.as_str())
        .bind(i64::from(run.steps))
        .bind(tokens(run.input_tokens))
        .bind(tokens(run.output_tokens))
        .bind(run.summary.as_deref())
        .bind(run.error.as_deref())
        .bind(run.id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(JournalError::Invalid(format!("no agent run {}", run.id)));
        }
        Ok(())
    }

    pub(crate) async fn select_agent_runs(
        &self,
        agent: Option<&str>,
        max: usize,
    ) -> Result<Vec<AgentRun>, JournalError> {
        let rows = sqlx::query(
            "SELECT id, agent, task, trigger, started_at, finished_at, status, steps, \
             input_tokens, output_tokens, summary, error FROM agent_runs \
             WHERE ?1 IS NULL OR agent = ?1 ORDER BY id DESC LIMIT ?2",
        )
        .bind(agent)
        .bind(i64::try_from(max).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(run_from).collect()
    }

    pub(crate) async fn sum_agent_tokens(
        &self,
        agent: &str,
        since: DateTime<Utc>,
    ) -> Result<u64, JournalError> {
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT SUM(input_tokens + output_tokens) FROM agent_runs \
             WHERE agent = ? AND started_at >= ?",
        )
        .bind(agent)
        .bind(format_ts(since))
        .fetch_one(&self.pool)
        .await?;
        Ok(total.and_then(|t| u64::try_from(t).ok()).unwrap_or(0))
    }
}
