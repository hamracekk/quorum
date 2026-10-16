# quorum-evaluator (`evaluator`)

The trade evaluator (DESIGN §10), **slice 1: shadow**. It builds a case file for every entry a
strategy proposes, labels each with how it turned out — **taken or not** (counterfactual
tracking, §10.3) — and studies whether anything in the case files tells winners from losers out
of sample. Nothing here changes a trade: the size multiplier stays 1.0 until a meta model beats
"always take" out of sample.

## Module map

| File | What it holds |
|---|---|
| `case.rs` | `Case`, `CaseConfig`, `cases`: entries from the engine's `Target` events (flat → position, or a flip), point-in-time features, labels at `HORIZONS` and at the strategy's own `EXIT` |
| `study.rs` | `study`: base rates per label and regime, regime skips (a regime's entries skipped if they lost in sample, judged out of sample with a threshold corrected for the regimes tested), the "always take" baseline in and out of sample, per-feature AUC with the direction fixed in sample, Bonferroni over features tested |
| `tests/evaluator.rs` | Labels after costs, counterfactuals, no look-ahead in features, flips, the warm-up of a paper session, AUC, study verdicts |

## A case

- **When:** the bar whose close produced the entry (`ts`); it fills at the next bar's open.
- **Features (bars up to `ts` only):** `f:<factor>` from the price family, `rank:<factor>`
  across the slot's instruments, `fit:held` (positions opened on an earlier bar) and `fit:corr`
  (mean 60-bar return correlation to them); the market's regime.
- **Labels (the future, after the round-trip cost, signed by direction):** `h1`, `h5`, `h20`
  bars after the fill, and `exit` at the strategy's own exit. Missing until the bars exist.
- **Taken:** a fill on the entry bar. A paper session uses its journaled fills, so orders the
  risk process refused show up as untaken cases that are still followed.
- **A paper session's warm-up** only prepares the strategy: what it leaves is bought on the
  first traded bar, so each such position is one case decided on the bar before it.

## Invariants

- **Shadow only.** Reads decisions and bars; never feeds a target, order or size.
- **No look-ahead in features**, only in labels (test `features_use_only_bars_up_to_the_decision`).
- **Every entry is labelled, taken or not.**
- **Judged like factors:** the direction is fixed in sample, read out of sample (latest 30% of
  closed cases), and must clear a one-sided z corrected for every feature tested.
- **Rebuildable.** Cases are derived from stored bars and decisions on every run; there is no
  evaluator state to drift.

## Not yet

News features (readings started 2026-10-16), the meta model (gradient-boosted trees,
calibrated), Claude case review (§10.2), portfolio-fit sizing (§10.4), and scoring the
evaluator's verdicts against its own counterfactuals once it decides anything.

## Test

```bash
moon run evaluator:test
```
