---
name: rcc-smoke-test
description: RemoteCC 배포 후 선별적 스모크테스트. 작업 범위에 따라 채널을 선정하고 PCD API로 테스트 메시지를 전송한 뒤 응답 여부를 확인한다.
---

RemoteCC 스모크테스트 스킬. `/pcd-send` 스킬을 기반으로 동작한다.

## Flow

### 1. 테스트 대상 선정

작업 범위에 따라 테스트 채널을 선별한다:

- **최소 (코어 변경 없음)**: remotecc-cc, remotecc-cdx (2개)
- **중간 (provider 라우팅/메시지 처리 변경)**: 코어 2개 + cookingheart 대표 2개 (cc/cdx 각 1) + 오비서/요비서
- **전체 (배포/dcserver 재시작)**: role_map.json 전체 채널

채널 목록은 `~/.remotecc/role_map.json`의 `byChannelId`에서 조회한다.

### 2. 테스트 메시지 전송

각 채널에 `/pcd-send` 방식으로 전송:

```bash
curl -s -X POST http://localhost:8791/api/discord/send-target \
  -H "Content-Type: application/json" \
  -d '{"target": "CHANNEL_ID", "content": "[스모크테스트] ROLE_ID (PROVIDER) 응답 확인 - 한줄로 역할 이름만 답해줘", "source": "smoke_test"}'
```

각 전송 사이 1초 딜레이. 전송 결과(ok/fail)를 기록한다.

### 3. DM 테스트

오부장에게 DM 전송으로 DM 경로도 검증:

```bash
curl -s -X POST http://localhost:8791/api/discord/send-target \
  -H "Content-Type: application/json" \
  -d '{"target": "dm:1479017284805722200", "content": "[스모크테스트] DM 전송 확인", "source": "smoke_test"}'
```

### 4. 응답 확인

전송 완료 후 60초 대기한 뒤:

1. **tmux 세션 확인**: `tmux ls | grep remoteCC-` 로 활성 세션 확인
2. **채널별 응답 판정**:
   - OK: 전송 성공 + 세션 활동 확인
   - TIMEOUT: 전송 성공했으나 응답 없음
   - SEND_FAIL: PCD API 전송 실패
   - SSH_FAIL: 원격 세션(mac-book) 연결 불가

### 5. 결과 보고

```
=== Smoke Test Results ===
범위: 중간 (6채널 + DM)
  OK: 5/6
  TIMEOUT: 1 (ch-td → mac-book offline)
  DM: OK

채널별:
  remotecc-cc (claude) — OK
  remotecc-cdx (codex) — OK
  cookingheart-td-cc (claude) — TIMEOUT (SSH)
  ...
```

## Notes

- 비파괴적 테스트 — 테스트 메시지만 전송
- 원격 세션(mac-book)은 해당 머신 온라인 필요
- PCD 서버(localhost:8791) 실행 필수
