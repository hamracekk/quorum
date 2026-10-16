# quorum-runner (`runner`)

Paper-trades **one slot** end to end on the shared engine (`core::Engine`): the P0 port of
trader's paper daemon, scoped to a slot and its envelope.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `Ports` (market data, stream, bar store, journal, event log, notifier, risk gate, optional passive benchmark and extra lines (`DailyNote`, e.g. the shadow evaluator's record) for the daily summary), `RunnerError` |
| `build.rs` | `strategy` (slot strategy → per-holding vol sizing → optional top-volume universe → optional book vol target), `backtest_config` (slot costs, brakes, exits + the envelope's daily loss), `interval_duration` |
| `session.rs` | `SlotFiles` (`<data>/slots/<id>/`), `Session`, `StateFile`, `request_stop` |
| `gate.rs` | `GatedBroker`: every order goes to the risk process first; verdicts recorded per session in `verdicts.jsonl` so replay repeats them |
| `paper.rs` | `run`: preflight → open/resume → sync history → rebuild by replay → live loop; `RunOptions`, `Outcome`, `Status` |
| `watchdog.rs` | The watchdog's decisions, pure: incidents (paper heartbeat stale or failed, recorder stale, risk process not answering), alert once on open and once on recovery, skip right after a sleep |
| `tests/paper.rs` | End to end on fakes: journaling from the start, resume == uninterrupted run, offline days, gap backfill, stop, refusals, risk gate (approval, process down, replayed verdicts) |

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
- **The risk process approves every order** (`quorum_risk`, DESIGN §9). The engine's broker is a
  `GatedBroker` around the `PaperBroker`: it sends each order with the slot's book (cash,
  positions, marks kept from the fills it passed) and refuses what the process refuses. When the
  process cannot be reached, risk-reducing orders pass and all others are refused; the heartbeat
  pings it and alerts once per outage. A refused order is rejected and retried at the next
  timestamp, like any order that never reached the venue.
- **Verdicts are replayed, not asked again.** Each verdict is appended to
  `<data>/slots/<id>/verdicts.jsonl` before the order goes out; replay uses the recorded verdict,
  and orders at or before the watermark with no record (sent before the gate existed) count as
  approved (test `replay_repeats_the_recorded_verdicts`).
- **The envelope's daily loss limit** is the engine's kill switch for the day, measured against the previous close (on daily bars: yesterday's close; on hourly bars the first hour counts). On daily crypto a 2% limit halts about 17 times a year and hurts (`quorum backtest --daily-loss`).
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
