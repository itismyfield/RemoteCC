use std::fs;
use std::sync::Arc;

use rand::Rng;
use serenity::{ChannelId, CreateMessage};
use poise::serenity_prelude as serenity;

use crate::services::claude;

use super::formatting::send_long_message_raw;
use super::settings::{load_role_prompt, RoleBinding};
use super::{rate_limit_wait, SharedData};

// ─── Data Structures ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(super) struct MeetingParticipant {
    pub role_id: String,
    pub prompt_file: String,
    pub display_name: String,
}

#[derive(Clone, Debug)]
pub(super) struct MeetingUtterance {
    pub role_id: String,
    pub display_name: String,
    pub round: u32,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum MeetingStatus {
    SelectingParticipants,
    InProgress,
    Concluding,
    Completed,
    Cancelled,
}

pub(super) struct Meeting {
    pub id: String,
    pub channel_id: ChannelId,
    pub agenda: String,
    pub participants: Vec<MeetingParticipant>,
    pub transcript: Vec<MeetingUtterance>,
    pub current_round: u32,
    pub max_rounds: u32,
    pub status: MeetingStatus,
    /// Final summary produced by the summary agent
    pub summary: Option<String>,
    /// Meeting start timestamp (RFC 3339)
    pub started_at: String,
}

/// Meeting configuration from role_map.json "meeting" section
#[derive(Clone, Debug)]
pub(super) struct MeetingConfig {
    pub channel_name: String,
    pub max_rounds: u32,
    pub summary_agent: String,
    pub available_agents: Vec<MeetingAgentConfig>,
}

#[derive(Clone, Debug)]
pub(super) struct MeetingAgentConfig {
    pub role_id: String,
    pub display_name: String,
    pub keywords: Vec<String>,
    pub prompt_file: String,
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Generate a unique meeting ID (timestamp + random hex)
fn generate_meeting_id() -> String {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let random: u32 = rand::thread_rng().gen();
    format!("mtg-{}-{:08x}", ts, random)
}

// ─── Config Parsing ──────────────────────────────────────────────────────────

/// Load meeting config from role_map.json "meeting" section
pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    let path = dirs::home_dir()?.join(".remotecc").join("role_map.json");
    let content = fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let meeting = json.get("meeting")?;

    let channel_name = meeting.get("channel_name")?.as_str()?.to_string();
    let max_rounds = meeting
        .get("max_rounds")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as u32;
    let summary_agent = meeting.get("summary_agent")?.as_str()?.to_string();

    let agents_arr = meeting.get("available_agents")?.as_array()?;
    let mut available_agents = Vec::new();
    for agent in agents_arr {
        let role_id = agent.get("role_id")?.as_str()?.to_string();
        let display_name = agent.get("display_name")?.as_str()?.to_string();
        let prompt_file = agent
            .get("prompt_file")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let keywords = agent
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        available_agents.push(MeetingAgentConfig {
            role_id,
            display_name,
            keywords,
            prompt_file,
        });
    }

