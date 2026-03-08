# RemoteCC Agent Notes

## Runtime Topology

- Stable Discord control plane은 launchd job `com.itismyfield.remotecc.dcserver`로 운영한다.
- launchd 실행 경로는 `~/.remotecc/bin/remotecc --dcserver`다.
- `~/.remotecc/bin/remotecc`는 `~/.remotecc/releases/current/remotecc`를 우선 사용하고, stable release가 없을 때만 repo build output으로 fallback 한다.
- 채널별 작업은 `remoteCC-*` tmux 세션에 남고, dcserver 재시작 시 watcher가 다시 붙는다.

## Safe Workflow

- 일반 개발은 `/Users/itismyfield/remotecc` working tree에서 진행한다.
- stable control-plane 배포는 `./scripts/install-stable.sh`로만 한다.
- dcserver 재시작은 `~/.remotecc/bin/remotecc --restart-dcserver` 또는 `launchctl kickstart -k gui/$(id -u)/com.itismyfield.remotecc.dcserver`만 사용한다.
- 지속 실행용 dcserver를 `target/debug/remotecc --dcserver`나 `target/release/remotecc --dcserver`로 직접 띄우지 않는다.
- 사용자가 명시적으로 원하지 않는 한 `remoteCC-*` tmux 세션을 죽이지 않는다.

## Operational Guardrails

- 운영 로그는 `~/.remotecc/dcserver.stdout.log`, `~/.remotecc/dcserver.stderr.log`에서 확인한다.
- 현재는 별도 rescue bot이 없다. Discord 안에서 죽은 메인 bot을 자동 복구할 수 없으므로, launchd-managed stable 경로를 우선 보호한다.
- risky change를 적용할 때는 먼저 repo에서 수정하고, stable 반영은 배포 스크립트 이후에만 수행한다.
