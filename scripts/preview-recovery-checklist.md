# Preview Recovery Checklist

Use this when you need to verify preview Discord auto-recovery before touching the stable control plane.

## Preconditions

- Preview launchd job `com.itismyfield.remotecc.dcserver.preview` is installed and running.
- Preview runtime root `~/.remotecc-preview` has a valid `bot_settings.json`.
- Preview `allowed_bot_ids` includes a sender bot that can post into the test channel.
- Preview role map points the test channel at the intended workspace and provider.

Reference files:

- `~/.remotecc-preview/bot_settings.json`
- `~/.remotecc-preview/role_map.json`
- `~/.remotecc-preview/dcserver.stdout.log`

## Run

From the repo:

```bash
cd /Users/itismyfield/remotecc
REMOTECC_TEST_SENDER_TOKEN='***' scripts/preview-recovery-stress.sh --iterations 10
```

Or fold it into the live smoke/deploy gate:

```bash
cd /Users/itismyfield/remotecc
REMOTECC_TEST_SENDER_TOKEN='***' scripts/remotecc-discord-smoke.sh --deploy-live --preview-recovery-stress --preview-iterations 10
```

Optional flags:

- `--iterations N` to shorten or extend the run
- `--channel ID` to point at a different preview test channel

## What The Script Verifies

For each iteration it does all of the following:

1. Sends a real Discord message into the preview test channel
2. Waits for inflight state under `~/.remotecc-preview/runtime/discord_inflight/codex/<channel>.json`
3. Restarts `com.itismyfield.remotecc.dcserver.preview`
4. Waits for a `type=result` event after the saved `last_offset`
5. Confirms inflight cleanup
6. Captures per-iteration log snippets and a markdown/json report

Note:

- The script does not depend on Discord message-history reads.
- Current preview/sender bots can hit `403` on `messages` history, so success is judged from inflight state, output jsonl, and dcserver logs.

## Pass Criteria

- Report shows `Pass: 10`
- Report shows `Warn: 0`
- Report shows `Fail: 0`
- Each iteration has:
  - `Inflight detected: 1`
  - `Restart state: running`
  - `Marker count: 1`
  - `Error events after offset: 0`
  - `Response sent count: 1`
- No leftover inflight file remains after the run
- `launchctl print gui/$(id -u)/com.itismyfield.remotecc.dcserver.preview` still shows `state = running`

## If It Fails

Check these in order:

1. The generated `report.md`
2. The generated `results.json`
3. The per-iteration `log-*.txt` files next to the report
4. `~/.remotecc-preview/dcserver.stdout.log`
5. The wrapper output path recorded in the report for the failed iteration

## Stable Promotion Gate

Do not treat preview recovery as proven until:

- The stress run passes
- The preview job remains healthy after the last iteration
- There is no leftover inflight state
- The fix being tested is already covered by repo tests where practical
- If preview stress is run from `remotecc-discord-smoke.sh`, stable restart only proceeds after the preview gate passes
