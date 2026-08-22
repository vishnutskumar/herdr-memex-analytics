#!/usr/bin/env bash
# Fail when code or plugin metadata changed since origin/main but the
# Cargo.toml version was not bumped alongside it.
set -euo pipefail

merge_base="$(git merge-base HEAD origin/main 2>/dev/null)" || exit 0

if git diff --quiet "$merge_base" -- src tests herdr-plugin.toml Cargo.toml; then
    exit 0
fi

old_version="$(git show "$merge_base":Cargo.toml | grep -m1 '^version' | sed 's/^version *= *"\([^"]*\)".*/\1/')"
new_version="$(grep -m1 '^version' Cargo.toml | sed 's/^version *= *"\([^"]*\)".*/\1/')"

if [ "$new_version" = "$old_version" ]; then
    echo "version must be bumped in Cargo.toml and herdr-plugin.toml for this change" >&2
    exit 1
fi
