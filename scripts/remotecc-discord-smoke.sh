#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/remotecc-discord-smoke.sh [--deploy-live] [--reset-wrappers]

What it does:
  1. Runs the UTF-8 regression tests for tmux wrapper tool previews
  2. Runs the full cargo test suite
  3. Builds the release binary
  4. Optionally deploys the release binary to ~/.remotecc/bin/remotecc
  5. Optionally restarts dcserver and resets remoteCC-* wrapper sessions

Options:
  --deploy-live      Install target/release/remotecc into ~/.remotecc/bin/remotecc
                     and restart dcserver via --restart-dcserver.
  --reset-wrappers   Kill remoteCC-* tmux sessions after deploy so the next
                     Discord message recreates every wrapper with the new binary.
                     Requires --deploy-live.
  --help             Show this message.

Notes:
  - This script cannot inject a real Discord user message.
  - After --deploy-live, you still need one real Korean prompt in #mac-mini
    (or the affected channel) to confirm end-to-end reply generation.
EOF
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: required command not found: $1" >&2
    exit 1
  }
}

deploy_live=0
reset_wrappers=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --deploy-live)
      deploy_live=1
      ;;
    --reset-wrappers)
      reset_wrappers=1
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

if [[ "$reset_wrappers" -eq 1 && "$deploy_live" -ne 1 ]]; then
  echo "error: --reset-wrappers requires --deploy-live" >&2
  exit 1
fi

need_cmd cargo
need_cmd shasum
if [[ "$deploy_live" -eq 1 ]]; then
  need_cmd tmux
  need_cmd ps
  need_cmd awk
  need_cmd codesign
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release_bin="$repo_dir/target/release/remotecc"
prod_bin="$HOME/.remotecc/bin/remotecc"

cd "$repo_dir"

echo "[1/4] UTF-8 regression smoke"
cargo test format_tool_detail -- --nocapture

echo "[2/4] Full cargo test"
cargo test

echo "[3/4] Release build"
cargo build --release

release_sha="$(shasum -a 256 "$release_bin" | awk '{print $1}')"
echo "release_sha256=$release_sha"

if [[ "$deploy_live" -ne 1 ]]; then
  echo
  echo "Local smoke passed."
  echo "Manual check required: if this change touches Discord runtime paths,"
  echo "rerun with --deploy-live and send a short Korean prompt in #mac-mini."
  exit 0
fi

if [[ ! -x "$prod_bin" ]]; then
  echo "error: production binary not found: $prod_bin" >&2
  exit 1
fi

echo "[4/4] Deploy live binary"
backup_path="$prod_bin.$(date +%Y%m%d-%H%M%S).bak"
cp "$prod_bin" "$backup_path"
tmp_deploy="$prod_bin.new.$$"
rm -f "$tmp_deploy"
install -m 755 "$release_bin" "$tmp_deploy"
codesign --force --sign - "$tmp_deploy" >/dev/null
signed_sha="$(shasum -a 256 "$tmp_deploy" | awk '{print $1}')"
mv -f "$tmp_deploy" "$prod_bin"

prod_sha="$(shasum -a 256 "$prod_bin" | awk '{print $1}')"
echo "prod_sha256=$prod_sha"
echo "signed_sha256=$signed_sha"
echo "backup_path=$backup_path"

if [[ "$prod_sha" != "$signed_sha" ]]; then
  echo "error: deployed binary hash mismatch after codesign" >&2
  exit 1
fi
codesign --verify --verbose=2 "$prod_bin" >/dev/null

echo "Restarting dcserver"
"$prod_bin" --restart-dcserver
sleep 3

if ! tmux has-session -t remoteCC 2>/dev/null; then
  echo "error: tmux session remoteCC is not running after restart" >&2
  exit 1
fi

dcserver_ps="$(ps -axo pid,etime,command | awk '/remotecc --dcserver/ && $0 !~ /awk/')"
if [[ -z "$dcserver_ps" ]]; then
  echo "error: no running dcserver process found after restart" >&2
  exit 1
fi
echo "$dcserver_ps"

if [[ "$reset_wrappers" -eq 1 ]]; then
  echo "Resetting remoteCC-* wrapper sessions"
  while read -r session_name; do
    [[ -n "$session_name" ]] || continue
    tmux kill-session -t "$session_name" >/dev/null 2>&1 || true
    rm -f "/tmp/remotecc-$session_name.jsonl" "/tmp/remotecc-$session_name.input" "/tmp/remotecc-$session_name.prompt"
  done < <(tmux list-sessions 2>/dev/null | awk -F: '/^remoteCC-/ {print $1}')

  sleep 2
  if ps -axo pid,etime,command | awk '/remotecc --tmux-wrapper/ && $0 !~ /awk/' | grep -q .; then
    echo "error: tmux wrapper processes are still running after reset" >&2
    exit 1
  fi
  echo "wrapper_processes=0"
fi

echo
echo "Live smoke passed."
echo "Manual Discord check required: send one short Korean prompt in #mac-mini and confirm a reply."
