#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  REMOTECC_TEST_SENDER_TOKEN=... scripts/preview-recovery-stress.sh [--iterations N] [--channel ID]

What it does:
  1. Sends a real Discord message into the preview test channel via an allowed sender bot
  2. Waits for preview inflight state to appear
  3. Restarts the preview dcserver launchd job
  4. Waits for the response to complete and verifies inflight cleanup
  5. Repeats the cycle and writes markdown/json reports

Options:
  --iterations N   Number of restart/recovery cycles to run (default: 10)
  --channel ID     Preview test channel ID (default: 1480377507390689430)
  --help           Show this message

Required env:
  REMOTECC_TEST_SENDER_TOKEN   Discord bot token for a sender bot already allowed by preview bot settings

Optional env:
  REMOTECC_PREVIEW_ROOT        Preview runtime root (default: ~/.remotecc-preview)
  REMOTECC_PREVIEW_LABEL       Preview launchd label (default: com.itismyfield.remotecc.dcserver.preview)
  REMOTECC_SENDER_BOT_ID       Sender bot user ID for report metadata (default: 1479017284805722200)
EOF
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "error: required command not found: $1" >&2
    exit 1
  }
}

epoch_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

iso_now() {
  date '+%Y-%m-%d %H:%M:%S %z'
}

json_post() {
  local token="$1"
  local url="$2"
  local body="$3"
  curl -fsS \
    -H "Authorization: Bot $token" \
    -H "Content-Type: application/json" \
    -X POST \
    -d "$body" \
    "$url"
}

json_get() {
  local token="$1"
  local url="$2"
  curl -fsS \
    -H "Authorization: Bot $token" \
    "$url"
}

append_json_result() {
  local file="$1"
  local item="$2"
  python3 - "$file" "$item" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
item = json.loads(sys.argv[2])
if path.exists():
    data = json.loads(path.read_text())
else:
    data = []
data.append(item)
path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
PY
}

iterations=10
channel_id="1480377507390689430"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --iterations)
      iterations="${2:-}"
      shift
      ;;
    --channel)
      channel_id="${2:-}"
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

if [[ -z "${REMOTECC_TEST_SENDER_TOKEN:-}" ]]; then
  echo "error: REMOTECC_TEST_SENDER_TOKEN is required" >&2
  exit 1
fi

need_cmd curl
need_cmd jq
need_cmd launchctl
need_cmd python3
need_cmd mktemp
need_cmd wc

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
preview_root="${REMOTECC_PREVIEW_ROOT:-$HOME/.remotecc-preview}"
preview_label="${REMOTECC_PREVIEW_LABEL:-com.itismyfield.remotecc.dcserver.preview}"
sender_bot_id="${REMOTECC_SENDER_BOT_ID:-1479017284805722200}"
api_base="https://discord.com/api/v10"
bot_settings="$preview_root/bot_settings.json"
stdout_log="$preview_root/dcserver.stdout.log"
inflight_path="$preview_root/runtime/discord_inflight/codex/$channel_id.json"
run_stamp="$(date '+%Y%m%d-%H%M%S')"
report_dir="${TMPDIR:-/tmp}remotecc-preview-recovery-stress-$run_stamp"
report_json="$report_dir/results.json"
report_md="$report_dir/report.md"

mkdir -p "$report_dir"
echo "[]" > "$report_json"

if [[ ! -f "$bot_settings" ]]; then
  echo "error: preview bot settings missing: $bot_settings" >&2
  exit 1
fi

preview_token="$(jq -r 'to_entries[0].value.token // empty' "$bot_settings")"
preview_key="$(jq -r 'to_entries[0].key // empty' "$bot_settings")"
if [[ -z "$preview_token" || -z "$preview_key" ]]; then
  echo "error: failed to resolve preview bot token from $bot_settings" >&2
  exit 1
fi

preview_me="$(json_get "$preview_token" "$api_base/users/@me")"
preview_bot_id="$(jq -r '.id' <<<"$preview_me")"
preview_bot_name="$(jq -r '.username' <<<"$preview_me")"

sender_me="$(json_get "$REMOTECC_TEST_SENDER_TOKEN" "$api_base/users/@me")"
sender_live_id="$(jq -r '.id' <<<"$sender_me")"
sender_bot_name="$(jq -r '.username' <<<"$sender_me")"

if [[ "$sender_live_id" != "$sender_bot_id" ]]; then
  echo "warning: configured sender bot id ($sender_bot_id) != live token owner ($sender_live_id)" >&2
  sender_bot_id="$sender_live_id"
