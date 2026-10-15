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
