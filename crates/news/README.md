# quorum-news (`news`)

News ingestion (M5a): RSS 0.x/1.0/2.0, Atom and JSON Feed — crypto news sites and SEC EDGAR
filing feeds — turned into sanitized `NewsItem`s behind the `NewsSource` port (`quorum_core::ports`).
The service tags items with configured instruments and stores them in the journal
(`news_items`, migration 0006); M5b extracts typed events from them with an LLM.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `RssSource` (HTTP fetch with timeout, size cap, User-Agent) implementing `NewsSource`; `parse_feed` (pure: bytes → items); `NewsError` |
| `sanitize.rs` | `plain_text` (tags, `script`/`style` blocks and entities removed, whitespace collapsed, word-boundary truncation) and `canonical_url` (http(s) only, no fragment, no tracking parameters) |
| `tests.rs` | RSS and Atom (EDGAR-shaped) fixtures with hostile content, bounds, HTTP via a local mock server |

## Rules — feed content is untrusted

- **Data, never instructions.** Titles and summaries are plain text, capped (`MAX_TITLE`,
  `MAX_SUMMARY`); markup and scripts are dropped. Text like "ignore previous instructions" stays
  inert data — and must stay so in every consumer (dashboard, MCP, LLM prompts in M5b).
- **Links are http(s) only** and canonicalized; entries without a usable title or link are dropped.
  Nothing inside an item is ever fetched, followed or executed.
- **Bounded work:** 20 s timeout, at most `MAX_BYTES` per response and `MAX_ENTRIES` per fetch.
- **No keys.** Public feeds only. SEC EDGAR requires a descriptive User-Agent with a contact
  address: set `user_agent` on that source.
- Deduplication (same canonical URL; same headline within 48 h across feeds) happens in the store.

## Testing

```bash
moon run news:test         # or: cargo test -p quorum-news
```
