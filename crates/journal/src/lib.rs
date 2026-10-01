//! The journal (ADR 0002): a SQLite record of decisions and executions, grouped by run.
//!
//! [`SqliteJournal`] implements the `Journal` port on SQLite in WAL mode via `sqlx`.
//! Migrations live in `migrations/`, are embedded at compile time and run on open.
//! Queries are checked at runtime and covered by tests (compile-time checked macros would
//! need `sqlx prepare` in CI; revisit when the schema grows).

mod agents;
mod error;

pub use error::JournalError;

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use trader_core::{
    AgentCall, Event, Fill, InstrumentId, Journal, Order, OrderId, OrderStatus, PortError, RunInfo,
    RunKind, Side,
};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Clone)]
pub struct SqliteJournal {
    pool: SqlitePool,
}

impl SqliteJournal {
    /// Open (creating if needed) the journal database at `path` and apply migrations.
    pub async fn open(path: &Path) -> Result<Self, JournalError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|source| JournalError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        Self::migrated(pool).await
    }

    /// A private in-memory journal (tests, throwaway runs). One connection that never idles
    /// out, because each SQLite in-memory connection is its own database.
    pub async fn in_memory() -> Result<Self, JournalError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Self::migrated(pool).await
    }

    /// Write a consistent, compacted copy of the database to `target` (`VACUUM INTO`), safe
    /// while other connections read and write. `target` must not exist yet.
    pub async fn backup_to(&self, target: &Path) -> Result<(), JournalError> {
        let text = target.to_str().ok_or_else(|| {
            JournalError::Corrupt(format!("backup path {} is not UTF-8", target.display()))
        })?;
        sqlx::query("VACUUM INTO ?")
            .bind(text)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Open the database file at `path` read-only and run SQLite's integrity check.
    pub async fn check_file(path: &Path) -> Result<(), JournalError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .create_if_missing(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let result: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&pool)
            .await?;
        pool.close().await;
        if result == "ok" {
            Ok(())
        } else {
            Err(JournalError::Corrupt(format!(
                "{} failed the integrity check: {result}",
                path.display()
            )))
        }
    }

    async fn migrated(pool: SqlitePool) -> Result<Self, JournalError> {
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    async fn insert_run(&self, run: &RunInfo) -> Result<(), JournalError> {
        sqlx::query("INSERT INTO runs (id, kind, label, started_at) VALUES (?, ?, ?, ?)")
            .bind(&run.id)
            .bind(run.kind.as_str())
            .bind(&run.label)
            .bind(format_ts(run.started_at))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn insert_events(&self, run_id: &str, events: &[Event]) -> Result<usize, JournalError> {
        let mut tx = self.pool.begin().await?;
        let mut rows = 0;
        for event in events {
            let query = match event {
                Event::Signal {
                    ts,
                    instrument,
                    strategy,
                    target,
                } => sqlx::query(
                    "INSERT INTO signals (run_id, ts, instrument, strategy, target) \
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(run_id)
                .bind(format_ts(*ts))
                .bind(instrument.to_string())
                .bind(strategy.as_str())
                .bind(*target),
                Event::Target {
                    ts,
                    instrument,
                    target,
                } => sqlx::query(
                    "INSERT INTO targets (run_id, ts, instrument, target) VALUES (?, ?, ?, ?)",
                )
                .bind(run_id)
                .bind(format_ts(*ts))
                .bind(instrument.to_string())
                .bind(*target),
                Event::Fill(fill) => sqlx::query(
                    "INSERT INTO fills (run_id, ts, instrument, side, qty, price, fee) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(run_id)
                .bind(format_ts(fill.ts))
                .bind(fill.instrument.to_string())
                .bind(side_text(fill.side))
                .bind(fill.qty)
                .bind(fill.price)
                .bind(fill.fee),
                Event::Order(order) => sqlx::query(
                    "INSERT INTO orders (run_id, order_id, instrument, side, qty, status, \
                     filled_qty, avg_price, fees, reason, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (run_id, order_id) DO UPDATE SET status = excluded.status, \
                     filled_qty = excluded.filled_qty, avg_price = excluded.avg_price, \
                     fees = excluded.fees, reason = excluded.reason, \
                     updated_at = excluded.updated_at",
                )
                .bind(run_id)
                .bind(i64::try_from(order.id.0).map_err(|_| {
                    JournalError::Corrupt(format!("order id {} out of range", order.id))
                })?)
                .bind(order.instrument.to_string())
                .bind(side_text(order.side))
                .bind(order.qty)
                .bind(order.status.as_str())
                .bind(order.filled_qty)
                .bind(order.avg_price)
                .bind(order.fees)
                .bind(order.reason.as_deref())
                .bind(format_ts(order.created_at))
                .bind(format_ts(order.updated_at)),
                Event::RiskHalt { ts, equity } => {
                    sqlx::query("INSERT INTO risk_halts (run_id, ts, equity) VALUES (?, ?, ?)")
                        .bind(run_id)
                        .bind(format_ts(*ts))
                        .bind(*equity)
                }
                // Bars live in the bar store and event log; future event types are journaled
                // once they get a table.
                other => {
                    tracing::debug!(kind = other.kind(), "event not journaled");
                    continue;
                }
            };
            query.execute(&mut *tx).await?;
            rows += 1;
        }
        tx.commit().await?;
        Ok(rows)
    }

    async fn select_runs(&self) -> Result<Vec<RunInfo>, JournalError> {
        let rows = sqlx::query(
            "SELECT id, kind, label, started_at FROM runs ORDER BY started_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                let started_at: String = row.try_get("started_at")?;
                Ok(RunInfo {
                    id: row.try_get("id")?,
                    kind: RunKind::from_str(&kind)
                        .map_err(|e| JournalError::Corrupt(e.to_string()))?,
                    label: row.try_get("label")?,
                    started_at: parse_ts(&started_at)?,
                })
            })
            .collect()
    }

    async fn select_fills(&self, run_id: &str) -> Result<Vec<Fill>, JournalError> {
        let rows = sqlx::query(
            "SELECT ts, instrument, side, qty, price, fee FROM fills WHERE run_id = ? ORDER BY id",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let ts: String = row.try_get("ts")?;
                let instrument: String = row.try_get("instrument")?;
                let side: String = row.try_get("side")?;
                Ok(Fill {
                    ts: parse_ts(&ts)?,
                    instrument: InstrumentId::from_str(&instrument)
                        .map_err(|e| JournalError::Corrupt(e.to_string()))?,
                    side: parse_side(&side)?,
                    qty: row.try_get("qty")?,
                    price: row.try_get("price")?,
                    fee: row.try_get("fee")?,
                })
            })
            .collect()
    }
}

impl SqliteJournal {
    async fn select_orders(&self, run_id: &str) -> Result<Vec<Order>, JournalError> {
        let rows = sqlx::query(
            "SELECT order_id, instrument, side, qty, status, filled_qty, avg_price, fees, \
             reason, created_at, updated_at FROM orders WHERE run_id = ? ORDER BY order_id",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let id: i64 = row.try_get("order_id")?;
                let instrument: String = row.try_get("instrument")?;
                let side: String = row.try_get("side")?;
                let status: String = row.try_get("status")?;
                let created_at: String = row.try_get("created_at")?;
                let updated_at: String = row.try_get("updated_at")?;
                Ok(Order {
                    id: OrderId(
                        u64::try_from(id)
                            .map_err(|_| JournalError::Corrupt(format!("order id {id}")))?,
                    ),
                    instrument: InstrumentId::from_str(&instrument)
                        .map_err(|e| JournalError::Corrupt(e.to_string()))?,
                    side: parse_side(&side)?,
                    qty: row.try_get("qty")?,
                    status: OrderStatus::from_str(&status)
                        .map_err(|e| JournalError::Corrupt(e.to_string()))?,
                    filled_qty: row.try_get("filled_qty")?,
                    avg_price: row.try_get("avg_price")?,
                    fees: row.try_get("fees")?,
                    reason: row.try_get("reason")?,
                    created_at: parse_ts(&created_at)?,
                    updated_at: parse_ts(&updated_at)?,
                })
            })
            .collect()
    }

    async fn insert_agent_call(&self, call: &AgentCall) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO agent_calls (ts, agent, tool, arguments, ok, summary, duration_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(format_ts(call.ts))
        .bind(&call.agent)
        .bind(&call.tool)
        .bind(&call.arguments)
        .bind(call.ok)
        .bind(&call.summary)
        .bind(i64::try_from(call.duration_ms).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn select_agent_calls(&self, limit: usize) -> Result<Vec<AgentCall>, JournalError> {
        let rows = sqlx::query(
            "SELECT ts, agent, tool, arguments, ok, summary, duration_ms \
             FROM agent_calls ORDER BY id DESC LIMIT ?",
        )
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                let ts: String = row.try_get("ts")?;
                let duration: i64 = row.try_get("duration_ms")?;
                Ok(AgentCall {
                    ts: parse_ts(&ts)?,
                    agent: row.try_get("agent")?,
                    tool: row.try_get("tool")?,
                    arguments: row.try_get("arguments")?,
                    ok: row.try_get("ok")?,
                    summary: row.try_get("summary")?,
                    duration_ms: u64::try_from(duration).unwrap_or(0),
                })
            })
            .collect()
    }
}

