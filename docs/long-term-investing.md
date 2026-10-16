# Long-term investing: evidence and a separate long-term book

Research notes (October 2026) and the design sketch for a **long-term book**: buy-and-hold
investments evaluated with AI, kept **separate from the trading slots**. Planned, not built
(DESIGN §20).

> General education from published historical data, not personal investment advice. Tax points
> are as understood in October 2026 and must be confirmed with a Czech tax advisor
> (daňový poradce) before real money. Items marked (~) come from secondary sources.

## 1. What has paid over the long run

World, 1900–2024, annual, after inflation (DMS / UBS Global Investment Returns Yearbook 2025;
Jordà et al. 2019; Damodaran 1928–2025):

| Asset | Real return | What the bad times looked like |
|---|---|---|
| Global equities | ~5.2% (3.5% since 2000) | −50…60% falls; about a decade to recover in real terms after 2000 |
| US equities | ~6.6% — the best market of the century, so it flatters | −79% real 1929–32; 13 years without a new nominal high after 2000 |
| Developed ex-US equities | ~4.3% | Japan: 34 years (1989 → 2024) to regain its price peak |
| Emerging markets | ~10.5% nominal USD since 1987 (MSCI) | −65% in 2007–08; ~15 years below the 2007 peak |
| Government bonds | ~1.7% | Long Treasuries −48% in 2020–23; near −50% real in the 1970s |
| Cash / T-bills | ~0.5% | Below inflation for long stretches (1940s, 1970s) |
| Housing | ~6–7% (before costs, taxes, leverage; not investable directly) | REITs behave like equities: −75% in 2007–09 |
| Gold | ~0–2% | −83% real after 1980; ~45 years to regain the real peak (~) |
| Bitcoin | Very high, but ~15 years of data | −75…93% in every cycle so far |

**Forecasts for the next 10 years** (not history; nominal USD): J.P. Morgan 2026 LTCMA — global
equities 7.0%, US large cap 6.7%, EM 7.8%, 60/40 6.4%; Vanguard (Jan 2026) — US equities ~4–6%,
ex-US ~5–7%, high-quality bonds ~4–5%; BlackRock (2025) — MSCI USA 5.8%, World ex-US 7.3%. All
expect US equities below their history and bonds close to equities.

## 2. What the evidence agrees improves outcomes

1. **Costs.** 84% of US large-cap active funds lagged their index over 10 years, 98% of euro
   global equity funds (SPIVA). A 1%/year fee costs ~25% of final wealth over 30 years.
2. **Diversify globally.** A single country can stall for decades (Japan); US-only history
   flatters.
3. **The stock/bond mix sets the ride.** US 1926–2015: 100/0 averaged 10.1%/yr (worst year
   −43%), 60/40 8.7% (−27%). Rebalance about once a year or on bands; more often only adds
   turnover.
4. **Stay invested.** Investors earn ~1.2 points/yr less than their funds (Morningstar *Mind the
   Gap* 2025) from timing and chasing. Lump sum beats spreading purchases ~2/3 of the time
   (Vanguard).
5. **Factor tilts** (value, momentum, quality) are real but ~30–58% smaller after publication and
   can lag for 10+ years (US value −55% vs growth, 2007–2020).
6. **Withdrawals:** a 4% rate was "safe" historically in the US, in only 4 of 17 countries
   (Pfau 2010); fees cut it sharply.
7. **Gold and crypto** are weak or unproven diversifiers and do not compound over the long run.

## 3. Holding it from the Czech Republic

- **Instruments:** UCITS ETFs (US-listed ETFs are not sold to EU retail). Accumulating,
  Irish-domiciled funds pay 15% US withholding at fund level and nothing yearly at the investor.
  Examples (check before use): VWCE `IE00BK5BQT80` FTSE All-World 0.14%; IWDA `IE00B4L5Y983` MSCI
  World 0.20%; CSPX `IE00B5BMR087` S&P 500 0.07%; SPYI `IE00B3YLTY66` MSCI ACWI IMI 0.17%; AGGH
  `IE00BDBRDM35` global bonds EUR-hedged 0.10%; XEON `LU0290358497` € overnight ~0.10%; SGLN
  `IE00B4ND3602` physical gold (~).
