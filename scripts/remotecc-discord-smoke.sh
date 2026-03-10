#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/remotecc-discord-smoke.sh [--deploy-live] [--reset-wrappers] [--preview-recovery-stress] [--preview-iterations N]
                                    [--report-channel-id ID --report-provider claude|codex]

What it does:
  1. Runs the UTF-8 regression tests for tmux wrapper tool previews
  2. Runs the full cargo test suite
  3. Builds the release binary
  4. Optionally installs a stable release via scripts/install-stable.sh
  5. Optionally runs preview restart/inflight recovery stress before stable restart
  6. Optionally restarts dcserver and resets remoteCC-* wrapper sessions

Options:
  --deploy-live      Install a new stable release with scripts/install-stable.sh
                     and restart dcserver via ~/.remotecc/bin/remotecc --restart-dcserver.
  --reset-wrappers   Kill remoteCC-* tmux sessions after deploy so the next
                     Discord message recreates every wrapper with the new binary.
                     Requires --deploy-live.
  --preview-recovery-stress
                     Run scripts/preview-recovery-stress.sh after install-stable.sh
                     and before stable dcserver restart. Requires --deploy-live and
                     REMOTECC_TEST_SENDER_TOKEN in the environment.
  --preview-iterations N
                     Iteration count for preview-recovery-stress.sh (default: 10).
                     Requires --preview-recovery-stress.
  --report-channel-id ID
                     When restarting stable dcserver, send restart completion follow-up
                     to this Discord channel.
  --report-provider PROVIDER
                     Provider for the restart follow-up channel (`claude` or `codex`).
                     Requires --report-channel-id. If both are omitted, the script falls
                     back to REMOTECC_REPORT_CHANNEL_ID / REMOTECC_REPORT_PROVIDER.
  --help             Show this message.

Notes:
  - This script cannot inject a real Discord user message.
  - After --deploy-live, you should still send one real Korean prompt or `/health`
    in the affected Discord channel to confirm end-to-end reply generation.
  - If preview stress fails, current release symlink is rolled back to previous
    and stable dcserver restart is skipped.
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
preview_recovery_stress=0
preview_iterations=10
preview_iterations_explicit=0
report_channel_id="${REMOTECC_REPORT_CHANNEL_ID:-}"
report_provider="${REMOTECC_REPORT_PROVIDER:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --deploy-live)
      deploy_live=1
      ;;
    --reset-wrappers)
      reset_wrappers=1
      ;;
    --preview-recovery-stress)
      preview_recovery_stress=1
      ;;
    --preview-iterations)
      preview_iterations="${2:-}"
      preview_iterations_explicit=1
      shift
      ;;
    --report-channel-id)
      report_channel_id="${2:-}"
      shift
      ;;
    --report-provider)
      report_provider="${2:-}"
      shift
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

if [[ "$preview_recovery_stress" -eq 1 && "$deploy_live" -ne 1 ]]; then
  echo "error: --preview-recovery-stress requires --deploy-live" >&2
  exit 1
fi

if [[ "$preview_recovery_stress" -ne 1 && "$preview_iterations_explicit" -eq 1 ]]; then
  echo "error: --preview-iterations requires --preview-recovery-stress" >&2
  exit 1
fi

if [[ -n "${report_channel_id:-}" && -z "${report_provider:-}" ]]; then
  echo "error: report target requires both --report-channel-id and --report-provider" >&2
  exit 1
fi

if [[ -z "${report_channel_id:-}" && -n "${report_provider:-}" ]]; then
  echo "error: report target requires both --report-channel-id and --report-provider" >&2
  exit 1
fi

if [[ -n "${report_provider:-}" && "$report_provider" != "claude" && "$report_provider" != "codex" ]]; then
  echo "error: --report-provider must be 'claude' or 'codex'" >&2
  exit 1
fi

need_cmd cargo
need_cmd shasum
if [[ "$deploy_live" -eq 1 ]]; then
  need_cmd launchctl
  need_cmd tmux
  need_cmd awk
  need_cmd rg
  need_cmd lsof
fi
if [[ "$preview_recovery_stress" -eq 1 ]]; then
  need_cmd python3
fi

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
release_bin="$repo_dir/target/release/remotecc"
stable_bin="$HOME/.remotecc/bin/remotecc"
current_link="$HOME/.remotecc/releases/current"
previous_link="$HOME/.remotecc/releases/previous"
stdout_log="$HOME/.remotecc/dcserver.stdout.log"
launchd_label="com.itismyfield.remotecc.dcserver"
preview_label="com.itismyfield.remotecc.dcserver.preview"

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

if [[ ! -x "$stable_bin" ]]; then
  echo "error: stable wrapper not found: $stable_bin" >&2
  exit 1
fi

