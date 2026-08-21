# Agents

## Before committing

Always run these checks before committing. Do not commit if any fails.

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo deny check
```

If `cargo fmt --check` fails, run `cargo fmt` and include the formatting fix in your commit.

## Design conventions

- Mirror memex where the concepts overlap: `Paths`-style path resolution, config
  with safe defaults, a background daemon on a fixed cadence (like memex's
  periodic reindex), and herdr scripts under `herdr/`.
- Actions refuse loudly (one stderr line, exit 1); the startup hook refuses
  silently and never blocks session start.
- The daemon must survive transient scan failures; panes read the warm snapshot,
  never a half-written one (atomic rename).
