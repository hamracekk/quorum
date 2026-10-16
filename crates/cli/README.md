# quorum-cli (`cli`)

The `quorum` binary, and the **composition root**: the only place that names concrete adapters
(Binance spot + websocket, Parquet bars, SQLite journal, JSONL event log). It parses input and
prints output; logic lives in the library crates.

## Commands

```bash
quorum paper run --slot crypto-spot [--fresh]   # paper-trade until Ctrl+C or `paper stop`
quorum paper status --slot crypto-spot          # session, watermark, equity, positions
quorum paper stop --slot crypto-spot            # stop at the next heartbeat (resumable)
quorum record                                   # forward recorder (config/recorder.toml)
quorum research sync --slot multi-asset --days 7500   # research history → data/research/bars
quorum factors --slot multi-asset --horizon 5   # factor study → data/factors/
quorum backtest --slot crypto-spot --daily-loss 0.05   # slot strategy over research history: CAGR, Sharpe, DD, halts
quorum allocate                                 # walk-forward slots, study allocating across them
quorum stress                                   # stress gate per slot and for the book → data/reports/
quorum secrets set tiingo | status              # API keys in the macOS keychain (service quorum)
```

Exit codes of `paper run`: `0` after a requested stop (Ctrl+C, SIGTERM, `paper stop`), `75`
when the live stream ended (a supervisor should restart), `1` on an error. Logs are plain text
when stderr is not a terminal. Running it unattended: [deploy/README.md](../../deploy/README.md).

Global: `--config <dir>` (default `config`: `envelope.toml`, `slots/<id>.toml`), `--data <dir>`
(default `data`: `bars/`, `quorum.db`, `events/`, `slots/<id>/`). Logs go to stderr
(`RUST_LOG`), results to stdout.

## Test

`tests/cli.rs` runs the real binary without network (status, stop, config errors).

```bash
moon run cli:test
```
