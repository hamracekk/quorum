# quorum-envelope (`envelope`)

The operator's **envelope** (DESIGN §5.1): total capital, per-slot capital caps, the highest
mode each slot may reach, loss limits, leverage and allowed venues. Everything else in quorum
runs inside it.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `Mode` (the autonomy ladder's rungs), `Envelope` (parse, validate, query), `SlotBounds`, `LossLimits`, `EnvelopeError` |
| `tests.rs` | Parsing, validation, overrides, and a check that `config/envelope.toml` is valid |

## File format

`config/envelope.toml`:

```toml
currency = "EUR"                                   # ISO code
capital_total = 0                                  # ≥ 0
leverage_max = 1.0                                 # > 0
venues_allowed = ["binance"]                       # venue ids, paper included
loss = { daily_max = 0.05, total_max = 0.15 }      # fractions in (0, 1], daily ≤ total
slot_default = { capital_max = 0, max_mode = "paper" }
position_max = 0.25                                # optional, (0, leverage_max]: one position's share of equity
cluster_max = 1.0                                  # optional: positions correlated ≥ 0.7 with one instrument

[slots.crypto-spot]                                # optional per-slot override
capital_max = 2000                                 # ≤ capital_total
max_mode = "live-micro"                            # research | paper | live-micro | live
```

## Invariants

- **Read-only to the system.** `Envelope` has private fields, no setters and no `Serialize`.
  Only the operator edits the file; nothing in quorum writes it.
- **Unknown keys are errors** (`deny_unknown_fields`): a typo never falls back to a default.
- **A live `max_mode` needs `capital_max > 0`.** Real money with no capital is rejected, not
  resolved quietly.
- **Current decision: paper only** (DESIGN §19.2). `capital_total = 0`, every slot capped at
  `paper`.

## Not yet

- Signature verification of the file (DESIGN §5.1 "signed config"). Today the protection is
  that no code path writes it; an operator signature check comes with the server move (§19.3).

## Test

```bash
moon run envelope:test
```
