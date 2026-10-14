# crates/ml (`trader-ml`)

Machine-learned signal sources (M9). Models are `trader_core::strategies::Learner`s: they plug
into the cross-sectional scaffold (`XsLearner`), which labels each weekly snapshot of features
with the return realised by the next rebalance and trains only on labelled snapshots, so a model
never learns from the future. Every model is judged like any strategy — walk-forward, out of
sample, after costs, with PBO — against the linear benchmark `xs-ridge`.

## Module map

| File | What |
|---|---|
| `gbdt.rs` | `Gbdt`: gradient-boosted regression trees (squared loss, histogram splits at training quantiles, `min_leaf`, shrinkage, labels winsorised at 2.5/97.5%); `BoostParams`; `XsBoost::new` → the `xs-boost` strategy |

## Invariants

- **No look-ahead:** models only see what `XsLearner` gives them (labelled snapshots); a test
  checks that later bars cannot change earlier targets.
- **Deterministic:** no randomness, ties broken by order — the same bars give the same trees, so
  replays and the paper replay check hold.
- **No I/O, no new dependencies:** pure computation on `trader-core` types.
- A model earns nothing by existing: it is a strategy kind (`[strategy] kind = "xs-boost"`)
  for backtests, walk-forward and the tournament; paper changes only through the tournament.

## Extending

Add a `Learner` (fit on `(Row, f64)` samples, predict a `Row`), a constructor returning
`XsLearner`, a `StrategyKind` in `service::settings`, and the two match arms in `service::ops`
(`build_inner`, the walk-forward factory). Compare it walk-forward with `xs-ridge` before
anything else.

## Testing

`cargo test -p trader-ml`: trees learn a U-shape a line cannot, the same data gives bit-identical
trees, too few samples fit nothing, one extreme label does not move predictions, and as a
strategy it holds the rising coins without look-ahead.
