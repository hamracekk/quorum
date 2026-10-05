# trader-lab (`lab`)

Research tooling on top of the backtester: parameter sweeps (in-sample) and walk-forward
evaluation (out-of-sample). Later milestones add the strategy tournament and self-improvement
loop here (M9).

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `StrategyFactory<P>` (any `Fn(&P) -> Box<dyn Strategy>`), `slice_all` helper |
| `sweep.rs` | `sweep(grid, factory, series, cfg)`: backtest every parameter set, sorted by Sharpe |
| `carry.rs` | ADR 0007: funding-carry simulation (long spot + short perpetual): coins whose trailing funding clears `min_annual`, equal notional, funding per print, both legs' costs, margin, 20% resize band; yearly returns |
| `walk_forward.rs` | `walk_forward(...)`: rolling train/test windows, stitched out-of-sample `Report`; `walk_forward_with(...)` lets a parameter set also change the backtest config (per-trade stops chosen with the SMA pair) |
| `signals.rs` | Event study: bias-signed forward returns (next open → close h bars later) per signal, baseline, net of costs, t-stat, in-sample vs out-of-sample, verdict |
| `scoreboard.rs` | Scoreboard: directional occurrences per source traded from the first open after they were known, at time horizons; mean, hit rate, net, t, IC (Spearman), pending outcomes, recent-window decay; baselines `always_long` and `random`; excess over the drift (`mean − bias × always-long mean`) and its t decide the verdict (edge / no_edge / collecting / too_few); a source's `since` keeps earlier occurrences (why it was picked) out of the record, reported as `in_sample` |
| `tournament.rs` | Section 5 rules: `review(standings, rules)` → promote the best challenger that beat the champion's Sharpe without a worse drawdown on enough days and trades; retire one trailing buy & hold at consecutive reviews |
| `error.rs` | `LabError`: empty grid, empty window, not enough data |

## How walk-forward works

For each window: backtest every parameter set on `train_bars` timestamps → pick the best Sharpe →
trade it on the next `test_bars` timestamps, with the training bars used only as indicator
warm-up (`core::run_from`). Windows advance by `test_bars`, so test periods never overlap.
Test windows are chained (compounded) into one out-of-sample equity curve. Only full windows are
evaluated; a partial tail is skipped.

## Invariants

- **No test bar ever influences parameter choice**, and **later data can never change earlier
  windows** — guarded by `later_data_cannot_change_earlier_windows` and
  `chosen_params_score_best_on_their_training_window`. Keep both passing.
- Sweeps are labelled in-sample everywhere they are shown; they overfit by design.
- Pure computation: no I/O, deterministic.

## Extending

- New objective (e.g. Sortino, return/drawdown): make the selection metric a parameter rather than
  hard-coding Sharpe; test that selection follows the objective.
- New parameter type: any `P: Clone`; supply a factory closure. The CLI's `sma_factory` is the example.

## Testing

```bash
moon run lab:test         # or: cargo test -p trader-lab
```

`walk_forward::tests::trending_series` is a deterministic synthetic price series shared by the
lab tests.
