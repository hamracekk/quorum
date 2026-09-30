# trader-journal (`journal`)

The journal (ADR 0002): a SQLite record of **decisions and executions, grouped by run**
(backtest, replay, paper, live). Implements the `Journal` port.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `SqliteJournal` (`open(path)`, `in_memory()`), `Journal` impl, timestamp/side encoding |
| `error.rs` | `JournalError`; wrapped in `PortError` at the port |
| `migrations/` | Versioned SQL migrations, embedded at compile time, applied on open |
| `build.rs` | Rebuild when migrations change |

## Schema (v1, `migrations/0001_init.sql`)

| Table | Columns |
|---|---|
| `runs` | `id` (UUID v7), `kind` (`backtest`\|`replay`\|`paper`\|`live`), `label`, `started_at` |
| `signals` | `run_id`, `ts`, `instrument`, `strategy`, `target` |
| `targets` | `run_id`, `ts`, `instrument`, `target` |
| `fills` | `run_id`, `ts`, `instrument`, `side` (`buy`\|`sell`), `qty`, `price`, `fee` |
| `risk_halts` | `run_id`, `ts`, `equity` |
| `orders` (0003) | `run_id`, `order_id`, `instrument`, `side`, `qty`, `status`, `filled_qty`, `avg_price`, `fees`, `reason`, `created_at`, `updated_at` — one row per order, upserted by every `order` event so it holds the latest state |

Timestamps are RFC 3339 UTC with milliseconds (`2026-09-30T12:00:00.000Z`) so text order is time
order; instruments use their text form (`binance:BTCUSDT`). Bars are not journaled — they live in
the bar store and the event log. File: `<data dir>/trader.db`, WAL mode, foreign keys on.

Ad-hoc analysis works with plain SQL:

```bash
sqlite3 data/trader.db "SELECT r.label, COUNT(f.id), SUM(f.fee) FROM runs r LEFT JOIN fills f ON f.run_id = r.id GROUP BY r.id"
```

## Rules

- **Migrations are append-only**: never edit a merged migration — add `000N_description.sql`.
- **`record` is all-or-nothing** (one transaction per batch).
- **Static SQL only.** sqlx 0.9 rejects runtime-built SQL strings (injection guard); bind values
  with `?` instead of formatting them in.
- Queries are runtime-checked and covered by tests (compile-time `query!` macros would need
  `sqlx prepare` in CI — revisit when the schema grows).
- The daemon is the single writer (from M3); other readers go through it or open read-only.

## Extending

- New journaled event: add a table in a new migration, a match arm in `insert_events`, a query
  method if needed, and round-trip + rollback tests.

## Testing

```bash
moon run journal:test     # or: cargo test -p trader-journal
```

Tests use `SqliteJournal::in_memory()`; one test exercises a real file (WAL mode, reopen,
idempotent migrations).
