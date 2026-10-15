# quorum-recorder (`recorder`)

The **forward recorder** (DESIGN §6, P1). It records market structure that has little or no
public history, so that a year from now it is a data advantage:

| Feed | Source (Binance, public, keyless) | Cadence | Why |
|---|---|---|---|
| `liquidations` | futures websocket `/market/ws/!forceOrder@arr` (every perpetual; the largest per symbol per second) | as they happen | no REST history at all |
| `book` | spot `/api/v3/depth` (1000 levels), summarized | every `book_every_secs` | no history at all |
| `derivs` | `/fapi/v1/premiumIndex` + `/fapi/v1/openInterest` | every `derivs_every_secs` | minute-level basis, funding estimate and OI, stamped with when we knew them |

## Module map

| File | What it holds |
|---|---|
| `config.rs` | `RecorderConfig` (`config/recorder.toml`): slots and symbols to record, intervals, `resolve` → symbols |
| `store.rs` | `RecordStore`: append-only `<data>/recorder/<feed>/<YYYY-MM-DD>.jsonl`, `read`, the recorder lock |
| `run.rs` | `record`: first samples at start, then polls on intervals and liquidations as they come; `Feeds`, `Status`, `Outcome` |
| `tests/record.rs` | Fakes end to end: every feed, gap markers, stop markers, switched-off feeds, the lock |

Types, ports and the book summary math live in `core::recorder` (`Record`, `BookFeed`,
`DerivsFeed`, `LiquidationFeed`, `summarize_book`); the Binance adapters in `data`.

## File format

One JSON object per line, tagged by `kind`: `liquidation`, `book`, `derivs`, `marker`. Every
line has `known_time` (when quorum received it) and, except markers, `event_time` (the
venue's time; for spot depth, which has none, the response time). The day in the file name is
the UTC date of `known_time`.

```json
{"kind":"marker","known_time":"…","feed":"liquidations","state":"down","detail":"connection reset"}
{"kind":"book","known_time":"…","symbol":"BTCUSDT","mid":…,"spread_bps":…,"bids":[{"pct":0.1,"notional":…},…],"imbalance_1pct":…}
```

## Invariants

- **Gaps are explicit.** `start`/`stop` per feed and run, `up`/`down` per stream connection,
  `error` per failed poll. Research must treat time between `down` and `up` (or with no
  `start`) as unknown, never as "no liquidations".
- **Depth beyond the snapshot is unknown** (`notional: null`), not zero.
- **Append only**, one write per line; a crash loses at most the line being written.
- **One recorder per data directory** (`<data>/recorder/LOCK`).
- **Its own process.** It never touches trading; trading never waits for it.
- Polls stay far inside Binance's request weights (minimum interval 60 s).

## Test

```bash
moon run recorder:test
```
