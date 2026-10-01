//! `AgentStore` on the journal's SQLite database (migration `0004_agents.sql`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use trader_core::agents::{
    AgentControl, AgentMessage, EVERYONE, Finding, FindingQuery, FindingStatus, MessageQuery,
    NewFinding, NewMessage, OPERATOR, Verdict,
};
use trader_core::{AgentStore, PortError};

use crate::{JournalError, SqliteJournal, format_ts, parse_ts, port};

fn to_json(value: &impl Serialize) -> Result<String, JournalError> {
    serde_json::to_string(value).map_err(|e| JournalError::Corrupt(format!("encode: {e}")))
}

fn from_json<T: DeserializeOwned>(column: &str, text: &str) -> Result<T, JournalError> {
    serde_json::from_str(text).map_err(|e| JournalError::Corrupt(format!("{column}: {e}")))
}

fn finding_from(row: &SqliteRow) -> Result<Finding, JournalError> {
    let text = |name: &str| -> Result<String, JournalError> { Ok(row.try_get(name)?) };
    let hypothesis: Option<String> = row.try_get("hypothesis")?;
    Ok(Finding {
        id: row.try_get("id")?,
        agent: text("agent")?,
        role: text("role")?,
        created_at: parse_ts(&text("created_at")?)?,
        updated_at: parse_ts(&text("updated_at")?)?,
        kind: text("kind")?
            .parse()
            .map_err(|e: trader_core::agents::AgentError| JournalError::Corrupt(e.0))?,
        instruments: from_json("instruments", &text("instruments")?)?,
        claim: text("claim")?,
        hypothesis: hypothesis
            .map(|h| from_json("hypothesis", &h))
            .transpose()?,
        evidence: from_json("evidence", &text("evidence")?)?,
        status: text("status")?
            .parse()
            .map_err(|e: trader_core::agents::AgentError| JournalError::Corrupt(e.0))?,
        supersedes: row.try_get("supersedes")?,
        superseded_by: row.try_get("superseded_by")?,
        confidence: row.try_get("confidence")?,
        verdict: row
            .try_get::<Option<String>, _>("verdict")?
            .map(|v| from_json("verdict", &v))
            .transpose()?,
    })
}

fn message_from(row: &SqliteRow) -> Result<AgentMessage, JournalError> {
    let ts: String = row.try_get("ts")?;
    let refs: String = row.try_get("refs")?;
    Ok(AgentMessage {
        id: row.try_get("id")?,
        ts: parse_ts(&ts)?,
        thread: row.try_get("thread")?,
        sender: row.try_get("sender")?,
        recipient: row.try_get("recipient")?,
        body: row.try_get("body")?,
        refs: from_json("refs", &refs)?,
    })
}

