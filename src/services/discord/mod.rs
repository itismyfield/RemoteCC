mod formatting;
mod inflight;
mod meeting;
mod pcd;
mod prompt_builder;
mod recovery;
pub(crate) mod restart_report;
mod role_map;
mod router;
mod runtime_store;
mod settings;
mod shared_memory;
mod tmux;
mod turn_bridge;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, EditMessage, MessageId, UserId};

use crate::services::claude::{
    self, CancelToken, ReadOutputResult, StreamMessage, DEFAULT_ALLOWED_TOOLS,
};
use crate::services::codex;
use crate::services::provider::ProviderKind;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

use formatting::{
    add_reaction_raw, canonical_tool_name, extract_skill_description, format_for_discord,
    format_tool_input, normalize_empty_lines, remove_reaction_raw, risk_badge,
    send_long_message_ctx, send_long_message_raw, tool_info, truncate_str, BUILTIN_SKILLS,
};
use inflight::{
    clear_inflight_state, load_inflight_states, save_inflight_state, InflightTurnState,
};
use pcd::{build_pcd_session_key, derive_pcd_session_info, post_pcd_session_status};
use prompt_builder::build_system_prompt;
use recovery::restore_inflight_turns;
use restart_report::flush_restart_reports;
use router::{handle_event, handle_text_message};
use runtime_store::{workspace_root, worktrees_root};
use settings::{
    channel_supports_provider, channel_upload_dir, cleanup_channel_uploads, cleanup_old_uploads,
    load_bot_settings, resolve_role_binding, save_bot_settings, RoleBinding,
};
use shared_memory::{append_shared_memory_turn, build_shared_memory_context};
use tmux::{cleanup_orphan_tmux_sessions, restore_tmux_watchers, tmux_output_watcher};
use turn_bridge::{cancel_active_token, spawn_turn_bridge, tmux_runtime_paths, TurnBridgeContext};

pub use settings::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;
const MAX_INTERVENTIONS_PER_CHANNEL: usize = 3;
const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
const SESSION_MAX_IDLE: Duration = Duration::from_secs(24 * 60 * 60); // 1 day
const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Per-channel session state
pub(super) struct DiscordSession {
    pub(super) session_id: Option<String>,
    pub(super) current_path: Option<String>,
    pub(super) history: Vec<HistoryItem>,
    pub(super) pending_uploads: Vec<String>,
    pub(super) cleared: bool,
    /// Remote profile name for SSH execution (None = local)
    pub(super) remote_profile_name: Option<String>,
    pub(super) channel_id: Option<u64>,
    pub(super) channel_name: Option<String>,
    pub(super) category_name: Option<String>,
    /// Last time this session was actively used (for TTL cleanup)
    pub(super) last_active: tokio::time::Instant,
    /// If this session runs in a git worktree, store the info here
    pub(super) worktree: Option<WorktreeInfo>,
}

/// Worktree info for sessions that were auto-redirected to avoid conflicts
#[derive(Clone, Debug)]
pub(super) struct WorktreeInfo {
    /// The original repo path that was conflicted
    pub original_path: String,
    /// The worktree directory path
    pub worktree_path: String,
    /// The branch name created for this worktree
    pub branch_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(super) struct Intervention {
    author_id: UserId,
    message_id: MessageId,
    text: String,
    mode: InterventionMode,
    created_at: Instant,
}

/// Bot-level settings persisted to disk
#[derive(Clone)]
pub(super) struct DiscordBotSettings {
    pub(super) provider: ProviderKind,
    pub(super) allowed_tools: Vec<String>,
    /// channel_id (string) → last working directory path
    pub(super) last_sessions: std::collections::HashMap<String, String>,
    /// channel_id (string) → last remote profile name
    pub(super) last_remotes: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (imprinting auth)
    pub(super) owner_user_id: Option<u64>,
    /// Additional authorized user IDs (added by owner via /adduser)
    pub(super) allowed_user_ids: Vec<u64>,
    /// Bot IDs whose messages are NOT ignored (e.g. announce bot for CEO directives)
    pub(super) allowed_bot_ids: Vec<u64>,
}

impl Default for DiscordBotSettings {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Claude,
            allowed_tools: DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            last_sessions: std::collections::HashMap::new(),
            last_remotes: std::collections::HashMap::new(),
            owner_user_id: None,
            allowed_user_ids: Vec::new(),
            allowed_bot_ids: Vec::new(),
        }
    }
}

/// Shared state for the Discord bot (multi-channel: each channel has its own session)
/// Handle for a background tmux output watcher
pub(super) struct TmuxWatcherHandle {
    /// Signal to pause monitoring (while Discord handler reads its own turn)
    pub(super) paused: Arc<std::sync::atomic::AtomicBool>,
    /// After Discord handler finishes its turn, set this offset so watcher resumes from here
    pub(super) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    /// Signal to cancel the watcher (quiet exit, no "session ended" message)
    pub(super) cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// Core state that requires atomic multi-field access (always locked together)
pub(super) struct CoreState {
    /// Per-channel sessions (each Discord channel can have its own Claude Code session)
    pub(super) sessions: HashMap<ChannelId, DiscordSession>,
    /// Per-channel cancel tokens for in-progress AI requests
    pub(super) cancel_tokens: HashMap<ChannelId, Arc<CancelToken>>,
    /// Per-channel owner of the currently running request
    pub(super) active_request_owner: HashMap<ChannelId, UserId>,
    /// Per-channel message queue: messages arriving during an active turn are queued here
    /// and executed as subsequent turns after the current one finishes.
    intervention_queue: HashMap<ChannelId, Vec<Intervention>>,
    /// Per-channel active meeting (one meeting per channel)
    active_meetings: HashMap<ChannelId, meeting::Meeting>,
}

/// Shared state for the Discord bot — split into independently-lockable groups
pub(super) struct SharedData {
    /// Core state (sessions + request lifecycle) — requires atomic access
    pub(super) core: Mutex<CoreState>,
    /// Bot settings — mostly reads, rare writes
    pub(super) settings: tokio::sync::RwLock<DiscordBotSettings>,
    /// Per-channel timestamps of the last Discord API call (for rate limiting)
    pub(super) api_timestamps: dashmap::DashMap<ChannelId, tokio::time::Instant>,
    /// Cached skill list: (name, description)
    pub(super) skills_cache: tokio::sync::RwLock<Vec<(String, String)>>,
    /// Per-channel tmux output watchers for terminal→Discord relay
    pub(super) tmux_watchers: dashmap::DashMap<ChannelId, TmuxWatcherHandle>,
    /// Per-channel in-flight turn recovery marker (restart resume in progress)
    pub(super) recovering_channels: dashmap::DashMap<ChannelId, ()>,
    /// Global shutdown flag — when set, watchers exit quietly via cancel path
    pub(super) shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

/// Poise user data type
struct Data {
    shared: Arc<SharedData>,
    token: String,
    provider: ProviderKind,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;


fn prune_interventions(queue: &mut Vec<Intervention>) {
    let now = Instant::now();
    queue.retain(|i| now.duration_since(i.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
}

fn enqueue_intervention(queue: &mut Vec<Intervention>, intervention: Intervention) -> bool {
    prune_interventions(queue);

    if let Some(last) = queue.last() {
        if last.author_id == intervention.author_id
            && last.text == intervention.text
            && intervention.created_at.duration_since(last.created_at) <= INTERVENTION_DEDUP_WINDOW
        {
            return false;
        }
    }

    queue.push(intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
    true
}

pub(super) fn has_soft_intervention(queue: &mut Vec<Intervention>) -> bool {
    prune_interventions(queue);
    queue.iter().any(|item| item.mode == InterventionMode::Soft)
}

pub(super) fn dequeue_next_soft_intervention(
    queue: &mut Vec<Intervention>,
) -> Option<Intervention> {
    prune_interventions(queue);
    let index = queue
        .iter()
        .position(|item| item.mode == InterventionMode::Soft)?;
    Some(queue.remove(index))
}

pub(super) fn requeue_intervention_front(
    queue: &mut Vec<Intervention>,
    intervention: Intervention,
) {
    prune_interventions(queue);
    queue.insert(0, intervention);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        queue.truncate(MAX_INTERVENTIONS_PER_CHANNEL);
    }
}

// ─── Pending queue persistence (SIGTERM → restore) ──────────────────────────

/// Serializable form of a queued intervention for disk persistence.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingQueueItem {
    author_id: u64,
    message_id: u64,
    text: String,
}

/// Save all non-empty intervention queues to `discord_pending_queue/{provider}/`.
fn save_pending_queues(provider: ProviderKind, queues: &HashMap<ChannelId, Vec<Intervention>>) {
    let Some(root) = runtime_store::discord_pending_queue_root() else {
        return;
    };
    let dir = root.join(provider.as_str());
    let _ = fs::create_dir_all(&dir);
    // Clean stale files first
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let _ = fs::remove_file(entry.path());
        }
    }
    for (channel_id, queue) in queues {
        if queue.is_empty() {
            continue;
        }
        let items: Vec<PendingQueueItem> = queue
            .iter()
            .map(|i| PendingQueueItem {
                author_id: i.author_id.get(),
                message_id: i.message_id.get(),
                text: i.text.clone(),
            })
            .collect();
        if let Ok(json) = serde_json::to_string_pretty(&items) {
            let path = dir.join(format!("{}.json", channel_id.get()));
            let _ = runtime_store::atomic_write(&path, &json);
        }
    }
}

/// Load persisted pending queues and delete the files.
fn load_pending_queues(provider: ProviderKind) -> HashMap<ChannelId, Vec<Intervention>> {
    let Some(root) = runtime_store::discord_pending_queue_root() else {
        return HashMap::new();
    };
    let dir = root.join(provider.as_str());
    let Ok(entries) = fs::read_dir(&dir) else {
        return HashMap::new();
    };
    let now = Instant::now();
    let mut result: HashMap<ChannelId, Vec<Intervention>> = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let channel_id: u64 = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(id) => id,
            None => continue,
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(items) = serde_json::from_str::<Vec<PendingQueueItem>>(&content) else {
            let _ = fs::remove_file(&path);
            continue;
        };
        let interventions: Vec<Intervention> = items
            .into_iter()
            .map(|item| Intervention {
                author_id: UserId::new(item.author_id),
                message_id: MessageId::new(item.message_id),
                text: item.text,
                mode: InterventionMode::Soft,
                created_at: now,
            })
            .collect();
        if !interventions.is_empty() {
            result.insert(ChannelId::new(channel_id), interventions);
        }
        let _ = fs::remove_file(&path);
    }
    result
}

/// Scan for provider-specific skills available to this bot.
fn scan_skills(provider: ProviderKind, project_path: Option<&str>) -> Vec<(String, String)> {
    let mut skills: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    match provider {
        ProviderKind::Claude => {
            for (name, desc) in BUILTIN_SKILLS {
                seen.insert(name.to_string());
                skills.push((name.to_string(), desc.to_string()));
            }

            let mut dirs_to_scan: Vec<std::path::PathBuf> = Vec::new();
            if let Some(home) = dirs::home_dir() {
                dirs_to_scan.push(home.join(".claude").join("commands"));
            }
            if let Some(proj) = project_path {
                dirs_to_scan.push(Path::new(proj).join(".claude").join("commands"));
            }

            for dir in dirs_to_scan {
                if !dir.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            let name = stem.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                    }
                }
            }
        }
        ProviderKind::Codex => {
            let mut roots = Vec::new();
            if let Some(home) = dirs::home_dir() {
                roots.push(home.join(".codex").join("skills"));
            }
            if let Some(proj) = project_path {
                roots.push(Path::new(proj).join(".codex").join("skills"));
            }

            for root in roots {
                if !root.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&root) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if let Some(skill_path) = resolve_codex_skill_file(&path) {
                        if let Some(name) = skill_path
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                        {
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                        continue;
                    }

                    if path.is_dir() {
                        let Ok(nested) = fs::read_dir(&path) else {
                            continue;
                        };
                        for child in nested.filter_map(|e| e.ok()) {
                            let child_path = child.path();
                            let Some(skill_path) = resolve_codex_skill_file(&child_path) else {
                                continue;
                            };
                            let Some(name) = skill_path
                                .parent()
                                .and_then(|p| p.file_name())
                                .and_then(|s| s.to_str())
                            else {
                                continue;
                            };
                            let name = name.to_string();
                            if seen.insert(name.clone()) {
                                let desc = fs::read_to_string(&skill_path)
                                    .ok()
                                    .map(|content| extract_skill_description(&content))
                                    .unwrap_or_else(|| format!("Skill: {}", name));
                                skills.push((name, desc));
                            }
                        }
                    }
                }
            }
        }
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
}