    Some(MeetingConfig {
        channel_name,
        max_rounds,
        summary_agent,
        available_agents,
    })
}

/// Check if a channel name matches the configured meeting channel
#[allow(dead_code)]
pub(super) fn is_meeting_channel(channel_name: &str) -> bool {
    load_meeting_config()
        .map(|cfg| cfg.channel_name == channel_name)
        .unwrap_or(false)
}

// ─── Meeting Lifecycle ───────────────────────────────────────────────────────

/// Start a new meeting: select participants via Claude, then begin rounds.
/// Returns the meeting ID on success.
pub(super) async fn start_meeting(
    http: &serenity::Http,
    channel_id: ChannelId,
    agenda: &str,
    shared: &Arc<SharedData>,
) -> Result<String, Error> {
    let config = load_meeting_config().ok_or("Meeting config not found in role_map.json")?;

    let meeting_id = generate_meeting_id();

    // Register meeting as SelectingParticipants
    {
        let mut core = shared.core.lock().await;
        if core.active_meetings.contains_key(&channel_id) {
            return Err("이 채널에서 이미 회의가 진행 중이야.".into());
        }
        core.active_meetings.insert(
            channel_id,
            Meeting {
                id: meeting_id.clone(),
                channel_id,
                agenda: agenda.to_string(),
                participants: Vec::new(),
                transcript: Vec::new(),
                current_round: 0,
                max_rounds: config.max_rounds,
                status: MeetingStatus::SelectingParticipants,
                summary: None,
                started_at: chrono::Local::now().to_rfc3339(),
            },
        );
    }

    rate_limit_wait(shared, channel_id).await;
    let _ = channel_id
        .send_message(
            http,
            CreateMessage::new().content(format!(
                "📋 **라운드 테이블 회의 시작**\n안건: {}\n참여자 선정 중...",
                agenda
            )),
        )
        .await;

    // Select participants via Claude
    let participants = match select_participants(&config, agenda).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => {
            cleanup_meeting(shared, channel_id).await;
            return Err("참여자를 선정하지 못했어.".into());
        }
        Err(e) => {
            cleanup_meeting(shared, channel_id).await;
            return Err(format!("참여자 선정 실패: {}", e).into());
        }
    };

    // Check if cancelled during participant selection
    {
        let core = shared.core.lock().await;
        if let Some(m) = core.active_meetings.get(&channel_id) {
            if m.status == MeetingStatus::Cancelled {
                drop(core);
                cleanup_meeting(shared, channel_id).await;
                return Err("회의가 취소됐어.".into());
            }
        }
    }

    // Announce participants
    let participant_list: Vec<String> = participants
        .iter()
        .map(|p| format!("• {}", p.display_name))
        .collect();
    rate_limit_wait(shared, channel_id).await;
    let _ = channel_id
        .send_message(
            http,
            CreateMessage::new().content(format!(
                "👥 **참여자 확정** ({}명)\n{}",
                participants.len(),
                participant_list.join("\n")
            )),
        )
        .await;

    // Update meeting state
    {
        let mut core = shared.core.lock().await;
        if let Some(m) = core.active_meetings.get_mut(&channel_id) {
            m.participants = participants;
            m.status = MeetingStatus::InProgress;
        }
    }

    // Run meeting rounds
    let max_rounds = config.max_rounds;
    for round in 1..=max_rounds {
        // Check if cancelled
        {
            let core = shared.core.lock().await;
            if let Some(m) = core.active_meetings.get(&channel_id) {
                if m.status == MeetingStatus::Cancelled {
                    break;
                }
            } else {
                break;
            }
        }

        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .send_message(
                http,
                CreateMessage::new()
                    .content(format!("─── **라운드 {}/{}** ───", round, max_rounds)),
            )
            .await;

        let consensus = run_meeting_round(http, channel_id, round, shared).await?;

        // Update round counter
        {
            let mut core = shared.core.lock().await;
            if let Some(m) = core.active_meetings.get_mut(&channel_id) {
                m.current_round = round;
            }
        }

        if consensus {
            rate_limit_wait(shared, channel_id).await;
            let _ = channel_id
                .send_message(
                    http,
                    CreateMessage::new().content("✅ **합의 도달! 회의를 마무리할게.**"),
                )
                .await;
            break;
        }
    }

    // Conclude meeting
    conclude_meeting(http, channel_id, &config, shared).await?;

    // Save record
    save_meeting_record(shared, channel_id).await?;

    // Clean up
    let mid = {
        let core = shared.core.lock().await;
        core.active_meetings
            .get(&channel_id)
            .map(|m| m.id.clone())
            .unwrap_or_default()
    };
    cleanup_meeting(shared, channel_id).await;

    Ok(mid)
}

