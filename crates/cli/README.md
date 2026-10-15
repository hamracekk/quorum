# quorum-cli (`cli`)

The `quorum` binary, and the **composition root**: the only place that names concrete adapters
(Binance spot + websocket, Parquet bars, SQLite journal, JSONL event log). It parses input and
prints output; logic lives in the library crates.

## Commands

```bash
quorum paper run --slot crypto-spot [--fresh]   # paper-trade until Ctrl+C or `paper stop`
quorum paper status --slot crypto-spot          # session, watermark, equity, positions
quorum paper stop --slot crypto-spot            # stop at the next heartbeat (resumable)
```

Global: `--config <dir>` (default `config`: `envelope.toml`, `slots/<id>.toml`), `--data <dir>`
(default `data`: `bars/`, `quorum.db`, `events/`, `slots/<id>/`). Logs go to stderr
(`RUST_LOG`), results to stdout.

## Test

`tests/cli.rs` runs the real binary without network (status, stop, config errors).

```bash
moon run cli:test
```
