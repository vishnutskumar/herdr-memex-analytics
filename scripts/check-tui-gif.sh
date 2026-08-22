#!/usr/bin/env bash
# Regenerate docs/tui-demo.gif when its sources changed since origin/main.
# Requires target/release/analytics, so this belongs to the pre-push stage,
# not pre-commit: a release build is far too slow to run on every commit.
set -euo pipefail

merge_base="$(git merge-base HEAD origin/main 2>/dev/null)" || exit 0

if git diff --quiet "$merge_base" -- src/tui.rs src/render.rs; then
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1 \
    || ! python3 -c 'import pyte, PIL' >/dev/null 2>&1; then
    echo "warning: python3 with pyte+PIL unavailable; skipping docs/tui-demo.gif freshness check" >&2
    exit 0
fi

cargo build --release

python3 scripts/record-tui.py

if ! git diff --quiet -- docs/tui-demo.gif; then
    echo "docs/tui-demo.gif is stale — run: python3 scripts/record-tui.py && git add -f docs/tui-demo.gif" >&2
    exit 1
fi