/// Cancel a running meeting
pub(super) async fn cancel_meeting(
    http: &serenity::Http,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let had_meeting = {
        let mut core = shared.core.lock().await;
        if let Some(m) = core.active_meetings.get_mut(&channel_id) {
            m.status = MeetingStatus::Cancelled;
            true
        } else {
            false
        }
    };

    if had_meeting {
        // Save whatever transcript we have
        let _ = save_meeting_record(shared, channel_id).await;
        cleanup_meeting(shared, channel_id).await;
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .send_message(
                http,
                CreateMessage::new().content("🛑 **회의가 취소됐어.** 현재까지 트랜스크립트가 저장됐어."),
            )
            .await;
        Ok(())
    } else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .send_message(
                http,
                CreateMessage::new().content("진행 중인 회의가 없어."),
            )
            .await;
        Ok(())
    }
}

/// Get meeting status info
pub(super) async fn meeting_status(
    http: &serenity::Http,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let info = {
        let core = shared.core.lock().await;
        core.active_meetings.get(&channel_id).map(|m| {
            (
                m.agenda.clone(),
                m.current_round,
                m.max_rounds,
                m.participants.len(),
                m.transcript.len(),
                m.status.clone(),
            )
        })
    };

    rate_limit_wait(shared, channel_id).await;
    match info {
        Some((agenda, round, max_rounds, participants, utterances, status)) => {
            let status_str = match status {
                MeetingStatus::SelectingParticipants => "참여자 선정 중",
                MeetingStatus::InProgress => "진행 중",
                MeetingStatus::Concluding => "마무리 중",
                MeetingStatus::Completed => "완료",
                MeetingStatus::Cancelled => "취소됨",
            };
            let _ = channel_id
                .send_message(
                    http,
                    CreateMessage::new().content(format!(
                        "📊 **회의 현황**\n안건: {}\n상태: {}\n라운드: {}/{}\n참여자: {}명\n발언: {}개",
                        agenda, status_str, round, max_rounds, participants, utterances
                    )),
                )
                .await;
        }
        None => {
            let _ = channel_id
                .send_message(
                    http,
                    CreateMessage::new().content("진행 중인 회의가 없어."),
                )
                .await;
        }
    }
    Ok(())
}

// ─── Internal Functions ──────────────────────────────────────────────────────

/// Select participants using Claude `-p --print` (no tools, quick response)
async fn select_participants(
    config: &MeetingConfig,
    agenda: &str,
) -> Result<Vec<MeetingParticipant>, String> {
    let agents_desc: Vec<String> = config
        .available_agents
        .iter()
        .map(|a| {
            format!(
                "- {} ({}): keywords=[{}]",
                a.role_id,
                a.display_name,
                a.keywords.join(", ")
            )
        })
        .collect();

    let prompt = format!(
        r#"다음 안건에 대한 라운드 테이블 회의에 참여할 에이전트를 선정해줘.

안건: {}

사용 가능한 에이전트:
{}

규칙:
- 2~5명 선정
- 안건과 관련된 전문성을 가진 에이전트만 선택
- JSON 배열로만 응답 (다른 텍스트 없이)
- 형식: ["role_id1", "role_id2", ...]"#,
        agenda,
        agents_desc.join("\n")
    );

    let response = claude::execute_command_simple(&prompt)?;

    // Parse JSON array from response
    let trimmed = response.trim();
    // Try to find JSON array in the response
    let json_str = if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            &trimmed[start..=end]
        } else {
            return Err("Invalid JSON response from participant selection".to_string());
        }
    } else {
        return Err("No JSON array in participant selection response".to_string());
    };

    let selected: Vec<String> = serde_json::from_str(json_str)
        .map_err(|e| format!("Failed to parse participant selection: {}", e))?;

    let participants: Vec<MeetingParticipant> = selected
        .iter()
        .filter_map(|role_id| {
            config
                .available_agents
                .iter()
                .find(|a| &a.role_id == role_id)
                .map(|a| MeetingParticipant {
                    role_id: a.role_id.clone(),
                    prompt_file: a.prompt_file.clone(),
                    display_name: a.display_name.clone(),
                })
        })
        .collect();

    Ok(participants)
}

