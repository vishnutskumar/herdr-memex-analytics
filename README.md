# herdr-memex-analytics

![analytics dashboard](docs/tui-demo.gif)

A [Herdr](https://herdr.dev) plugin that turns [memex](https://github.com/nicosuave/memex)
history into efficiency analytics and realtime guidance for the agents running in
your panes.

On one real machine, the last 30 days alone surfaced: **$114 of prompt-cache
waste**, **2,412 cache misses** (101 after idle gaps), and **244 hours** of agent
time across projects — numbers nobody had looked at before.

## What it does

**Retro report** — a Herdr pane showing, per project: session counts, message
volume, active hours, and (when memex token-usage tracking is enabled) tokens,
known cost, and **prompt-cache waste** — tokens you paid input rates for that a
warm cache would have served. Includes where the waste came from (idle-gap
misses, model switches), the **cache hit-rate** (share of prompt tokens served
from cache), the **model mix** behind the spend, and **per-project cost
attribution** (top projects by known cost). An activity section adds a daily
cost sparkline, trailing-hour burn rate and today's spend, a week-over-week
delta when the window has a bounded start (`--since`), reasoning-token share,
and a warning when a session's uncached prompt keeps growing past
`context_bloat_tokens`.

**Turn quality** — completed turns are summarized as p50/p95 durations, an
intervention rate (share of turns that ended blocked), rework turns (quick
follow-ups right after a blocked turn), and how long humans took to unblock
agents.

**Live fleet** — each daemon cycle samples `herdr api snapshot` and reports
how many agents are working, blocked, or idle right now, plus per-pane revision
churn.

**Realtime guidance** — a manifest event hook fires on every agent status
transition. You get a Herdr notification the moment an agent blocks, the daemon
re-nags if one stays blocked, and long single turns get flagged. Completed-turn
durations are logged for later analysis. Repeated output matches in a pane
(retry loops) become an urgent tip, and budget alerts fire when today's spend
passes `daily_cost_usd` (once per local day) or the trailing-hour burn rate
exceeds `block_burn_rate_usd_hr` (at most once per hour).

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

Installs prefer a prebuilt release binary for your platform (macOS arm64/x86_64,
Linux x86_64/arm64); building from source is only the fallback when no release
matches, and then a Rust toolchain is needed.

## Use

| Surface | What it is |
|---|---|
| `analytics: toggle report pane` action | Open/close the auto-refreshing report beside your work |
| `analytics: efficiency report` action | Open the report pane |
| `report` pane | Live report + current tips, re-rendered every 30 s (`q` quit, `j/k` or mouse wheel move, `r` rescan, `c` cycle the activity chart between tokens/cost/sessions; table border shows the selected row as `sel/total`) |
| `pane.agent_status_changed` hook | Records transitions, notifies on blocked agents |

### Keybinding

Toggle the report pane from anywhere (like memex's `prefix+M`) by adding this to
`~/.config/herdr/config.toml`, then running `herdr server reload-config`:

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "vishnutskumar.memex-analytics.toggle"
description = "toggle analytics report pane"
```

(The `vishnutskumar.` prefix is Herdr's plugin-action namespacing, not part of
the action id.)

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
`turns.jsonl`, `gaps.jsonl`, `tips.json`, and an optional `config.toml`:

```toml
scan_interval_secs = 900              # daemon rescan cadence
daily_cost_usd = 20.0                # alert when today's spend passes this (omit to disable)
block_burn_rate_usd_hr = 15.0        # alert when trailing-hour burn rate exceeds this
context_bloat_tokens = 100_000       # flag sessions whose uncached prompt grows past this
```

The TUI dashboard (`analytics ui`) adds a braille activity chart over the
daily series (`c` cycles tokens/cost/sessions) and a 7x24 heatmap of which
hours of the last seven days saw the most token traffic.

## Agent skill

A skill ships alongside the plugin so coding agents can answer efficiency
questions ("what did I spend this week?", "why is my bill high?", "how long are
my agent turns?") directly. Symlink it into your skills directory from the
plugin root:

```bash
ln -s "$PLUGIN_ROOT/skills/analytics-report" ~/.claude/skills/analytics-report
```

The skill teaches agents to use the `analytics` binary's JSON output and the
turn log; see [skills/analytics-report/SKILL.md](skills/analytics-report/SKILL.md).

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
reads its index. Bugs in memex belong in [nicosuave/memex](https://github.com/nicosuave/memex),
not here.

## Development

```bash
cargo build --release
bash herdr/install.sh        # installs target/release/analytics into bin/
herdr plugin link "$PWD"     # local dev; skips the build step
```

Before committing: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo deny check --warn warnings`, `cargo test` (pre-commit and pre-push hooks run these).

## License

MIT
