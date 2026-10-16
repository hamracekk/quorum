# quorum-slots (`slots`)

**Slots** (DESIGN §5.2): one market on one venue — universe, strategies, calendar, costs — and
its rung on the **autonomy ladder** (§5.3).

Not the same as `quorum_core::strategies::Slots`, a *strategy* holding the best N instruments
of a universe. A slot here is what the allocator funds and the ladder promotes; that strategy
can run inside one.

## Module map

| File | What it holds |
|---|---|
| `config.rs` | `SlotId`, `SlotConfig` (parse, validate, `check_envelope`), `Universe` (fixed or point-in-time top volume → `core::strategies::UniverseRule`), `StrategySpec` (`trend-momentum`, `xs-momentum`), `Sizing`, `Brakes` (`vol_regime`, `crisis_entries`), `Exits`, `TradingCalendar` (`always-open` 365 days, `weekdays` 252; `bars_per_year` annualises), `CostModel` (→ `BacktestConfig`) |
| `ladder.rs` | `SlotState` (mode, since, halted; serializable), `review` → `Transition`; `Gates` (thresholds), `Evidence` (grouped per gate), `Blocker`, `Trigger` |
| `config/tests.rs`, `ladder/tests.rs` | Validation, the shipped `config/slots/crypto-spot.toml` inside the shipped envelope, every ladder path |

## The ladder

`research → paper → live-micro → live`, one rung per review, never above the envelope's
`max_mode` for the slot. `review` checks, in order:

1. **Halted** → held until the operator calls `resume` (which restarts at paper at most).
2. **Reconciliation break** → halt.
3. **Demotion**: envelope lowered → down to its cap; costs over model (live modes) → paper;
   drawdown over budget or drift → one rung down.
4. **Promotion** to the next rung if every gate passes; otherwise `Held` with all blockers.

| To | Gate (defaults in `Gates::default`) |
|---|---|
| paper | beats benchmark net of costs, deflated Sharpe ≥ 0.9, PBO ≤ 0.3 |
| live-micro | ≥ 8 weeks in paper, tracking OK, stress tests passed |
| live | ≥ 12 weeks in live-micro, slippage ≤ 1.5× the cost model |

## Invariants

- **The envelope is the ceiling.** No transition passes `envelope.max_mode(slot)`; a lowered
  envelope demotes at the next review.
- **Live modes only through the ladder.** `start_mode` may be `research` or `paper`.
- **The clock restarts on every transition**, so "8 weeks in paper" means 8 weeks since entering it.
- **The ladder only decides.** Evidence comes from the lab, journal and execution; the caller
  journals each `Transition` and acts on it.
- **Unknown config keys are errors.**

## Test

```bash
moon run slots:test
```
