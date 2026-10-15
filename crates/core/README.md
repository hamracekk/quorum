# quorum-core (`core`)

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
| `engine.rs` | `Engine`: event-driven core — bars in (time order), decision events out; used by backtest, replay and live; per-trade stops (T-2) and operator-approved `manual_trade`s (T-3) |
| `ports.rs` | Port traits `MarketData`, `MarketStream` (async), `BarStore`, `EventLog`, `Journal`, `AgentStore`, `NewsSource`, `NewsStore`, `LlmProvider`, `Notifier` (async), `RunInfo`/`RunKind`, `PortError` |
| `fakes.rs` | In-memory `MemoryMarketData`, `MemoryMarketStream`, `MemoryBarStore`, `MemoryEventLog`, `MemoryNewsSource`, `MemoryLlm`, `MemoryNotifier` for tests in any crate |
| `indicators.rs` | Streaming `Ema`, `Rsi` (Wilder), `Macd`, `Bollinger` |
| `patterns.rs` | 13 candlestick patterns with bias; `detect_at` uses only bars up to `i` |
| `signals.rs` | `Signal` (patterns + 8 indicator events), `SignalDetector` (streaming, fires on crossings), `occurrences` |
| `strategy.rs` | `Strategy` trait: returns target exposure per instrument on each closed bar |
| `strategy.rs` | `Strategy` (`on_bar`; optional `explain` → recorded with the signal, `inform` for live `ExternalCall`s), `ExternalCall` |
| `strategies/` | `SmaCross`, `BuyAndHold`, `SignalFollower` (trade chosen signals, hold N bars), `PerInstrument` (one instance per instrument, equal weight), `Sma`, `Ensemble` (M8: technical signals + `ExternalCall`s weighted by walk-forward evidence — independent outcomes, excess over drift, Bonferroni-raised t, net of costs; volatility-targeted, covariance-scaled sizing; every target explained) |
| `broker.rs` | `Broker` port (sync, non-blocking: `submit`/`cancel`, outcomes via `poll`), `PaperBroker` (fills market orders against the latest quote with fees + slippage + square-root market impact from `liquidity` hints), `Portfolio` (cash, positions, marks; order sizing with min notional; changed only by fills) |
| `agents.rs` | ADR 0003 types: `NewFinding`/`Finding` (kind, claim, typed `Hypothesis`, `Evidence`, confidence, status, supersession, latest `Verdict`), `TrackRecord` (hit rate, Brier), `NewMessage`/`AgentMessage`, `AgentControl`, agent-id rules and shape validation |
| `news.rs` | `NewsItem`, `NewsQuery`, `StoredNews`, `NewsSourceStats`, `title_key`, `DUPLICATE_WINDOW_HOURS`; M5b: `NewsEvent` (direction, magnitude, confidence, horizon, event type), `Extraction`, `TokenUsage` |
| `order.rs` | `Order`, `OrderId`, `OrderStatus`, `ExecutionReport`, `OrderBook`: the order state machine, rejecting illegal transitions and overfills |
| `risk.rs` | `RiskLimits`, `RiskManager`: per-instrument and gross exposure caps, daily-loss kill switch; E-5 `PortfolioRisk` brakes (drawdown, volatility regime) scaling every target in 25% steps |
| `strategies/follow_traders.rs` | T-4: `FollowTraders` — each instrument on the average side of the followed traders who trade it (from `follow:` position states), equal share per instrument |
| `strategies/learner.rs` | R-5 / M9: `Learner` (fit on labelled feature rows, predict), `XsLearner` (weekly snapshots of 6 price/volume features labelled by the next rebalance's return, refit on labelled history only, hold the top predictions), `features` |
| `strategies/ridge.rs` | R-5: `Ridge` learner (standardised ridge regression) and `XsRidge::new` — the linear benchmark every learner must beat |
| `strategies/slots.rs` | `Slots`: hold the best N instruments, ranked daily by trend strength (return over volatility) plus the evidence-weighted conviction of outside sources (an inner `Ensemble`: no record, no weight); kept filled, a holding keeps its slot within N + 25%, equal volatility-scaled shares |
| `strategies/universe.rs` | ADR 0007 step 3: `TopVolume` — any strategy restricted to the instruments with the most trailing dollar volume at each re-ranking (point-in-time universe); members are handed to the inner strategy (`set_universe`) so rankings choose among them |
| `strategies/exposure.rs` | R-6: `ExposureSchedule` — any strategy's targets times a scale schedule known in advance (regime filters) |
| `strategies/portfolio_vol.rs` | RB-3: `PortfolioVolTarget` — the whole book scaled to a volatility target from the instruments' covariance |
| `strategies/delist_exit.rs` | RB-4: `DelistExit` — an instrument held flat from its delisting announcement (stored or live via `inform`) |
| `strategies/ridge.rs` | R-5: `XsRidge` — cross-sectional ridge regression on price/volume features, refitted every rebalance on labelled past snapshots only; the honest ML benchmark |
| `strategies/momentum.rs` | E-1: `TrendMomentum` (long while the `lookback` return is positive; run with `PerInstrument`) and `XsMomentum` (the `top` rising instruments by `lookback` return, ranked on the previous timestamp so feed order never matters); `VolTarget` sizing (`min(1, target_vol / realised vol)`) with a rebalance `band` |
| `suggestions.rs` | T-3: `NewSuggestion` (validation: size cap, stop required to open, bounds), `Suggestion`, `Side`, `Status` |
| `impact.rs` | ADR 0008: `ImpactMap` (operator-written themes → `ImpactLink`s: instrument, sign ±1, why, `since`, horizon), `validate`, `implications` (a theme moving up/down → per-instrument direction) |
| `sizing.rs` | `SizingRules`: `mode` (`strategy` / `equal` / `fixed` — fixed trades keep their entry size), `max_notional`, `max_positions` (open trades keep their place), per-market `MarketBudget` (`venue` or `venue:kind`: equity share, position count) — applied by the engine to each timestamp's targets before the risk limits; defaults change nothing |
| `trades.rs` | The trade ledger: `round_trips` (fills + the decisions behind them → entry, exit, size, P&L after fees, days held, opener, exit reason; open trades at a mark) and `stats` (win rate, profit factor, expectancy, exits by reason) |
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
moon run core:test        # or: cargo test -p quorum-core
```

Floats are compared with a tolerance, never `==`. Test helpers build synthetic bars and
`InstrumentId::spot("test", …)` ids. Use `fakes` rather than touching disk or network.
