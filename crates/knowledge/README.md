# quorum-knowledge (`knowledge`)

The **knowledge plane** (DESIGN §8): news feeds in, typed and scored events out. Port of
trader's M5 orchestration, with a cloud first pass instead of a local 8B model.

## Module map

| File | What it holds |
|---|---|
| `config.rs` | `NewsConfig` (`config/news.toml`: poll, slots, aliases, sources, tiers, budgets), `TierConfig`, `Tier` (journaled as `local` = first pass, `remote` = escalation), `load_impact` (`config/impact.toml`) |
| `fetch.rs` | `fetch` (every source, a failing one never stops the rest), `tag` (whole-word ticker / coin name / alias match: a label, not a signal), `base_asset` |
| `extract.rs` | `extract`: first pass on every new item, escalation for items read at ≥ `escalate` magnitude, impact-map themes on the first-pass model; a daily token budget per tier; `final_readings` (escalation over first pass), `escalates`, `assets` |
| `tests/knowledge.rs` | Fakes + in-memory journal: tagging, failing sources, both tiers, the cache, budgets, theme implications across markets |

## Invariants

- **News is untrusted.** Items reach a model only as inert data (`quorum_ai::extract`); answers
  are validated against a closed schema and rejected whole otherwise.
- **A reading is data, never an action.** It is journaled for scoring; nothing here can place,
  size or change an order.
- **Every attempt is journaled**, which is also the cache: an item is read once per tier and
  prompt version (a failed attempt is retried, a rejected one is not).
- **Each tier stops at its daily token budget** (`daily_tokens`), counted per UTC day.
- **The impact map is the operator's.** Models classify items into its themes; they never add or
  change a link.

## Not yet (next P4 slice)

Scoring per event kind and per impact link (`lab::scoreboard`), the priced-in check (how far
the price moved before quorum received the item), prompt and model records.

## Test

```bash
moon run knowledge:test
```
