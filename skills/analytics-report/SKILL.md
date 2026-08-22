---
name: analytics-report
description: Answer questions about agent usage efficiency using memex-backed analytics — token spend, prompt-cache waste, session counts, active hours per project, and completed-turn durations. Use when the user asks how much they spent on tokens, which projects consume the most agent time, why costs are high, what cache waste means for them, or wants an efficiency report for a date range or project.
allowed-tools: Bash(analytics:*), Bash(memex:*)
---

# Analytics Report

Use the `analytics` binary (from the herdr-memex-analytics plugin) as the read
layer over memex history. It joins memex's session index with reconstructed
token usage and cache-waste analysis.

## Core Rules

1. **Prefer `--json` when computing anything.** The text render is for humans;
   JSON gives you stable fields to reason over.
2. **Scope before scanning.** Always pass `--since` (e.g. `7d`, `30d`,
   `2026-01-01`) or `--project` when the question allows it; full-history
   scans parse large corpora.
3. **Trust the snapshot when fresh.** Without flags, `analytics report` reads
   the daemon's snapshot (refreshed every 15 min). Only force a rescan (`r` in
   the TUI, or `analytics snapshot`) when the user asks for up-to-the-minute
   numbers.
4. **Explain cache waste, don't just report it.** Missed tokens were billed at
   input rates though a warm cache could have served them. Idle-gap misses
   mean returning after the cache TTL expired; model-switch misses mean
   switching models mid-task. The actionable advice is batching related work
   into contiguous sessions.
5. **Distinguish cost certainty.** `known_cost_usd` covers only priced events;
   say so when quoting dollar figures rather than implying exact totals. This
   goes for per-project figures too: `project_usage.missed_cost_usd` is always
   0.0 — waste cost is only known at the digest level (`usage.missed_cost_usd`)
   — so rank projects by known cost and say "known cost".
6. **Turn durations come from herdr events**, not memex: `turns.jsonl` in the
   plugin state dir holds one line per completed working segment. Read it
   directly for "how long are my agent turns" questions.

## Step 0: Classify the Question

| User intent | First move |
| --- | --- |
| "how much am I spending" | `analytics report --since 30d --json` → `usage.known_cost_usd`, `usage.by_source` |
| "why is my bill high" | same, then rank `by_source` and check `usage.missed_cost_usd` share |
| "which model costs me the most" | `report --json --since Nd` → rank `usage.by_model` by `known_cost_usd` (already sorted by tokens desc) |
| "which project eats my time" | `report --json` → sort `projects` by `active_ms` |
| "which project burns budget" | `report --json` → `project_usage`, ranked by `known_cost_usd` (top 10) |
| "efficiency this week" | `report --since 7d --json` |
| "am I using the cache well" | `usage`: compare `missed_tokens` against total input; high `idle_misses` = fragmented sessions |
| "is my cache working" | `usage.cache_hit_rate` = `cache_read_tokens / input_tokens`, where the denominator (`input_tokens`) is uncached input + cache read + cache write; pair with `missed_tokens`/`idle_misses` |
| "how long are agent turns" | read `$HERDR_PLUGIN_STATE_DIR/turns.jsonl` (default `~/.herdr-memex-analytics/turns.jsonl`), summarize `duration_ms` |
| "is anything stuck right now" | read `agent-states.json` in the state dir; status `blocked` with old `since_ms` |

## Output Guidance

- Quote tokens in K/M/B and dollars to 2 decimals; name the window you used.
- When usage is unavailable, say token tracking is disabled
  (`token_usage = true` in `~/.memex/config.toml`) instead of guessing.
- For "improve my efficiency" asks, lead with the single largest lever:
  usually idle-gap cache misses or one dominant project's session fragmentation.
