# quorum

An autonomous, multi-market trading system in Rust. Several **slots** (one per market and venue)
propose trades; every trade is judged on many **factors** (price, flow, news, macro,
positioning, fundamentals, cost); an **allocator** spreads capital across slots by measured
evidence; a separate **risk process** enforces the **envelope** the operator sets.

> **Status:** P0 Foundation in progress. Paper only (§19.2 of the design). Not financial advice.

- Design, principles and phases: [DESIGN.md](DESIGN.md)
- Successor to [trader](../trader) (`hamracekk/llm-trader`): its proven crates were copied here
  with their git history (DESIGN §19.4).

## Crates

| Crate | Responsibility | Origin |
|---|---|---|
| `core` | Domain types, venue-qualified instruments, events, ports + fakes, event-driven engine, order state machine, paper broker, strategies, risk, metrics | trader |
| `bus` | In-process bus, append-only JSONL event log, deterministic replay | trader |
| `data` | Binance spot/futures/archive data, Tiingo equities, Parquet store, incremental sync, data quality | trader |
| `journal` | SQLite journal (runs, signals, targets, fills, findings, news) | trader |
| `lab` | Walk-forward, sweeps, deflated Sharpe, PBO, purged CV, scoreboard, tournament, carry, portfolio | trader |
| `news` | RSS/Atom/JSON feeds (incl. SEC EDGAR), sanitised and deduped | trader |
| `ai` | LLM providers (Claude, Ollama), typed news-event extraction, impact themes | trader |
| `ml` | Gradient-boosted trees as cross-sectional learners | trader |
| `hyperliquid` | Followed traders, read only | trader |
| `predictions` | Prediction markets (Kalshi), read only | trader |
| `envelope` | The operator envelope: capital, loss limits, venues, highest mode per slot; read-only to the system | new |
| `slots` | Slot config (venue, universe, strategy, sizing, brakes, costs) and the autonomy ladder | new |
| `runner` | Paper-trades one slot: history sync, resume by replay, journal watermark, live bars, gap backfill | new (port of trader's paper daemon) |
| `cli` | The `quorum` binary and composition root | new |

trader's `service`, `mcp`, `agents`, `server`, `cli`, `app` and dashboard were **not** copied:
they are built around one paper session and are rebuilt here around slots, the evaluator and the
allocator (DESIGN §16).

## Config

| File | What it is |
|---|---|
| `config/envelope.toml` | The operator envelope. Paper only today |
| `config/slots/crypto-spot.toml` | Crypto spot slot: daily trend-momentum, top 25 by volume, Binance |

## Paper trading

```bash
cargo run --release -p quorum-cli -- paper run --slot crypto-spot   # or: moon run cli:paper
cargo run --release -p quorum-cli -- paper status --slot crypto-spot
cargo run --release -p quorum-cli -- paper stop --slot crypto-spot
```

Unattended on the laptop (launchd: start at login, restart on failure, logs): see
[deploy/README.md](deploy/README.md).

```bash
./deploy/launchd/install.sh crypto-spot
```

## Gates

```bash
moon run root:verify        # fmt, clippy -D warnings, tests for every crate
cargo fmt-check && cargo lint && cargo test --workspace --all-features   # same, without moon
```

`crates/core/tests/golden.rs` pins backtest results bit for bit. It passing here proves the
copied engine behaves exactly as it did in trader.
