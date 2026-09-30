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
| `ports.rs` | Port traits `MarketData` (async), `BarStore`, `EventLog`, `Journal` (async), `RunInfo`/`RunKind`, `PortError` |
| `fakes.rs` | In-memory `MemoryMarketData`, `MemoryBarStore`, `MemoryEventLog` for tests in any crate |
| `indicators.rs` | Streaming `Ema`, `Rsi` (Wilder), `Macd`, `Bollinger` |
| `patterns.rs` | 13 candlestick patterns with bias; `detect_at` uses only bars up to `i` |
| `signals.rs` | `Signal` (patterns + 8 indicator events), `SignalDetector` (streaming, fires on crossings), `occurrences` |
| `strategy.rs` | `Strategy` trait: returns target exposure per instrument on each closed bar |
| `strategies/` | `SmaCross`, `BuyAndHold`, `SignalFollower` (trade chosen signals, hold N bars), `PerInstrument` (one instance per instrument, equal weight), `Sma` |
| `broker.rs` | `SimBroker`: multi-instrument positions, fees, slippage, min notional |
| `risk.rs` | `RiskLimits`, `RiskManager`: per-instrument and gross exposure caps, daily-loss kill switch |
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
