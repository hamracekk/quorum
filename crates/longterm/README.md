# quorum-longterm (`longterm`)

The long-term book (DESIGN §20): buy-and-hold investing kept apart from the trading slots. A
policy (target weights, a monthly contribution, a rebalancing band) runs on the one engine; fills
become tax lots in CZK. It runs over history (`quorum longterm backtest`) and on paper
(`quorum longterm paper`, `runner::longterm`), and an AI review proposes changes that are costed,
decided by the operator and scored against keeping the book unchanged.

## Module map

| File | What it holds |
|---|---|
| `policy.rs` | `LongTermConfig` (`config/longterm.toml`), `Policy`, `plan` (start: every target; a contribution: the holdings furthest below target, nothing sold; otherwise a full rebalance only on a band breach), `PolicyStrategy` (the policy through `Strategy::on_book`) |
| `ledger.rs` | `Ledger`: tax lots in CZK at the ČNB rate of each fill, FIFO; the three-year time test; `tax_by_year` (15% of each year's non-exempt net gain); `if_sold` (taxable vs tax-free gain, value turning tax-free within a year) |
| `sim.rs` | `drive`: a policy's bars through an engine from a date — the start amount on the first close, a contribution each month (CZK at ČNB rates, `Engine::deposit`), lots from the fills; the paper book calls it with a gated broker. `report` (money-weighted `xirr`, time-weighted, drawdown of a unit of the book), `simulate` (history on a plain engine), `engine_config` |
| `review.rs` | The review: `ReviewConfig` (`[review]`), `Facts` (what the reviewer sees), `switch_cost` (turnover, trading cost, tax on lots under three years, FIFO), `score` (the proposal bought and held after its cost vs the book unchanged), `ReviewJournal` (`reviews.jsonl`, `decisions.jsonl`), `due` |
| `tests/longterm.rs` | The planner, FIFO and exemption, the money-weighted return, a whole run that never sells inside the band; switch costs, scoring, the review journal |

## Invariants

- **Separate from the slots:** its own config and capital; nothing here feeds a slot or the
  allocator.
- **One engine:** policies run through `core::Engine` (`deposit`, `Strategy::on_book`), never a
  loop of their own.
- **Rules, not predictions:** contributions only buy; sales happen only on a band breach, and lots
  are sold oldest first.
- **CZK at ČNB rates**, as Czech tax counts it; to be confirmed with a tax advisor before real
  money.

- **The review only proposes.** A proposal is a whole set of target weights over the policy's
  instruments and those pricing the allowed UCITS funds; it is costed when made, journaled, and
  scored later against the book unchanged. Accepting it is the operator's edit of
  `config/longterm.toml`.

## Not yet

UCITS prices from IBKR (US ETFs stand in), lot-aware selling (the newest lots first when that
saves tax), applying an accepted proposal inside the running session (today it needs `--fresh`).

## Test

```bash
moon run longterm:test
```