echo "[4/4] Install stable release"
"$repo_dir/scripts/install-stable.sh"

if [[ ! -L "$current_link" ]]; then
  echo "error: current release symlink missing after install: $current_link" >&2
  exit 1
fi

current_target="$(readlink "$current_link")"
if [[ ! -x "$current_target/remotecc" ]]; then
  echo "error: installed release missing binary: $current_target/remotecc" >&2
  exit 1
fi

current_sha="$(shasum -a 256 "$current_target/remotecc" | awk '{print $1}')"
echo "current_release=$current_target"
echo "current_sha256=$current_sha"

if [[ ! -L "$stable_bin" ]]; then
  echo "error: stable launcher is not a symlink: $stable_bin" >&2
  exit 1
fi

stable_target="$(readlink "$stable_bin")"
echo "stable_launcher=$stable_target"
if [[ "$stable_target" != "$current_link/remotecc" ]]; then
  echo "error: stable launcher does not point at current release: $stable_target" >&2
  exit 1
fi

if [[ -L "$previous_link" ]]; then
  echo "previous_release=$(readlink "$previous_link")"
fi

if [[ "$preview_recovery_stress" -eq 1 ]]; then
  if [[ -z "${REMOTECC_TEST_SENDER_TOKEN:-}" ]]; then
    echo "error: REMOTECC_TEST_SENDER_TOKEN is required for --preview-recovery-stress" >&2
    exit 1
  fi

  echo "[5/6] Preview recovery stress"
  if ! REMOTECC_TEST_SENDER_TOKEN="$REMOTECC_TEST_SENDER_TOKEN" \
    "$repo_dir/scripts/preview-recovery-stress.sh" --iterations "$preview_iterations"; then
    echo "error: preview recovery stress failed" >&2
    if [[ -L "$previous_link" ]]; then
      previous_target="$(readlink "$previous_link")"
      ln -sfn "$previous_target" "$current_link"
      echo "rolled_back_current_release=$previous_target"
      launchctl kickstart -k "gui/$(id -u)/$preview_label" >/dev/null 2>&1 || true
    else
      echo "warning: preview stress failed and no previous release link was available for rollback" >&2
    fi
    exit 1
  fi
fi

restart_step_label="[5/5]"
if [[ "$preview_recovery_stress" -eq 1 ]]; then
  restart_step_label="[6/6]"
fi

echo "$restart_step_label Restart stable dcserver"
echo "Restarting dcserver"
restart_cmd=("$stable_bin" "--restart-dcserver")
if [[ -n "${report_channel_id:-}" ]]; then
  echo "restart_report_target=${report_provider}:${report_channel_id}"
  restart_cmd+=(
    "--report-channel-id" "$report_channel_id"
    "--report-provider" "$report_provider"
  )
fi
"${restart_cmd[@]}"
sleep 2

launchd_dump="$(launchctl print "gui/$(id -u)/$launchd_label" 2>/dev/null || true)"
if [[ -z "$launchd_dump" ]]; then
  echo "error: launchctl has no state for $launchd_label after restart" >&2
  exit 1
fi

if ! grep -q "state = running" <<<"$launchd_dump"; then
  echo "error: launchd job is not running after restart" >&2
  echo "$launchd_dump"
  exit 1
fi
echo "$launchd_dump" | rg "state =|pid =|program =" || true

dcserver_pid="$(awk '/pid = / {print $3; exit}' <<<"$launchd_dump")"
if [[ -z "${dcserver_pid:-}" ]]; then
  echo "error: failed to parse dcserver pid from launchctl output" >&2
  exit 1
fi

mapped_binary="$(lsof -p "$dcserver_pid" 2>/dev/null | awk '/ txt / && /remotecc$/ {print $NF; exit}')"
if [[ -z "${mapped_binary:-}" ]]; then
  echo "error: failed to resolve mapped dcserver binary via lsof for pid $dcserver_pid" >&2
  exit 1
fi
echo "dcserver_mapped_binary=$mapped_binary"
if [[ "$mapped_binary" != "$current_target/remotecc" ]]; then
  echo "error: dcserver is not running the current release binary" >&2
  exit 1
fi

if [[ ! -f "$stdout_log" ]]; then
  echo "error: stdout log missing after restart: $stdout_log" >&2
  exit 1
fi

if ! tail -n 200 "$stdout_log" | rg -q "Bot connected|Codex bot ready|Claude bot ready"; then
  echo "error: ready markers not found in $stdout_log after restart" >&2
  tail -n 80 "$stdout_log"
  exit 1
fi

echo "ready_markers=ok"
echo "tmux_sessions=$(tmux list-sessions 2>/dev/null | awk -F: '/^remoteCC-/ {count++} END {print count+0}')"

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
echo "Recommended Discord check: run /health and send one short Korean prompt in the affected channel."