fi

json_get "$preview_token" "$api_base/channels/$channel_id" >/dev/null

echo "preview_key=$preview_key"
echo "preview_bot_id=$preview_bot_id"
echo "preview_bot_name=$preview_bot_name"
echo "sender_bot_id=$sender_bot_id"
echo "sender_bot_name=$sender_bot_name"
echo "channel_id=$channel_id"
echo "iterations=$iterations"
echo "report_dir=$report_dir"

for ((i = 1; i <= iterations; i++)); do
  iter_marker="RCC-RECOVERY-STRESS-${run_stamp}-${i}"
  iter_start_ms="$(epoch_ms)"
  iter_start_readable="$(iso_now)"
  log_start_bytes=0
  if [[ -f "$stdout_log" ]]; then
    log_start_bytes="$(wc -c < "$stdout_log" | tr -d ' ')"
  fi

  prompt=$(
    cat <<EOF
MARKER:$iter_marker
Start your first line with exactly MARKER:$iter_marker.
Then read these files and summarize how restart + inflight recovery works, including one likely failure mode and the guard now preventing it:
- /Users/itismyfield/remotecc/src/services/discord/recovery.rs
- /Users/itismyfield/remotecc/src/services/discord/tmux.rs
- /Users/itismyfield/remotecc/src/services/claude.rs
- /Users/itismyfield/remotecc/src/services/codex.rs
- /Users/itismyfield/remotecc/src/services/discord/inflight.rs
- /Users/itismyfield/remotecc/src/services/discord/runtime_store.rs
- /Users/itismyfield/remotecc/src/services/discord/mod.rs
- /Users/itismyfield/remotecc/src/main.rs
Keep it concise but do the file reads first.
EOF
  )

  body="$(jq -n --arg content "$prompt" '{content: $content}')"
  send_json="$(json_post "$REMOTECC_TEST_SENDER_TOKEN" "$api_base/channels/$channel_id/messages" "$body")"
  user_message_id="$(jq -r '.id' <<<"$send_json")"
  user_message_ts="$(jq -r '.timestamp' <<<"$send_json")"
  echo
  echo "[$i/$iterations] sent marker=$iter_marker message_id=$user_message_id at $iter_start_readable"

  inflight_detected=0
  inflight_session_id=""
  inflight_output_path=""
  inflight_offset=""
  for _ in $(seq 1 60); do
    if [[ -f "$inflight_path" ]]; then
      inflight_mtime="$(stat -f '%m' "$inflight_path")"
      if [[ "$inflight_mtime" -ge $((iter_start_ms / 1000)) ]]; then
        inflight_detected=1
        inflight_json="$(cat "$inflight_path")"
        inflight_session_id="$(jq -r '.session_id // empty' <<<"$inflight_json")"
        inflight_output_path="$(jq -r '.output_path // empty' <<<"$inflight_json")"
        inflight_offset="$(jq -r '.last_offset // empty' <<<"$inflight_json")"
        break
      fi
    fi
    sleep 1
  done

  restart_state="skipped"
  restart_pid=""
  if [[ "$inflight_detected" -eq 1 ]]; then
    launchctl kickstart -k "gui/$(id -u)/$preview_label"
    sleep 2
    launch_dump="$(launchctl print "gui/$(id -u)/$preview_label" 2>/dev/null || true)"
    if grep -q "state = running" <<<"$launch_dump"; then
      restart_state="running"
      restart_pid="$(awk '/pid = / {print $3; exit}' <<<"$launch_dump")"
    else
      restart_state="not-running"
    fi
  fi

  marker_count=0
  bot_after_count=0
  result_count=0
  error_count=0
  response_done=0
  deadline_epoch=$(( $(date +%s) + 240 ))

  while [[ "$(date +%s)" -lt "$deadline_epoch" ]]; do
    if [[ -n "$inflight_output_path" && -f "$inflight_output_path" ]]; then
      output_probe="$(
        python3 - "$inflight_output_path" "$inflight_offset" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
offset = int(sys.argv[2] or "0")
chunk = path.read_bytes()[offset:]
result_count = 0
error_count = 0
for raw in chunk.splitlines():
    try:
        obj = json.loads(raw)
    except Exception:
        continue
    item_type = obj.get("type")
    if item_type == "result":
        result_count += 1
    elif item_type == "error":
        error_count += 1
