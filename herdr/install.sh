#!/usr/bin/env bash
# herdr `[[build]]` step: make a working analytics binary available in $ROOT/bin.
#
# Runs on `herdr plugin install vishnutskumar/herdr-memex-analytics`. `herdr plugin link`
# skips the build step — from a linked checkout run `cargo build --release` and the
# plugin scripts pick up target/release/analytics directly.
#
# Order of preference:
#   1. release tarball matching this manifest's version -> $ROOT/bin/analytics
#   2. an existing target/release build from this checkout
#   3. cargo build --release from this checkout
#
# Build commands run with the plugin checkout as the working directory and may not
# receive the runtime HERDR_* env, so the plugin root is resolved from this script's
# own location, and cargo needs help finding homebrew's rustup.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="$ROOT/bin"

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
      echo "analytics: installed $BIN_DIR/analytics (latest release build)"
      install_bin "$tmp/analytics"
      exit 0
    fi
  fi
  echo "analytics: no release build available, falling back to a local build" >&2
fi

if [ -x "$ROOT/target/release/analytics" ]; then
  echo "analytics: using existing target/release build"
  install_bin "$ROOT/target/release/analytics"
  exit 0
fi

if command -v cargo >/dev/null 2>&1; then
  echo "analytics: building from source (first build compiles memex too; this takes a while)"
  (cd "$ROOT" && cargo build --release)
  install_bin "$ROOT/target/release/analytics"
  echo "analytics: installed $BIN_DIR/analytics (source build)"
  exit 0
fi

echo "analytics: no target/release build and no cargo toolchain found" >&2
exit 1