/// Run one round: each participant speaks in order
async fn run_meeting_round(
    http: &serenity::Http,
    channel_id: ChannelId,
    round: u32,
    shared: &Arc<SharedData>,
) -> Result<bool, Error> {
    // Snapshot participants and transcript for this round
    let (participants, agenda) = {
        let core = shared.core.lock().await;
        let m = core
            .active_meetings
            .get(&channel_id)
            .ok_or("Meeting not found")?;
        (m.participants.clone(), m.agenda.clone())
    };

    for participant in &participants {
        // Check cancellation
        {
            let core = shared.core.lock().await;
            if let Some(m) = core.active_meetings.get(&channel_id) {
                if m.status == MeetingStatus::Cancelled {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }

        // Get current transcript for context
        let transcript_text = {
            let core = shared.core.lock().await;
            let m = core
                .active_meetings
                .get(&channel_id)
                .ok_or("Meeting not found")?;
            format_transcript(&m.transcript)
        };

        // Execute agent turn
        match execute_agent_turn(participant, &agenda, round, &transcript_text).await {
            Ok(response) => {
                // Post to Discord
                let discord_msg = format!(
                    "**[{}]** (R{})\n{}",
                    participant.display_name, round, response
                );
                send_long_message_raw(http, channel_id, &discord_msg, shared).await?;

                // Append to transcript
                {
                    let mut core = shared.core.lock().await;
                    if let Some(m) = core.active_meetings.get_mut(&channel_id) {
                        m.transcript.push(MeetingUtterance {
                            role_id: participant.role_id.clone(),
                            display_name: participant.display_name.clone(),
                            round,
                            content: response,
                        });
                    }
                }
            }
            Err(e) => {
                // Skip this agent, post error
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .send_message(
                        http,
                        CreateMessage::new().content(format!(
                            "⚠️ {} 발언 실패: {}",
                            participant.display_name, e
                        )),
                    )
                    .await;
            }
        }
    }

    // Check consensus
    let consensus = {
        let core = shared.core.lock().await;
        let m = core
            .active_meetings
            .get(&channel_id)
            .ok_or("Meeting not found")?;
        check_consensus(&m.transcript, round, m.participants.len())
    };

    Ok(consensus)
}

/// Execute a single agent turn using Claude `-p --print`
async fn execute_agent_turn(
    participant: &MeetingParticipant,
    agenda: &str,
    round: u32,
    transcript: &str,
) -> Result<String, String> {
    // Load role prompt if available
    let role_context = if !participant.prompt_file.is_empty() {
        load_role_prompt(&RoleBinding {
            role_id: participant.role_id.clone(),
            prompt_file: participant.prompt_file.clone(),
        })
        .unwrap_or_default()
    } else {
        String::new()
    };

    let prompt = format!(
        r#"당신은 라운드 테이블 회의에 참여한 {name}입니다.

{role_context}

## 회의 안건
{agenda}

## 현재 라운드: {round}

## 이전 발언 기록
{transcript}

## 지시사항
- 당신의 전문 분야 관점에서 안건에 대해 의견을 제시하세요
- 이전 발언자들의 의견을 참고하고 필요시 반론/보충하세요
- 답변은 300자 이내로 간결하게 작성하세요
- 합의에 도달했다고 판단되면, 반드시 "CONSENSUS:" 로 시작하는 한 줄 요약을 마지막에 추가하세요
- 아직 논의가 더 필요하면 CONSENSUS: 키워드를 사용하지 마세요"#,
        name = participant.display_name,
        role_context = if role_context.is_empty() {
            String::new()
        } else {
            format!("## 역할 컨텍스트\n{}", role_context)
        },
        agenda = agenda,
        round = round,
        transcript = if transcript.is_empty() {
            "(아직 발언 없음)".to_string()
        } else {
            transcript.to_string()
        },
    );

    // Run on blocking thread since execute_command_simple is synchronous
    let response =
        tokio::task::spawn_blocking(move || claude::execute_command_simple(&prompt)).await;

    match response {
        Ok(Ok(text)) => {
            // Truncate if too long (2000 chars max for a single utterance)
            let trimmed = text.trim().to_string();
            if trimmed.chars().count() > 1500 {
                Ok(trimmed.chars().take(1500).collect::<String>() + "...")
            } else {
                Ok(trimmed)
            }
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("Task join error: {}", e)),
    }
}

/// Check if majority of participants in a given round used CONSENSUS: keyword
fn check_consensus(transcript: &[MeetingUtterance], round: u32, participant_count: usize) -> bool {
    if participant_count == 0 {
        return false;
    }
    let consensus_count = transcript
        .iter()
        .filter(|u| u.round == round && u.content.contains("CONSENSUS:"))
        .count();
    // Majority = more than half
    consensus_count * 2 > participant_count
}

/// Conclude meeting: summary agent produces minutes
async fn conclude_meeting(
    http: &serenity::Http,
    channel_id: ChannelId,
    config: &MeetingConfig,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    // Update status
    {
        let mut core = shared.core.lock().await;
        if let Some(m) = core.active_meetings.get_mut(&channel_id) {
            if m.status == MeetingStatus::Cancelled {
                return Ok(());
            }
            m.status = MeetingStatus::Concluding;
        }
    }

    let (agenda, transcript_text, participants_list) = {
        let core = shared.core.lock().await;
        let m = core
            .active_meetings
            .get(&channel_id)
            .ok_or("Meeting not found")?;
        let t = format_transcript(&m.transcript);
        let p: Vec<String> = m.participants.iter().map(|p| p.display_name.clone()).collect();
        (m.agenda.clone(), t, p.join(", "))
    };

    // Find summary agent's prompt file
    let summary_prompt_file = config
        .available_agents
        .iter()
        .find(|a| a.role_id == config.summary_agent)
        .map(|a| a.prompt_file.clone())
        .unwrap_or_default();

    let summary_role_context = if !summary_prompt_file.is_empty() {
        load_role_prompt(&RoleBinding {
            role_id: config.summary_agent.clone(),
            prompt_file: summary_prompt_file,
        })
        .unwrap_or_default()
    } else {
        String::new()
    };

    let prompt = format!(
        r#"당신은 회의록을 작성하는 {agent}입니다.

{role_context}

다음 라운드 테이블 회의의 회의록을 작성해주세요.

## 안건
{agenda}

## 참여자
{participants}

## 전체 발언 기록
{transcript}

## 회의록 형식
다음 형식으로 작성하세요:

### 📋 회의록: [안건 요약]
**참여자**: [이름 목록]

#### 주요 논의
- [핵심 논의 사항 1]
- [핵심 논의 사항 2]

#### 결론
[합의 사항 또는 미합의 시 각 입장 정리]

#### Action Items
- [ ] [담당자] — [할 일]"#,
        agent = config.summary_agent,
        role_context = if summary_role_context.is_empty() {
            String::new()
        } else {
            format!("## 역할 컨텍스트\n{}", summary_role_context)
        },
        agenda = agenda,
        participants = participants_list,
        transcript = transcript_text,
    );

    rate_limit_wait(shared, channel_id).await;
    let _ = channel_id
        .send_message(
            http,
            CreateMessage::new().content("📝 **회의록 작성 중...**"),
        )
        .await;

    let summary =
        tokio::task::spawn_blocking(move || claude::execute_command_simple(&prompt)).await;

    let summary_text = match summary {
        Ok(Ok(text)) => {
            let trimmed = text.trim().to_string();
            send_long_message_raw(http, channel_id, &trimmed, shared).await?;
            Some(trimmed)
        }
        Ok(Err(e)) => {
            rate_limit_wait(shared, channel_id).await;
            let _ = channel_id
                .send_message(
                    http,
                    CreateMessage::new()
                        .content(format!("⚠️ 회의록 작성 실패: {}", e)),
                )
                .await;
            None
        }
        Err(e) => {
            rate_limit_wait(shared, channel_id).await;
            let _ = channel_id
                .send_message(
                    http,
                    CreateMessage::new()
                        .content(format!("⚠️ 회의록 작성 실패: {}", e)),
                )
                .await;
            None
        }
    };

    // Mark completed and save summary
    {
        let mut core = shared.core.lock().await;
        if let Some(m) = core.active_meetings.get_mut(&channel_id) {
            m.summary = summary_text;
            m.status = MeetingStatus::Completed;
        }
    }

    Ok(())
}

/// Save meeting record as Obsidian Markdown to CookingHeart/meetings/
async fn save_meeting_record(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> Result<(), Error> {
    let (md, meeting_id, pcd_payload) = {
        let core = shared.core.lock().await;
        let m = core
            .active_meetings
            .get(&channel_id)
            .ok_or("Meeting not found")?;

        let payload = build_pcd_payload(m);
        (build_meeting_markdown(m), m.id.clone(), payload)
    };

    let meetings_dir = dirs::home_dir()
        .ok_or("Home dir not found")?
        .join("ObsidianVault")
        .join("CookingHeart")
        .join("meetings");
    fs::create_dir_all(&meetings_dir)?;

    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = meetings_dir.join(format!("{}_{}.md", date_str, meeting_id));
    fs::write(&path, md)?;

    // POST meeting data to PCD (fire-and-forget, ignore errors)
    if let Some(payload) = pcd_payload {
        tokio::spawn(async move {
            let _ = post_meeting_to_pcd(payload).await;
        });
    }

    Ok(())
}

/// Build PCD API payload from meeting
fn build_pcd_payload(m: &Meeting) -> Option<serde_json::Value> {
    let status_str = match &m.status {
        MeetingStatus::Completed => "completed",
        MeetingStatus::Cancelled => "cancelled",
        _ => "in_progress",
    };

    let participant_names: Vec<&str> = m.participants.iter().map(|p| p.display_name.as_str()).collect();

    let entries: Vec<serde_json::Value> = m
        .transcript
        .iter()
        .enumerate()
        .map(|(i, u)| {
            serde_json::json!({
                "seq": i + 1,
                "round": u.round,
                "speaker_role_id": u.role_id,
                "speaker_name": u.display_name,
                "content": u.content,
                "is_summary": false,
            })
        })
        .collect();

    let started_at = chrono::DateTime::parse_from_rfc3339(&m.started_at)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|_| chrono::Local::now().timestamp_millis());

    Some(serde_json::json!({
        "id": m.id,
        "agenda": m.agenda,
        "summary": m.summary,
        "status": status_str,
        "participant_names": participant_names,
        "total_rounds": m.current_round,
        "started_at": started_at,
        "completed_at": if m.status == MeetingStatus::Completed { serde_json::Value::from(chrono::Local::now().timestamp_millis()) } else { serde_json::Value::Null },
        "entries": entries,
    }))
}

/// POST meeting data to PCD server
async fn post_meeting_to_pcd(payload: serde_json::Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let _ = client
        .post("http://localhost:8791/api/round-table-meetings")
        .json(&payload)
        .send()
        .await?;
    Ok(())
}

/// Build Obsidian Markdown content for a meeting
fn build_meeting_markdown(m: &Meeting) -> String {
    let now = chrono::Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let datetime_str = now.format("%Y-%m-%d %H:%M").to_string();

    let status_str = match &m.status {
        MeetingStatus::SelectingParticipants | MeetingStatus::InProgress => "진행중",
        MeetingStatus::Concluding => "마무리중",
        MeetingStatus::Completed => "완료",
        MeetingStatus::Cancelled => "취소",
    };

    let participants_inline = m
        .participants
        .iter()
        .map(|p| p.display_name.clone())
        .collect::<Vec<_>>()
        .join(", ");

    // Build transcript grouped by rounds
    let max_round = m.transcript.iter().map(|u| u.round).max().unwrap_or(0);
    let mut transcript_sections = Vec::new();
    for round in 1..=max_round {
        let mut section = format!("### 라운드 {}\n", round);
        for u in m.transcript.iter().filter(|u| u.round == round) {
            section.push_str(&format!("\n**{}**\n\n{}\n", u.display_name, u.content));
        }
        transcript_sections.push(section);
    }

    let summary_section = m
        .summary
        .clone()
        .unwrap_or_else(|| "_회의록이 작성되지 않았습니다._".to_string());

    format!(
        "---\ntags: [meeting, cookingheart]\ndate: {date}\nstatus: {status}\nparticipants: [{participants}]\nagenda: \"{agenda}\"\nmeeting_id: {id}\n---\n\n# 회의록: {agenda}\n\n> **날짜**: {datetime}\n> **참여자**: {participants}\n> **라운드**: {rounds}/{max_rounds}\n> **상태**: {status}\n\n---\n\n## 요약\n\n{summary}\n\n---\n\n## 전체 발언 기록\n\n{transcript}\n",
        date = date_str,
        status = status_str,
        participants = participants_inline,
        agenda = m.agenda,
        id = m.id,
        datetime = datetime_str,
        rounds = m.current_round,
        max_rounds = m.max_rounds,
        summary = summary_section,
        transcript = transcript_sections.join("\n"),
    )
}

/// Format transcript for inclusion in prompts
fn format_transcript(transcript: &[MeetingUtterance]) -> String {
    if transcript.is_empty() {
        return String::new();
    }
    transcript
        .iter()
        .map(|u| format!("[R{} - {}]: {}", u.round, u.display_name, u.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Remove meeting from active_meetings
async fn cleanup_meeting(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let mut core = shared.core.lock().await;
    core.active_meetings.remove(&channel_id);
}

// ─── Command Handler ─────────────────────────────────────────────────────────

/// Handle meeting commands from Discord messages.
/// Returns true if the message was a meeting command (consumed), false otherwise.
pub(super) async fn handle_meeting_command(
    http: Arc<serenity::Http>,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<bool, Error> {
    let text = text.trim().to_string();

    // /meeting start <agenda>
    if let Some(agenda) = text.strip_prefix("/meeting start ") {
        let agenda = agenda.trim().to_string();
        if agenda.is_empty() {
            rate_limit_wait(shared, channel_id).await;
            let _ = channel_id
                .send_message(
                    &*http,
                    CreateMessage::new().content("사용법: `/meeting start <안건>`"),
                )
                .await;
            return Ok(true);
        }

        let http_clone = http.clone();
        let shared_clone = shared.clone();

        // Spawn meeting as a background task so it doesn't block message handling
        tokio::spawn(async move {
            match start_meeting(&*http_clone, channel_id, &agenda, &shared_clone).await {
                Ok(id) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ✅ Meeting completed: {id}");
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ❌ Meeting error: {e}");
                    rate_limit_wait(&shared_clone, channel_id).await;
                    let _ = channel_id
                        .send_message(
                            &*http_clone,
                            CreateMessage::new().content(format!("❌ 회의 오류: {}", e)),
                        )
                        .await;
                }
            }
        });

        return Ok(true);
    }

    // /meeting stop
    if text == "/meeting stop" {
        cancel_meeting(&*http, channel_id, shared).await?;
        return Ok(true);
    }

    // /meeting status
    if text == "/meeting status" {
        meeting_status(&*http, channel_id, shared).await?;
        return Ok(true);
    }

    Ok(false)
}
