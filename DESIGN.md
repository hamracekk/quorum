# quorum — design

An autonomous, multi-market trading system. It runs several **slots** (one per market and
venue), evaluates every proposed trade on many **factors** (price, flow, news, macro,
positioning, fundamentals, cost), allocates capital between slots on measured evidence, and
promotes or demotes itself inside an **envelope** the operator sets.

> **Status:** design, 2026-10-15. Nothing is built. Decisions on jurisdiction, envelope,
> hosting, code reuse and name: §19. Successor to
> [trader](../trader) (`hamracekk/llm-trader`), whose proven parts are ported, not rewritten.
> Not financial advice — the design assumes there is no guaranteed edge and is built to find out
> cheaply which edges are real.

---

## 1. Goals and non-goals

**Goals**

1. **Autonomous inside an envelope.** No human is needed to trade, re-weight, retrain, promote,
   demote or halt. A human is needed only to change the envelope (capital, loss limits, which
   slots may go live).
2. **Many markets, one system.** Crypto spot and perpetuals, US equities and ETFs, futures and
   FX, prediction markets — each a slot behind the same interfaces.
3. **Every trade judged on every relevant input.** Strategy, technicals, market structure, news,
   macro, positioning, fundamentals, portfolio fit and cost — combined by a model that is
   itself scored.
4. **Evidence decides.** Every factor, strategy, prompt, model and slot has an out-of-sample,
   after-cost record, and its weight follows that record.
5. **Runs 24/7**, observable from a dashboard and a phone. On the laptop at first, on a server
   before any live money (§19.3).

**Non-goals**

- An LLM that picks trades or forecasts prices. trader measured this: agent judgement had no
  record. LLMs read, classify, explain and research here.
- High-frequency trading. At this scale costs grow faster than any edge (trader §9b: the 1h
  strategy paid 31% of capital in costs).
- Copy-trading, social signals with no record, or anything that bypasses the risk process.
- Managing other people's money.

---

## 2. What trader taught us

The design follows the evidence from trader (PLAN §9, §9b, ADR 0007), not intuition.

| Finding | Consequence here |
|---|---|
| Every candlestick/indicator signal: **no edge** after costs | Signals are not the core; they are factors that must earn weight |
| Funding extremes were wrong-way at +1w (t −4.2) | Factors are scored, never assumed to mean what the textbook says |
| Gains came from **structure**: slower bars, more coins, vol targeting, a vol brake, carry | Strategies are structural; the portfolio across slots is the main edge |
| Costs dominate short horizons | Execution quality is a first-class subsystem with its own metrics |
| Overlapping outcomes + multiple testing produced a −47% "edge" | Deflated Sharpe, purged CV, independent outcomes are mandatory everywhere |
| `llama3.1:8b` extraction often truncated or off-schema | Tiered Claude models for news; local models only for cheap pre-filters |
| OI, liquidations, basis have ~no free history | A forward recorder runs from day one |
| Stayed in paper, single venue, on a laptop | Live-micro is designed in from the start; services are server-ready from P0, and a server is required before live money |

Ported from trader (proven, tested): the event-driven engine, event bus with deterministic
replay, journal, lab (walk-forward, sweeps, deflated Sharpe, PBO, purged CV), tournament,
point-in-time universe, risk brakes, news ingestion and sanitising, keychain secrets, MCP
server, notifications.

---

## 3. Principles

1. **Envelope, not approvals.** The operator sets hard bounds once. Inside them the system acts
   alone; outside them it cannot act at all.
2. **Asymmetric decision rights.** Anything may **reduce** risk at once (pause, shrink, flatten).
   Risk is **added** only by the allocator, only on evidence.
3. **One code path.** Backtest, replay, paper and live run the same strategies, evaluator,
   allocator and risk over the same event stream.
4. **Point in time, always.** Every value is stored with when it became *known to us*. Nothing
   uses data after the decision time; tests guard it.
