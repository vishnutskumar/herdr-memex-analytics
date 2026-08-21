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

# Release tarball for this exact manifest version, so a checkout always pulls its
# own build. Same OS/arch tokens the release workflow packages.
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
os="$(uname -s)"
case "$os" in Darwin) os="macos" ;; Linux) os="linux" ;; *) os="" ;; esac
arch="$(uname -m)"
case "$arch" in x86_64 | amd64) arch="x86_64" ;; arm64 | aarch64) arch="arm64" ;; *) arch="" ;; esac

if [ -n "$os" ] && [ -n "$arch" ]; then
  archive="analytics-${VERSION}-${os}-${arch}.tar.gz"
  url="https://github.com/vishnutskumar/herdr-memex-analytics/releases/download/v${VERSION}/${archive}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  # Release assets are eventually consistent after a tag lands; retry 404s.
  if curl -fsSL --retry 5 --retry-delay 3 --retry-all-errors --retry-connrefused "$url" -o "$tmp/$archive" 2>/dev/null &&
    curl -fsSL --retry 3 --retry-delay 2 --retry-all-errors "$url.sha256" -o "$tmp/$archive.sha256" 2>/dev/null; then
    # Verify the checksum sidecar when one exists; a mismatch is fatal.
    expected="$(awk '{print $1}' "$tmp/$archive.sha256" 2>/dev/null)"
    if [ -n "$expected" ]; then
      if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
      else
        actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
      fi
      if [ "$expected" != "$actual" ]; then
        echo "analytics: checksum mismatch for $archive" >&2
        exit 1
      fi
    fi
    if tar -xzf "$tmp/$archive" -C "$tmp" 2>/dev/null &&
      [ -f "$tmp/analytics" ]; then
      echo "analytics: installed $BIN_DIR/analytics (v$VERSION release build)"
      install_bin "$tmp/analytics"
      exit 0
    fi
  fi
  # Manifest version not released yet (common right after a bump): fall back to
  # the latest published release so installs never require a toolchain.
  if command -v jq >/dev/null 2>&1 || command -v python3 >/dev/null 2>&1; then
    latest_url=$(curl -fsSL "https://api.github.com/repos/vishnutskumar/herdr-memex-analytics/releases/latest" 2>/dev/null | {
      if command -v jq >/dev/null 2>&1; then
        jq -r ".assets[] | select(.name | endswith(\"${os}-${arch}.tar.gz\")) | .browser_download_url" 2>/dev/null | head -1
      else
        python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(next(a['browser_download_url'] for a in d['assets'] if a['name'].endswith('${os}-${arch}.tar.gz')))
except Exception:
    pass
"
      fi
    })
    if [ -n "$latest_url" ] &&
      curl -fsSL --retry 3 --retry-delay 2 --retry-all-errors "$latest_url" -o "$tmp/$archive" 2>/dev/null &&
      tar -xzf "$tmp/$archive" -C "$tmp" 2>/dev/null &&
      [ -f "$tmp/analytics" ]; then
      echo "analytics: installed $BIN_DIR/analytics (latest release build)"
      install_bin "$tmp/analytics"
      exit 0
    fi
  fi
  echo "analytics: no release build for v$VERSION ${os}-${arch}, falling back to a local build" >&2
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
