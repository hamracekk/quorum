# trader-bus (`bus`)

Event plumbing (ADR 0002): the append-only **event log**, the in-process **bus**, and
deterministic **replay** of recorded market data through the trading engine.

## Module map

| File | What it holds |
|---|---|
| `log.rs` | `JsonlEventLog`: `EventLog` port on JSON Lines files, one per UTC day |
| `bus.rs` | `Bus`: append to the log first, then broadcast to subscribers (`tokio::sync::broadcast`) |
| `replay.rs` | `replay(log, start, end, strategy, cfg)`: re-derive decisions from recorded bars with `core::Engine` |
| `error.rs` | `BusError` (I/O, corrupt line with file:line, serialization); wrapped in `PortError` at the port |
| `tests/replay_determinism.rs` | M2 exit criterion: a recorded day replays identically; replay == backtest |

## Event log format

- Directory: `<data dir>/events/`; file per UTC date of the **event time**: `YYYY-MM-DD.jsonl`.
- One `Envelope` per line: `{"seq":42,"type":"bar","instrument":"binance:BTCUSDT","bar":{…}}`.
  Types: `bar`, `signal`, `target`, `fill`, `risk_halt` (see `core::events`).
- `seq` is strictly increasing across files and restarts (recovered from the highest stored seq on open).
- Each append is one `write` of a complete line in append mode; appends are serialized by a lock.
- Unrelated files in the directory are ignored; a corrupt line fails loudly with file and line number.

## Invariants

- **Log before broadcast.** A subscriber never sees an event the log doesn't have.
- **Replay re-derives, never copies** recorded decisions — that is what makes it a determinism check.
- **Prices round-trip bit-for-bit** through JSON: the workspace enables serde_json's
  `float_roundtrip` feature. Without it ~15% of prices change in the last bit and replays drift
  (guarded by `core::events::tests::prices_survive_json_bit_for_bit` and the replay tests).
- Slow subscribers get `RecvError::Lagged` rather than blocking publishers; they must catch up
  from the log by `seq`.

## Extending

- New event type: add the variant in `core::events` (tagged, `#[non_exhaustive]`), a JSON
  round-trip test there, and decide whether replay feeds it to the engine.
- Different storage (binary, rotation policy): implement `EventLog` in a new type; keep the
  sequence and ordering guarantees and reuse the replay tests.

## Testing

```bash
moon run bus:test         # or: cargo test -p trader-bus
```
