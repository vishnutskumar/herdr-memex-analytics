# herdr-memex-analytics

Herdr plugin providing session efficiency analytics and realtime guidance, backed
by memex as a git library dependency. Separate repo from memex; the binary
(`analytics`) reads memex's data at `~/.memex` (or a `--root` override).

## Structure

```
herdr-memex-analytics/
  src/              # Rust binary: config, report, render, watch
  herdr/            # Shell scripts: plugin hook, pane renderer, installer
  herdr-plugin.toml # Herdr plugin manifest
```

## Build & Check

Before committing, all of these must pass:

```bash
cargo build
cargo fmt --check        # run `cargo fmt` first if it fails
cargo clippy --all-targets -- -D warnings
cargo deny check --warn warnings
cargo test
```

Pre-commit hooks enforce the universal checks, `cargo fmt`, and `cargo clippy`;
`cargo test` and `cargo deny` run on pre-push.

## Design conventions

- Mirror memex where concepts overlap: `Paths`-style path resolution, config
  with safe defaults, a background daemon on a fixed cadence (like memex's
  periodic reindex), and herdr scripts under `herdr/`.
- Actions refuse loudly (one stderr line, exit 1); the startup hook refuses
  silently and never blocks session start.
- The daemon must survive transient scan failures; panes read the warm
  snapshot, never a half-written one (atomic rename on write).
- The daemon snapshot is the only cross-process data path: daemon writes,
  panes read; nothing else touches the snapshot file.

## memex boundary

memex's own code lives upstream — bugs there get fixed upstream
(nicosuave/memex) and picked up via the git dependency, never vendored or
patched from this repo. `Cargo.toml` pins `ort = "=2.0.0-rc.10"` to keep
version unification with memex's ONNX runtime (rc.13 breaks memex's
unconditional CoreML import on macOS); revisit when upstream fixes it.