print(json.dumps({
    "result_count": result_count,
    "error_count": error_count,
    "bytes_after_offset": len(chunk),
}))
PY
      )"
      result_count="$(jq -r '.result_count' <<<"$output_probe")"
      error_count="$(jq -r '.error_count' <<<"$output_probe")"
      bot_after_count="$(jq -r '.bytes_after_offset' <<<"$output_probe")"
      marker_count="$result_count"
    fi
    if [[ "$result_count" -ge 1 && ! -f "$inflight_path" ]]; then
      response_done=1
      break
    fi
    sleep 3
  done

  iter_end_ms="$(epoch_ms)"
  duration_ms=$((iter_end_ms - iter_start_ms))

  log_tmp="$report_dir/log-$i.txt"
  if [[ -f "$stdout_log" ]]; then
    python3 - "$stdout_log" "$log_start_bytes" "$log_tmp" <<'PY'
import pathlib
import sys

src = pathlib.Path(sys.argv[1])
offset = int(sys.argv[2])
dst = pathlib.Path(sys.argv[3])
data = src.read_bytes()
dst.write_bytes(data[offset:])
PY
  else
    : > "$log_tmp"
  fi

  watcher_restarted=0
  response_sent_count="$(rg -c --no-filename "▶ Response sent" "$log_tmp" || true)"
  if rg -q "tmux watcher started" "$log_tmp"; then
    watcher_restarted=1
  fi

  suspicious_patterns="$(rg -n "panic|error|failed|queued command|output already contains result|duplicate" "$log_tmp" || true)"
  suspicious_compact="$(printf '%s' "$suspicious_patterns" | tr '\n' ';' | sed 's/"/\\"/g')"

  status="pass"
  notes=()
  if [[ "$inflight_detected" -ne 1 ]]; then
    status="fail"
    notes+=("inflight_not_detected")
  fi
  if [[ "$restart_state" != "running" ]]; then
    status="fail"
    notes+=("preview_restart_not_running")
  fi
  if [[ "$response_done" -ne 1 ]]; then
    status="fail"
    notes+=("response_not_completed")
  fi
  if [[ -f "$inflight_path" ]]; then
    status="fail"
    notes+=("inflight_leftover")
  fi
  if [[ "$result_count" -lt 1 ]]; then
    status="fail"
    notes+=("result_missing")
  fi
  if [[ "$result_count" -gt 1 ]]; then
    if [[ "$status" == "pass" ]]; then
      status="warn"
    fi
    notes+=("duplicate_result_events")
  fi
  if [[ "$error_count" -gt 0 ]]; then
    if [[ "$status" == "pass" ]]; then
      status="warn"
    fi
    notes+=("error_event_after_offset")
  fi
  if [[ "$watcher_restarted" -ne 1 ]]; then
    notes+=("watcher_restart_not_seen")
  fi
  if [[ -n "$suspicious_patterns" ]]; then
    if [[ "$status" == "pass" ]]; then
      status="warn"
    fi
    notes+=("suspicious_log_pattern")
  fi

  if [[ "${#notes[@]}" -gt 0 ]]; then
    notes_json="$(printf '%s\n' "${notes[@]}" | jq -R . | jq -s .)"
  else
    notes_json='[]'
  fi
  item_json="$(jq -n \
    --arg marker "$iter_marker" \
    --arg status "$status" \
    --arg start "$iter_start_readable" \
    --arg start_ts "$user_message_ts" \
    --arg user_message_id "$user_message_id" \
    --arg inflight_session_id "$inflight_session_id" \
    --arg inflight_output_path "$inflight_output_path" \
    --arg inflight_offset "$inflight_offset" \
    --arg restart_state "$restart_state" \
    --arg restart_pid "$restart_pid" \
    --arg response_ids "" \
    --arg suspicious_log "$suspicious_compact" \
    --arg log_file "$log_tmp" \
    --argjson inflight_detected "$inflight_detected" \
    --argjson response_done "$response_done" \
    --argjson marker_count "$result_count" \
    --argjson bot_after_count "$bot_after_count" \
    --argjson watcher_restarted "$watcher_restarted" \
    --argjson error_count "$error_count" \
    --argjson response_sent_count "${response_sent_count:-0}" \
    --argjson duration_ms "$duration_ms" \
    --argjson notes "$notes_json" \
    '{
      marker: $marker,
      status: $status,
      started_at: $start,
      user_message_timestamp: $start_ts,
      user_message_id: $user_message_id,
      inflight_detected: $inflight_detected,
      inflight_session_id: $inflight_session_id,
      inflight_output_path: $inflight_output_path,
      inflight_offset: $inflight_offset,
      restart_state: $restart_state,
      restart_pid: $restart_pid,
      response_done: $response_done,
      response_ids: ($response_ids | select(length > 0) // ""),
      marker_count: $marker_count,
      bot_after_count: $bot_after_count,
      watcher_restarted: $watcher_restarted,
      error_count: $error_count,
      response_sent_count: $response_sent_count,
      duration_ms: $duration_ms,
      suspicious_log: ($suspicious_log | select(length > 0) // ""),
      notes: $notes,
      log_file: $log_file
    }')"
  append_json_result "$report_json" "$item_json"

  echo "[$i/$iterations] status=$status inflight=$inflight_detected restart=$restart_state marker_count=$marker_count bot_msgs=$bot_after_count duration_ms=$duration_ms"
  if [[ "${#notes[@]}" -gt 0 ]]; then
    echo "[$i/$iterations] notes=$(printf '%s,' "${notes[@]}" | sed 's/,$//')"
  fi
done

summary_json="$(jq -n \
  --arg generated_at "$(iso_now)" \
  --arg preview_root "$preview_root" \
  --arg preview_label "$preview_label" \
  --arg preview_key "$preview_key" \
  --arg preview_bot_id "$preview_bot_id" \
  --arg preview_bot_name "$preview_bot_name" \
  --arg sender_bot_id "$sender_bot_id" \
  --arg sender_bot_name "$sender_bot_name" \
  --arg channel_id "$channel_id" \
  --argjson iterations "$iterations" \
  --slurpfile results "$report_json" \
  '{
    generated_at: $generated_at,
    preview_root: $preview_root,
    preview_label: $preview_label,
    preview_key: $preview_key,
    preview_bot_id: $preview_bot_id,
    preview_bot_name: $preview_bot_name,
    sender_bot_id: $sender_bot_id,
    sender_bot_name: $sender_bot_name,
    channel_id: $channel_id,
    iterations: $iterations,
    results: $results[0]
  }')"

