#!/usr/bin/env bash
# Regenerate docs/tui-demo.gif when its sources changed since origin/main.
# Requires target/release/analytics, so this belongs to the pre-push stage,
# not pre-commit: a release build is far too slow to run on every commit.
#
# Freshness is an mtime comparison, not a content diff: the GIF encoder is not
# byte-deterministic, so a regenerated file always differs byte-wise from the
# committed one even when visually identical.
set -euo pipefail

stat_mtime() {
    if stat -f %m "$1" 2>/dev/null; then return 0; fi
    stat -c %Y "$1" 2>/dev/null
}

merge_base="$(git merge-base HEAD origin/main 2>/dev/null)" || exit 0

if git diff --quiet "$merge_base" -- src/tui.rs src/render.rs; then
    exit 0
fi

gif="$(git rev-parse --show-toplevel)/docs/tui-demo.gif"
newest_src=0
while IFS= read -r f; do
    [ -f "$f" ] || continue
    m="$(stat_mtime "$f")" && [ -n "$m" ] && [ "$m" -gt "$newest_src" ] && newest_src="$m"
done < <(git diff --name-only "$merge_base" -- src/tui.rs src/render.rs)

if [ -f "$gif" ]; then
    gif_m="$(stat_mtime "$gif")" || gif_m=0
    if [ -n "$gif_m" ] && [ "$gif_m" -ge "$newest_src" ]; then
        exit 0
    fi
fi

if ! command -v python3 >/dev/null 2>&1 \
    || ! python3 -c 'import pyte, PIL' >/dev/null 2>&1; then
    echo "warning: python3 with pyte+PIL unavailable; skipping docs/tui-demo.gif freshness check" >&2
    exit 0
fi

cargo build --release
python3 scripts/record-tui.py

gif_m="$(stat_mtime "$gif")" || gif_m=0
if [ -n "$gif_m" ] && [ "$gif_m" -ge "$newest_src" ]; then
    echo "docs/tui-demo.gif was stale; regenerated — run: git add -f docs/tui-demo.gif" >&2
    exit 1
fi