5. **Everything earns its weight.** Factors, strategies, prompts, models, agents and slots start
   at zero weight and are scored out of sample, after costs, corrected for how many were tried.
6. **Track the road not taken.** Every rejected, reduced or delayed trade is followed to its
   hypothetical outcome, so filters are judged too.
7. **External content is data.** News, social text, model output and third-party tools are
   parsed into closed schemas or discarded. Nothing external reaches execution.
8. **Risk is a separate process.** A bug or a compromised component upstream cannot exceed the
   limits.

---

## 4. Architecture

```
┌──────────────────────── OPERATOR ─────────────────────────┐
│ ENVELOPE: total capital · per-slot caps · max daily/total │   dashboard · phone (ntfy) · MCP
│ loss · which slots may reach live · venues allowed        │
└─────────────────────────────┬─────────────────────────────┘
                              │ (signed config, human only)
 ┌──────────── DATA PLANE ────┴───────────┐     ┌──────── RESEARCH PLANE ────────┐
 │ market data · derivatives · order book │     │ agents propose hypotheses      │
 │ news · filings · macro · positioning   │     │ lab: walk-forward, deflated SR │
 │ forward recorder · point-in-time store │     │ stress tests · tournament      │
 └──────┬──────────────────────┬──────────┘     │ promotion ladder · post-mortem │
        ▼                      ▼                └───────────────┬────────────────┘
 ┌─ FACTOR STORE ─┐   ┌─ KNOWLEDGE PLANE ─┐                     │ scores, weights,
 │ per market,    │◄──│ news → typed      │                     │ promotions
 │ versioned,     │   │ events → impact   │                     ▼
 │ regime model   │   │ map → factors     │       ┌──────── LEARNING / RECORDS ────────┐
 └──────┬─────────┘   └───────────────────┘       │ every factor, strategy, prompt,    │
        │                                         │ model, agent, slot: a scored record│
        ▼                                         └────────────────────────────────────┘
 ┌──────────────────────────── SLOTS ────────────────────────────┐
 │ crypto · US equities · futures/FX · prediction markets        │
 │ each: universe · strategies · mode · risk budget · venue      │
 └──────┬────────────────────────────────────────────────────────┘
        │ proposed trades (direction, horizon, base size)
        ▼
 TRADE EVALUATOR ── meta-label P(success) · Claude case review · portfolio fit · cost check
        │ take / smaller / delay / reject  (+ counterfactual tracking of the rest)
        ▼
 ALLOCATOR ── capital across slots · risk budgets · regime · fractional Kelly
        │ target positions
        ▼
 RISK PROCESS (separate) ── envelope limits · brakes · kill switch
        │ approved orders
        ▼
 EXECUTION ── OMS/EMS per venue · order tactics · TCA · reconciliation ──► venues
        │ fills
        └──► event bus ──► journal ──► learning
```

### Data flow for one trade

