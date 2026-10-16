# quorum-longterm (`longterm`)

The long-term book (DESIGN §20): buy-and-hold investing kept apart from the trading slots. A
policy (target weights, a monthly contribution, a rebalancing band) runs on the one engine; fills
become tax lots in CZK. Foundation only: history runs today, paper running and the AI review of
the holdings come next.

## Module map

| File | What it holds |
|---|---|
| `policy.rs` | `LongTermConfig` (`config/longterm.toml`), `Policy`, `plan` (start: every target; a contribution: the holdings furthest below target, nothing sold; otherwise a full rebalance only on a band breach), `PolicyStrategy` (the policy through `Strategy::on_book`) |
| `ledger.rs` | `Ledger`: tax lots in CZK at the ČNB rate of each fill, FIFO; the three-year time test; `tax_by_year` (15% of each year's non-exempt net gain); `if_sold` (taxable vs tax-free gain, value turning tax-free within a year) |
| `sim.rs` | `simulate`: a policy over history in CZK on `core::Engine` — start amount and monthly contributions converted at ČNB rates (`Engine::deposit`), lots from the fills; money-weighted (`xirr`) and time-weighted results, drawdown of a unit of the book |
| `tests/longterm.rs` | The planner, FIFO and exemption, the money-weighted return, a whole run that never sells inside the band |

## Invariants

- **Separate from the slots:** its own config and capital; nothing here feeds a slot or the
  allocator.
- **One engine:** policies run through `core::Engine` (`deposit`, `Strategy::on_book`), never a
  loop of their own.
- **Rules, not predictions:** contributions only buy; sales happen only on a band breach, and lots
  are sold oldest first.
- **CZK at ČNB rates**, as Czech tax counts it; to be confirmed with a tax advisor before real
  money.

## Not yet

Paper running with real contributions, UCITS prices from IBKR (US ETFs stand in), lot-aware
selling (the newest lots first when that saves tax), the quarterly AI review of the holdings.

## Test

```bash
moon run longterm:test
```
