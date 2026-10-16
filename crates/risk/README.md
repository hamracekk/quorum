# quorum-risk (`risk`)

The **risk process** (DESIGN §9): a separate OS process and the only component that approves
orders. Paper slots ask it about every order before the order reaches the (simulated) venue;
the operator pulls the kill switch through it.

## Module map

| File | What it holds |
|---|---|
| `protocol.rs` | `OrderCheck` (the order + the slot's book before it), `Verdict`, `Ping`, `Request`/`Response` (one JSON line each), `StatusReport` |
| `rules.rs` | The decisions, pure: `check`, `observe` (loss tracking, halts), `halt`, `resume`, `LEVERAGE_ALLOWANCE` |
| `state.rs` | `RiskState` (kill switch + `SlotRisk` per slot: halt, day reference, peak, counts), `Halt`; kept in `<data>/risk/state.json` |
| `server.rs` | `serve`: the process on `<data>/risk/risk.sock` (mode 0600, one process per data dir via `LOCK`), alerts on halts; `RiskPaths` |
| `client.rs` | `RiskGate` (the order path's view, blocking), `SocketGate` |
| `fakes.rs` | `FakeGate` for tests of the order path |
| `tests/risk.rs` | The rules on synthetic books; the process end to end over its socket |

## Rules, in order

1. **Risk-reducing orders always pass** (shrink a position, never flip it), halted or not, even
   with an unreadable envelope. Anything may reduce risk.
2. The **envelope** must read and validate (re-read on every request: a lowered envelope applies
   at once). Unreadable → only risk-reducing orders.
3. The **kill switch** (every slot) and the slot's own **halt** refuse new risk.
4. The slot's **mode** must be at least `paper` and within the envelope's `max_mode`; live modes
   are refused (no live execution exists).
5. The instrument's **venue** must be in the envelope.
6. **Leverage:** gross exposure after the order ≤ `leverage_max` × equity (+1% allowance for fees
   and slippage).

Every order and ping carries the slot's equity. The process measures the **daily loss** against
the last equity it saw before the UTC day began (halt until the next day) and the **total
drawdown** against the highest equity seen (halt until the operator resumes). `resume` restarts
both references from the last equity, so the same loss does not halt again.

## Invariants

- **Fail closed.** A slot that cannot reach the process refuses every order that adds risk.
- **The envelope is read, never written.**
- **Halts survive restarts** (`state.json`, written atomically after every request).
- **Resume is the operator's.** `halt` may come from anything; only the CLI sends `resume`.
- **Alerts are best effort** (the `Notifier` port); they never fail a request.
- **Short socket path.** macOS limits a Unix socket path to 103 bytes: keep `--data` short
  (the default `data/`, run from the repo root, is fine).

## Not yet

NATS instead of the local socket (with the server move); positions read from venues instead of
the slot's own book; per-position, correlation and liquidity limits; flattening on a halt;
heartbeat-based halts when a slot or its data goes stale.

## Test

```bash
moon run risk:test
```
