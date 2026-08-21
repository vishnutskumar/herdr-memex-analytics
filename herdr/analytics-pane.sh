#!/usr/bin/env sh
# Pane entrypoint: resolve the analytics binary and exec the requested mode.
# The pane command runs with $HERDR_PLUGIN_ROOT set; the report pane auto-refreshes.
set -u
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"

mode="${1:-report}"

BIN=$(resolve_analytics) || {
  printf 'analytics binary not found (run cargo build --release in the plugin checkout, or herdr plugin install %s)\n' "$PLUGIN_ID"
  exit 1
}

case "$mode" in
report)
  exec "$BIN" ui
  ;;
*)
  printf 'unknown pane mode %s\n' "$mode"
  exit 1
  ;;
esac
