#!/usr/bin/env bash
# herdr `[[build]]` step: make a working analytics binary available in $ROOT/bin.
#
# Runs on `herdr plugin install vishnutskumar/herdr-memex-analytics` and on every
# update. `herdr plugin link` skips the build step — from a linked checkout run
# `cargo build --release` and the plugin scripts pick up target/release/analytics.
#
# Order of preference:
#   1. latest release tarball for this platform -> $ROOT/bin/analytics
#   2. an existing target/release build from this checkout
#   3. cargo build --release from this checkout
#
# On every successful install the snapshot daemon is restarted cleanly against
# the new binary, and required settings (memex token usage, herdr toasts) are
# enabled or called out.
#
# Build commands run with the plugin checkout as the working directory and may not
# receive the runtime HERDR_* env, so the plugin root is resolved from this script's
# own location, and cargo needs help finding homebrew's rustup.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="$ROOT/bin"
PLUGIN_ID="vishnutskumar.memex-analytics"

export PATH="/opt/homebrew/opt/rustup/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$HOME/.cargo/bin:${PATH:-}"

install_bin() {
  mkdir -p "$BIN_DIR"
  # Overwriting a Mach-O in place invalidates its code signature; fresh inode + ad-hoc sign.
  rm -f "$BIN_DIR/analytics"
  install -m 0755 "$1" "$BIN_DIR/analytics"
  if [ "$(uname -s)" = "Darwin" ] && command -v codesign >/dev/null 2>&1; then
    codesign --force --sign - "$BIN_DIR/analytics" >/dev/null 2>&1 || true
  fi
  "$BIN_DIR/analytics" --version >/dev/null 2>&1
}

# Stop any running daemon and start a fresh one against the installed binary.
# A plugin update must never leave an old binary serving stale tips. Never
# fails the install.
restart_daemon() {
  local dir="${HERDR_PLUGIN_STATE_DIR:-$HOME/.config/herdr/plugins/config/$PLUGIN_ID}"
  mkdir -p "$dir" 2>/dev/null || return 0
  if pgrep -f "analytics watch" >/dev/null 2>&1; then
    pkill -f "analytics watch" 2>/dev/null || true
    sleep 1
    echo "analytics: stopped old daemon"
  fi
  nohup "$BIN_DIR/analytics" watch >>"$dir/watch.log" 2>&1 </dev/null &
  echo "analytics: daemon started (log: $dir/watch.log)"
}

# Install the binary, restart the daemon against it, and apply settings.
finish_install() {
  local source="$1" label="$2"
  install_bin "$source"
  restart_daemon
  configure_notes
  echo "analytics: installed $BIN_DIR/analytics ($label)"
}

# Enable what the plugin needs, or say exactly what to change. Appends missing
# settings; never rewrites existing values.
configure_notes() {
  # 1. memex token usage: required for the cost / cache-waste panel.
  local memex_conf="${MEMEX_ROOT:-$HOME/.memex}/config.toml"
  if [ ! -f "$memex_conf" ]; then
    printf 'token_usage = true\n' > "$memex_conf"
    echo "analytics: enabled token_usage in $memex_conf (created)"
  elif grep -qE '^token_usage[[:space:]]*=[[:space:]]*true' "$memex_conf" 2>/dev/null; then
    : # already enabled
  elif grep -qE '^token_usage' "$memex_conf" 2>/dev/null; then
    echo "analytics: NOTE - token_usage is disabled in $memex_conf;"
    echo "  set token_usage = true there for cost / cache-waste analytics"
  else
    printf '\ntoken_usage = true\n' >> "$memex_conf"
    echo "analytics: enabled token_usage in $memex_conf"
  fi

  # 2. herdr toasts: realtime tips arrive as in-app notifications.
  local herdr_conf="${HERDR_CONFIG_DIR:-$HOME/.config/herdr}/config.toml"
  if [ -f "$herdr_conf" ] && grep -qE '^delivery[[:space:]]*=' "$herdr_conf" 2>/dev/null; then
    if grep -qE '^delivery[[:space:]]*=[[:space:]]*"off"' "$herdr_conf" 2>/dev/null; then
      echo "analytics: NOTE - [ui.toast] delivery is \"off\" in $herdr_conf;"
      echo "  set delivery = \"herdr\" under [ui.toast] to see realtime tips"
    fi
  elif [ -f "$herdr_conf" ]; then
    printf '\n[ui.toast]\ndelivery = "herdr"\n' >> "$herdr_conf"
    echo "analytics: enabled herdr toasts in $herdr_conf"
  else
    echo "analytics: NOTE - add [ui.toast] delivery = \"herdr\" to $herdr_conf"
    echo "  to see realtime tips as in-app notifications"
  fi
}

# Latest release tarball for this platform, packaged by the release workflow.
os="$(uname -s)"
case "$os" in Darwin) os="macos" ;; Linux) os="linux" ;; *) os="" ;; esac
arch="$(uname -m)"
case "$arch" in x86_64 | amd64) arch="x86_64" ;; arm64 | aarch64) arch="arm64" ;; *) arch="" ;; esac

if [ -n "$os" ] && [ -n "$arch" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  # Resolve the newest release asset for this platform. python3 or jq parses the
  # API; either is fine, both are optional. Network failure here just means we
  # fall through to a local build.
  asset_url=$(curl -fsSL --retry 3 --retry-delay 2 \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/vishnutskumar/herdr-memex-analytics/releases/latest" 2>/dev/null |
    {
      if command -v jq >/dev/null 2>&1; then
        jq -r ".assets[] | select(.name | endswith(\"-${os}-${arch}.tar.gz\")) | .browser_download_url" 2>/dev/null | head -n1
      elif command -v python3 >/dev/null 2>&1; then
        python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(next(a['browser_download_url'] for a in d['assets'] if a['name'].endswith('-${os}-${arch}.tar.gz')))
except Exception:
    pass
"
      fi
    } || true)

  if [ -n "$asset_url" ] &&
    curl -fsSL --retry 5 --retry-delay 3 --retry-all-errors --retry-connrefused "$asset_url" -o "$tmp/analytics.tar.gz" &&
    curl -fsSL --retry 3 --retry-delay 2 --retry-all-errors "$asset_url.sha256" -o "$tmp/analytics.tar.gz.sha256"; then
    # A checksum mismatch is fatal; a missing sidecar is tolerated.
    expected="$(awk '{print $1}' "$tmp/analytics.tar.gz.sha256" 2>/dev/null)"
    if [ -n "$expected" ]; then
      if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp/analytics.tar.gz" | awk '{print $1}')"
      else
        actual="$(shasum -a 256 "$tmp/analytics.tar.gz" | awk '{print $1}')"
      fi
      if [ "$expected" != "$actual" ]; then
        echo "analytics: checksum mismatch for release tarball" >&2
        exit 1
      fi
    fi
    if tar -xzf "$tmp/analytics.tar.gz" -C "$tmp" && [ -f "$tmp/analytics" ]; then
      finish_install "$tmp/analytics" "latest release build"
      exit 0
    fi
  fi
  echo "analytics: no release build available, falling back to a local build" >&2
fi

if [ -x "$ROOT/target/release/analytics" ]; then
  finish_install "$ROOT/target/release/analytics" "existing target/release build"
  exit 0
fi

if command -v cargo >/dev/null 2>&1; then
  echo "analytics: building from source (first build compiles memex too; this takes a while)"
  (cd "$ROOT" && cargo build --release)
  finish_install "$ROOT/target/release/analytics" "source build"
  exit 0
fi

echo "analytics: no release build and no cargo toolchain found" >&2
exit 1
