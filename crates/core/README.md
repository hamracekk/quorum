# trader-core (`core`)

The domain heart of trader: types, the strategy interface, the port traits every external
boundary goes through, a simulated broker, risk limits, metrics and the backtest engine.
Pure logic — **no I/O**, no network, no files. Every other crate depends on this one; it
depends on no internal crate.

## Module map

| File | What it holds |
|---|---|
| `instrument.rs` | `InstrumentId { venue, symbol, kind }`, `VenueId`, `InstrumentKind`; validated; text form `venue:SYMBOL[:kind]` |
| `types.rs` | `Bar` (keyed by **open** time), `Series` (one instrument's bars), `Fill`, `Side`, `EquityPoint` |
| `events.rs` | `Event` (bar, signal, target, fill, risk_halt; tagged JSON) and `Envelope` (with log `seq`) |
| `engine.rs` | `Engine`: event-driven core — bars in (time order), decision events out; used by backtest, replay and live |
| `ports.rs` | Port traits `MarketData`, `MarketStream` (async), `BarStore`, `EventLog`, `Journal`, `AgentStore`, `NewsSource`, `NewsStore`, `LlmProvider`, `Notifier` (async), `RunInfo`/`RunKind`, `PortError` |
| `fakes.rs` | In-memory `MemoryMarketData`, `MemoryMarketStream`, `MemoryBarStore`, `MemoryEventLog`, `MemoryNewsSource`, `MemoryLlm`, `MemoryNotifier` for tests in any crate |
| `indicators.rs` | Streaming `Ema`, `Rsi` (Wilder), `Macd`, `Bollinger` |
| `patterns.rs` | 13 candlestick patterns with bias; `detect_at` uses only bars up to `i` |
| `signals.rs` | `Signal` (patterns + 8 indicator events), `SignalDetector` (streaming, fires on crossings), `occurrences` |
| `strategy.rs` | `Strategy` trait: returns target exposure per instrument on each closed bar |
| `strategy.rs` | `Strategy` (`on_bar`; optional `explain` → recorded with the signal, `inform` for live `ExternalCall`s), `ExternalCall` |
| `strategies/` | `SmaCross`, `BuyAndHold`, `SignalFollower` (trade chosen signals, hold N bars), `PerInstrument` (one instance per instrument, equal weight), `Sma`, `Ensemble` (M8: technical signals + `ExternalCall`s weighted by walk-forward evidence — independent outcomes, excess over drift, Bonferroni-raised t, net of costs; volatility-targeted, covariance-scaled sizing; every target explained) |
| `broker.rs` | `Broker` port (sync, non-blocking: `submit`/`cancel`, outcomes via `poll`), `PaperBroker` (fills market orders against the latest quote with fees + slippage), `Portfolio` (cash, positions, marks; order sizing with min notional; changed only by fills) |
| `agents.rs` | ADR 0003 types: `NewFinding`/`Finding` (kind, claim, typed `Hypothesis`, `Evidence`, confidence, status, supersession, latest `Verdict`), `TrackRecord` (hit rate, Brier), `NewMessage`/`AgentMessage`, `AgentControl`, agent-id rules and shape validation |
| `news.rs` | `NewsItem`, `NewsQuery`, `StoredNews`, `NewsSourceStats`, `title_key`, `DUPLICATE_WINDOW_HOURS`; M5b: `NewsEvent` (direction, magnitude, confidence, horizon, event type), `Extraction`, `TokenUsage` |
| `order.rs` | `Order`, `OrderId`, `OrderStatus`, `ExecutionReport`, `OrderBook`: the order state machine, rejecting illegal transitions and overfills |
| `risk.rs` | `RiskLimits`, `RiskManager`: per-instrument and gross exposure caps, daily-loss kill switch |
| `stops.rs` | T-2: `StopRules` (stop as % or ATR multiple, take-profit in R, trailing, risk-per-trade sizing, cooldown), `Atr`, `OpenTrade` (levels set before a bar, judged by its low/high, trailed after); the engine applies them to every strategy, off by default |
| `backtest.rs` | `run` / `run_from` (warm-up) / `run_with_events`: merge series and feed the `Engine`; `BacktestConfig`, `BacktestResult` |
| `metrics.rs` | `Performance` (return, CAGR, drawdown, Sharpe), `Report` (strategy vs benchmark, `Display`) |

## Invariants — never break these

- **No look-ahead.** A target returned on bar `t` executes at that instrument's bar `t+1` open.
  `backtest.rs` tests `signal_executes_on_next_bar_open` and `warm_up_bars_are_not_traded_or_recorded`
  guard this; add a test for any engine change.
- **Costs always apply.** Fees and slippage are part of every simulated fill.
- **Targets are fractions of total equity**, clamped by risk before execution. Trades happen only
  when the risk-adjusted target changes (no drift churn).
- **The benchmark is pure**: equal-weight buy & hold, no fees, no risk limits.
- **One engine**: never add a second simulation loop — backtest, replay and live all use `Engine`.
- **Strategies are `Send`** so the daemon can run them on async tasks.
- **Determinism**: `BTreeMap` everywhere order matters; no randomness, no wall clock.
- **No I/O here.** Adapters live in other crates and implement the ports.
- **Positions change only on reported fills.** The engine never books a trade on submission; every
  broker report goes through `OrderBook::apply`, and a report the state machine refuses stops the
  engine (`EngineError::Order`) instead of being ignored.
- **Results are pinned.** `tests/golden.rs` fingerprints a fixed synthetic backtest bit for bit. A
  refactor must leave it green; update the constants only for an intended behaviour change, and say
  so in the commit.

## Extending

- **New strategy:** implement `Strategy` (derive `Debug`, `Clone` if it will be wrapped in
  `PerInstrument`), return `None` until warmed up, add tests for warm-up, long/flat/short, and
  parameter validation. Register it in the CLI (`crates/cli/src/main.rs::build_strategy`).
- **New port:** add the trait to `ports.rs` in the milestone that first needs it, return
  `PortError`, add an in-memory fake to `fakes.rs`, update ADR 0001 if the port set changes.
- **New instrument kind:** extend `InstrumentKind` + `code()` + `FromStr`; file names and text form
  follow automatically.

## Testing

```bash
moon run core:test        # or: cargo test -p trader-core
```

Floats are compared with a tolerance, never `==`. Test helpers build synthetic bars and
`InstrumentId::spot("test", …)` ids. Use `fakes` rather than touching disk or network.
