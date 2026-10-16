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
quorum research sync --slot crypto-pit           # top-volume slot: every USDT pair ever listed, from Binance's archive
quorum factors --slot multi-asset --horizon 5   # factor study → data/factors/
quorum backtest --slot crypto-spot --daily-loss 0.05   # slot strategy over research history: CAGR, Sharpe, DD, halts
quorum allocate                                 # walk-forward slots, study allocating across them, compare with config/benchmark.toml
quorum stress                                   # stress gate per slot and for the book → data/reports/
quorum secrets set tiingo | ntfy-topic | status   # secrets in the macOS keychain (service quorum)
quorum notify --test                            # phone alert test through ntfy
quorum watchdog --slots crypto-spot             # one health check (launchd runs it every 5 min)
quorum news fetch | extract | run | list | status   # knowledge plane (config/news.toml, impact.toml)
quorum evaluate --slot S [--source history|paper]   # trade evaluator in shadow: case files, counterfactual labels, study
quorum risk run | status | halt [--slot S] --reason R | resume [--slot S]   # risk process + kill switch
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