fn resolve_codex_skill_file(path: &Path) -> Option<std::path::PathBuf> {
    if path.is_dir() {
        let skill_path = path.join("SKILL.md");
        if skill_path.is_file() {
            return Some(skill_path);
        }
    }
    None
}

/// Entry point: start the Discord bot
pub async fn run_bot(token: &str, provider: ProviderKind) {
    // Initialize debug logging from environment variable
    claude::init_debug_from_env();

    let mut bot_settings = load_bot_settings(token);
    bot_settings.provider = provider;

    match bot_settings.owner_user_id {
        Some(owner_id) => println!("  ✓ Owner: {owner_id}"),
        None => println!("  ⚠ No owner registered — first user will be registered as owner"),
    }

    let initial_skills = scan_skills(provider, None);
    let skill_count = initial_skills.len();
    println!(
        "  ✓ {} bot ready — Skills loaded: {}",
        provider.display_name(),
        skill_count
    );

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let shared = Arc::new(SharedData {
        core: Mutex::new(CoreState {
            sessions: HashMap::new(),
            cancel_tokens: HashMap::new(),
            active_request_owner: HashMap::new(),
            intervention_queue: HashMap::new(),
            active_meetings: HashMap::new(),
        }),
        settings: tokio::sync::RwLock::new(bot_settings),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(initial_skills),
        tmux_watchers: dashmap::DashMap::new(),
        recovering_channels: dashmap::DashMap::new(),
        shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });

    let token_owned = token.to_string();
    let shared_clone = shared.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                cmd_start(),
                cmd_pwd(),
                cmd_status(),
                cmd_inflight(),
                cmd_clear(),
                cmd_stop(),
                cmd_down(),
                cmd_shell(),
                cmd_cc(),
                cmd_health(),
                cmd_allowedtools(),
                cmd_allowed(),
                cmd_debug(),
                cmd_adduser(),
                cmd_removeuser(),
                cmd_help(),
                cmd_meeting(),
            ],
            event_handler: |ctx, event, _framework, data| Box::pin(handle_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let ctx_clone = ctx.clone();
            let shared_for_migrate = shared_clone.clone();
            Box::pin(async move {
                // Register in each guild for instant slash command propagation
                // (register_globally can take up to 1 hour)
                let commands = &framework.options().commands;
                for guild in &_ready.guilds {
                    if let Err(e) =
                        poise::builtins::register_in_guild(ctx, commands, guild.id).await
                    {
                        eprintln!(
                            "  ⚠ Failed to register commands in guild {}: {}",
                            guild.id, e
                        );
                    }
                }
                println!(
                    "  ✓ Bot connected — Registered commands in {} guild(s)",
                    _ready.guilds.len()
                );

                // Background: resolve category names for all known channels
                let shared_for_tmux = shared_for_migrate.clone();
                tokio::spawn(async move {
                    migrate_session_categories(&ctx_clone, &shared_for_migrate).await;
                });

                // Background: flush delayed restart follow-up reports until they are delivered
                let http_for_restart_reports = ctx.http.clone();
                let shared_for_restart_reports = shared_for_tmux.clone();
                flush_restart_reports(
                    &http_for_restart_reports,
                    &shared_for_restart_reports,
                    provider,
                )
                .await;
                tokio::spawn(async move {
                    loop {
                        flush_restart_reports(
                            &http_for_restart_reports,
                            &shared_for_restart_reports,
                            provider,
                        )
                        .await;
                        tokio::time::sleep(RESTART_REPORT_FLUSH_INTERVAL).await;
                    }
                });

                // Background: restore tmux watchers for surviving tmux sessions, then clean orphans
                let http_for_tmux = ctx.http.clone();
                let shared_for_tmux2 = shared_for_tmux.clone();
                tokio::spawn(async move {
                    restore_inflight_turns(&http_for_tmux, &shared_for_tmux2, provider).await;

                    // Restore pending intervention queues saved during previous SIGTERM
                    let restored_queues = load_pending_queues(provider);
                    if !restored_queues.is_empty() {
                        let total: usize = restored_queues.values().map(|q| q.len()).sum();
                        let mut data = shared_for_tmux2.core.lock().await;
                        for (channel_id, items) in restored_queues {
                            let queue = data.intervention_queue.entry(channel_id).or_default();
                            queue.extend(items);
                        }
                        drop(data);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 📋 Restored {total} pending queue item(s) from disk");
                    }

                    restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                    cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;
                });

                // Background: periodic cleanup for stale Discord upload files
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(UPLOAD_CLEANUP_INTERVAL).await;
                        cleanup_old_uploads(UPLOAD_MAX_AGE);
                    }
                });

                Ok(Data {
                    shared: shared_clone,
                    token: token_owned,
                    provider,
                })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    // Graceful shutdown: on SIGTERM, cancel all tmux watchers before dying
    let shared_for_signal = shared.clone();
    let token_for_signal = token.to_string();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 🛑 SIGTERM received — graceful shutdown");

                // Set global shutdown flag
                shared_for_signal
                    .shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Cancel all active tmux watchers (quiet exit, no "session ended" messages)
                for entry in shared_for_signal.tmux_watchers.iter() {
                    entry.value().cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                }

                // Grace period for watchers to see cancel flag and exit cleanly.
                // Active turns may also finish during this window.
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                // Create restart reports AFTER grace period so that turns that
                // completed (and cleared their inflight state) during the window
                // are not given a spurious recovery follow-up.
                let inflight_states = inflight::load_inflight_states(provider);

                // Update in-flight placeholder messages to indicate restart
                if !inflight_states.is_empty() {
                    let http = serenity::Http::new(&token_for_signal);
                    for state in &inflight_states {
                        let channel = ChannelId::new(state.channel_id);
                        let msg_id = MessageId::new(state.current_msg_id);
                        let restart_notice = if state.full_response.trim().is_empty() {
                            "⚠️ dcserver 재시작으로 중단됨 — 곧 복원됩니다".to_string()
                        } else {
                            let partial = formatting::format_for_discord(state.full_response.trim());
                            format!("{partial}\n\n⚠️ dcserver 재시작으로 중단됨 — 곧 복원됩니다")
                        };
                        match channel
                            .edit_message(&http, msg_id, EditMessage::new().content(&restart_notice))
                            .await
                        {
                            Ok(_) => {
                                let ts_ok = chrono::Local::now().format("%H:%M:%S");
                                println!(
                                    "  [{ts_ok}] ✓ Updated placeholder msg {} in channel {} with restart notice",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                            Err(e) => {
                                eprintln!(
                                    "  ⚠ Failed to update placeholder msg {} in channel {}: {e}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                        }
                    }
                }

                for state in &inflight_states {
                    // Skip only if a "pending" report exists (from --restart-dcserver).
                    // Overwrite any other stale/failed report so the latest SIGTERM wins.
                    let existing = restart_report::load_restart_report(provider, state.channel_id);
                    if existing.as_ref().map(|r| r.status.as_str()) == Some("pending") {
                        continue;
                    }
                    let report = restart_report::RestartCompletionReport::new(
                        provider,
                        state.channel_id,
                        "sigterm",
                        "dcserver가 SIGTERM으로 종료되었습니다. 재시작 후 작업을 이어받습니다.",
                    );
                    if let Err(e) = restart_report::save_restart_report(&report) {
                        eprintln!("  ⚠ failed to save restart report for channel {}: {e}", state.channel_id);
                    }
                }
                if !inflight_states.is_empty() {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts2}] 📝 saved {} restart report(s) for inflight channels", inflight_states.len());
                }

                // Persist pending intervention queues so they survive restart
                {
                    let data = shared_for_signal.core.lock().await;
                    let queue_count: usize = data.intervention_queue.values().map(|q| q.len()).sum();
                    if queue_count > 0 {
                        save_pending_queues(provider, &data.intervention_queue);
                        let ts3 = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts3}] 📋 saved {queue_count} pending queue item(s) to disk");
                    }
                }

                std::process::exit(0);
            }
        }
    });

    if let Err(e) = client.start().await {
        eprintln!("  ✗ {} bot error: {e}", provider.display_name());
    }
}

