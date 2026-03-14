#!/bin/bash
set -euo pipefail

# Scheduled release script for RemoteCC
# Performs: version bump → merge → build → tag+push → restart

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="$HOME/.remotecc/scheduled-release.log"

log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }

cd "$REPO_ROOT"

log "=== Scheduled Release Start ==="

# 1. Ensure on dev branch
BRANCH=$(git branch --show-current)
if [[ "$BRANCH" != "dev" ]]; then
  log "ERROR: not on dev branch (on $BRANCH), aborting"
  exit 1
fi

# 2. Read current version and bump patch
CURRENT_VER=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_VER"
NEW_PATCH=$((PATCH + 1))
NEW_VER="$MAJOR.$MINOR.$NEW_PATCH"
log "Version bump: $CURRENT_VER → $NEW_VER"

sed -i '' "s/^version = \"$CURRENT_VER\"/version = \"$NEW_VER\"/" Cargo.toml
git add Cargo.toml
git commit -m "chore: bump version to v$NEW_VER"

# 3. Merge to main
git checkout main
git merge dev --no-ff -m "release: v$NEW_VER"
log "Merged dev → main"

# 4. Build
log "Building..."
python3 build.py --macos >> "$LOG" 2>&1
log "Build complete"

# 5. Tag and push
git tag "v$NEW_VER"
git push origin main --tags >> "$LOG" 2>&1
log "Tagged and pushed v$NEW_VER"

# 6. Rebase dev
git checkout dev
git rebase main
log "Rebased dev onto main"

# 7. Restart dcserver
~/.remotecc/bin/remotecc --restart-dcserver >> "$LOG" 2>&1
log "dcserver restarted"

log "=== Release v$NEW_VER Complete ==="
