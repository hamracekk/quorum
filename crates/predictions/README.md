# trader-predictions

Prediction markets, read only (SRC-2): Kalshi behind the `PredictionSource` port.

- **Kalshi** (`KalshiSource`): `GET /markets/{ticker}` and `/events/{event}` for the question,
  status, close time and implied probability (last trade, else the bid/ask mid);
  `/series/{series}/markets/{ticker}/candlesticks` for hourly history, in chunks of 4000 hours.
  Public API: no key, no account, nothing that could place an order.
- **Untrusted input:** questions are stripped of control characters and capped at 300 characters;
  tickers (or a pasted market link, of which the last path segment is used) must be 3–80
  characters of `A–Z 0–9 . -` starting with a letter or digit before they reach a URL path;
  responses are capped at 5 MB; probabilities outside 0–1 are dropped.
- **Polymarket is not implemented:** it is blocked on the operator's connection (Czech DNS block of
  unlicensed gambling) and trader does not work around such blocks (ADR 0004).

Turning probabilities into calls is `trader_core::predictions::moves`; scoring them is the
scoreboard's job (`trader-service::predictions`).

| File | What |
|---|---|
| `src/lib.rs` | `KalshiSource`, `ticker` (normalise and validate), response types, error type |
| `src/tests.rs` | Wiremock tests: market, history (trades, quotes, crossed quotes, sorting), errors, tickers |
