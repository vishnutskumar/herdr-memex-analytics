#!/usr/bin/env bash
# herdr `[[build]]` step: build the analytics binary into $ROOT/bin.
#
# Runs on `herdr plugin install vishnutskumar/herdr-memex-analytics`. `herdr plugin link`
# skips the build step — from a linked checkout run `cargo build --release` and the
# plugin scripts pick up target/release/analytics directly.
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