/// Check if a user is authorized (owner or allowed user)
/// Returns true if authorized, false if rejected.
/// On first use, registers the user as owner.
async fn check_auth(
    user_id: UserId,
    user_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
) -> bool {
    let mut settings = shared.settings.write().await;
    match settings.owner_user_id {
        None => {
            // Imprint: register first user as owner
            settings.owner_user_id = Some(user_id.get());
            save_bot_settings(token, &settings);
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ★ Owner registered: {user_name} (id:{})",
                user_id.get()
            );
            true
        }
        Some(owner_id) => {
            let uid = user_id.get();
            if uid == owner_id || settings.allowed_user_ids.contains(&uid) {
                true
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✗ Rejected: {user_name} (id:{})", uid);
                false
            }
        }
    }
}

/// Check if a user is the owner (not just allowed)
async fn check_owner(user_id: UserId, shared: &Arc<SharedData>) -> bool {
    let settings = shared.settings.read().await;
    settings.owner_user_id == Some(user_id.get())
}

fn family_profile_probe_script_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("ObsidianVault")
            .join("RemoteVault")
            .join("99_Skills")
            .join("family-profile-probe")
            .join("scripts")
            .join("select_profile_probe.py")
    })
}

fn family_profile_probe_state_paths() -> Vec<std::path::PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".local")
            .join("state")
            .join("family-profile-probe")
            .join("profile_probe_state.json"),
        home.join(".openclaw")
            .join("workspace")
            .join("state")
            .join("profile_probe_state.json"),
    ]
}

fn profile_probe_target_user_id(target: &str) -> Option<u64> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    for prefix in ["user:", "dm:"] {
        if let Some(raw) = trimmed.strip_prefix(prefix) {
            return raw.trim().parse::<u64>().ok();
        }
    }

    trimmed.parse::<u64>().ok()
}

fn pending_family_profile_probe_for_user(user_id: u64) -> Option<(String, String)> {
    for path in family_profile_probe_state_paths() {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(pending) = json.get("pending").and_then(|v| v.as_object()) else {
            continue;
        };

        for (target, entry) in pending {
            if profile_probe_target_user_id(target) != Some(user_id) {
                continue;
            }
            let Some(topic_key) = entry.get("topicKey").and_then(|v| v.as_str()) else {
                continue;
            };
            return Some((topic_key.to_string(), target.to_string()));
        }
    }

    None
}

fn record_family_profile_probe_answer(
    topic_key: &str,
    target: &str,
    answer: &str,
) -> Result<bool, String> {
    let Some(script_path) = family_profile_probe_script_path() else {
        return Err("family_profile_probe_script_missing".to_string());
    };
    if !script_path.exists() {
        return Err(format!(
            "family_profile_probe_script_not_found:{}",
            script_path.display()
        ));
    }

    let output = Command::new("/usr/bin/python3")
        .arg(script_path)
        .arg("--record-answer")
        .arg("--topic-key")
        .arg(topic_key)
        .arg("--target")
        .arg(target)
        .arg("--answer")
        .arg(answer)
        .output()
        .map_err(|err| err.to_string())?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }

    let payload = serde_json::from_str::<serde_json::Value>(&stdout)
        .map_err(|err| format!("record_answer_parse_failed:{err}: {stdout}"))?;
    Ok(payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
}

async fn try_handle_family_profile_probe_reply(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
    provider: ProviderKind,
) -> Result<bool, Error> {
    if provider != ProviderKind::Claude || msg.author.bot || msg.guild_id.is_some() {
        return Ok(false);
    }

    let answer = msg.content.trim();
    if answer.is_empty() {
        return Ok(false);
    }

    let Some((topic_key, target)) = pending_family_profile_probe_for_user(msg.author.id.get())
    else {
        return Ok(false);
    };

    let topic_key_owned = topic_key.clone();
    let target_owned = target.clone();
    let answer_owned = answer.to_string();
    let recorded = tokio::task::spawn_blocking(move || {
        record_family_profile_probe_answer(&topic_key_owned, &target_owned, &answer_owned)
    })
    .await
    .map_err(|err| format!("profile_probe_join_failed:{err}"))?;

    let response = match recorded {
        Ok(true) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ Recorded family profile probe answer: user={} topic={}",
                msg.author.id.get(),
                topic_key
            );
            "답변 고마워요. 프로필에 반영해둘게요."
        }
        Ok(false) => {
            eprintln!(
                "  [profile-probe] record_answer returned false for user={} topic={}",
                msg.author.id.get(),
                topic_key
            );
            "답변은 받았는데 저장 대상에 바로 반영하지 못했어요. 제가 다시 확인할게요."
        }
        Err(err) => {
            eprintln!(
                "  [profile-probe] failed to record answer for user={} topic={} error={}",
                msg.author.id.get(),
                topic_key,
                err
            );
            "답변은 받았는데 저장 중 오류가 있었어요. 다시 확인해서 반영할게요."
        }
    };

    rate_limit_wait(shared, msg.channel_id).await;
    let _ = msg.channel_id.say(&ctx.http, response).await;
    Ok(true)
}

/// Rate limit helper — ensures minimum 1s gap between API calls per channel
pub(super) async fn rate_limit_wait(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let min_gap = tokio::time::Duration::from_millis(1000);
    let sleep_until = {
        let now = tokio::time::Instant::now();
        let default_ts = now - tokio::time::Duration::from_secs(10);
        let last_ts = shared
            .api_timestamps
            .get(&channel_id)
            .map(|r| *r.value())
            .unwrap_or(default_ts);
        let earliest_next = last_ts + min_gap;
        let target = if earliest_next > now {
            earliest_next
        } else {
            now
        };
        shared.api_timestamps.insert(channel_id, target);
        target
    };
    tokio::time::sleep_until(sleep_until).await;
}

/// Add a reaction to a message
async fn add_reaction(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    let _ = channel_id
        .create_reaction(&ctx.http, message_id, reaction)
        .await;
}

// ─── Event handler ───────────────────────────────────────────────────────────

/// Periodically clean up idle sessions and their associated data.
/// Called from handle_event; uses a static Mutex to track the last cleanup time.
async fn maybe_cleanup_sessions(shared: &Arc<SharedData>) {
    use std::sync::OnceLock;
    static LAST_CLEANUP: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    let last = LAST_CLEANUP.get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now()));
    let mut last_guard = last.lock().await;
    if last_guard.elapsed() < SESSION_CLEANUP_INTERVAL {
        return;
    }
    *last_guard = tokio::time::Instant::now();
    drop(last_guard);

    let expired: Vec<ChannelId> = {
        let data = shared.core.lock().await;
        let now = tokio::time::Instant::now();
        data.sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_active) > SESSION_MAX_IDLE)
            .map(|(ch, _)| *ch)
            .collect()
    };
    if expired.is_empty() {
        return;
    }
    {
        let mut data = shared.core.lock().await;
        for ch in &expired {
            // Clean up worktree if session had one
            if let Some(session) = data.sessions.get(ch) {
                if let Some(ref wt) = session.worktree {
                    cleanup_git_worktree(wt);
                }
            }
            data.sessions.remove(ch);
            data.cancel_tokens.remove(ch);
            data.active_request_owner.remove(ch);
            data.intervention_queue.remove(ch);
        }
    }
    for ch in &expired {
        shared.api_timestamps.remove(ch);
        shared.tmux_watchers.remove(ch);
    }
    println!("  [cleanup] Removed {} idle session(s)", expired.len());
}

/// Handle raw Discord events (non-slash-command messages, file uploads)
// ─── Slash commands ──────────────────────────────────────────────────────────

/// Autocomplete handler for remote profile names in /start
async fn autocomplete_remote_profile<'a>(
    _ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    let settings = crate::config::Settings::load();
    let partial_lower = partial.to_lowercase();
    let mut choices = Vec::new();
    if partial.is_empty() || "off".contains(&partial_lower) {
        choices.push(serenity::AutocompleteChoice::new(
            "off (local execution)",
            "off",
        ));
    }
    for p in &settings.remote_profiles {
        if partial.is_empty() || p.name.to_lowercase().contains(&partial_lower) {
            choices.push(serenity::AutocompleteChoice::new(
                format!("{} — {}@{}:{}", p.name, p.user, p.host, p.port),
                p.name.clone(),
            ));
        }
    }
    choices.into_iter().take(25).collect()
}