1. A bar closes, or an event arrives. Factor values update in the factor store.
2. A strategy in a slot proposes a trade: instrument, direction, horizon, base size, reason.
3. The **trade evaluator** builds a case file (all factors, regime, recent events, the
   strategy's record, portfolio overlap, expected costs) and returns a verdict with a
   probability and a size multiplier.
4. The **allocator** turns verdicts from all slots into target positions inside each slot's
   risk budget.
5. The **risk process** clamps targets to the envelope and brakes, or halts.
6. **Execution** works the order, reports fills, and measures slippage against the decision
   price.
7. The **journal** links fill → verdict → factors → strategy. Outcomes at fixed horizons are
   scored later, for taken *and* untaken trades.

---

## 5. Core concepts

### 5.1 Envelope

The only thing the operator must set. Stored as a signed config file that the system can read
and never write. The values below are examples for when live money starts. The current
envelope is paper only (§19.2).

| Field | Example | Meaning |
|---|---|---|
| `capital_total` | 20,000 EUR | Most capital the system may ever deploy |
| `slot.<name>.capital_max` | 5,000 | Cap per slot |
| `slot.<name>.max_mode` | `live-micro` | Highest rung the slot may reach on its own |
| `loss.daily_max` | 5% | Breach (against the previous close) → flatten everything, halt until the next UTC day. 2% halted daily crypto 17×/yr and hurt; 5% is the shipped value |
| `loss.total_max` | 15% from peak | Breach → flatten, halt, require operator |
| `leverage_max` | 1.5× | Gross exposure / equity |
| `venues_allowed` | ibkr, a MiCA-authorised crypto exchange | Nothing else can be traded |

### 5.2 Slot

A self-contained trading unit for one market on one venue.

```rust
struct Slot {
    id: SlotId,                 // "crypto-spot", "equities-ibkr", ...
    venue: VenueId,
    universe: UniverseRule,     // point-in-time, e.g. top 25 by 60-day dollar volume
    strategies: Vec<StrategyId>,
    mode: Mode,                 // Research | Paper | LiveMicro | Live
    budget: RiskBudget,         // set by the allocator, capped by the envelope
    calendar: TradingCalendar,  // 24/7, NYSE hours, CME sessions, ...
    costs: CostModel,           // fees, spread, impact, funding / borrow
}
```

Initial slots (venues as decided for an EU/CZ operator in §19.1):

| Slot | Venue | Strategies to start | Notes |
|---|---|---|---|
| `crypto-spot` | MiCA-authorised exchange (Binance data) | trend-momentum, xs-momentum (from trader) | Proven in walk-forward; trader paper-trades it now |
| `crypto-carry` | EU-licensed perpetuals venue | funding carry (delta-neutral) | Needs two-leg execution; waits for a licensed venue |
| `equities` | IBKR | UCITS ETF trend / rotation, xs-momentum on single stocks | No US-domiciled ETFs for EU retail |
| `futures` | IBKR | multi-asset trend (indices, rates, energy, metals, FX) | Later; the classic diversifier |
| `prediction` | none (read-only) | none | Odds from Kalshi/Polymarket feed factors only |

### 5.3 Autonomy ladder

```
 Research ──► Paper ──► LiveMicro ──► Live
     ▲          │           │           │
     └──────────┴───────────┴───────────┘   demotion: automatic, immediate
```

| Promotion | Gate (all required) |
|---|---|
| Research → Paper | Walk-forward beats the slot's benchmark net of costs; deflated Sharpe ≥ 0.9 over every variant tried; PBO < 0.3 |
| Paper → LiveMicro | ≥ 8 weeks paper; paper record consistent with backtest (tracking error inside a band); all stress scenarios within loss limits; slot allowed by envelope |
| LiveMicro → Live | ≥ 12 weeks live-micro (1–5% of slot cap); realised slippage within 1.5× the cost model; zero reconciliation breaks; envelope allows `Live` |

| Demotion trigger | Action |
|---|---|
| Slot drawdown > its budget | Down one rung, flatten to the new rung's size |
| Drift alarm on the strategy (§13) | Down one rung |
| Realised costs > 2× model for 2 weeks | Down to Paper |
| Reconciliation break or venue error spike | Halt slot, alert |
| Envelope loss limit | Halt everything (§9) |

---

## 6. Data plane

- **Adapters** per source behind ports: `MarketData`, `MarketStream`, `DerivativesData`,
  `OrderBookData`, `NewsSource`, `MacroSource`, `PositioningSource`, `FundamentalsSource`.
- **Point-in-time store.** Every record has `event_time` (when it happened) and `known_time`
  (when we received it). Queries are always "as known at T". Bars are only usable after close.
- **Forward recorder** (from day one): open interest, liquidations, basis, funding,
  top-of-book and depth snapshots, borrow rates. These have little or no free history; a year of
  our own recording becomes a data advantage.
- **Universes** are point-in-time (trader's `TopVolume`), and delisted instruments are kept, to
  avoid survivorship bias.
- **Quality checks** on ingest: gaps, stale feeds, outliers, clock skew. A stale feed marks
  dependent factors as unavailable, and never silently forward-fills.

---

## 7. Factor store

### 7.1 Factor families

| Family | Examples | Markets |
|---|---|---|
| Price & trend | momentum (several lookbacks), trend strength, realised vol, vol-of-vol, mean-reversion z | all |
| Market structure & flow | funding, basis, OI change, liquidations, book imbalance, volume surprise, exchange flows | crypto, futures |
| News & events | typed event scores, novelty, credibility, priced-in fraction (§8) | all |
| Macro & cross-asset | rates, curve, DXY, oil/gas, equity index regime, credit spreads, calendar proximity (FOMC, CPI) | all |
| Positioning & crowd | followed-trader flow, prediction-market odds, attention spikes, COT | crypto, futures |
| Fundamentals | earnings surprise, revisions, valuation, inventories, on-chain activity | equities, commodities, crypto |
| Strategy context | proposing strategy's live record, recent hit rate, regime fit | all |
| Portfolio | correlation to holdings, marginal risk contribution, concentration | all |
| Cost & feasibility | spread, expected impact, liquidity, fees, funding/borrow | all |

### 7.2 Factor contract

```rust
trait Factor {
    fn id(&self) -> FactorId;                 // versioned: "funding_z@2"
    fn applies_to(&self, i: &InstrumentId) -> bool;
    fn compute(&self, i: &InstrumentId, as_of: Timestamp, store: &PitStore)
        -> Option<FactorValue>;               // None = unavailable, never a guess
}
struct FactorValue { z: f64, raw: f64, as_of: Timestamp, confidence: f64 }
```

- Values are normalised (rolling z or rank) **per market** so they are comparable.
- A factor change bumps its version; records are kept per version.
- Every new factor is evaluated by an event study (in-sample vs unseen tail, after costs, t-stat,
  count of factors tested) before the evaluator may use it.

### 7.3 Regime model

A small, interpretable model (HMM or clustering on realised vol, cross-asset correlation, trend
breadth, credit spreads) labels each market: **trending, choppy, risk-off, crisis**. The regime
is an input to the evaluator, to factor weights, and to the allocator's budgets. It is scored
like everything else: does conditioning on it improve out-of-sample results?

---

## 8. Knowledge plane (news and events)

```
 feeds ─► sanitise, dedupe ─► triage (Haiku) ─► extraction (Sonnet; Opus for high impact)
       ─► typed Event { kind, entities, direction, magnitude, horizon, novelty, credibility }
       ─► impact map (operator-written) ─► one event → many instruments
       ─► priced-in check ─► news factors in the factor store
```

- **Typed events only.** A closed schema; anything else is rejected whole. Headlines are passed
  to models as inert data.
- **Tiered models.** Cheap triage on every item; the strong model only on items that pass;
  a daily token budget per tier.
- **Received time and priced-in check.** Each event stores when we received it. The price move
  between the event time and our receipt is measured: if most of the expected move already
  happened, the factor's value shrinks to zero.
- **Impact map** (from trader ADR 0008): themes → linked instruments with a sign and a reason,
  versioned in git, edited only by the operator. Models classify events into themes; they
  may *suggest* new links only as findings.
- **Prompts are versioned and scored.** Each (model, prompt version) pair has a record:
  extraction accuracy against outcomes, cost per useful item. A new prompt is a challenger and
  is promoted only on its record.

---

## 9. Risk process

A separate OS process with its own minimal code, reading targets and positions from the bus and
the venues directly. It is the only component allowed to approve orders.

- **Envelope limits:** capital, per-slot caps, leverage, daily and total loss.
- **Per-position limits:** max weight, max loss per trade (stops chosen walk-forward), liquidity
  cap (position ≤ x% of daily volume).
- **Brakes** (from trader E-5): volatility-regime brake on by default; drawdown brake only where
  it tested better.
- **Correlation limit:** total exposure to one cluster (e.g. all large-cap crypto) capped.
- **Kill switch:** CLI, dashboard, phone, and any agent (risk-reducing). Flattens and halts.
- **Heartbeats:** if the risk process does not see the strategy side, or venue data goes stale,
  it stops new risk; if it fails itself, venue-side protective orders (stops) remain.
- **Venue keys:** trade-only, never withdrawal, IP-restricted, stored in the secret store and
  readable only by execution.

**Built (P3 slice 1, `crates/risk`):** `quorum risk run` approves every paper order over a Unix
socket (`data/risk/risk.sock`); NATS replaces the socket when the processes move to the server.
It checks the envelope (re-read per request), the slot's rung, venue, leverage, its own daily and
total loss tracking, and the kill switch (`quorum risk halt|resume`); risk-reducing orders always
pass; unreachable means no new risk. The book it judges is still the slot's report (from fills);
reading positions from venues, per-position and correlation limits, liquidity caps and
flattening on halt come with live execution.

---

## 10. Trade evaluator

The core addition over trader. Takes each proposed trade and decides **take, take smaller, delay
or reject**, with a size multiplier in `[0, 1.5]`.

### 10.1 Meta-labeling model

- **Primary model** = the strategy (decides direction and horizon).
- **Meta model** = gradient-boosted trees on the case file's features, predicting
  `P(trade is profitable after costs at its horizon)`. Trained on the journal (taken and
  counterfactual trades), purged walk-forward, recalibrated (isotonic) on recent data.
- Below a threshold → reject. Above → size by the calibrated probability (§11).
- Starts **disabled** (multiplier 1.0) until it has enough labelled trades and beats "always
  take" out of sample.

### 10.2 Claude case review

For trades above a size or novelty threshold, or when factors conflict strongly, Claude gets a
structured case file and returns:

```json
{ "action": "take | smaller | delay | reject",
  "size_multiplier": 0.0,
  "confidence": 0.0,
  "reasons": ["..."],
  "risk_flags": ["event_risk", "crowded", "regime_mismatch", "..."] }
```

- Schema-validated, clamped, journaled. It can **lower** the meta model's size freely; it can
  **raise** it only once its own record earns that right.
- Scored like any source: does following its adjustments beat the meta model alone?
- If not, its weight decays to zero automatically, and it is only used to write explanations.

### 10.3 Counterfactual tracking

Every rejected, reduced or delayed trade is followed to its hypothetical outcome (with modelled
costs). This answers: are the evaluator, each factor and Claude's verdicts saving money or
costing winners? Without it, filters can't be judged.

### 10.4 Portfolio fit

A trade is scored on its **marginal** effect on the portfolio: added variance, correlation to
holdings, cluster concentration. A good trade that duplicates existing risk is shrunk.

---

## 11. Allocator

- **Across slots:** capped inverse-volatility (risk parity) as the base, tilted towards slots
  with a better out-of-sample record, rebalanced weekly, inside envelope caps. Correlation between
  slots is estimated on trailing data only.
- **Within a slot:** targets from verdicts; size = base × evaluator multiplier, with
  **fractional Kelly** (¼) on the calibrated probability as a ceiling, and volatility targeting.
- **Regime:** budgets shift with regime (e.g. smaller trend budgets in choppy regimes) only where
  that was shown to help out of sample.
- **Turnover control:** rebalance bands; a trade must clear its expected cost by a margin.
- **Idle cash:** earns yield where the venue allows it safely (money-market or T-bill ETFs,
  venue savings) — optional, its own small slot.

---

## 12. Execution

- **OMS:** one order state machine (from trader); positions change only on reported fills.
- **EMS tactics:** maker-first limit orders with a timeout, then taker; TWAP/slicing above a
  size threshold; two-leg execution for carry (legs within a tolerance or unwind).
- **Transaction cost analysis:** every order records decision price, arrival price, fill price,
  fees, slippage vs the cost model. Per-venue and per-tactic reports feed the cost model.
- **Reconciliation:** venue balances and positions vs the journal every minute; any break
  halts the slot.
- **Paper and live-micro** use the same EMS; paper simulates fills from the order book.

---

## 13. Learning loop

1. **Scoring:** every factor, strategy, verdict, prompt, agent finding and slot is scored at
   fixed horizons on independent outcomes, net of costs, from when it became known.
2. **Weights:** follow the records (walk-forward only, multiple-testing corrected); new things
   start at zero.
3. **Drift detection:** CUSUM / rolling hit rate vs expected, per factor and strategy; an alarm
   cuts weight or demotes (§5.3).
4. **Research:** agents propose hypotheses (new factor, parameter, strategy, impact link) as
   typed findings; the lab tests them; survivors enter the tournament as challengers.
5. **Stress tests** (mandatory before promotion): March 2020, LUNA (May 2022), FTX (Nov 2022),
   the August 2024 carry unwind, plus synthetic scenarios: venue outage, 30% gap, funding at
   1%/8h, liquidity drying up, stale data feed.
6. **Post-mortem:** a weekly report of what changed, which factors helped, what the evaluator
   saved or cost, and which promotions or demotions happened, written by Claude from the
   journal's facts.

---

## 14. Agents

Agents are analysts and operators, never traders. They connect through MCP with per-role tool
allow-lists; every call is journaled.

| Role | Does | Can't |
|---|---|---|
| Researcher | Proposes hypotheses, runs lab tests | Promote anything |
| News analyst | Reviews extraction quality, suggests impact links | Edit the impact map |
| Risk reviewer | Watches exposures and anomalies, can pause or shrink | Add risk |
| Post-mortem writer | Weekly report from facts | Change state |
| Orchestrator | Schedules agents on events, budgets tokens | Create agents, grant permissions |

Agents wake on events (trades, large moves, news bursts), not on a clock, with a daily token
budget each.

---

## 15. Operations

- **Deployment:** on the operator's Mac for now (`launchd`, §19.3). Before any live money: a
  Linux VPS (e.g. Hetzner, Germany), systemd services, backups to object storage, and the laptop
  runs only the dashboard client.
- **Processes:** `ingest`, `factors`, `knowledge`, `engine` (slots, evaluator, allocator),
  `risk`, `execution`, `lab` (batch), `api` (dashboard + MCP). They talk over **NATS
  JetStream** (persistent streams = the event log; replay by stream position).
- **Storage:** Postgres (journal, state, records); Parquet + DuckDB (market and factor history);
  object storage for backups and raw news.
- **Observability:** metrics (Prometheus/Grafana or a lighter equivalent), structured logs,
  per-process health; alerts to phone (ntfy) for halts, demotions, reconciliation breaks,
  data staleness and loss thresholds.
- **Secrets:** the macOS keychain on the laptop, a secret store on the server; venue keys
  trade-only and IP-restricted.
- **Dashboard:** portfolio, slots and their rung, open trades with their case files, verdicts and
  counterfactuals, factor and strategy records, agents, envelope (read-only), kill switch.
- **Daily brief:** one page to the phone: P&L, positions and why, what the evaluator rejected,
  notable events, anything waiting on the operator.

---

## 16. Technology and repository

- **Rust** for every service (ported from trader: engine, bus, journal, lab, risk, data, news,
  MCP). Python only for offline research notebooks, never in the trading path.
- **moon** monorepo and the same gates as trader (fmt, clippy `-D warnings`, tests, golden
  backtest test).
- **Claude API** (Haiku triage, Sonnet extraction and case review, Opus for high-impact items
  and weekly research), with prompt caching and per-tier budgets.

```
quorum/
├── crates/            # each published as quorum-<name>
│   ├── core/          # types, instruments, events, ports, engine (from trader)
│   ├── bus/           # NATS JetStream event log + replay
│   ├── data/          # market/derivatives/macro adapters, PIT store, recorder
│   ├── factors/       # factor trait, families, regime model
│   ├── knowledge/     # news pipeline, typed events, impact map, priced-in check
│   ├── slots/         # slot runtime, strategies, universes, calendars
│   ├── evaluator/     # case files, meta-labeling, Claude review, counterfactuals
│   ├── allocator/     # cross-slot budgets, sizing
│   ├── risk/          # risk process (separate binary)
│   ├── execution/     # OMS/EMS, venue adapters, TCA, reconciliation
│   ├── lab/           # walk-forward, overfit control, tournament, stress tests (from trader)
│   ├── records/       # scoring, weights, drift detection
│   ├── journal/       # Postgres journal
│   ├── agents/        # agent runner, roles, budgets
│   ├── mcp/           # MCP server
│   └── api/           # dashboard API
├── apps/dashboard/    # web UI
├── config/            # envelope (signed), slots, factors, impact map
├── deploy/            # systemd units, compose for side services
└── docs/adr/
```

---

## 17. Phases

Each phase ends with a measurable exit criterion, not a feature list.

| Phase | Build | Exit criterion |
|---|---|---|
| **P0 Foundation** 🔄 | Repo, copy core/bus/journal/lab/risk from trader with history, envelope (paper only), slot model, NATS, Postgres, launchd services | trader's golden backtest reproduces bit for bit; crypto-spot slot paper-trades on the laptop and resumes cleanly after sleep |
| **P1 Recorder + factors** 🔄 recorder ✅, price factors + study ✅, regime model ✅ | Forward recorder, PIT store, first factor families (price, structure, macro), regime model | 2 weeks of clean recorded data; each factor has an event-study report |
| **P2 Second market** 🔄 multi-asset ETF research slot (Tiingo) ✅ | `equities` slot on IBKR paper (UCITS ETFs, stocks), trading calendars, equity data | Equity slot runs paper 4 weeks alongside crypto |
| **P3 Allocator + risk process** 🔄 allocation study ✅ (risk parity + regime: drawdown 6–10% vs 35% crypto alone; strict exit not met: ETFs alone had the better later Sharpe), stress tests ✅ (book passes all 9; crypto alone fails covid 2020 and 2022 at the 15% account limit; ETFs alone fail a −30% gap), risk process ✅ slice 1 (separate process approves every paper order: envelope, leverage, daily/total loss halts, kill switch; Unix socket on the laptop, NATS with the server) | Cross-slot allocator, separate risk process, brakes, stress tests | Combined walk-forward beats each slot alone on Sharpe and drawdown |
| **P4 Knowledge plane** 🔄 fetch + tiered Claude extraction + impact themes ✅ (scoring, priced-in next) | Tiered Claude extraction, impact map, priced-in check, prompt records | 4 weeks of extraction with a scored record per event kind |
| **P5 Evaluator** | Case files, counterfactual tracking, meta-labeling (shadow first), Claude review (shadow first) | Meta model beats "always take" out of sample on ≥ 200 trades |
| **P6 Execution + live-micro** | Move to a VPS, raise the envelope, EMS tactics, TCA, reconciliation, first slot to LiveMicro via the gate | 12 weeks live-micro with slippage inside 1.5× model and no reconciliation break |
| **P7 Expand** | Carry slot, futures slot, more factors, research agents at scale | Each new slot passes the ladder on its own |

Shadow mode first: the evaluator and Claude review run and are scored for weeks *before* they
are allowed to change any trade.

---

## 18. Risks

| Risk | Mitigation |
|---|---|
| Overfitting with many factors | Zero starting weight, deflated Sharpe, purged CV, counted trials, shadow periods |
| No real edge exists | The design measures this cheaply; the envelope caps the cost of finding out |
| Venue failure or insolvency (FTX-type) | Per-venue capital caps, operator withdraws profits on a schedule, reconciliation |
| LLM cost growth | Tiered models, budgets, event-driven agents, prompt records include cost |
| Prompt injection via news | Inert data, closed schemas, no model output reaches execution |
| Laptop sleep or downtime (paper phase) | Resume by replay; recorder marks gaps; server required before live money |
| Silent data errors | Quality checks, staleness marks factors unavailable, recorder health alerts |
| Operator over-trust in autonomy | Envelope limits are hard; daily brief and alerts keep the operator informed |
| Regulation and tax | Venues limited to lawful ones for the operator's jurisdiction; journal exports for tax |

---

## 19. Decisions (2026-10-15)

### 19.1 Jurisdiction: Czech Republic (EU)

Venues and instruments must be lawful for an EU (CZ) retail client. That changes the slots:

| Slot | Venue | What it means |
|---|---|---|
| `crypto-spot` | A MiCA-authorised exchange (Binance's EU/CZ status to be confirmed; Kraken, Bitstamp, Coinbase as alternatives) | Spot only at first. Binance and Hyperliquid **market data** stays usable as read-only input whichever venue trades |
| `crypto-carry` | Perpetuals only through a venue licensed for EU retail derivatives (e.g. Kraken's EU derivatives arm) | Hyperliquid is not used for trading (unclear legal status); carry waits until a licensed venue is confirmed |
| `equities` | IBKR | EU retail cannot buy US-domiciled ETFs (PRIIPs: no KID). Use **UCITS ETFs** (Xetra / LSE listings) and single stocks |
| `futures` | IBKR | Multi-asset trend on exchange futures; needs IBKR's derivatives permissions and margin |
| `prediction` | none | **Read-only factor source.** Kalshi is unavailable in the EU, and Polymarket is blocked on the operator's connection (trader SRC-2) |

Tax: gains are reported in CZK. The journal exports every fill with CZK values at the ČNB daily
rate. Frequent trading means the holding-period exemptions mostly don't apply. Rules to be
confirmed with a tax advisor before going live.

### 19.2 Envelope: paper only

```toml
[envelope]
capital_total = 0                 # no live capital
slot.default.max_mode = "paper"   # no slot can go above Paper
```

The autonomy ladder still runs: slots are promoted up to Paper and demoted automatically.
Moving any slot to LiveMicro requires the operator to raise the envelope. That's also when to
set capital and loss limits (§5.1 shows example values).

### 19.3 Hosting: laptop for now

- P0–P5 run on the operator's Mac: services under `launchd`, `caffeinate` while running,
  Postgres and NATS installed natively (Homebrew), backups to an external disk or cloud.
- Paper sessions resume by replay (trader's rule), so sleep or reboots lose no decisions, only
  live data. The forward recorder marks such gaps instead of interpolating them.
- **Gate:** moving any slot to LiveMicro also requires moving to an always-on server (e.g. a
  Hetzner VPS in Germany). The design keeps processes and config server-ready from P0.

### 19.4 Code from trader: copy, with git history

- trader's proven crates (core — incl. engine and risk —, bus, data, journal, lab, news, ai, ml,
  hyperliquid, predictions) were brought into quorum with `git filter-repo` from trader `main`
  (780f841), keeping their history. trader's `service`, `mcp`, `agents`, `server`, `cli`, `app`
  and dashboard were not copied: they depend on its single-session service layer and are rebuilt
  around slots (MCP and notifications included).
- The two repos then evolve separately. trader keeps paper trading untouched until quorum's
  crypto-spot slot passes P0, then it is archived.
- The golden backtest test comes along, to prove the copied engine behaves identically.

### 19.5 Name: quorum

Several factors have to agree before a trade happens. The repo is `quorum`, binaries
`quorum` / `quorum-risk` / `quorum-mcp`, and crates are prefixed `quorum-`.