summary_path="$report_dir/summary.json"
printf '%s\n' "$summary_json" > "$summary_path"

pass_count="$(jq '[.[] | select(.status == "pass")] | length' "$report_json")"
warn_count="$(jq '[.[] | select(.status == "warn")] | length' "$report_json")"
fail_count="$(jq '[.[] | select(.status == "fail")] | length' "$report_json")"

{
  echo "# Preview Recovery Stress Report"
  echo
  echo "- Generated: $(iso_now)"
  echo "- Preview root: \`$preview_root\`"
  echo "- Preview label: \`$preview_label\`"
  echo "- Channel: \`$channel_id\`"
  echo "- Preview bot: \`$preview_bot_name ($preview_bot_id)\`"
  echo "- Sender bot: \`$sender_bot_name ($sender_bot_id)\`"
  echo "- Iterations: \`$iterations\`"
  echo "- Pass: \`$pass_count\`"
  echo "- Warn: \`$warn_count\`"
  echo "- Fail: \`$fail_count\`"
  echo
  echo "## Iterations"
  jq -r '.[] | [
      "### " + .marker,
      "- Status: `" + .status + "`",
      "- Duration ms: `" + (.duration_ms|tostring) + "`",
      "- Inflight detected: `" + (.inflight_detected|tostring) + "`",
      "- Restart state: `" + .restart_state + "`",
      "- Marker count: `" + (.marker_count|tostring) + "`",
      "- Bytes written after offset: `" + (.bot_after_count|tostring) + "`",
      "- Error events after offset: `" + (.error_count|tostring) + "`",
      "- Watcher restarted seen: `" + (.watcher_restarted|tostring) + "`",
      "- Response sent count: `" + (.response_sent_count|tostring) + "`",
      "- Notes: " + (if (.notes|length) > 0 then (.notes | join(", ")) else "none" end),
      "- Response IDs: " + (if .response_ids != "" then .response_ids else "none" end),
      "- Log file: `" + .log_file + "`",
      (if .suspicious_log != "" then "- Suspicious log: `" + .suspicious_log + "`" else "- Suspicious log: none" end),
      ""
    ] | .[]' "$report_json"
} > "$report_md"

echo
echo "report_json=$report_json"
echo "summary_json=$summary_path"
echo "report_md=$report_md"