/// /start [path] [remote] — Start session at directory
#[poise::command(slash_command, rename = "start")]
async fn cmd_start(
    ctx: Context<'_>,
    #[description = "Directory path (empty for auto workspace)"] path: Option<String>,
    #[description = "Remote profile ('off' for local)"]
    #[autocomplete = "autocomplete_remote_profile"]
    remote: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] ◀ [{user_name}] /start path={:?} remote={:?}",
        path, remote
    );

    let path_str = path.as_deref().unwrap_or("").trim();

    // remote_override: None=not specified, Some(None)="off", Some(Some(name))=profile
    let remote_override = match remote.as_deref() {
        None => None,
        Some("off") => Some(None),
        Some(name) => {
            let settings = crate::config::Settings::load();
            if settings.remote_profiles.iter().any(|p| p.name == name) {
                Some(Some(name.to_string()))
            } else {
                ctx.say(format!("Remote profile '{}' not found.", name))
                    .await?;
                return Ok(());
            }
        }
    };

    // Determine if session will be remote (for path validation logic)
    let will_be_remote = match &remote_override {
        Some(Some(_)) => true,
        Some(None) => false,
        None => {
            let data = ctx.data().shared.core.lock().await;
            data.sessions
                .get(&ctx.channel_id())
                .and_then(|s| s.remote_profile_name.as_ref())
                .is_some()
        }
    };

    let canonical_path = if path_str.is_empty() && will_be_remote {
        // Remote + no path: use profile's default_path or "~"
        if let Some(Some(ref name)) = remote_override {
            let settings = crate::config::Settings::load();
            settings
                .remote_profiles
                .iter()
                .find(|p| p.name == *name)
                .map(|p| {
                    if p.default_path.is_empty() {
                        "~".to_string()
                    } else {
                        p.default_path.clone()
                    }
                })
                .unwrap_or_else(|| "~".to_string())
        } else {
            "~".to_string()
        }
    } else if path_str.is_empty() {
        // Local + no path: create random workspace directory
        let Some(workspace_dir) = workspace_root() else {
            ctx.say("Error: cannot determine workspace root.").await?;
            return Ok(());
        };
        use rand::Rng;
        let random_name: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(8)
            .map(|b| (b as char).to_ascii_lowercase())
            .collect();
        let new_dir = workspace_dir.join(&random_name);
        if let Err(e) = fs::create_dir_all(&new_dir) {
            ctx.say(format!("Error: failed to create workspace: {}", e))
                .await?;
            return Ok(());
        }
        new_dir.display().to_string()
    } else if will_be_remote {
        // Remote + path specified: expand tilde only, skip local validation
        if path_str.starts_with("~/") || path_str == "~" {
            // Keep tilde as-is for remote (remote shell will expand it)
            path_str.to_string()
        } else {
            path_str.to_string()
        }
    } else {
        // Local + path specified: expand ~ and validate locally
        let expanded = if path_str.starts_with("~/") || path_str == "~" {
            if let Some(home) = dirs::home_dir() {
                home.join(path_str.strip_prefix("~/").unwrap_or(""))
                    .display()
                    .to_string()
            } else {
                path_str.to_string()
            }
        } else {
            path_str.to_string()
        };
        let p = Path::new(&expanded);
        if !p.exists() || !p.is_dir() {
            ctx.say(format!("Error: '{}' is not a valid directory.", expanded))
                .await?;
            return Ok(());
        }
        p.canonicalize()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| expanded)
    };

    // Resolve channel/category names before taking the lock
    let (ch_name, cat_name) =
        resolve_channel_category(ctx.serenity_context(), ctx.channel_id()).await;

    // Check for worktree conflict (another channel using same git repo path)
    let worktree_info = {
        let data = ctx.data().shared.core.lock().await;
        let conflict = detect_worktree_conflict(&data.sessions, &canonical_path, ctx.channel_id());
        drop(data);
        if let Some(conflicting_channel) = conflict {
            let provider_str = {
                let settings = ctx.data().shared.settings.read().await;
                settings.provider.as_str().to_string()
            };
            let ch = ch_name.as_deref().unwrap_or("unknown");
            match create_git_worktree(&canonical_path, ch, &provider_str) {
                Ok((wt_path, branch)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🌿 Worktree conflict: {} already uses {}. Created worktree.",
                        conflicting_channel, canonical_path
                    );
                    Some(WorktreeInfo {
                        original_path: canonical_path.clone(),
                        worktree_path: wt_path,
                        branch_name: branch,
                    })
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 🌿 Worktree creation skipped: {e}");
                    None
                }
            }
        } else {
            None
        }
    };

    // Use worktree path if created, otherwise original
    let effective_path = worktree_info
        .as_ref()
        .map(|wt| wt.worktree_path.clone())
        .unwrap_or_else(|| canonical_path.clone());

    // Try to load existing session for this path
    let existing = load_existing_session(&effective_path, Some(ctx.channel_id().get()));

    let mut response_lines = Vec::new();

    {
        let mut data = ctx.data().shared.core.lock().await;
        let channel_id = ctx.channel_id();

        // Check if session already exists in memory (e.g. user already ran /remote off)
        let session_existed = data.sessions.contains_key(&channel_id);

        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: None,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_name: None,
                category_name: None,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),

                last_active: tokio::time::Instant::now(),
                worktree: None,
            });
        session.channel_id = Some(channel_id.get());
        session.channel_name = ch_name;
        session.category_name = cat_name;
        session.last_active = tokio::time::Instant::now();

        // Apply remote override from /start parameter
        if let Some(ref new_remote) = remote_override {
            let old_remote = session.remote_profile_name.clone();
            session.remote_profile_name = new_remote.clone();
            if old_remote != *new_remote {
                session.session_id = None;
            }
        }

        // Apply worktree info if created
        session.worktree = worktree_info.clone();

        if let Some((session_data, _)) = &existing {
            session.current_path = Some(effective_path.clone());
            session.history = session_data.history.clone();
            // Only restore remote_profile_name from file if session is newly created.
            // If session already existed in memory, the user may have explicitly set
            // remote to off (/remote off), so don't overwrite with saved value.
            if !session_existed && session.remote_profile_name.is_none() {
                session.remote_profile_name = session_data.remote_profile_name.clone();
            }
            // Only restore session_id if remote context matches
            // (don't resume a remote session locally or vice versa)
            let saved_is_remote = session_data.remote_profile_name.is_some();
            let current_is_remote = session.remote_profile_name.is_some();
            if saved_is_remote == current_is_remote {
                session.session_id = Some(session_data.session_id.clone());
            } else {
                session.session_id = None; // Mismatch: start fresh
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = session
                .remote_profile_name
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ▶ Session restored: {effective_path}{remote_info}");
            response_lines.push(format!(
                "Session restored at `{}`{}.",
                effective_path, remote_info
            ));
            response_lines.push(String::new());

            // Show last 5 conversation items
            let history_len = session_data.history.len();
            let start_idx = if history_len > 5 { history_len - 5 } else { 0 };
            for item in &session_data.history[start_idx..] {
                let prefix = match item.item_type {
                    HistoryType::User => "You",
                    HistoryType::Assistant => "AI",
                    HistoryType::Error => "Error",
                    HistoryType::System => "System",
                    HistoryType::ToolUse => "Tool",
                    HistoryType::ToolResult => "Result",
                };
                let content: String = item.content.chars().take(200).collect();
                let truncated = if item.content.chars().count() > 200 {
                    "..."
                } else {
                    ""
                };
                response_lines.push(format!("[{}] {}{}", prefix, content, truncated));
            }
        } else {
            session.session_id = None;
            session.current_path = Some(effective_path.clone());
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = session
                .remote_profile_name
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ▶ Session started: {effective_path}{remote_info}");
            response_lines.push(format!(
                "Session started at `{}`{}.",
                effective_path, remote_info
            ));
        }

        // Notify about worktree if created
        if let Some(ref wt) = session.worktree {
            response_lines.push(format!(
                "🌿 Worktree: `{}` 가 이미 사용 중이라 분리된 worktree에서 작업합니다.",
                wt.original_path
            ));
            response_lines.push(format!("Branch: `{}`", wt.branch_name));
        }

        // Persist channel → path mapping for auto-restore
        let ch_key = channel_id.get().to_string();
        let current_remote_for_settings = match &remote_override {
            None => {
                // No explicit override — persist current session state
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.remote_profile_name.clone())
            }
            _ => None,
        };
        drop(data);

        let mut settings = ctx.data().shared.settings.write().await;
        settings
            .last_sessions
            .insert(ch_key.clone(), canonical_path.clone());
        // Persist remote profile: store if active, remove if cleared
        match &remote_override {
            Some(Some(name)) => {
                settings.last_remotes.insert(ch_key, name.clone());
            }
            Some(None) => {
                settings.last_remotes.remove(&ch_key);
            }
            None => {
                if let Some(name) = current_remote_for_settings {
                    settings.last_remotes.insert(ch_key, name);
                }
            }
        }
        save_bot_settings(&ctx.data().token, &settings);
        drop(settings);

        // Rescan skills with project path to pick up project-level commands
        let new_skills = scan_skills(ctx.data().provider, Some(&effective_path));
        *ctx.data().shared.skills_cache.write().await = new_skills;
    }

    let response_text = response_lines.join("\n");
    send_long_message_ctx(ctx, &response_text).await?;

    Ok(())
}

/// /pwd — Show current working directory
#[poise::command(slash_command, rename = "pwd")]
async fn cmd_pwd(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /pwd");

    // Auto-restore session
    auto_restore_session(&ctx.data().shared, ctx.channel_id(), ctx.serenity_context()).await;

    let (current_path, remote_name) = {
        let data = ctx.data().shared.core.lock().await;
        let session = data.sessions.get(&ctx.channel_id());
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.remote_profile_name.clone()),
        )
    };

    match current_path {
        Some(path) => {
            let remote_info = remote_name
                .map(|n| format!(" (remote: **{}**)", n))
                .unwrap_or_else(|| " (local)".to_string());
            ctx.say(format!("`{}`{}", path, remote_info)).await?
        }
        None => {
            ctx.say("No active session. Use `/start <path>` first.")
                .await?
        }
    };
    Ok(())
}