/// Plain words → an FTS5 query matching all of them, with no operators interpreted.
/// `None` when the text has no searchable word.
fn fts_query(text: &str) -> Option<String> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(16)
        .map(|w| format!("\"{w}\""))
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl SqliteJournal {
    async fn finding_by_id(&self, id: i64) -> Result<Option<Finding>, JournalError> {
        let row = sqlx::query(
            "SELECT f.id, f.agent, f.role, f.created_at, f.updated_at, f.kind, f.instruments, \
             f.claim, f.hypothesis, f.evidence, f.status, f.supersedes, f.confidence, f.verdict, \
             (SELECT MIN(n.id) FROM findings n WHERE n.supersedes = f.id) AS superseded_by \
             FROM findings f WHERE f.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(finding_from).transpose()
    }

    async fn insert_finding(
        &self,
        agent: &str,
        role: &str,
        finding: &NewFinding,
        ts: DateTime<Utc>,
    ) -> Result<Finding, JournalError> {
        let mut tx = self.pool.begin().await?;
        if let Some(old) = finding.supersedes {
            let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM findings WHERE id = ?")
                .bind(old)
                .fetch_optional(&mut *tx)
                .await?;
            if exists.is_none() {
                return Err(JournalError::Invalid(format!(
                    "finding {old} does not exist, so it cannot be superseded"
                )));
            }
        }
        let stamp = format_ts(ts);
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO findings (agent, role, created_at, updated_at, kind, instruments, claim, \
             hypothesis, evidence, supersedes, confidence) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(agent)
        .bind(role)
        .bind(&stamp)
        .bind(&stamp)
        .bind(finding.kind.as_str())
        .bind(to_json(&finding.instruments)?)
        .bind(finding.claim.trim())
        .bind(finding.hypothesis.as_ref().map(to_json).transpose()?)
        .bind(to_json(&finding.evidence)?)
        .bind(finding.supersedes)
        .bind(finding.confidence)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        self.finding_by_id(id)
            .await?
            .ok_or_else(|| JournalError::Corrupt(format!("finding {id} vanished after insert")))
    }

    async fn store_verdict(&self, id: i64, verdict: &Verdict) -> Result<Finding, JournalError> {
        let mut tx = self.pool.begin().await?;
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM findings WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let status =
            status.ok_or_else(|| JournalError::Invalid(format!("finding {id} does not exist")))?;
        if status == FindingStatus::Retired.as_str() && verdict.by != OPERATOR {
            return Err(JournalError::Invalid(format!(
                "finding {id} is retired; only the operator can change it"
            )));
        }
        let text = to_json(verdict)?;
        let stamp = format_ts(verdict.evaluated_at);
        sqlx::query(
            "INSERT INTO finding_evaluations (finding_id, ts, by_whom, status, verdict) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&stamp)
        .bind(&verdict.by)
        .bind(verdict.outcome.as_str())
        .bind(&text)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE findings SET status = ?, verdict = ?, updated_at = ? WHERE id = ?")
            .bind(verdict.outcome.as_str())
            .bind(&text)
            .bind(&stamp)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.finding_by_id(id)
            .await?
            .ok_or_else(|| JournalError::Corrupt(format!("finding {id} vanished")))
    }

    async fn select_findings(&self, query: &FindingQuery) -> Result<Vec<Finding>, JournalError> {
        let status = query.status.map(FindingStatus::as_str);
        let rows = match query.text.as_deref().map(fts_query) {
            // Text given but nothing searchable in it: nothing can match.
            Some(None) => return Ok(Vec::new()),
            Some(Some(fts)) => {
                sqlx::query(
                    "SELECT f.id, f.agent, f.role, f.created_at, f.updated_at, f.kind, \
                     f.instruments, f.claim, f.hypothesis, f.evidence, f.status, f.supersedes, f.confidence, f.verdict, \
                     (SELECT MIN(n.id) FROM findings n WHERE n.supersedes = f.id) AS superseded_by \
                     FROM findings f \
                     WHERE (?1 IS NULL OR f.agent = ?1) AND (?2 IS NULL OR f.status = ?2) \
                     AND f.id IN (SELECT rowid FROM findings_fts WHERE findings_fts MATCH ?4) \
                     ORDER BY f.id DESC LIMIT ?3",
                )
                .bind(query.agent.as_deref())
                .bind(status)
                .bind(limit(query.limit))
                .bind(fts)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT f.id, f.agent, f.role, f.created_at, f.updated_at, f.kind, \
                     f.instruments, f.claim, f.hypothesis, f.evidence, f.status, f.supersedes, f.confidence, f.verdict, \
                     (SELECT MIN(n.id) FROM findings n WHERE n.supersedes = f.id) AS superseded_by \
                     FROM findings f \
                     WHERE (?1 IS NULL OR f.agent = ?1) AND (?2 IS NULL OR f.status = ?2) \
                     ORDER BY f.id DESC LIMIT ?3",
                )
                .bind(query.agent.as_deref())
                .bind(status)
                .bind(limit(query.limit))
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(finding_from).collect()
    }

    async fn insert_message(
        &self,
        sender: &str,
        message: &NewMessage,
        ts: DateTime<Utc>,
    ) -> Result<AgentMessage, JournalError> {
        let mut tx = self.pool.begin().await?;
        let thread = match message.thread {
            Some(any_id) => Some(
                sqlx::query_scalar::<_, i64>("SELECT thread FROM agent_messages WHERE id = ?")
                    .bind(any_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| {
                        JournalError::Invalid(format!("thread {any_id} does not exist"))
                    })?,
            ),
            None => None,
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO agent_messages (ts, thread, sender, recipient, body, refs) \
             VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(format_ts(ts))
        .bind(thread.unwrap_or(0))
        .bind(sender)
        .bind(&message.recipient)
        .bind(message.body.trim())
        .bind(to_json(&message.refs)?)
        .fetch_one(&mut *tx)
        .await?;
        if thread.is_none() {
            sqlx::query("UPDATE agent_messages SET thread = id WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        let row = sqlx::query(
            "SELECT id, ts, thread, sender, recipient, body, refs FROM agent_messages WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        message_from(&row)
    }

    async fn select_messages(
        &self,
        query: &MessageQuery,
    ) -> Result<Vec<AgentMessage>, JournalError> {
        let rows = sqlx::query(
            "SELECT id, ts, thread, sender, recipient, body, refs FROM ( \
               SELECT * FROM agent_messages \
               WHERE (?1 IS NULL OR thread = (SELECT t.thread FROM agent_messages t WHERE t.id = ?1)) \
               AND (?2 IS NULL OR sender = ?2 OR recipient = ?2 OR recipient = ?4) \
               ORDER BY id DESC LIMIT ?3 \
             ) ORDER BY id",
        )
        .bind(query.thread)
        .bind(query.participant.as_deref())
        .bind(limit(query.limit))
        .bind(EVERYONE)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(message_from).collect()
    }

    async fn select_unread(
        &self,
        reader: &str,
        max: usize,
        mark: bool,
    ) -> Result<Vec<AgentMessage>, JournalError> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT m.id, m.ts, m.thread, m.sender, m.recipient, m.body, m.refs \
             FROM agent_messages m \
             WHERE (m.recipient = ?1 OR m.recipient = ?3) AND m.sender != ?1 \
             AND NOT EXISTS (SELECT 1 FROM agent_message_reads r \
                             WHERE r.message_id = m.id AND r.reader = ?1) \
             ORDER BY m.id LIMIT ?2",
        )
        .bind(reader)
        .bind(limit(max))
        .bind(EVERYONE)
        .fetch_all(&mut *tx)
        .await?;
        let messages = rows
            .iter()
            .map(message_from)
            .collect::<Result<Vec<_>, _>>()?;
        if mark {
            let now = format_ts(Utc::now());
            for message in &messages {
                sqlx::query(
                    "INSERT OR IGNORE INTO agent_message_reads (message_id, reader, read_at) \
                     VALUES (?, ?, ?)",
                )
                .bind(message.id)
                .bind(reader)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(messages)
    }

    async fn count_unread(&self, reader: &str) -> Result<usize, JournalError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_messages m \
             WHERE (m.recipient = ?1 OR m.recipient = ?2) AND m.sender != ?1 \
             AND NOT EXISTS (SELECT 1 FROM agent_message_reads r \
                             WHERE r.message_id = m.id AND r.reader = ?1)",
        )
        .bind(reader)
        .bind(EVERYONE)
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    async fn upsert_control(&self, control: &AgentControl) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO agent_controls (agent, enabled, reason, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (agent) DO UPDATE SET enabled = excluded.enabled, \
             reason = excluded.reason, updated_at = excluded.updated_at",
        )
        .bind(&control.agent)
        .bind(control.enabled)
        .bind(control.reason.as_deref())
        .bind(format_ts(control.updated_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn select_controls(&self) -> Result<Vec<AgentControl>, JournalError> {
        let rows = sqlx::query(
            "SELECT agent, enabled, reason, updated_at FROM agent_controls ORDER BY agent",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let updated_at: String = row.try_get("updated_at")?;
                Ok(AgentControl {
                    agent: row.try_get("agent")?,
                    enabled: row.try_get("enabled")?,
                    reason: row.try_get("reason")?,
                    updated_at: parse_ts(&updated_at)?,
                })
            })
            .collect()
    }
}

/// Invalid references become `PortError::Invalid` (the caller's mistake, readable as-is).
fn agent_port(err: JournalError) -> PortError {
    match err {
        JournalError::Invalid(message) => PortError::Invalid(message),
        other => port(other),
    }
}

#[async_trait]
impl AgentStore for SqliteJournal {
    async fn post_finding(
        &self,
        agent: &str,
        role: &str,
        finding: &NewFinding,
        ts: DateTime<Utc>,
    ) -> Result<Finding, PortError> {
        self.insert_finding(agent, role, finding, ts)
            .await
            .map_err(agent_port)
    }

    async fn findings(&self, query: &FindingQuery) -> Result<Vec<Finding>, PortError> {
        self.select_findings(query).await.map_err(agent_port)
    }

    async fn finding(&self, id: i64) -> Result<Option<Finding>, PortError> {
        self.finding_by_id(id).await.map_err(agent_port)
    }

    async fn record_verdict(&self, id: i64, verdict: &Verdict) -> Result<Finding, PortError> {
        self.store_verdict(id, verdict).await.map_err(agent_port)
    }

    async fn send_message(
        &self,
        sender: &str,
        message: &NewMessage,
        ts: DateTime<Utc>,
    ) -> Result<AgentMessage, PortError> {
        self.insert_message(sender, message, ts)
            .await
            .map_err(agent_port)
    }

    async fn messages(&self, query: &MessageQuery) -> Result<Vec<AgentMessage>, PortError> {
        self.select_messages(query).await.map_err(agent_port)
    }

    async fn take_unread(
        &self,
        reader: &str,
        limit: usize,
    ) -> Result<Vec<AgentMessage>, PortError> {
        self.select_unread(reader, limit, true)
            .await
            .map_err(agent_port)
    }

    async fn unread_count(&self, reader: &str) -> Result<usize, PortError> {
        self.count_unread(reader).await.map_err(agent_port)
    }

    async fn set_control(&self, control: &AgentControl) -> Result<(), PortError> {
        self.upsert_control(control).await.map_err(agent_port)
    }

    async fn controls(&self) -> Result<Vec<AgentControl>, PortError> {
        self.select_controls().await.map_err(agent_port)
    }
}

#[cfg(test)]
mod tests;
