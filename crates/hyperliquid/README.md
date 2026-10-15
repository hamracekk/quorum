# quorum-hyperliquid

Followed traders, read only (SRC-3): Hyperliquid behind the `TraderSource` port.

- **Account** (`clearinghouseState`): account value and open perp positions (coin, signed size,
  entry price, value, unrealised PnL, leverage), largest first.
- **Fills** (`userFillsByTime`): pages of 2000, at most 10 pages per request (Hyperliquid keeps
  about the latest 10k fills per account). `opens` marks fills that open or flip into a position:
  `Open Long`/`Short > Long` → long, `Open Short`/`Long > Short` → short; closes, settlements and
  spot trades are not calls.
- **Leaderboard** (`stats-data.hyperliquid.xyz/Mainnet/leaderboard`, ~40 MB, up to 128 MB
  accepted): address, display name (sanitised), account value, day/week/month/all-time PnL, ROI and
  volume; rows with a bad address are skipped.
- **Public API, no key:** nothing is signed or sent; trader cannot trade on Hyperliquid.
- **Rate limits:** the info API weighs requests (fills pages are heavy); a 429 is retried after 2 s
  and 4 s.
- **Untrusted input:** addresses must be `0x` + 40 hex digits (lower-cased); numbers are parsed
  strictly and unparseable rows skipped; responses are capped at 10 MB.

Turning fills into calls is `quorum_core::traders::calls`; mapping coins to instruments and
scoring is the service layer (not copied; rebuilt in quorum).

| File | What |
|---|---|
| `src/lib.rs` | `HyperliquidSource`, `address` (validate), `opens` (direction of a fill), response types |
| `src/tests.rs` | Wiremock tests: account, paged fills, directions, rate-limit retry, addresses |
