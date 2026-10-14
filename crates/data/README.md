# trader-data (`data`)

Adapters for market data and bar storage, plus the incremental sync that connects them.
Implements the `core` ports; nothing outside this crate knows about Binance or Parquet.

## Module map

| File | What it holds |
|---|---|
| `binance.rs` | `BinanceSpot`: async `MarketData` adapter for Binance spot klines (public REST, no key). Pages 1000 bars/request, drops still-forming bars, `end` exclusive, 200 ms between pages |
| `binance_archive.rs` | R-1: Binance public data archive (data.binance.vision) — paged S3 listing of every spot symbol ever listed, monthly kline zips (CSV, ms or µs times) for delisted coins |
| `binance_futures.rs` | E-2: `BinanceFutures`, public keyless USDT-perpetual funding-rate history (`/fapi/v1/fundingRate`), paged |
| `binance_announcements.rs` | RB-4: Binance's public delisting announcements (CMS catalog 161), titles parsed to tickers only |
| `quality.rs` | RB-6: data-quality checks — missing bars, impossible candles, non-positive prices, outlier moves, dead feeds |
| `indicators.rs` | Public, keyless market-wide series: Deribit DVOL, `DefiLlama` stablecoin supply, Coinbase daily closes (cross-check); `binance_archive.rs` also reads the futures metrics archive (open interest, long/short ratio) |
| `binance_testnet.rs` | T-7: `BinanceTestnet`, signed (HMAC-SHA256) spot client for **testnet only** — refuses every host but `testnet.binance.vision` (and loopback mocks in tests); account (permissions, balances), `exchangeInfo` rules (step size, min notional), market orders with a client id, order lookup; redacted credentials |
| `binance_ws.rs` | `BinanceStream`: `MarketStream` over the public websocket; closed klines only; reconnect with exponential backoff (tested against a local websocket server) |
| `store.rs` | `ParquetStore`: `BarStore` on Parquet; `read_parquet` / `write_parquet` (atomic temp-file + rename); `migrate_flat_layout` for M1 files |
| `lib.rs` | `sync(source, store, instrument, interval, since_if_empty, until)`: fetches only bars newer than the last stored one |
| `error.rs` | `DataError` (detailed); wrapped into `PortError` at the port boundary |

## Storage format

- Path: `<data dir>/<venue>/<SYMBOL>-<interval>[-<kind>].parquet` (no suffix for spot), e.g.
  `data/binance/BTCUSDT-1h.parquet`, `data/hyperliquid/BTC-4h-perp.parquet`.
- Columns: `ts` (timestamp ms, UTC, bar **open** time), `open`, `high`, `low`, `close`, `volume`
  (f64, non-null). Snappy compression.
- Rows sorted by `ts`, no duplicates — verified on read (`DataError::Corrupt` otherwise).
- Intervals must be alphanumeric (`1h`, `15m`) — they are part of the file name.

## Rules

- Adapters refuse what they don't serve (`PortError::Unsupported`): `BinanceSpot` only accepts
  `binance:*` spot instruments.
- Never return a bar that is still forming or at/after `end`.
- All writes are atomic; a crash never leaves a half-written file under the real name.
- Stay inside exchange rate limits (sleep between pages).

## Adding a market data adapter

1. New module (or crate if it pulls heavy deps) with a struct implementing `MarketData`;
   `venue()` returns its `VenueId`; map its errors into `PortError::adapter(context, err)`.
2. Support `with_base_url` (or equivalent) so tests can point it at a mock server.
3. Tests with `wiremock`: paging, forming-bar filtering, end-exclusive, HTTP errors, refusing
   other venues/kinds — mirror `binance.rs`.
4. Register the `kind` in `crates/cli/src/registry.rs` and document it in `config/default.toml`.

## Testing

```bash
moon run data:test        # or: cargo test -p trader-data
```

No test touches the real network: Binance tests use a local `wiremock` server, sync tests use
`trader_core::fakes`, store tests use per-test temp directories.
