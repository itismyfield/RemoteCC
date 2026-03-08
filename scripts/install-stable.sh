#!/bin/bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASES_DIR="$HOME/.remotecc/releases"
CURRENT_LINK="$RELEASES_DIR/current"
STAMP="$(date +%Y%m%d-%H%M%S)"
RELEASE_DIR="$RELEASES_DIR/$STAMP"

mkdir -p "$RELEASE_DIR"

cd "$REPO_ROOT"
cargo build --release

install -m 755 "$REPO_ROOT/target/release/remotecc" "$RELEASE_DIR/remotecc"
ln -sfn "$RELEASE_DIR" "$CURRENT_LINK"

echo "Installed RemoteCC stable release: $RELEASE_DIR"
echo "Updated current symlink: $CURRENT_LINK"
