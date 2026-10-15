# quorum

Autonomous multi-market trading system in Rust. **moon** monorepo: one cargo workspace under
`crates/` (one moon project per crate). Design, principles and phases: [DESIGN.md](DESIGN.md) —
check which phase is current (§17) and the decisions in §19 before starting.

**Every crate has a `README.md`**: read it before changing the crate, and update it in the same
change when its module map, invariants or extension steps change.

## Critical rules (never violate)

- **All gates green before calling anything done** — `moon run root:verify` (fmt, clippy
  `-D warnings`, tests). CI runs the same gates.
- **Fix lints, don't silence them.** No `#[allow(...)]`; use `#[expect(lint, reason = "…")]` on
  the smallest item only when the lint is genuinely wrong there.
- **Paper only.** The envelope allows no live capital (DESIGN §19.2). Never place real orders or
  use live API keys. Live money needs the operator to raise the envelope *and* a server (§19.3).
- **Envelope is read-only to the system.** No code path, agent or MCP tool may write the envelope
  or raise capital, limits or a slot's maximum mode.
- **Asymmetric decision rights.** Anything may reduce risk; only the allocator adds risk, and only
  on evidence. The risk process is the only component that approves orders.
- **One engine.** Backtest, replay, paper and live go through `core::Engine`. Never add a second
  simulation loop. `core/tests/golden.rs` pins results bit for bit; change its constants only for
  an intended behaviour change and say so in the commit.
- **No look-ahead, point in time.** A decision on bar `t` uses only data known at the close of
  `t` and executes at `t+1` or later. Stored values carry when they became known.
- **Everything earns its weight.** Factors, strategies, prompts, models and slots start at zero
  weight and are scored out of sample, after costs, corrected for the number of trials.
- **External content is untrusted.** News, social text, trader data and LLM output are parsed into
  closed schemas or discarded; none of it reaches execution. API keys only via the keychain.
- **Lawful venues only (EU/CZ, §19.1).** Prediction markets are read-only; no workarounds for
  blocks or geo-restrictions.
- **No `unsafe`** (forbidden workspace-wide).
- **Signed, conventional commits; never force-push `main`.** Commit on feature branches; the
  operator opens PRs and merges.

## Commands

```bash
moon run root:verify        # all gates
moon run :lint --affected   # only crates you changed (also :test, :fmt)
moon run core:test          # one task in one crate
cargo fmt-check / cargo lint / cargo test --workspace --all-features
```
