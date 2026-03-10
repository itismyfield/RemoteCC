#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASES_DIR="$HOME/.remotecc/releases"
CURRENT_LINK="$RELEASES_DIR/current"
PREVIOUS_LINK="$RELEASES_DIR/previous"
BIN_DIR="$HOME/.remotecc/bin"
STABLE_BIN="$BIN_DIR/remotecc"
STAMP="$(date +%Y%m%d-%H%M%S)"
RELEASE_DIR="$RELEASES_DIR/$STAMP"

mkdir -p "$RELEASE_DIR"
mkdir -p "$BIN_DIR"

if [[ -L "$CURRENT_LINK" ]]; then
  PREVIOUS_TARGET="$(readlink "$CURRENT_LINK" || true)"
  if [[ -n "${PREVIOUS_TARGET:-}" ]]; then
    ln -sfn "$PREVIOUS_TARGET" "$PREVIOUS_LINK"
  fi
fi

cd "$REPO_ROOT"
cargo build --release

install -m 755 "$REPO_ROOT/target/release/remotecc" "$RELEASE_DIR/remotecc"
ln -sfn "$RELEASE_DIR" "$CURRENT_LINK"
rm -f "$STABLE_BIN"
ln -s "$CURRENT_LINK/remotecc" "$STABLE_BIN"

echo "Installed RemoteCC stable release: $RELEASE_DIR"
echo "Updated current symlink: $CURRENT_LINK"
echo "Updated stable launcher: $STABLE_BIN -> $CURRENT_LINK/remotecc"
if [[ -L "$PREVIOUS_LINK" ]]; then
  echo "Updated previous symlink: $PREVIOUS_LINK"
fi
