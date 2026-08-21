#!/usr/bin/env bash
# Every memex-analytics herdr action, plus the session-start hook.
#
#   plugin.sh report    open the auto-refreshing report pane (split)
#   plugin.sh toggle    open the report pane, or close it if one is already open
#   plugin.sh close     close every analytics pane in the workspace, no-op if none
#   plugin.sh startup   session start: background the snapshot daemon
#
# Modeled on memex's plugin.sh: actions refuse loudly (exit 1, one stderr line),
# startup refuses silently so it can never block a herdr session start.
set -uo pipefail
mode="${1:-toggle}"
# shellcheck source=herdr/lib.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

ws="${HERDR_WORKSPACE_ID:-}"
pane="${HERDR_PANE_ID:-}"
PANES_JSON=""
EXISTING=""

require_workspace() {
  [ -n "$ws" ] || refuse "no workspace context (invoke from inside herdr)"
}

load_panes() {
  [ -n "$PANES_JSON" ] && return 0
  PANES_JSON=$("$H" pane list --workspace "$ws" 2>/dev/null) && [ -n "$PANES_JSON" ] ||
    refuse "herdr pane list failed for $ws"
}

find_existing() {
  load_panes
  EXISTING=$(printf '%s' "$PANES_JSON" | jq -r '.result.panes[] | select(.label == "analytics") | .pane_id' 2>/dev/null)
}

attach_pane() {
  if [ -z "$pane" ]; then
    load_panes
    pane=$(printf '%s' "$PANES_JSON" | jq -r '.result.panes[0].pane_id // empty' 2>/dev/null)
  fi
  [ -n "$pane" ] || refuse "no pane to attach to in $ws"
}

close_all() {
  local closed="" failed="" p
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if "$H" pane close "$p" >/dev/null 2>&1; then closed="$closed $p"; else failed="$failed $p"; fi
  done <<EOF
$EXISTING
EOF
  [ -z "$failed" ] || refuse "failed to close$failed in $ws"
  printf 'closed%s in %s\n' "$closed" "$ws"
}

open_report() {
  local focus="$1" out opened
  attach_pane
  out=$("$H" plugin pane open --plugin "$PLUGIN_ID" --entrypoint report \
    --placement split --target-pane "$pane" --direction right "$focus" 2>/dev/null) ||
    refuse "herdr plugin pane open failed (report)"
  opened=$(printf '%s' "$out" | jq -r '.result.plugin_pane.pane.pane_id // empty' 2>/dev/null)
  printf 'opened report pane %s in %s\n' "${opened:-pane}" "$ws"
}

case "$mode" in
report)
  require_workspace
  open_report --no-focus
  ;;

toggle)
  require_workspace
  find_existing
  if [ -n "$EXISTING" ]; then
    close_all
  else
    open_report --no-focus
  fi
  ;;

close)
  require_workspace
  find_existing
  [ -n "$EXISTING" ] || {
    printf 'close: no analytics pane open in %s\n' "$ws"
    exit 0
  }
  close_all
  ;;

event-hook)
  # Invoked by the [[events]] hook on every agent status transition. Fast path:
  # exec the binary directly; HERDR_PLUGIN_EVENT_JSON carries the payload. A
  # missing binary must never make herdr report a failed event.
  BIN=$(resolve_analytics) || exit 0
  exec "$BIN" event-hook
  ;;

daemon)
  # Start (or leave running) the snapshot daemon. Uses the runtime state dir
  # when herdr provides one, else the standard plugin config path, so the
  # daemon writes where panes and hooks read.
  BIN=$(resolve_analytics) || refuse "analytics binary not found"
  dir="${HERDR_PLUGIN_STATE_DIR:-$HOME/.config/herdr/plugins/config/$PLUGIN_ID}"
  mkdir -p "$dir"
  if pgrep -f "analytics watch" >/dev/null 2>&1; then
    printf 'analytics daemon already running\n'
    exit 0
  fi
  nohup "$BIN" watch >>"$dir/watch.log" 2>&1 </dev/null &
  printf 'analytics daemon started (log: %s/watch.log)\n' "$dir"
  ;;

startup)
  # Session start: same surface as `daemon`, but silent — a startup hook must
  # never block or report failure.
  BIN=$(resolve_analytics) || exit 0
  dir="${HERDR_PLUGIN_STATE_DIR:-$HOME/.config/herdr/plugins/config/$PLUGIN_ID}"
  mkdir -p "$dir" 2>/dev/null || exit 0
  pgrep -f "analytics watch" >/dev/null 2>&1 && exit 0
  nohup "$BIN" watch >>"$dir/watch.log" 2>&1 </dev/null &
  exit 0
  ;;

*)
  refuse "unknown mode '$mode' (report | toggle | close | event-hook | daemon | startup)"
  ;;
esac