fn port(err: JournalError) -> PortError {
    PortError::adapter("sqlite journal", err)
}

#[async_trait]
impl Journal for SqliteJournal {
    async fn start_run(&self, run: &RunInfo) -> Result<(), PortError> {
        self.insert_run(run).await.map_err(port)
    }

    async fn record(&self, run_id: &str, events: &[Event]) -> Result<usize, PortError> {
        self.insert_events(run_id, events).await.map_err(port)
    }

    async fn runs(&self) -> Result<Vec<RunInfo>, PortError> {
        self.select_runs().await.map_err(port)
    }

    async fn fills(&self, run_id: &str) -> Result<Vec<Fill>, PortError> {
        self.select_fills(run_id).await.map_err(port)
    }

    async fn orders(&self, run_id: &str) -> Result<Vec<Order>, PortError> {
        self.select_orders(run_id).await.map_err(port)
    }

    async fn record_agent_call(&self, call: &AgentCall) -> Result<(), PortError> {
        self.insert_agent_call(call).await.map_err(port)
    }

    async fn agent_calls(&self, limit: usize) -> Result<Vec<AgentCall>, PortError> {
        self.select_agent_calls(limit).await.map_err(port)
    }
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_ts(text: &str) -> Result<DateTime<Utc>, JournalError> {
    DateTime::parse_from_rfc3339(text)
        .map(|ts| ts.with_timezone(&Utc))
        .map_err(|e| JournalError::Corrupt(format!("timestamp `{text}`: {e}")))
}

const fn side_text(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn parse_side(text: &str) -> Result<Side, JournalError> {
    match text {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        other => Err(JournalError::Corrupt(format!("side `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn btc() -> InstrumentId {
        InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
    }

    fn run(id: &str, hour: i64) -> RunInfo {
        RunInfo {
            id: id.into(),
            kind: RunKind::Backtest,
            label: format!("test run {id}"),
            started_at: at(hour),
        }
    }

    fn fill(hour: i64, side: Side) -> Fill {
        Fill {
            ts: at(hour),
            instrument: btc(),
            side,
            qty: 0.5,
            price: 100.25,
            fee: 0.05,
        }
    }

    fn decisions() -> Vec<Event> {
        vec![
            Event::Bar {
                instrument: btc(),
                bar: trader_core::Bar {
                    ts: at(0),
                    open: 1.0,
                    high: 1.0,
                    low: 1.0,
                    close: 1.0,
                    volume: 1.0,
                },
            },
            Event::Signal {
                ts: at(0),
                instrument: btc(),
                strategy: "sma-cross".into(),
                target: 1.0,
            },
            Event::Target {
                ts: at(0),
                instrument: btc(),
                target: 1.0,
            },
            Event::Fill(fill(1, Side::Buy)),
            Event::RiskHalt {
                ts: at(2),
                equity: 900.0,
            },
            Event::Fill(fill(3, Side::Sell)),
        ]
    }

    /// Row count for a fixed `SELECT COUNT(*) ...` statement (static SQL only).
    async fn count(journal: &SqliteJournal, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&journal.pool)
            .await
            .expect("count")
    }

    #[tokio::test]
    async fn decisions_round_trip_and_bars_are_skipped() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        journal.start_run(&run("r1", 0)).await.expect("start run");

        let rows = journal.record("r1", &decisions()).await.expect("record");
        assert_eq!(rows, 5);
        assert_eq!(
            journal.fills("r1").await.expect("fills"),
            vec![fill(1, Side::Buy), fill(3, Side::Sell)]
        );
        assert_eq!(count(&journal, "SELECT COUNT(*) FROM signals").await, 1);
        assert_eq!(count(&journal, "SELECT COUNT(*) FROM targets").await, 1);
        assert_eq!(count(&journal, "SELECT COUNT(*) FROM risk_halts").await, 1);
    }

    #[tokio::test]
    async fn order_events_upsert_the_latest_state_per_run() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        journal.start_run(&run("a", 0)).await.expect("start");
        journal.start_run(&run("b", 1)).await.expect("start");

        let new = Order::new(OrderId(1), at(1), btc(), Side::Buy, 0.5);
        let accepted = Order {
            status: OrderStatus::Accepted,
            ..new.clone()
        };
        let filled = Order {
            status: OrderStatus::Filled,
            filled_qty: 0.5,
            avg_price: 100.25,
            fees: 0.05,
            updated_at: at(2),
            ..new.clone()
        };
        let rejected = Order {
            status: OrderStatus::Rejected,
            reason: Some("venue down".into()),
            ..Order::new(OrderId(2), at(3), btc(), Side::Sell, 0.5)
        };
        let rows = journal
            .record(
                "a",
                &[
                    Event::Order(accepted),
                    Event::Order(filled.clone()),
                    Event::Order(rejected.clone()),
                ],
            )
            .await
            .expect("record");
        assert_eq!(rows, 3);
        // Order ids restart per run; the same id in another run is another order.
        journal
            .record("b", &[Event::Order(new.clone())])
            .await
            .expect("record");

        assert_eq!(
            journal.orders("a").await.expect("orders"),
            vec![filled, rejected]
        );
        assert_eq!(journal.orders("b").await.expect("orders"), vec![new]);
        assert!(journal.orders("missing").await.expect("orders").is_empty());
    }

    #[tokio::test]
    async fn backups_are_consistent_copies_that_pass_the_integrity_check() {
        let dir =
            std::env::temp_dir().join(format!("trader-journal-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal = SqliteJournal::open(&dir.join("trader.db"))
            .await
            .expect("journal");
        journal.start_run(&run("r1", 0)).await.expect("start");
        journal.record("r1", &decisions()).await.expect("record");

        let copy = dir.join("copy.db");
        journal.backup_to(&copy).await.expect("backup");
        SqliteJournal::check_file(&copy).await.expect("intact");
        let restored = SqliteJournal::open(&copy).await.expect("open copy");
        assert_eq!(
            restored.fills("r1").await.expect("fills"),
            journal.fills("r1").await.expect("fills")
        );
        assert!(
            journal.backup_to(&copy).await.is_err(),
            "an existing target is never overwritten"
        );

        std::fs::write(dir.join("garbage.db"), b"not a database").expect("write");
        assert!(
            SqliteJournal::check_file(&dir.join("garbage.db"))
                .await
                .is_err()
        );
        assert!(
            SqliteJournal::check_file(&dir.join("missing.db"))
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn runs_are_listed_newest_first() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        journal.start_run(&run("old", 0)).await.expect("start");
        journal.start_run(&run("new", 5)).await.expect("start");
        let ids: Vec<String> = journal
            .runs()
            .await
            .expect("runs")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["new", "old"]);
    }

    #[tokio::test]
    async fn events_for_an_unknown_run_are_rejected() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        assert!(journal.record("missing", &decisions()).await.is_err());
        assert_eq!(count(&journal, "SELECT COUNT(*) FROM fills").await, 0);
    }

    #[tokio::test]
    async fn a_failing_batch_writes_nothing() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        journal.start_run(&run("r1", 0)).await.expect("start run");
        let batch = vec![
            Event::Signal {
                ts: at(0),
                instrument: btc(),
                strategy: "s".into(),
                target: 1.0,
            },
            // NaN is stored as NULL by SQLite and violates NOT NULL: the batch must roll back.
            Event::Target {
                ts: at(0),
                instrument: btc(),
                target: f64::NAN,
            },
        ];
        assert!(journal.record("r1", &batch).await.is_err());
        assert_eq!(count(&journal, "SELECT COUNT(*) FROM signals").await, 0);
    }

    #[tokio::test]
    async fn duplicate_run_ids_are_rejected() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        journal.start_run(&run("r1", 0)).await.expect("start");
        assert!(journal.start_run(&run("r1", 1)).await.is_err());
    }

    #[tokio::test]
    async fn file_journal_persists_across_reopen_in_wal_mode() {
        let dir = std::env::temp_dir().join(format!("trader-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // stale leftovers from an earlier run, if any
        let path = dir.join("trader.db");

        let journal = SqliteJournal::open(&path).await.expect("open");
        journal.start_run(&run("r1", 0)).await.expect("start");
        journal.record("r1", &decisions()).await.expect("record");
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&journal.pool)
            .await
            .expect("pragma");
        assert_eq!(mode, "wal");
        journal.pool.close().await;

        let reopened = SqliteJournal::open(&path)
            .await
            .expect("reopen (migrations idempotent)");
        assert_eq!(reopened.fills("r1").await.expect("fills").len(), 2);
    }

    fn agent_call(hour: i64, tool: &str, ok: bool) -> AgentCall {
        AgentCall {
            ts: at(hour),
            agent: "claude-code".into(),
            tool: tool.into(),
            arguments: r#"{"fast":20}"#.into(),
            ok,
            summary: if ok { "fine".into() } else { "boom".into() },
            duration_ms: 42,
        }
    }

    #[tokio::test]
    async fn agent_calls_round_trip_newest_first_with_a_limit() {
        let journal = SqliteJournal::in_memory().await.expect("journal");
        for (hour, tool, ok) in [
            (0, "backtest", true),
            (1, "sweep", false),
            (2, "walk_forward", true),
        ] {
            journal
                .record_agent_call(&agent_call(hour, tool, ok))
                .await
                .expect("record");
        }
        let calls = journal.agent_calls(2).await.expect("calls");
        assert_eq!(
            calls,
            vec![
                agent_call(2, "walk_forward", true),
                agent_call(1, "sweep", false)
            ]
        );
    }
}
