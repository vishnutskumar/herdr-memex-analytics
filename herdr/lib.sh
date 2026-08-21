#!/usr/bin/env bash
# Shared helpers for the memex-analytics herdr plugin scripts.
# shellcheck disable=SC2034  # PLUGIN_ID and H are consumed by sourcing scripts

PLUGIN_ID="vishnutskumar.memex-analytics"

# herdr CLI; actions run with a minimal PATH, so help homebrew along.
H="herdr"
command -v herdr >/dev/null 2>&1 || H="/opt/homebrew/bin/herdr"

# Refuse loudly: exactly one stderr line, exit 1.
refuse() {
  printf 'analytics: %s\n' "$1" >&2
  exit 1
}

# The plugin state dir; herdr sets HERDR_PLUGIN_STATE_DIR for actions.
state_dir() {
  local dir="${HERDR_PLUGIN_STATE_DIR:-$HOME/.herdr-memex-analytics}"
  mkdir -p "$dir" 2>/dev/null && printf '%s' "$dir"
}

# Resolve the analytics binary: the install.sh copy, a linked checkout build, or
# an already-installed binary on PATH.
resolve_analytics() {
  local root="${HERDR_PLUGIN_ROOT:-}"
  if [ -n "$root" ] && [ -x "$root/bin/analytics" ]; then
    printf '%s' "$root/bin/analytics"
    return 0
  fi
  if [ -n "$root" ] && [ -x "$root/target/release/analytics" ]; then
    printf '%s' "$root/target/release/analytics"
    return 0
  fi
  if command -v analytics >/dev/null 2>&1; then
    command -v analytics
    return 0
  fi
  return 1
}
