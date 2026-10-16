# deploy

How quorum runs unattended. Today: **launchd on the operator's Mac** (DESIGN §19.3); a server
comes before any live money.

## launchd (macOS)

One agent per slot, `com.quorum.paper.<slot>`, running `quorum paper run --slot <slot>` from the
repo root.

```bash
cargo build --release -p quorum-cli
./deploy/launchd/install.sh crypto-spot       # load now and at every login
quorum paper status --slot crypto-spot        # heartbeat age, equity, positions
tail -f ~/Library/Logs/quorum/paper-crypto-spot.log
./deploy/launchd/uninstall.sh crypto-spot     # remove (the session stays resumable)
```

| Event | What happens |
|---|---|
| Login | launchd starts the run (`RunAtLoad`) |
| Crash, error, ended stream | non-zero exit (75 for an ended stream) → restarted, at most once a minute (`ThrottleInterval`) |
| `quorum paper stop --slot …` | exit 0 → left down until the next login or `launchctl kickstart gui/$(id -u)/com.quorum.paper.<slot>` |
| `uninstall.sh`, `launchctl bootout` | SIGTERM → clean stop |
| Laptop sleep | the process is suspended; on wake the stream reconnects and the heartbeat catches up on bars closed meanwhile |
| A manual `quorum paper run` while the service runs | refused: one process per slot (`data/slots/<slot>/LOCK`) |
| A refused config (envelope, ladder, changed slot) | logged, retried every minute; fixing the config or running `--fresh` by hand resolves it |

Every restart resumes the open session by replay, so nothing is lost or journaled twice.

### The watchdog

`com.quorum.watchdog` runs `quorum watchdog --slots <slots>` at login and every 5 minutes. It
alerts (config/notify.toml) when a paper session is `running` but its heartbeat is older than
5 minutes, when a session `failed`, when the recorder wrote nothing for 15 minutes, or when the
risk process does not answer (slots then refuse every order that adds risk) — once when
it breaks, once when it recovers (state in `data/watchdog.json`). Right after a sleep it only
records the time. Read-only: it never starts, stops or trades anything.

```bash
./deploy/launchd/install-watchdog.sh crypto-spot
./deploy/launchd/uninstall-watchdog.sh
```

### The risk process

`com.quorum.risk` runs `quorum risk run`, the only approver of orders (DESIGN §9). Paper slots
ask it about every order over `data/risk/risk.sock`; **while it is down they refuse every order
that adds risk** (risk-reducing orders still pass) and alert once. Install it before the slots.
Its halts and loss references survive restarts in `data/risk/state.json`.

```bash
./deploy/launchd/install-risk.sh
quorum risk status                          # kill switch, halts, equity seen, order counts
quorum risk halt --reason "…"               # kill switch for every slot (or --slot crypto-spot)
quorum risk resume                          # operator only: lift halts
./deploy/launchd/uninstall-risk.sh
```

### The news service

`com.quorum.news` runs `quorum news run`: every `poll_minutes` it fetches the feeds in
`config/news.toml` and reads new items with the Claude tiers, within their daily budgets. Logs:
`~/Library/Logs/quorum/news.log`. Without an Anthropic key it still collects news and reads none.

```bash
./deploy/launchd/install-news.sh
./deploy/launchd/uninstall-news.sh          # stored news and readings stay in the journal
```

### The forward recorder

`com.quorum.recorder` runs `quorum record` (`config/recorder.toml`) the same way: started at
login, restarted after a crash, an error or an ended stream, left down after a clean stop.
Logs: `~/Library/Logs/quorum/recorder.log`; data: `data/recorder/<feed>/<day>.jsonl`.

```bash
./deploy/launchd/install-recorder.sh
./deploy/launchd/uninstall-recorder.sh      # recorded files stay
```

While the laptop sleeps nothing is recorded: the next `up`/`start` marker after a `down` (or
the missing samples) shows the gap.

### Notes

- The agent runs `target/release/quorum` in place: after `cargo build --release`, restart it
  (`launchctl kickstart -k gui/$(id -u)/com.quorum.paper.<slot>`); `cargo clean` removes the
  binary and the agent fails until the next build.
- Logs grow by a few lines per bar and per restart; nothing rotates them yet.
- Sleep is not prevented: daily bars close at 00:00 UTC and are caught up on wake. Trading
  intraday intervals unattended needs the server (§19.3).
