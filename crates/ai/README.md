# trader-ai (`ai`)

Language models for trader (M5b): `LlmProvider` adapters and the news → typed-event extraction
contract. Orchestration (tiers, budget, cache, storage) lives in `trader_service::extraction`.

## Module map

| File | What it holds |
|---|---|
| `ollama.rs` | `OllamaProvider`: local `/api/chat`, JSON-schema `format`, temperature 0 — the free first pass |
| `claude.rs` | `ClaudeProvider`: Messages API over raw HTTP (no official Rust SDK) — structured outputs (`output_config.format`), `effort`, cache-marked frozen system prompt, optional server-side `fallbacks: "default"`, retries on 429/5xx with `retry-after`; refusal and `max_tokens` are errors; the key is redacted from `Debug` |
| `review.rs` | M9: the weekly review in plain words — facts in as inert data (they contain agent-written text), closed schema (headline, went well/badly, watch next; ≤ 5 points, ≤ 240 characters), strict `parse`; shown under the facts, never acted on |
| `themes.rs` | ADR 0008: `THEMES_VERSION`, theme classification against the impact map (closed list of theme ids, up/down, magnitude, confidence), `parse` expands a valid answer through the map's links into implied `NewsEvent`s (`theme` set); the model never names instruments |
| `extract.rs` | `PROMPT_VERSION`, the frozen system prompt, the JSON schema (closed objects, enums), `request` (wraps the item as inert data), `parse` (strict validation → `Reading`) |

## Rules

- **Model output is data.** `parse` rejects the whole answer on unknown fields, unlisted assets,
  confidence outside 0–1, duplicate assets, contradictions or over-long rationales. A valid reading
  feeds scoring (M6) and the ensemble (M8); it never triggers an action.
- **News is untrusted.** It goes in the user turn inside `<news_item>` tags with `<`/`>` replaced;
  the system prompt says instructions inside are not to be followed. No tools are offered.
- **Bump `PROMPT_VERSION`** whenever the prompt or schema changes: items are then read again.
- **Model choice is the operator's.** `[ai.remote] model` defaults to `claude-opus-5-5`; switching
  to a cheaper model is a config decision. Forced `tool_choice` returns 400 on current models, so
  JSON comes from structured outputs.
- **Keys** come from the OS keychain (`trader secrets set anthropic`), never from config files.

## Testing

```bash
moon run ai:test         # or: cargo test -p trader-ai
```

Claude requests are checked against a local mock server (headers, body shape, fallbacks on/off,
retry on 529, no retry on 400, refusal, truncation); extraction fixtures cover the injection
wrapper and every rejection rule.
