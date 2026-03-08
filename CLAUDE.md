You can get Rust build methods from build_manual.md file

## Trunk-Based Development

- **`dev` branch** = 개발 브랜치. 코드 수정은 여기서.
- **`main` branch** = 릴리즈 트렁크. 항상 배포 가능 상태.
- **dev 브랜치에서는 빌드/재시작 절대 금지** — 코드 수정만 한다
- 릴리즈 플로우: `dev` → `main` 머지 → 빌드 → `~/.remotecc/bin/remotecc`에 배포 → dcserver 재시작
- 프로덕션 바이너리: `~/.remotecc/bin/remotecc` (빌드 아웃풋 `target/release/`와 분리)
- dcserver 재시작 시 Claude 작업 세션(remoteCC-*)은 건드리지 않음 (자동 재연결됨)

## Runtime Operations

- Stable Discord control plane은 launchd job `com.itismyfield.remotecc.dcserver`로 운영한다.
- launchd는 `~/.remotecc/bin/remotecc --dcserver`를 실행하고, wrapper는 `~/.remotecc/releases/current/remotecc`를 우선 사용한다.
- 안정 배포는 `./scripts/install-stable.sh`로만 수행한다.
- dcserver 재시작은 `~/.remotecc/bin/remotecc --restart-dcserver` 또는 `launchctl kickstart -k gui/$(id -u)/com.itismyfield.remotecc.dcserver`만 사용한다.
- 지속 실행용 dcserver를 `target/debug/remotecc --dcserver` 또는 `target/release/remotecc --dcserver`로 직접 띄우지 않는다.
- `remoteCC-*` tmux 세션은 active work session이므로, 사용자가 명시적으로 원하지 않는 한 죽이지 않는다.
- 운영 로그는 `~/.remotecc/dcserver.stdout.log`, `~/.remotecc/dcserver.stderr.log`에서 확인한다.
- 현재는 별도 rescue bot이 없다. Discord 제어면을 잃지 않도록 launchd-managed stable 경로를 우선 보호한다.
- Codex/Claude 공용 운영 규칙은 `AGENTS.md`에도 동일하게 정리되어 있으니 함께 유지한다.

## CRITICAL: Do Not Change Design Without Permission

- **NEVER change product design/UX without explicit user request**
- Bug fix and design change are completely different things
- If you identify a "potential improvement" or "UX issue", only REPORT it - do NOT implement
- When user says "fix it", fix only the BUGS, not your suggestions
- If you think design change is needed, ASK FIRST before implementing
- Violating this rule wastes user's time and breaks trust

## Build Guidelines

- **IMPORTANT: Only build when the user explicitly requests it**
- Never run build commands automatically after code changes
- Never run build commands to "verify" or "check" code
- Do not use `cargo build`, `python3 build.py`, or any build commands unless user asks
- Focus only on code modifications; user handles all builds manually

## Version Management

- Version is defined in `Cargo.toml` (line 3: `version = "x.x.x"`)
- All version displays use `env!("CARGO_PKG_VERSION")` macro to read from Cargo.toml
- To update version: only modify `Cargo.toml`, all other locations reflect automatically
- Never hardcode version strings in source code

## Theme Color System

- All color definitions must use `Color::Indexed(number)` format directly
- Each UI element must have its own uniquely named color field, even if the color value is the same as another element
- Never reference another element's color (e.g., don't use `theme.bg_selected` for viewer search input)
- Define dedicated color fields in the appropriate Colors struct (e.g., `ViewerColors.search_input_text`)
- Color values may be duplicated across fields, but names must be unique and semantically meaningful

### Theme File Locations

- **Source of truth**: `src/ui/theme.rs` - 테마 색상 값과 JSON 주석 모두 이 파일에서 정의
- **Generated files**: `~/.remotecc/themes/*.json` - 프로그램 실행 시 생성되는 사용자 설정 파일
- 테마 수정 시 반드시 `src/ui/theme.rs`를 수정해야 함 (생성된 JSON 파일 직접 수정 금지)
- JSON 주석 형식: `"__field__": "설명"` - 이 주석들도 theme.rs의 `to_json()` 함수 내에 정의됨