- **Czech tax (ZDP § 4, § 10):**
  - Securities held **over 3 years are exempt**; the 40M CZK cap applied to 2025 sales only and
    was removed for securities from 2026 (it stays for crypto).
  - Value test: exempt if **gross proceeds** from securities in the year are ≤ 100,000 CZK.
  - Otherwise 15% (23% on the tax base above 36× the average wage: 1,762,812 CZK for 2026).
  - Accumulating funds defer tax to the sale — and after 3 years remove it.
  - Exempt income above 5M CZK must still be reported (§ 38v).
  - Crypto (from 2025): 3-year test and the 100k value test (not for stablecoins); 40M cap.
- **DIP (dlouhodobý investiční produkt):** up to 48,000 CZK/year deductible (saves up to 7,200
  CZK), shared with pension savings and qualifying insurance; payout from age 60 and after 10
  years. Providers include Fio, Portu, Fondee, Patria (not XTB or IBKR as of 2026).
- **Czech state anti-inflation bonds** (dluhopisy.gov.cz): CZK, no currency risk, issued in
  subscription windows.
- **Brokers:** Trading 212 (0% commission, 0.15% FX), XTB (0% to €100k/month, 0.5% FX), IBKR
  (~0.05% per trade, very cheap FX), Fio (DIP; costly FX on US shares). For small monthly
  deposits the FX cost matters most.

**Open questions for the advisor:** whether exempt sales count toward the 100k value test; the
exact 2026 cap repeal; foreign fund distributions (separate base at 15%); FIFO vs average cost
and the CZK exchange rate; DIP clawback and the shared limit; whether synthetic funds (XEON) and
ETCs (SGLN, debt securities) get the same time test.

## 4. What this means for quorum

The passive book is the bar (`config/benchmark.toml`, CLAUDE.md): a cheap global
equity-and-bond ETF portfolio has returned ~5% a year after inflation over decades, needs no
work, and in CZ is tax-free after 3 years. The trading slots pay 15% of each year's net gain.
So the comparison should:

- add a **global all-world** benchmark (the US-only one is the most flattering to beat), and
- add a **tax-adjusted view**: 15% of each year's net realized gain for the slots, 0% for a
  passive fund held over 3 years.

## 5. Design sketch: the long-term book (planned)

A separate part of quorum that **holds** a diversified ETF portfolio for years, with AI as a
reviewer — not a trader.

**Separate from the slots**
- Its own capital in the envelope (`[long_term]`: capital, allowed instruments, max turnover per
  year), never shared with or borrowed by trading slots; the allocator does not move money
  between the two.
- Its own account at the broker (or a DIP), so its tax lots and 3-year clocks are not mixed with
  trading.
- The risk process still approves every order; the long-term book adds no new path to a venue.

**How it runs**
- A **policy** the operator writes: target weights (e.g. global equity / bonds / gold), allowed
  UCITS funds by ISIN, rebalance bands, contribution schedule.
- **Rules, not predictions,** move money: new contributions buy what is furthest below target;
  sells happen only when a band is breached, preferring lots older than 3 years (tax-free) and
  never a lot about to turn 3 years old.
- Tracks every lot (date, cost in CZK at the ČNB rate) and shows what selling would cost in tax
  today.

**AI evaluation** (quarterly; Claude through `quorum_ai`, closed schema, journaled, scored)
- Reviews the *holdings*, not the market: fund costs (TER) and tracking difference, a cheaper or
  better-domiciled equivalent, index changes, concentration (e.g. US share of "world"),
  currency exposure, institutions' capital-market assumptions vs the policy's weights.
- Outputs **proposals only** — "switch fund X to Y", "policy drift: consider moving bonds from A
  to B" — each with the tax cost of acting. The operator decides; nothing trades on a proposal.
- Scored like everything else: each proposal is followed as a counterfactual against "keep the
  policy unchanged", net of costs and tax. A reviewer that does not beat doing nothing earns no
  weight and only writes explanations.

**Measured against**
- The same policy held with no changes (did anything we did help?), and the passive benchmarks.
- Reported in CZK, after costs and tax.

**Prerequisites:** an EU broker data/execution adapter for UCITS prices (IBKR), CZK conversion at
ČNB rates, lot accounting, the envelope section, and the tax confirmations above. Paper first,
like everything else.