async fn build_health_report(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) -> String {
    let (
        session_path,
        session_id,
        session_channel_name,
        pending_uploads,
        active_request,
        queued_count,
        session_count,
        active_request_count,
        queued_channel_count,
        queued_total,
    ) = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        let queued_count = data
            .intervention_queue
            .get(&channel_id)
            .map(|q| q.len())
            .unwrap_or(0);
        let queued_channel_count = data
            .intervention_queue
            .values()
            .filter(|q| !q.is_empty())
            .count();
        let queued_total: usize = data.intervention_queue.values().map(|q| q.len()).sum();
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.session_id.clone()),
            session.and_then(|s| s.channel_name.clone()),
            session.map(|s| s.pending_uploads.len()).unwrap_or(0),
            data.cancel_tokens.contains_key(&channel_id),
            queued_count,
            data.sessions.len(),
            data.cancel_tokens.len(),
            queued_channel_count,
            queued_total,
        )
    };

    let current_release = dirs::home_dir()
        .map(|h| h.join(".remotecc").join("releases").join("current"))
        .and_then(|p| fs::read_link(p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let previous_release = dirs::home_dir()
        .map(|h| h.join(".remotecc").join("releases").join("previous"))
        .and_then(|p| fs::read_link(p).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let release_label = |value: &str| value.rsplit('/').next().unwrap_or(value).to_string();
    let home_prefix = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/Users/itismyfield".to_string());
    let compact_path = |value: String| {
        if value.starts_with(&home_prefix) {
            value.replacen(&home_prefix, "~", 1)
        } else {
            value
        }
    };
    let inflight_states = load_inflight_states(provider);
    let inflight_count = inflight_states.len();
    let channel_inflight = inflight_states
        .iter()
        .find(|s| s.channel_id == channel_id.get());
    let recovering_count = shared.recovering_channels.len();
    let watchers = shared.tmux_watchers.len();
    let channel_watcher = shared.tmux_watchers.contains_key(&channel_id);
    let channel_recovering = shared.recovering_channels.contains_key(&channel_id);
    let current_path_text =
        compact_path(session_path.unwrap_or_else(|| "(no session)".to_string()));
    let session_id_text = session_id.unwrap_or_else(|| "(none)".to_string());
    let session_id_short = if session_id_text.len() > 24 {
        format!("{}...", &session_id_text[..24])
    } else {
        session_id_text.clone()
    };
    let tmux_session_name =
        session_channel_name.map(|name| provider.build_tmux_session_name(&name));
    let tmux_alive = if let Some(ref session_name) = tmux_session_name {
        match std::process::Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .status()
        {
            Ok(status) if status.success() => "alive",
            _ => "missing",
        }
    } else {
        "unknown"
    };
    let channel_state = if channel_recovering {
        "recovering"
    } else if active_request {
        "working"
    } else if channel_watcher {
        "watching"
    } else {
        "idle"
    };
    let inflight_text = channel_inflight
        .map(|state| format!("yes (offset {})", state.last_offset))
        .unwrap_or_else(|| "no".to_string());

    format!(
        "\
**RemoteCC Health**
- provider: `{}`
- dcserver pid: `{}`
- release: current `{}`, previous `{}`
- runtime: sessions `{}`, active `{}`, queued `{}/{}`
- bridge: watchers `{}`, recovering `{}`, inflight saved `{}`

**This Channel**
- state: `{}`
- path: `{}`
- session_id: `{}`
- tmux: `{}`
- bridge: active `{}`, watcher `{}`, inflight `{}`
- queue: interventions `{}`, uploads `{}`
",
        provider.as_str(),
        std::process::id(),
        release_label(&current_release),
        release_label(&previous_release),
        session_count,
        active_request_count,
        queued_channel_count,
        queued_total,
        watchers,
        recovering_count,
        inflight_count,
        channel_state,
        current_path_text,
        session_id_short,
        tmux_alive,
        if active_request { "yes" } else { "no" },
        if channel_watcher { "yes" } else { "no" },
        inflight_text,
        queued_count,
        pending_uploads
    )
}

async fn build_status_report(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) -> String {
    let (
        session_path,
        session_id,
        remote_name,
        pending_uploads,
        history_len,
        cleared,
        active_request,
        active_owner,
        queued_count,
        session_channel_name,
    ) = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        (
            session.and_then(|s| s.current_path.clone()),
            session.and_then(|s| s.session_id.clone()),
            session.and_then(|s| s.remote_profile_name.clone()),
            session.map(|s| s.pending_uploads.len()).unwrap_or(0),
            session.map(|s| s.history.len()).unwrap_or(0),
            session.map(|s| s.cleared).unwrap_or(false),
            data.cancel_tokens.contains_key(&channel_id),
            data.active_request_owner.get(&channel_id).copied(),
            data.intervention_queue
                .get(&channel_id)
                .map(|q| q.len())
                .unwrap_or(0),
            session.and_then(|s| s.channel_name.clone()),
        )
    };

    let home_prefix = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/Users/itismyfield".to_string());
    let compact_path = |value: String| {
        if value.starts_with(&home_prefix) {
            value.replacen(&home_prefix, "~", 1)
        } else {
            value
        }
    };
    let session_id_text = session_id.unwrap_or_else(|| "(none)".to_string());
    let session_id_short = if session_id_text.len() > 24 {
        format!("{}...", &session_id_text[..24])
    } else {
        session_id_text
    };
    let tmux_session_name =
        session_channel_name.map(|name| provider.build_tmux_session_name(&name));
    let tmux_alive = if let Some(ref session_name) = tmux_session_name {
        match std::process::Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .status()
        {
            Ok(status) if status.success() => "alive",
            _ => "missing",
        }
    } else {
        "unknown"
    };
    let channel_watcher = shared.tmux_watchers.contains_key(&channel_id);
    let channel_recovering = shared.recovering_channels.contains_key(&channel_id);
    let channel_state = if channel_recovering {
        "recovering"
    } else if active_request {
        "working"
    } else if channel_watcher {
        "watching"
    } else {
        "idle"
    };
    let owner_text = active_owner
        .map(|id| format!("<@{}>", id.get()))
        .unwrap_or_else(|| "(none)".to_string());
    let path_text = compact_path(session_path.unwrap_or_else(|| "(no session)".to_string()));
    let remote_text = remote_name.unwrap_or_else(|| "local".to_string());

    format!(
        "\
**Channel Status**
- provider: `{}`
- state: `{}`
- path: `{}`
- session_id: `{}`
- remote: `{}`
- tmux: `{}`
- owner: {}
- queue: interventions `{}`, uploads `{}`
- history: items `{}`, cleared `{}`
",
        provider.as_str(),
        channel_state,
        path_text,
        session_id_short,
        remote_text,
        tmux_alive,
        owner_text,
        queued_count,
        pending_uploads,
        history_len,
        if cleared { "yes" } else { "no" }
    )
}

async fn build_inflight_report(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
) -> String {
    let mut inflight_states = load_inflight_states(provider);
    inflight_states.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let recovering_count = shared.recovering_channels.len();
    let channel_inflight = inflight_states
        .iter()
        .find(|state| state.channel_id == channel_id.get());

    let channel_status = channel_inflight.map(|_| "saved").unwrap_or("none");

    let current_section = if let Some(state) = channel_inflight {
        let session_id = state
            .session_id
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        let session_id_short = if session_id.len() > 24 {
            format!("{}...", &session_id[..24])
        } else {
            session_id
        };
        let tmux_name = state
            .tmux_session_name
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        format!(
            "\
**This Channel**
- started: `{}`
- updated: `{}`
- offset: `{}`
- session_id: `{}`
- tmux: `{}`
- placeholder_msg: `{}`
- user_text: `{}`
",
            state.started_at,
            state.updated_at,
            state.last_offset,
            session_id_short,
            tmux_name,
            state.current_msg_id,
            truncate_str(&state.user_text, 80)
        )
    } else {
        "\
**This Channel**
- status: `none`
"
        .to_string()
    };

    let saved_channels = if inflight_states.is_empty() {
        "- (none)".to_string()
    } else {
        inflight_states
            .iter()
            .take(6)
            .map(|state| {
                format!(
                    "- `{}` (`{}`) offset `{}` updated `{}`",
                    state.channel_name.as_deref().unwrap_or("unknown"),
                    state.channel_id,
                    state.last_offset,
                    state.updated_at
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "\
**Inflight**
- provider: `{}`
- saved turns: `{}`
- recovering channels: `{}`
- this channel: `{}`

{}
**Saved Channels**
{}
",
        provider.as_str(),
        inflight_states.len(),
        recovering_count,
        channel_status,
        current_section,
        saved_channels
    )
}

/// /health — Show runtime health summary
#[poise::command(slash_command, rename = "health")]
async fn cmd_health(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /health");

    let text = build_health_report(&ctx.data().shared, ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /status — Show concise per-channel runtime state
#[poise::command(slash_command, rename = "status")]
async fn cmd_status(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /status");

    let text = build_status_report(&ctx.data().shared, ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /inflight — Show saved inflight turn state
#[poise::command(slash_command, rename = "inflight")]
async fn cmd_inflight(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /inflight");

    let text =
        build_inflight_report(&ctx.data().shared, ctx.data().provider, ctx.channel_id()).await;
    send_long_message_ctx(ctx, &text).await?;
    Ok(())
}

/// /clear — Clear AI conversation history
#[poise::command(slash_command, rename = "clear")]
async fn cmd_clear(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /clear");

    let channel_id = ctx.channel_id();

    // Cancel in-progress AI request if any
    let cancel_token = {
        let data = ctx.data().shared.core.lock().await;
        data.cancel_tokens.get(&channel_id).cloned()
    };
    if let Some(token) = cancel_token {
        cancel_active_token(&token, true);
    }

    {
        let mut data = ctx.data().shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            // Clean up ALL session files on disk (including current) when clearing
            if let Some(ref path) = session.current_path {
                cleanup_session_files(path, None);
            }
            cleanup_channel_uploads(channel_id);
            session.session_id = None;
            session.history.clear();
            session.pending_uploads.clear();
            session.cleared = true;
        }
        data.cancel_tokens.remove(&channel_id);
        data.active_request_owner.remove(&channel_id);
        data.intervention_queue.remove(&channel_id);
    }

    ctx.say("Session cleared.").await?;
    println!("  [{ts}] ▶ [{user_name}] Session cleared");
    Ok(())
}

/// /stop — Cancel in-progress AI request
#[poise::command(slash_command, rename = "stop")]
async fn cmd_stop(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /stop");

    let channel_id = ctx.channel_id();
    let token = {
        let data = ctx.data().shared.core.lock().await;
        data.cancel_tokens.get(&channel_id).cloned()
    };

    match token {
        Some(token) => {
            if token.cancelled.load(Ordering::Relaxed) {
                ctx.say("Already stopping...").await?;
                return Ok(());
            }

            ctx.say("Stopping...").await?;

            cancel_active_token(&token, true);
            println!("  [{ts}] ■ Cancel signal sent");
        }
        None => {
            ctx.say("No active request to stop.").await?;
        }
    }
    Ok(())
}

/// /down <file> — Download file from server
#[poise::command(slash_command, rename = "down")]
async fn cmd_down(
    ctx: Context<'_>,
    #[description = "File path to download"] file: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /down {file}");

    let file_path = file.trim();
    if file_path.is_empty() {
        ctx.say("Usage: `/down <filepath>`\nExample: `/down /home/user/file.txt`")
            .await?;
        return Ok(());
    }

    // Resolve relative path
    let resolved_path = if Path::new(file_path).is_absolute() {
        file_path.to_string()
    } else {
        let current_path = {
            let data = ctx.data().shared.core.lock().await;
            data.sessions
                .get(&ctx.channel_id())
                .and_then(|s| s.current_path.clone())
        };
        match current_path {
            Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_path),
            None => {
                ctx.say("No active session. Use absolute path or `/start <path>` first.")
                    .await?;
                return Ok(());
            }
        }
    };

    let path = Path::new(&resolved_path);
    if !path.exists() {
        ctx.say(format!("File not found: {}", resolved_path))
            .await?;
        return Ok(());
    }
    if !path.is_file() {
        ctx.say(format!("Not a file: {}", resolved_path)).await?;
        return Ok(());
    }

    // Send file as attachment
    let attachment = CreateAttachment::path(path).await?;
    ctx.send(poise::CreateReply::default().attachment(attachment))
        .await?;

    Ok(())
}

/// /shell <command> — Run shell command directly
#[poise::command(slash_command, rename = "shell")]
async fn cmd_shell(
    ctx: Context<'_>,
    #[description = "Shell command to execute"] command: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let preview = truncate_str(&command, 60);
    println!("  [{ts}] ◀ [{user_name}] /shell {preview}");

    // Defer for potentially long-running commands
    ctx.defer().await?;

    let working_dir = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&ctx.channel_id())
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = command.clone();
    let working_dir_clone = working_dir.clone();

    let result = tokio::task::spawn_blocking(move || {
        let child = std::process::Command::new("bash")
            .args(["-c", &cmd_owned])
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match child {
            Ok(child) => child.wait_with_output(),
            Err(e) => Err(e),
        }
    })
    .await;

    let response = match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);

            let mut parts = Vec::new();
            if !stdout.is_empty() {
                parts.push(format!("```\n{}\n```", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
            }
            if parts.is_empty() {
                parts.push(format!("(exit code: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(exit code: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => format!("Failed to execute: {}", e),
        Err(e) => format!("Task error: {}", e),
    };

    send_long_message_ctx(ctx, &response).await?;
    println!("  [{ts}] ▶ [{user_name}] Shell done");
    Ok(())
}

/// /allowedtools — Show currently allowed tools
#[poise::command(slash_command, rename = "allowedtools")]
async fn cmd_allowedtools(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /allowedtools");

    let tools = {
        let settings = ctx.data().shared.settings.read().await;
        settings.allowed_tools.clone()
    };

    let mut msg = String::from("**Allowed Tools**\n\n");
    for tool in &tools {
        let (desc, destructive) = tool_info(tool);
        let badge = risk_badge(destructive);
        if badge.is_empty() {
            msg.push_str(&format!("`{}` — {}\n", tool, desc));
        } else {
            msg.push_str(&format!("`{}` {} — {}\n", tool, badge, desc));
        }
    }
    msg.push_str(&format!(
        "\n{} = destructive\nTotal: {}",
        risk_badge(true),
        tools.len()
    ));

    send_long_message_ctx(ctx, &msg).await?;
    Ok(())
}

/// /allowed <+/-tool> — Add or remove a tool
#[poise::command(slash_command, rename = "allowed")]
async fn cmd_allowed(
    ctx: Context<'_>,
    #[description = "Use +name to add, -name to remove (e.g. +Bash or -Bash)"] action: String,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /allowed {action}");

    let arg = action.trim();
    let (op, raw_name) = if let Some(name) = arg.strip_prefix('+') {
        ('+', name.trim())
    } else if let Some(name) = arg.strip_prefix('-') {
        ('-', name.trim())
    } else {
        ctx.say("Use `+toolname` to add or `-toolname` to remove.\nExample: `/allowed +Bash`")
            .await?;
        return Ok(());
    };

    if raw_name.is_empty() {
        ctx.say("Tool name cannot be empty.").await?;
        return Ok(());
    }

    let Some(tool_name) = canonical_tool_name(raw_name).map(str::to_string) else {
        ctx.say(format!(
            "Unknown tool `{}`. Use `/allowedtools` to see valid tool names.",
            raw_name
        ))
        .await?;
        return Ok(());
    };

    let response_msg = {
        let mut settings = ctx.data().shared.settings.write().await;
        match op {
            '+' => {
                if settings.allowed_tools.iter().any(|t| t == &tool_name) {
                    format!("`{}` is already in the list.", tool_name)
                } else {
                    settings.allowed_tools.push(tool_name.clone());
                    save_bot_settings(&ctx.data().token, &settings);
                    format!("Added `{}`", tool_name)
                }
            }
            '-' => {
                let before_len = settings.allowed_tools.len();
                settings.allowed_tools.retain(|t| t != &tool_name);
                if settings.allowed_tools.len() < before_len {
                    save_bot_settings(&ctx.data().token, &settings);
                    format!("Removed `{}`", tool_name)
                } else {
                    format!("`{}` is not in the list.", tool_name)
                }
            }
            _ => unreachable!(),
        }
    };

    ctx.say(&response_msg).await?;
    Ok(())
}

/// /adduser @user — Allow another user to use the bot (owner only)
#[poise::command(slash_command, rename = "adduser")]
async fn cmd_adduser(
    ctx: Context<'_>,
    #[description = "User to add"] user: serenity::User,
) -> Result<(), Error> {
    let author_id = ctx.author().id;
    let author_name = &ctx.author().name;
    if !check_auth(
        author_id,
        author_name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
    {
        return Ok(());
    }
    if !check_owner(author_id, &ctx.data().shared).await {
        ctx.say("Only the owner can add users.").await?;
        return Ok(());
    }

    let target_id = user.id.get();
    let target_name = &user.name;

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{author_name}] /adduser {target_name}");

    {
        let mut settings = ctx.data().shared.settings.write().await;
        if settings.allowed_user_ids.contains(&target_id) {
            ctx.say(format!("`{}` is already authorized.", target_name))
                .await?;
            return Ok(());
        }
        settings.allowed_user_ids.push(target_id);
        save_bot_settings(&ctx.data().token, &settings);
    }

    ctx.say(format!("Added `{}` as authorized user.", target_name))
        .await?;
    println!("  [{ts}] ▶ Added user: {target_name} (id:{target_id})");
    Ok(())
}

/// /removeuser @user — Remove a user's access (owner only)
#[poise::command(slash_command, rename = "removeuser")]
async fn cmd_removeuser(
    ctx: Context<'_>,
    #[description = "User to remove"] user: serenity::User,
) -> Result<(), Error> {
    let author_id = ctx.author().id;
    let author_name = &ctx.author().name;
    if !check_auth(
        author_id,
        author_name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
    {
        return Ok(());
    }
    if !check_owner(author_id, &ctx.data().shared).await {
        ctx.say("Only the owner can remove users.").await?;
        return Ok(());
    }

    let target_id = user.id.get();
    let target_name = &user.name;

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{author_name}] /removeuser {target_name}");

    {
        let mut settings = ctx.data().shared.settings.write().await;
        let before_len = settings.allowed_user_ids.len();
        settings.allowed_user_ids.retain(|&id| id != target_id);
        if settings.allowed_user_ids.len() == before_len {
            ctx.say(format!("`{}` is not in the authorized list.", target_name))
                .await?;
            return Ok(());
        }
        save_bot_settings(&ctx.data().token, &settings);
    }

    ctx.say(format!("Removed `{}` from authorized users.", target_name))
        .await?;
    println!("  [{ts}] ▶ Removed user: {target_name} (id:{target_id})");
    Ok(())
}

/// /debug — Toggle debug logging at runtime
#[poise::command(slash_command, rename = "debug")]
async fn cmd_debug(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /debug");

    let new_state = claude::toggle_debug();
    let status = if new_state { "ON" } else { "OFF" };
    ctx.say(format!("Debug logging: **{}**", status)).await?;
    println!("  [{ts}] ▶ Debug logging toggled to {status}");
    Ok(())
}

/// /help — Show help information
#[poise::command(slash_command, rename = "help")]
async fn cmd_help(ctx: Context<'_>) -> Result<(), Error> {
    let provider_name = ctx.data().provider.display_name();
    let help = format!(
        "\
**RemoteCC Discord Bot**
Manage server files & chat with {}.
Each channel gets its own independent {} session.

**Session**
`/start <path> [remote]` — Start session at directory
`/start` — Start with auto-generated workspace
`/pwd` — Show current working directory
`/health` — Show runtime health summary
`/status` — Show this channel session status
`/inflight` — Show saved inflight turn state
`/clear` — Clear AI conversation history
`/stop` — Stop current AI request

**File Transfer**
`/down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!<command>` — Run shell command directly
`/shell <command>` — Run shell command (slash command)

**AI Chat**
Any other message is sent to {}.
AI can read, edit, and run commands in your session.

**Tool Management**
`/allowedtools` — Show currently allowed tools
`/allowed +name` — Add tool (e.g. `/allowed +Bash`)
`/allowed -name` — Remove tool

**Skills**
`/cc <skill>` — Run a provider skill (autocomplete)

**Settings**
`/debug` — Toggle debug logging

**User Management** (owner only)
`/adduser @user` — Allow a user to use the bot
`/removeuser @user` — Remove a user's access

`/help` — Show this help",
        provider_name, provider_name, provider_name
    );

    ctx.say(help).await?;
    Ok(())
}

/// Autocomplete handler for /cc skill names
async fn autocomplete_skill<'a>(
    ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    let builtins = [
        ("health", "Show runtime health summary"),
        ("status", "Show this channel session status"),
        ("inflight", "Show saved inflight turn state"),
        ("pwd", "Show current working directory"),
        ("clear", "Clear session state"),
        ("stop", "Stop current AI request"),
        ("help", "Show command help"),
    ];
    let skills = ctx.data().shared.skills_cache.read().await;
    let partial_lower = partial.to_lowercase();
    let mut choices = Vec::new();

    for (name, desc) in builtins {
        if partial.is_empty() || name.contains(&partial_lower) {
            let label = format!("{} — {}", name, truncate_str(desc, 60));
            choices.push(serenity::AutocompleteChoice::new(label, name.to_string()));
        }
    }

    for (name, desc) in skills.iter() {
        if choices.len() >= 25 {
            break;
        }
        if partial.is_empty() || name.to_lowercase().contains(&partial_lower) {
            let label = format!("{} — {}", name, truncate_str(desc, 60));
            choices.push(serenity::AutocompleteChoice::new(label, name.clone()));
        }
    }

    choices
}

/// /cc <skill> [args] — Run a provider skill
#[poise::command(slash_command, rename = "cc")]
async fn cmd_cc(
    ctx: Context<'_>,
    #[description = "Skill name"]
    #[autocomplete = "autocomplete_skill"]
    skill: String,
    #[description = "Additional arguments for the skill"] args: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let args_str = args.as_deref().unwrap_or("");
    println!("  [{ts}] ◀ [{user_name}] /cc {skill} {args_str}");

    // Handle built-in commands directly instead of sending to AI
    match skill.as_str() {
        "clear" => {
            let channel_id = ctx.channel_id();
            let cancel_token = {
                let data = ctx.data().shared.core.lock().await;
                data.cancel_tokens.get(&channel_id).cloned()
            };
            if let Some(token) = cancel_token {
                cancel_active_token(&token, true);
            }
            {
                let mut data = ctx.data().shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    if let Some(ref path) = session.current_path {
                        cleanup_session_files(path, None);
                    }
                    session.session_id = None;
                    session.history.clear();
                    session.pending_uploads.clear();
                    session.cleared = true;
                }
                cleanup_channel_uploads(channel_id);
                data.cancel_tokens.remove(&channel_id);
                data.active_request_owner.remove(&channel_id);
                data.intervention_queue.remove(&channel_id);
            }
            ctx.say("Session cleared.").await?;
            println!("  [{ts}] ▶ [{user_name}] Session cleared");
            return Ok(());
        }
        "stop" => {
            let channel_id = ctx.channel_id();
            let token = {
                let data = ctx.data().shared.core.lock().await;
                data.cancel_tokens.get(&channel_id).cloned()
            };
            match token {
                Some(token) => {
                    if token.cancelled.load(Ordering::Relaxed) {
                        ctx.say("Already stopping...").await?;
                        return Ok(());
                    }
                    ctx.say("Stopping...").await?;
                    cancel_active_token(&token, true);
                    println!("  [{ts}] ■ Cancel signal sent");
                }
                None => {
                    ctx.say("No active request to stop.").await?;
                }
            }
            return Ok(());
        }
        "pwd" => {
            let (current_path, remote_name) = {
                let data = ctx.data().shared.core.lock().await;
                let session = data.sessions.get(&ctx.channel_id());
                (
                    session.and_then(|s| s.current_path.clone()),
                    session.and_then(|s| s.remote_profile_name.clone()),
                )
            };
            match current_path {
                Some(path) => {
                    let remote_info = remote_name
                        .map(|n| format!(" (remote: **{}**)", n))
                        .unwrap_or_else(|| " (local)".to_string());
                    ctx.say(format!("`{}`{}", path, remote_info)).await?
                }
                None => {
                    ctx.say("No active session. Use `/start <path>` first.")
                        .await?
                }
            };
            return Ok(());
        }
        "health" => {
            let text =
                build_health_report(&ctx.data().shared, ctx.data().provider, ctx.channel_id())
                    .await;
            send_long_message_ctx(ctx, &text).await?;
            return Ok(());
        }
        "status" => {
            let text =
                build_status_report(&ctx.data().shared, ctx.data().provider, ctx.channel_id())
                    .await;
            send_long_message_ctx(ctx, &text).await?;
            return Ok(());
        }
        "inflight" => {
            let text =
                build_inflight_report(&ctx.data().shared, ctx.data().provider, ctx.channel_id())
                    .await;
            send_long_message_ctx(ctx, &text).await?;
            return Ok(());
        }
        "help" => {
            // Redirect to help — just tell user to use /help
            ctx.say("Use `/help` to see all commands.").await?;
            return Ok(());
        }
        _ => {}
    }

    // Auto-restore session (must run before skill check to refresh skills_cache with project path)
    auto_restore_session(&ctx.data().shared, ctx.channel_id(), ctx.serenity_context()).await;

    // Verify skill exists
    let skill_exists = {
        let skills = ctx.data().shared.skills_cache.read().await;
        skills.iter().any(|(name, _)| name == &skill)
    };

    if !skill_exists {
        ctx.say(format!(
            "Unknown skill: `{}`. Use `/cc` to see available skills.",
            skill
        ))
        .await?;
        return Ok(());
    }

    // Check session exists
    let has_session = {
        let data = ctx.data().shared.core.lock().await;
        data.sessions
            .get(&ctx.channel_id())
            .and_then(|s| s.current_path.as_ref())
            .is_some()
    };

    if !has_session {
        ctx.say("No active session. Use `/start <path>` first.")
            .await?;
        return Ok(());
    }

    // Block if AI is in progress
    {
        let d = ctx.data().shared.core.lock().await;
        if d.cancel_tokens.contains_key(&ctx.channel_id()) {
            drop(d);
            ctx.say("AI request in progress. Use `/stop` to cancel.")
                .await?;
            return Ok(());
        }
    }

    // Build the prompt that tells the active provider to invoke the skill
    let skill_prompt = match ctx.data().provider {
        ProviderKind::Claude => {
            if args_str.is_empty() {
                format!(
                    "Execute the skill `/{skill}` now. \
                     Use the Skill tool with skill=\"{skill}\"."
                )
            } else {
                format!(
                    "Execute the skill `/{skill}` with arguments: {args_str}\n\
                     Use the Skill tool with skill=\"{skill}\", args=\"{args_str}\"."
                )
            }
        }
        ProviderKind::Codex => {
            if args_str.is_empty() {
                format!(
                    "Use the local Codex skill `/{skill}` now. \
                     Follow its SKILL.md instructions exactly and complete the task."
                )
            } else {
                format!(
                    "Use the local Codex skill `/{skill}` now with this user request: {args_str}\n\
                     Follow its SKILL.md instructions exactly and adapt them to the request."
                )
            }
        }
    };

    // Send a confirmation message that we can use as the "user message" for reactions
    ctx.defer().await?;
    let confirm = ctx
        .channel_id()
        .send_message(
            ctx.serenity_context(),
            CreateMessage::new().content(format!("⚡ Running skill: `/{skill}`")),
        )
        .await?;

    // Hand off to the text message handler (it creates its own placeholder)
    handle_text_message(
        ctx.serenity_context(),
        ctx.channel_id(),
        confirm.id,
        ctx.author().id,
        &ctx.author().name,
        &skill_prompt,
        &ctx.data().shared,
        &ctx.data().token,
        false,
        false,
        false,
        None,
    )
    .await?;

    Ok(())
}

#[poise::command(slash_command, rename = "meeting")]
async fn cmd_meeting(
    ctx: Context<'_>,
    #[description = "Action: start / stop / status"] action: String,
    #[description = "Agenda (required for start)"] agenda: Option<String>,
    #[description = "Primary provider (optional: claude / codex)"] primary_provider: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let channel_id = ctx.channel_id();
    let agenda_str = agenda.as_deref().unwrap_or("");
    println!("  [{ts}] ◀ [{user_name}] /meeting {action} {agenda_str}");

    ctx.defer().await?;

    let http = ctx.serenity_context().http.clone();
    let shared = ctx.data().shared.clone();

    match action.as_str() {
        "start" => {
            let agenda_text = agenda_str.trim();
            if agenda_text.is_empty() {
                ctx.say(
                    "사용법: `/meeting start <안건>` + optional `primary_provider=claude|codex`",
                )
                .await?;
                return Ok(());
            }
            let selected_provider = match primary_provider.as_deref().map(ProviderKind::from_str) {
                Some(Some(provider)) => provider,
                Some(None) => {
                    ctx.say("primary_provider는 `claude` 또는 `codex`만 가능해.")
                        .await?;
                    return Ok(());
                }
                None => ctx.data().provider,
            };
            let agenda_owned = agenda_text.to_string();
            // Spawn as background task
            tokio::spawn(async move {
                match meeting::start_meeting(
                    &*http,
                    channel_id,
                    &agenda_owned,
                    selected_provider,
                    &shared,
                )
                .await
                {
                    Ok(Some(id)) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ✅ Meeting completed: {id}");
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ❌ Meeting error: {e}");
                        rate_limit_wait(&shared, channel_id).await;
                        let _ = channel_id
                            .send_message(
                                &*http,
                                CreateMessage::new().content(format!("❌ 회의 오류: {}", e)),
                            )
                            .await;
                    }
                }
            });
            ctx.say(format!(
                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                selected_provider.display_name(),
                selected_provider.counterpart().display_name()
            ))
            .await?;
        }
        "stop" => {
            meeting::cancel_meeting(&*http, channel_id, &shared).await?;
        }
        "status" => {
            meeting::meeting_status(&*http, channel_id, &shared).await?;
        }
        _ => {
            ctx.say("사용법: `/meeting start|stop|status`").await?;
        }
    }

    Ok(())
}

// ─── Text message → Claude AI ───────────────────────────────────────────────

/// Handle regular text messages — send to the active provider.
/// Check if a path is a git repo and if another channel already uses it.
/// Returns the conflicting channel's name if found.
fn detect_worktree_conflict(
    sessions: &HashMap<ChannelId, DiscordSession>,
    path: &str,
    my_channel: ChannelId,
) -> Option<String> {
    let norm = path.trim_end_matches('/');
    for (cid, session) in sessions {
        if *cid == my_channel {
            continue;
        }
        let other_path = if let Some(ref wt) = session.worktree {
            &wt.original_path
        } else {
            match &session.current_path {
                Some(p) => p.as_str(),
                None => continue,
            }
        };
        if other_path.trim_end_matches('/') == norm {
            return session
                .channel_name
                .clone()
                .or_else(|| Some(cid.get().to_string()));
        }
    }
    None
}

/// Create a git worktree for the given repo path.
/// Returns (worktree_path, branch_name) on success.
fn create_git_worktree(
    repo_path: &str,
    channel_name: &str,
    provider: &str,
) -> Result<(String, String), String> {
    let git_check = std::process::Command::new("git")
        .args(["-C", repo_path, "rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|e| format!("git check failed: {}", e))?;
    if !git_check.status.success() {
        return Err(format!("{} is not a git repository", repo_path));
    }

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let safe_name = channel_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let branch = format!("wt/{}-{}-{}", provider, safe_name, ts);

    let wt_base = worktrees_root().ok_or("Cannot determine worktree root")?;
    std::fs::create_dir_all(&wt_base)
        .map_err(|e| format!("Failed to create worktree base dir: {}", e))?;
    let wt_dir = wt_base.join(format!("{}-{}-{}", provider, safe_name, ts));
    let wt_path = wt_dir.display().to_string();

    let output = std::process::Command::new("git")
        .args(["-C", repo_path, "worktree", "add", &wt_path, "-b", &branch])
        .output()
        .map_err(|e| format!("git worktree add failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr));
    }

    let ts_log = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts_log}] 🌿 Created worktree: {} (branch: {})",
        wt_path, branch
    );
    Ok((wt_path, branch))
}

/// Clean up a git worktree after session ends.
fn cleanup_git_worktree(wt_info: &WorktreeInfo) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    let status = std::process::Command::new("git")
        .args(["-C", &wt_info.worktree_path, "status", "--porcelain"])
        .output();
    let has_changes = match &status {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    // Check if branch has new commits
    let diff = std::process::Command::new("git")
        .args([
            "-C",
            &wt_info.original_path,
            "log",
            "--oneline",
            &format!("HEAD..{}", wt_info.branch_name),
        ])
        .output();
    let has_commits = match &diff {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    if has_changes || has_commits {
        println!(
            "  [{ts}] 🌿 Worktree {} has changes/commits — keeping for manual merge",
            wt_info.worktree_path
        );
        println!(
            "  [{ts}] 🌿 Branch: {} | Original: {}",
            wt_info.branch_name, wt_info.original_path
        );
    } else {
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "worktree",
                "remove",
                &wt_info.worktree_path,
            ])
            .output();
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "branch",
                "-d",
                &wt_info.branch_name,
            ])
            .output();
        println!(
            "  [{ts}] 🌿 Cleaned up worktree: {} (no changes)",
            wt_info.worktree_path
        );
    }
}

// ─── File upload handling ────────────────────────────────────────────────────

// ─── Sendfile (CLI) ──────────────────────────────────────────────────────────

/// Send a file to a Discord channel (called from CLI --discord-sendfile)
pub async fn send_file_to_channel(
    token: &str,
    channel_id: u64,
    file_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path).into());
    }

    let http = serenity::Http::new(token);

    let channel = ChannelId::new(channel_id);
    let attachment = CreateAttachment::path(path).await?;

    channel
        .send_message(
            &http,
            CreateMessage::new()
                .content(format!(
                    "📎 {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
                .add_file(attachment),
        )
        .await?;

    Ok(())
}

/// Send a text message to a Discord channel (called from CLI --discord-sendmessage)
pub async fn send_message_to_channel(
    token: &str,
    channel_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let channel = ChannelId::new(channel_id);

    channel
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

/// Send a text message to a Discord user DM (called from CLI --discord-senddm)
pub async fn send_message_to_user(
    token: &str,
    user_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let dm_channel = UserId::new(user_id).create_dm_channel(&http).await?;

    dm_channel
        .id
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

// ─── Session persistence ─────────────────────────────────────────────────────

/// Auto-restore session from bot_settings.json if not in memory
async fn auto_restore_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    {
        let data = shared.core.lock().await;
        if data.sessions.contains_key(&channel_id) {
            return;
        }
    }

    // Resolve channel/category before taking the lock for mutation
    let (ch_name, cat_name) = resolve_channel_category(serenity_ctx, channel_id).await;

    // Read settings first to get last_sessions/last_remotes info
    let (last_path, is_remote, saved_remote, provider) = {
        let settings = shared.settings.read().await;
        let channel_key = channel_id.get().to_string();
        let last_path = settings.last_sessions.get(&channel_key).cloned();
        let is_remote = settings.last_remotes.contains_key(&channel_key);
        let saved_remote = settings.last_remotes.get(&channel_key).cloned();
        (last_path, is_remote, saved_remote, settings.provider)
    };

    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&channel_id) {
        return; // Double-check after re-acquiring lock
    }

    if let Some(last_path) = last_path {
        if is_remote || Path::new(&last_path).is_dir() {
            let existing = load_existing_session(&last_path, Some(channel_id.get()));
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: None,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    channel_id: Some(channel_id.get()),
                    channel_name: ch_name,
                    category_name: cat_name,
                    remote_profile_name: saved_remote.clone(),

                    last_active: tokio::time::Instant::now(),
                    worktree: None,
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            session.current_path = Some(last_path.clone());
            if let Some((session_data, _)) = existing {
                session.session_id = Some(session_data.session_id.clone());
                session.history = session_data.history.clone();
            }
            drop(data);
            // Rescan skills with project path
            let new_skills = scan_skills(provider, Some(&last_path));
            *shared.skills_cache.write().await = new_skills;
            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = saved_remote
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ↻ Auto-restored session: {last_path}{remote_info}");
        }
    }
}

/// Load existing session from ai_sessions directory.
/// Prefers sessions with a non-empty session_id. Among those, picks the most recently modified.
fn load_existing_session(
    current_path: &str,
    channel_id: Option<u64>,
) -> Option<(SessionData, std::time::SystemTime)> {
    let sessions_dir = ai_screen::ai_sessions_dir()?;

    if !sessions_dir.exists() {
        return None;
    }

    let mut best_with_id: Option<(SessionData, std::time::SystemTime)> = None;
    let mut best_without_id: Option<(SessionData, std::time::SystemTime)> = None;

    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(session_data) = serde_json::from_str::<SessionData>(&content) {
                        if session_data.current_path == current_path {
                            // Strict channel-aware restore when channel_id is provided.
                            if let Some(cid) = channel_id {
                                if session_data.discord_channel_id != Some(cid) {
                                    continue;
                                }
                            }

                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    let has_id = !session_data.session_id.is_empty();
                                    let target = if has_id {
                                        &mut best_with_id
                                    } else {
                                        &mut best_without_id
                                    };
                                    match target {
                                        None => *target = Some((session_data, modified)),
                                        Some((_, latest_time)) if modified > *latest_time => {
                                            *target = Some((session_data, modified));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Prefer sessions with a valid session_id
    best_with_id.or(best_without_id)
}

/// Clean up stale session files for a given path, keeping only the one matching current_session_id.
fn cleanup_session_files(current_path: &str, current_session_id: Option<&str>) {
    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };
    if !sessions_dir.exists() {
        return;
    }

    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        return;
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.extension().map(|e| e == "json").unwrap_or(false) {
            continue;
        }
        // Don't delete the current session file
        if let Some(sid) = current_session_id {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if stem == sid {
                    continue;
                }
            }
        }
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(old) = serde_json::from_str::<SessionData>(&content) {
                if old.current_path == current_path {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

/// Resolve the channel name and parent category name for a Discord channel.
async fn resolve_channel_category(
    ctx: &serenity::prelude::Context,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(&ctx.http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = ctx.cache.guild(gc.guild_id).and_then(|guild| {
            guild
                .channels
                .get(&parent_id)
                .map(|parent_ch| parent_ch.name.clone())
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(&ctx.http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
        None
    };
    (ch_name, cat_name)
}

/// On startup, resolve category names for all known channels and update session files.
async fn migrate_session_categories(ctx: &serenity::prelude::Context, shared: &Arc<SharedData>) {
    let sessions_dir = match ai_screen::ai_sessions_dir() {
        Some(d) if d.exists() => d,
        _ => return,
    };

    // Collect channel IDs from bot_settings.last_sessions
    let channel_keys: Vec<(String, String)> = {
        let settings = shared.settings.read().await;
        settings
            .last_sessions
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let mut updated = 0usize;
    for (channel_key, session_path) in &channel_keys {
        let Ok(cid) = channel_key.parse::<u64>() else {
            continue;
        };
        let channel_id = serenity::model::id::ChannelId::new(cid);
        let (ch_name, cat_name) = resolve_channel_category(ctx, channel_id).await;
        if ch_name.is_none() && cat_name.is_none() {
            continue;
        }

        // Find the session file for this channel's path
        let existing = load_existing_session(session_path, Some(cid));
        if let Some((session_data, _)) = existing {
            let file_path = sessions_dir.join(format!("{}.json", session_data.session_id));
            if file_path.exists() {
                // Read, update category fields, write back
                if let Ok(content) = fs::read_to_string(&file_path) {
                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert(
                                "discord_channel_id".to_string(),
                                serde_json::Value::Number(serde_json::Number::from(cid)),
                            );
                            if let Some(ref name) = ch_name {
                                obj.insert(
                                    "discord_channel_name".to_string(),
                                    serde_json::Value::String(name.clone()),
                                );
                            }
                            if let Some(ref cat) = cat_name {
                                obj.insert(
                                    "discord_category_name".to_string(),
                                    serde_json::Value::String(cat.clone()),
                                );
                            }
                            if let Ok(json) = serde_json::to_string_pretty(&val) {
                                let _ = fs::write(&file_path, json);
                                updated += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    if updated > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ✓ Updated {updated} session(s) with channel/category info");
    }
}

/// Save session to file in the ai_sessions directory
fn save_session_to_file(session: &DiscordSession, current_path: &str) {
    let Some(ref session_id) = session.session_id else {
        return;
    };

    if session.history.is_empty() {
        return;
    }

    let Some(sessions_dir) = ai_screen::ai_sessions_dir() else {
        return;
    };

    if fs::create_dir_all(&sessions_dir).is_err() {
        return;
    }

    let saveable_history: Vec<HistoryItem> = session
        .history
        .iter()
        .filter(|item| !matches!(item.item_type, HistoryType::System))
        .cloned()
        .collect();

    if saveable_history.is_empty() {
        return;
    }

    let file_path = sessions_dir.join(format!("{}.json", session_id));

    if let Some(parent) = file_path.parent() {
        if parent != sessions_dir {
            return;
        }
    }

    // Preserve existing category/channel names from the file when in-memory values are None
    let (effective_channel_name, effective_category_name) =
        if session.channel_name.is_none() || session.category_name.is_none() {
            if let Ok(content) = fs::read_to_string(&file_path) {
                if let Ok(existing) = serde_json::from_str::<SessionData>(&content) {
                    (
                        session
                            .channel_name
                            .clone()
                            .or(existing.discord_channel_name),
                        session
                            .category_name
                            .clone()
                            .or(existing.discord_category_name),
                    )
                } else {
                    (session.channel_name.clone(), session.category_name.clone())
                }
            } else {
                (session.channel_name.clone(), session.category_name.clone())
            }
        } else {
            (session.channel_name.clone(), session.category_name.clone())
        };

    // Clean up old session files for the same Discord channel (different session_id)
    if let Ok(entries) = fs::read_dir(&sessions_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let fname = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if fname == session_id {
                    continue;
                } // keep current
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(old) = serde_json::from_str::<SessionData>(&content) {
                        let same_channel = match (session.channel_id, old.discord_channel_id) {
                            (Some(cid), Some(old_cid)) => cid == old_cid,
                            _ => old.discord_channel_name == effective_channel_name,
                        };
                        if same_channel {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }

    let session_data = SessionData {
        session_id: session_id.clone(),
        history: saveable_history,
        current_path: current_path.to_string(),
        created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        discord_channel_id: session.channel_id,
        discord_channel_name: effective_channel_name,
        discord_category_name: effective_category_name,
        remote_profile_name: session.remote_profile_name.clone(),
    };

    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let _ = fs::write(file_path, json);
    }
}
