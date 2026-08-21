# herdr-memex-analytics

A [Herdr](https://herdr.dev) plugin that turns [memex](https://github.com/nicosuave/memex)
history into efficiency analytics and realtime guidance for the agents running in
your panes.

## What it does

**Retro report** — a Herdr pane showing, per project: session counts, message
volume, active hours, and (when memex token-usage tracking is enabled) tokens,
known cost, and **prompt-cache waste** — tokens you paid input rates for that a
warm cache would have served. Includes where the waste came from (idle-gap
misses, model switches).

**Realtime guidance** — a manifest event hook fires on every agent status
transition. You get a Herdr notification the moment an agent blocks, the daemon
re-nags if one stays blocked, and long single turns get flagged. Completed-turn
durations are logged for later analysis.

**Auto-refresh** — like memex's periodic reindex, a background daemon (started
by the plugin's `[[startup]]` hook) rescans on a fixed cadence (default 15 min)
and republishes a warm snapshot; the report pane re-renders from it every 30 s.
No manual refreshes.

## Install

```bash
herdr plugin install vishnutskumar/herdr-memex-analytics
```

Requirements:

- [Herdr](https://herdr.dev) >= 0.7.0 (macOS or Linux)
- [memex](https://github.com/nicosuave/memex) installed and indexed (`memex index`)
- For the token/cost/cache-waste section: `token_usage = true` in `~/.memex/config.toml`
  (the first usage scan parses your log corpora and can take a while; memex caches it)
- Rust toolchain (the plugin builds itself from source during install)

## Use

| Surface | What it is |
|---|---|
| `analytics: toggle report pane` action | Open/close the auto-refreshing report beside your work |
| `analytics: efficiency report` action | Open the report pane |
| `report` pane | Live report + current tips, re-rendered every 30 s |
| `[[startup]]` hook | Starts the snapshot daemon in the background |
| `pane.agent_status_changed` hook | Records transitions, notifies on blocked agents |

The binary is also usable standalone:

```bash
analytics report --since 7d            # one-week window
analytics report --project memex --json
analytics report --all                 # full history
analytics snapshot                     # one daemon scan cycle, writes snapshot.json
analytics watch --scan-interval-secs 600
```

State lives under `$HERDR_PLUGIN_STATE_DIR` inside Herdr
(`~/.herdr-memex-analytics` standalone): `snapshot.json`, `agent-states.json`,
`turns.jsonl`, `tips.json`, and an optional `config.toml`:

```toml
scan_interval_secs = 900   # daemon rescan cadence
```

## How it works

```
memex (~/.memex)                    herdr
  analytics.sqlite  ─┐              agent status events ──> [[events]] hook
  usage logs        ─┤─> analytics daemon (watch)          │ records transitions,
                      |    scan every N s                  │ notifies on blocked,
                      |    ├─ snapshot.json                │ logs turn durations
                      |    └─ tips.json                    v
                      └─> report pane  <───── tips.json ── agent-states.json
                           (re-renders 30 s)
```

memex's own code lives upstream; this repo only depends on it as a library and
reads its index. Bugs in memex belong in the
[memex fork](https://github.com/nicosuave/memex), not here.

## Development

```bash
cargo build --release
bash herdr/install.sh        # copies target/release/analytics into bin/
herdr plugin link "$PWD"     # local dev; skips the build step
```

Before committing: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo deny check --warn warnings`, `cargo test` (pre-commit and pre-push hooks run these).

## License

MIT
