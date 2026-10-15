# quorum-runner (`runner`)

Paper-trades **one slot** end to end on the shared engine (`core::Engine`): the P0 port of
trader's paper daemon, scoped to a slot and its envelope.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `Ports` (market data, stream, bar store, journal, event log), `RunnerError` |
| `build.rs` | `strategy` (slot strategy → per-holding vol sizing → optional top-volume universe → optional book vol target), `backtest_config` (slot costs, brakes, exits + the envelope's daily loss), `interval_duration` |
| `session.rs` | `SlotFiles` (`<data>/slots/<id>/`), `Session`, `StateFile`, `request_stop` |
| `paper.rs` | `run`: preflight → open/resume → sync history → rebuild by replay → live loop; `RunOptions`, `Outcome`, `Status` |
| `tests/paper.rs` | End to end on fakes: journaling from the start, resume == uninterrupted run, offline days, gap backfill, stop, refusals |

## A run

1. **Lock** the slot (one writer). **Preflight.** Venue in the envelope; ports on the slot's venue; the slot's ladder state
   (`slot.json`) reviewed against the envelope (a lowered envelope demotes it); halted or below
   `paper` → refused.
2. **Open or resume** the slot's open session. A changed slot config needs `--fresh`.
3. **Sync** each instrument's stored bars up to now (`quorum_data::sync`).
4. **Rebuild by replay**: every stored bar through a fresh engine, in time order. Bars before
   `trade_from` only warm up; bars past the `journaled_until` watermark are published and
   journaled.
5. **Live**: closed bars from the stream; duplicates skipped; a gap backfills **every**
   instrument up to the new bar, in time order; a heartbeat checks `STOP` and catches up on
   bars the stream missed (sleep, reconnects).

## Invariants

- **Resuming is replay.** No engine internals are persisted; a resumed run equals an
  uninterrupted one (test `resuming_rebuilds_by_replay_without_duplicates`).
- **Each decision is journaled once**, by the watermark.
- **Gaps are filled for all instruments together.** Backfilling one instrument alone would move
  the engine past bars the others still miss, and those would be skipped as out of order
  (test `a_gap_in_the_stream_is_backfilled_in_order`).
- **Paper only.** Fills come from the engine's `PaperBroker`; there is no order path to a venue.
- **The envelope's daily loss limit** is the engine's kill switch for the day.
- A stop leaves the session open; the next run resumes it.
- **One process per slot.** A run holds `<data>/slots/<id>/LOCK` (an OS file lock, released on
  exit or crash); a second run of the same slot is refused.
- `Status::restart` says whether a supervisor should start the run again (an ended stream, yes;
  a requested stop, no).

## Not yet (later P0 / P1)

Top-volume universes (candidate sync), several strategies per slot, delisting announcements,
notifications and daily summaries, NATS and Postgres behind the same ports.

## Test

```bash
moon run runner:test
```
