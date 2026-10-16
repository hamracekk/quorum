# quorum-notify (`notify`)

Operator notifications through the `core::Notifier` port.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | `NotifyConfig` (`config/notify.toml`), `LogNotifier` (`tracing`, level by severity), `Fanout` (every channel, best effort) |
| `phone.rs` | `PhoneNotifier`: ntfy push (`POST` to the server root, topic in the JSON body), priority by severity, `min_severity` filter, `from_secrets` |
| `phone/tests.rs` | Against a local `wiremock` server: one JSON post with title/priority/token, below-minimum skipped, failures never show the topic |

## What gets sent

| From | Severity | When |
|---|---|---|
| paper runner | info | session started / resumed / stopped; a **daily summary** once every instrument reported a day (equity, change since the last summary and since start, positions, % invested, fills) |
| paper runner | warning | risk halt (daily loss limit), live stream ended |
| paper runner | critical | stopped on an error |
| watchdog | critical / warning / info | paper heartbeat stale or session failed / recorder stale / recovered |

## Invariants

- **Best effort.** A failing channel is logged by `Fanout` and never fails a run.
- **The ntfy topic is a secret** (whoever knows it reads the alerts): keychain only
  (`quorum secrets set ntfy-topic`), sent in the request body, never in a URL, log or `Debug`.
- Messages are built from quorum's own numbers; never secrets, never external text.

## Test

```bash
moon run notify:test
```
