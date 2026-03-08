---
name: pcd-send
description: PCD API를 통해 공지사항봇으로 Discord 채널이나 DM에 메시지를 전송한다. 스모크테스트, 알림, 공지 전달 시 사용.
---

PCD Discord 메시지 전송 유틸리티.

## API

**엔드포인트**: `POST http://localhost:8791/api/discord/send-target`

**파라미터**:
- `target` (string): 채널 ID, 또는 `dm:USER_ID` 형식
- `content` (string): 전송할 메시지
- `source` (string, optional): 출처 식별 (기본값: "pcd")

## 사용법

### 채널에 메시지 전송

```bash
curl -s -X POST http://localhost:8791/api/discord/send-target \
  -H "Content-Type: application/json" \
  -d '{"target": "CHANNEL_ID", "content": "메시지 내용", "source": "remotecc"}'
```

### DM 전송

```bash
curl -s -X POST http://localhost:8791/api/discord/send-target \
  -H "Content-Type: application/json" \
  -d '{"target": "dm:USER_ID", "content": "메시지 내용", "source": "remotecc"}'
```

### 주요 User ID

- 오부장: `1479017284805722200`

## 채널 목록 조회

```bash
cat ~/.remotecc/role_map.json | python3 -c "
import json, sys
d = json.load(sys.stdin)
for cid, v in d.get('byChannelId', {}).items():
    role = v.get('roleId', '?')
    provider = v.get('provider', '?')
    print(f'{cid} | {role} | {provider}')
"
```

## 전제 조건

- PCD 서버가 localhost:8791에서 실행 중이어야 함
- 공지사항봇 토큰은 PCD 설정에 포함되어 있음
