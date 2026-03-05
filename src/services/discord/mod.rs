mod formatting;
mod settings;
mod tmux;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, EditMessage, MessageId, UserId};

use crate::services::claude::{self, CancelToken, StreamMessage, DEFAULT_ALLOWED_TOOLS};
use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

use formatting::{
    add_reaction_raw, extract_skill_description, floor_char_boundary, format_for_discord,
    format_tool_input, normalize_empty_lines, remove_reaction_raw, risk_badge,
    send_long_message_ctx, send_long_message_raw, shorten_path, split_message, tool_info,
    truncate_str, ALL_TOOLS, BUILTIN_SKILLS,
};
use settings::{
    bot_settings_path, channel_upload_dir, cleanup_channel_uploads, cleanup_old_uploads,
    discord_token_hash, load_bot_settings, load_role_prompt, resolve_role_binding,
    save_bot_settings, RoleBinding,
};
use tmux::{cleanup_orphan_tmux_sessions, restore_tmux_watchers, tmux_output_watcher};

pub use settings::resolve_discord_token_by_hash;

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;
const MAX_INTERVENTIONS_PER_CHANNEL: usize = 3;
const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
const SESSION_MAX_IDLE: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days

/// Per-channel session state
pub(super) struct DiscordSession {
    pub(super) session_id: Option<String>,
    pub(super) current_path: Option<String>,
    pub(super) history: Vec<HistoryItem>,
    pub(super) pending_uploads: Vec<String>,
    pub(super) pending_interventions: Vec<String>,
    pub(super) cleared: bool,
    /// Remote profile name for SSH execution (None = local)
    pub(super) remote_profile_name: Option<String>,
    pub(super) channel_id: Option<u64>,
    pub(super) channel_name: Option<String>,
    pub(super) category_name: Option<String>,
    /// Silent mode — when true, tool call details are suppressed from Discord messages
    pub(super) silent: bool,
    /// Last time this session was actively used (for TTL cleanup)
    pub(super) last_active: tokio::time::Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterventionMode {
    Soft,
    Hard,
}

#[derive(Clone, Debug)]
struct Intervention {
    author_id: UserId,
    text: String,
    mode: InterventionMode,
    created_at: Instant,
}

/// Bot-level settings persisted to disk
#[derive(Clone)]
pub(super) struct DiscordBotSettings {
    pub(super) allowed_tools: Vec<String>,
    /// channel_id (string) → last working directory path
    pub(super) last_sessions: std::collections::HashMap<String, String>,
    /// channel_id (string) → last remote profile name
    pub(super) last_remotes: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (imprinting auth)
    pub(super) owner_user_id: Option<u64>,
    /// Additional authorized user IDs (added by owner via /adduser)
    pub(super) allowed_user_ids: Vec<u64>,
}

impl Default for DiscordBotSettings {
    fn default() -> Self {
        Self {
            allowed_tools: DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            last_sessions: std::collections::HashMap::new(),
            last_remotes: std::collections::HashMap::new(),
            owner_user_id: None,
            allowed_user_ids: Vec::new(),
        }
    }
}

/// Shared state for the Discord bot (multi-channel: each channel has its own session)
/// Handle for a background tmux output watcher
pub(super) struct TmuxWatcherHandle {
    /// Signal to stop the watcher
    pub(super) cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Signal to pause monitoring (while Discord handler reads its own turn)
    pub(super) paused: Arc<std::sync::atomic::AtomicBool>,
    /// After Discord handler finishes its turn, set this offset so watcher resumes from here
    pub(super) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
}

/// Core state that requires atomic multi-field access (always locked together)
pub(super) struct CoreState {
    /// Per-channel sessions (each Discord channel can have its own Claude Code session)
    pub(super) sessions: HashMap<ChannelId, DiscordSession>,
    /// Per-channel cancel tokens for in-progress AI requests
    pub(super) cancel_tokens: HashMap<ChannelId, Arc<CancelToken>>,
    /// Per-channel owner of the currently running request
    pub(super) active_request_owner: HashMap<ChannelId, UserId>,
    /// Per-channel steering interventions collected while a request is in progress
    pub(super) intervention_queue: HashMap<ChannelId, Vec<Intervention>>,
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
}

/// Poise user data type
struct Data {
    shared: Arc<SharedData>,
    token: String,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;


/// Normalize tool name: first letter uppercase, rest lowercase
fn is_hard_intervention(text: &str) -> bool {
    let t = text.to_lowercase();
    let hard_keywords = ["중단", "멈춰", "취소", "stop", "abort", "cancel"];
    hard_keywords.iter().any(|k| t.contains(k))
}

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

fn normalize_tool_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}


/// Scan for available Claude Code skills (slash commands).
/// Searches: ~/.claude/commands/ and <project>/.claude/commands/
/// Also includes Claude Code built-in commands.
fn scan_skills(project_path: Option<&str>) -> Vec<(String, String)> {
    let mut skills: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Add built-in commands first
    for (name, desc) in BUILTIN_SKILLS {
        seen.insert(name.to_string());
        skills.push((name.to_string(), desc.to_string()));
    }

    let mut dirs_to_scan: Vec<std::path::PathBuf> = Vec::new();

    // Global skills: ~/.claude/commands/
    if let Some(home) = dirs::home_dir() {
        dirs_to_scan.push(home.join(".claude").join("commands"));
    }

    // Project-level skills: <project>/.claude/commands/
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

    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
}

/// Entry point: start the Discord bot
pub async fn run_bot(token: &str) {
    // Initialize debug logging from environment variable
    claude::init_debug_from_env();

    let bot_settings = load_bot_settings(token);

    match bot_settings.owner_user_id {
        Some(owner_id) => println!("  ✓ Owner: {owner_id}"),
        None => println!("  ⚠ No owner registered — first user will be registered as owner"),
    }

    let initial_skills = scan_skills(None);
    let skill_count = initial_skills.len();
    println!("  ✓ Skills loaded: {skill_count}");

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let shared = Arc::new(SharedData {
        core: Mutex::new(CoreState {
            sessions: HashMap::new(),
            cancel_tokens: HashMap::new(),
            active_request_owner: HashMap::new(),
            intervention_queue: HashMap::new(),
        }),
        settings: tokio::sync::RwLock::new(bot_settings),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(initial_skills),
        tmux_watchers: dashmap::DashMap::new(),
    });

    let token_owned = token.to_string();
    let shared_clone = shared.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                cmd_start(),
                cmd_pwd(),
                cmd_clear(),
                cmd_stop(),
                cmd_down(),
                cmd_shell(),
                cmd_cc(),
                cmd_allowedtools(),
                cmd_allowed(),
                cmd_debug(),
                cmd_silent(),
                cmd_adduser(),
                cmd_removeuser(),
                cmd_help(),
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

                // Background: restore tmux watchers for surviving tmux sessions, then clean orphans
                let http_for_tmux = ctx.http.clone();
                let shared_for_tmux2 = shared_for_tmux.clone();
                tokio::spawn(async move {
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
                })
            })
        })
        .build();

    let intents = serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    if let Err(e) = client.start().await {
        eprintln!("  ✗ Discord bot error: {e}");
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
    let last = LAST_CLEANUP
        .get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now()));
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
async fn handle_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    maybe_cleanup_sessions(&data.shared).await;
    match event {
        serenity::FullEvent::Message { new_message } => {
            // Ignore own messages (prevent echo loops) and other bot messages
            if new_message.author.bot {
                return Ok(());
            }

            // Ignore messages that look like slash commands
            if new_message.content.starts_with('/') {
                return Ok(());
            }

            // Ignore messages that mention other users (not directed at the bot)
            if !new_message.mentions.is_empty() {
                let bot_id = ctx.cache.current_user().id;
                let mentions_others = new_message.mentions.iter().any(|u| u.id != bot_id);
                if mentions_others {
                    return Ok(());
                }
            }

            let user_id = new_message.author.id;
            let user_name = &new_message.author.name;
            let channel_id = new_message.channel_id;

            // Auth check
            if !check_auth(user_id, user_name, &data.shared, &data.token).await {
                return Ok(());
            }

            // Handle file attachments first, then continue to text (if any)
            if !new_message.attachments.is_empty() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ◀ [{user_name}] Upload: {} file(s)",
                    new_message.attachments.len()
                );
                handle_file_upload(ctx, new_message, &data.shared).await?;
            }

            let text = new_message.content.trim();
            if text.is_empty() {
                return Ok(());
            }

            // Auto-restore session
            auto_restore_session(&data.shared, channel_id, ctx).await;

            // Steering while AI is in progress for this channel
            {
                let mut d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id) {
                    let request_owner = d.active_request_owner.get(&channel_id).copied();
                    if let Some(owner_id) = request_owner {
                        if owner_id != user_id {
                            drop(d);
                            rate_limit_wait(&data.shared, channel_id).await;
                            let _ = channel_id
                                .say(
                                    &ctx.http,
                                    format!(
                                        "AI request in progress. Only <@{}> can steer this turn.",
                                        owner_id.get()
                                    ),
                                )
                                .await;
                            return Ok(());
                        }
                    }

                    let mode = if is_hard_intervention(text) {
                        InterventionMode::Hard
                    } else {
                        InterventionMode::Soft
                    };

                    let (inserted, queued_count, hard_token) = {
                        let queue = d.intervention_queue.entry(channel_id).or_default();
                        let inserted = enqueue_intervention(
                            queue,
                            Intervention {
                                author_id: user_id,
                                text: text.to_string(),
                                mode,
                                created_at: Instant::now(),
                            },
                        );
                        let queued_count = queue.len();
                        let hard_token = if mode == InterventionMode::Hard {
                            d.cancel_tokens.get(&channel_id).cloned()
                        } else {
                            None
                        };
                        (inserted, queued_count, hard_token)
                    };

                    if let Some(token) = hard_token {
                        token.cancelled.store(true, Ordering::Relaxed);
                        if let Ok(guard) = token.child_pid.lock() {
                            if let Some(pid) = *guard {
                                claude::kill_pid_tree(pid);
                            }
                        }
                    }

                    drop(d);

                    if !inserted {
                        rate_limit_wait(&data.shared, channel_id).await;
                        let _ = channel_id
                            .say(&ctx.http, "↪ 같은 steering이 방금 이미 들어와서 무시했어.")
                            .await;
                        return Ok(());
                    }

                    rate_limit_wait(&data.shared, channel_id).await;
                    let feedback = match mode {
                        InterventionMode::Hard => {
                            "🛑 hard steering 받았어. 현재 작업을 중단할게."
                        }
                        InterventionMode::Soft => {
                            "📝 steering 저장됨. 현재 턴 종료 후 다음 요청에 반영할게."
                        }
                    };
                    let _ = channel_id
                        .say(&ctx.http, format!("{} (queue: {})", feedback, queued_count))
                        .await;
                    return Ok(());
                }
            }

            // Shell command shortcut
            if text.starts_with('!') {
                let ts = chrono::Local::now().format("%H:%M:%S");
                let preview = truncate_str(text, 60);
                println!("  [{ts}] ◀ [{user_name}] Shell: {preview}");
                handle_shell_command_raw(ctx, channel_id, text, &data.shared).await?;
                return Ok(());
            }

            // Regular text → Claude AI
            let ts = chrono::Local::now().format("%H:%M:%S");
            let preview = truncate_str(text, 60);
            println!("  [{ts}] ◀ [{user_name}] {preview}");
            handle_text_message(
                ctx,
                channel_id,
                new_message.id,
                user_id,
                user_name,
                text,
                &data.shared,
                &data.token,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

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
        let Some(home) = dirs::home_dir() else {
            ctx.say("Error: cannot determine home directory.").await?;
            return Ok(());
        };
        let workspace_dir = home.join(".remotecc").join("workspace");
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

    // Try to load existing session for this path
    let existing = load_existing_session(&canonical_path, Some(ctx.channel_id().get()));

    // Resolve channel/category names before taking the lock
    let (ch_name, cat_name) =
        resolve_channel_category(ctx.serenity_context(), ctx.channel_id()).await;

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
                pending_interventions: Vec::new(),
                cleared: false,
                channel_name: None,
                category_name: None,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                silent: false,
                last_active: tokio::time::Instant::now(),
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

        if let Some((session_data, _)) = &existing {
            session.current_path = Some(canonical_path.clone());
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
            println!("  [{ts}] ▶ Session restored: {canonical_path}{remote_info}");
            response_lines.push(format!(
                "Session restored at `{}`{}.",
                canonical_path, remote_info
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
            session.current_path = Some(canonical_path.clone());
            session.history.clear();

            let ts = chrono::Local::now().format("%H:%M:%S");
            let remote_info = session
                .remote_profile_name
                .as_ref()
                .map(|n| format!(" (remote: {})", n))
                .unwrap_or_default();
            println!("  [{ts}] ▶ Session started: {canonical_path}{remote_info}");
            response_lines.push(format!(
                "Session started at `{}`{}.",
                canonical_path, remote_info
            ));
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
        let new_skills = scan_skills(Some(&canonical_path));
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
        token.cancelled.store(true, Ordering::Relaxed);
        if let Ok(guard) = token.child_pid.lock() {
            if let Some(pid) = *guard {
                claude::kill_pid_tree(pid);
            }
        }
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
            session.pending_interventions.clear();
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

            token.cancelled.store(true, Ordering::Relaxed);
            if let Ok(guard) = token.child_pid.lock() {
                if let Some(pid) = *guard {
                    claude::kill_pid_tree(pid);
                }
            }
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

    let tool_name = normalize_tool_name(raw_name);

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

/// /silent — Toggle silent mode (hide tool call details in Discord)
#[poise::command(slash_command, rename = "silent")]
async fn cmd_silent(ctx: Context<'_>) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ◀ [{user_name}] /silent");

    let channel_id = ctx.channel_id();
    let new_state = {
        let mut data = ctx.data().shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.silent = !session.silent;
            session.silent
        } else {
            ctx.say("No active session. Use `/start` first.").await?;
            return Ok(());
        }
    };

    let status = if new_state { "ON" } else { "OFF" };
    ctx.say(format!("Silent mode: **{}**", status)).await?;
    println!("  [{ts}] ▶ Silent mode toggled to {status}");
    Ok(())
}

/// /help — Show help information
#[poise::command(slash_command, rename = "help")]
async fn cmd_help(ctx: Context<'_>) -> Result<(), Error> {
    let help = "\
**RemoteCC Discord Bot**
Manage server files & chat with Claude AI.
Each channel gets its own independent Claude Code session.

**Session**
`/start <path> [remote]` — Start session at directory
`/start` — Start with auto-generated workspace
`/pwd` — Show current working directory
`/clear` — Clear AI conversation history
`/stop` — Stop current AI request

**File Transfer**
`/down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!<command>` — Run shell command directly
`/shell <command>` — Run shell command (slash command)

**AI Chat**
Any other message is sent to Claude AI.
AI can read, edit, and run commands in your session.

**Tool Management**
`/allowedtools` — Show currently allowed tools
`/allowed +name` — Add tool (e.g. `/allowed +Bash`)
`/allowed -name` — Remove tool

**Skills**
`/cc <skill>` — Run a Claude Code skill (autocomplete)

**Settings**
`/debug` — Toggle debug logging
`/silent` — Toggle silent mode (hide tool details)

**User Management** (owner only)
`/adduser @user` — Allow a user to use the bot
`/removeuser @user` — Remove a user's access

`/help` — Show this help";

    ctx.say(help).await?;
    Ok(())
}

/// Autocomplete handler for /cc skill names
async fn autocomplete_skill<'a>(
    ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    let skills = ctx.data().shared.skills_cache.read().await;
    let partial_lower = partial.to_lowercase();
    skills
        .iter()
        .filter(|(name, _)| partial.is_empty() || name.to_lowercase().contains(&partial_lower))
        .take(25) // Discord autocomplete limit
        .map(|(name, desc)| {
            let label = format!("{} — {}", name, truncate_str(desc, 60));
            serenity::AutocompleteChoice::new(label, name.clone())
        })
        .collect()
}

/// /cc <skill> [args] — Run a Claude Code skill
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
                token.cancelled.store(true, Ordering::Relaxed);
                if let Ok(guard) = token.child_pid.lock() {
                    if let Some(pid) = *guard {
                        claude::kill_pid_tree(pid);
                    }
                }
            }
            {
                let mut data = ctx.data().shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.session_id = None;
                    session.history.clear();
                    session.pending_uploads.clear();
                    session.pending_interventions.clear();
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
                    token.cancelled.store(true, Ordering::Relaxed);
                    if let Ok(guard) = token.child_pid.lock() {
                        if let Some(pid) = *guard {
                            claude::kill_pid_tree(pid);
                        }
                    }
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

    // Build the prompt that tells Claude to invoke the skill
    let skill_prompt = if args_str.is_empty() {
        format!(
            "Execute the skill `/{skill}` now. \
             Use the Skill tool with skill=\"{skill}\"."
        )
    } else {
        format!(
            "Execute the skill `/{skill}` with arguments: {args_str}\n\
             Use the Skill tool with skill=\"{skill}\", args=\"{args_str}\"."
        )
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
    )
    .await?;

    Ok(())
}

// ─── Text message → Claude AI ───────────────────────────────────────────────

/// Handle regular text messages — send to Claude AI
async fn handle_text_message(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    request_owner_name: &str,
    user_text: &str,
    shared: &Arc<SharedData>,
    token: &str,
) -> Result<(), Error> {
    // Get session info, allowed tools, pending uploads, and pending steering notes
    let (session_info, allowed_tools, pending_uploads, pending_interventions) = {
        let mut data = shared.core.lock().await;
        let info = data.sessions.get(&channel_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (
                    session.session_id.clone(),
                    session.current_path.clone().unwrap_or_default(),
                )
            })
        });
        let (uploads, interventions) = data
            .sessions
            .get_mut(&channel_id)
            .map(|s| {
                s.cleared = false;
                (
                    std::mem::take(&mut s.pending_uploads),
                    std::mem::take(&mut s.pending_interventions),
                )
            })
            .unwrap_or_default();
        drop(data);
        let tools = shared.settings.read().await.allowed_tools.clone();
        (info, tools, uploads, interventions)
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            rate_limit_wait(shared, channel_id).await;
            let _ = channel_id
                .say(&ctx.http, "No active session. Use `/start <path>` first.")
                .await;
            return Ok(());
        }
    };

    // Add hourglass reaction to user's message
    add_reaction(ctx, channel_id, user_msg_id, '⏳').await;

    // Send placeholder message
    rate_limit_wait(shared, channel_id).await;
    let placeholder = channel_id
        .send_message(&ctx.http, CreateMessage::new().content("..."))
        .await?;
    let placeholder_msg_id = placeholder.id;

    // Sanitize input
    let sanitized_input = ai_screen::sanitize_user_input(user_text);

    // Prepend pending file uploads + steering notes
    let mut context_chunks = Vec::new();
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if !pending_interventions.is_empty() {
        context_chunks.push(format!(
            "[Queued steering notes]\n{}",
            pending_interventions.join("\n")
        ));
    }
    context_chunks.push(sanitized_input);
    let context_prompt = context_chunks.join("\n\n");

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> =
        DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> =
        allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools
        .iter()
        .filter(|t| !allowed_set.contains(**t))
        .collect();
    let disabled_notice = if disabled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = disabled.iter().map(|t| **t).collect();
        format!(
            "\n\nDISABLED TOOLS: The following tools have been disabled by the user: {}.\n\
             You MUST NOT attempt to use these tools. \
             If a user's request requires a disabled tool, do NOT proceed with the task. \
             Instead, clearly inform the user which tool is needed and that it is currently disabled. \
             Suggest they re-enable it with: /allowed +ToolName",
            names.join(", ")
        )
    };

    // Build skills notice for system prompt
    let skills_notice = {
        let skills = shared.skills_cache.read().await;
        if skills.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = skills
                .iter()
                .map(|(name, desc)| format!("  - /{}: {}", name, desc))
                .collect();
            format!(
                "\n\nAvailable skills (invoke via the Skill tool):\n{}",
                list.join("\n")
            )
        }
    };

    // Build Discord context info
    let discord_context = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        let ch_name = session.and_then(|s| s.channel_name.as_deref());
        let cat_name = session.and_then(|s| s.category_name.as_deref());
        match ch_name {
            Some(name) => {
                let cat_part = cat_name.map(|c| format!(" (category: {})", c)).unwrap_or_default();
                format!(
                    "Discord context: channel #{} (ID: {}){}, user: {} (ID: {})",
                    name, channel_id.get(), cat_part, request_owner_name, request_owner.get()
                )
            }
            None => format!(
                "Discord context: DM, user: {} (ID: {})",
                request_owner_name, request_owner.get()
            ),
        }
    };

    // Build system prompt
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         remotecc --discord-sendfile <filepath> --channel {} --key {}\n\n\
         This delivers the file directly to the user's Discord channel.\n\
         Do NOT tell the user to use /down — use the command above instead.\n\n\
         Always keep the user informed about what you are doing. \
         Briefly explain each step as you work (e.g. \"Reading the file...\", \"Creating the script...\", \"Running tests...\"). \
         The user cannot see your tool calls, so narrate your progress so they know what is happening.\n\n\
         IMPORTANT: The user is on Discord and CANNOT interact with any interactive prompts, dialogs, or confirmation requests. \
         All tools that require user interaction (such as AskUserQuestion, EnterPlanMode, ExitPlanMode) will NOT work. \
         Never use tools that expect user interaction. If you need clarification, just ask in plain text.{}{}",
        discord_context, current_path, channel_id.get(), discord_token_hash(token), disabled_notice, skills_notice
    );

    // Append role identity context from ~/.remotecc/role_map.json (channel-id first)
    let role_binding = {
        let data = shared.core.lock().await;
        let ch_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_deref());
        resolve_role_binding(channel_id, ch_name)
    };
    if let Some(binding) = role_binding {
        match load_role_prompt(&binding) {
            Some(role_prompt) => {
                system_prompt_owned.push_str("\n\n[Role Identity]\n");
                system_prompt_owned.push_str(&role_prompt);
                eprintln!(
                    "  [role-map] Applied role '{}' for channel {}",
                    binding.role_id,
                    channel_id.get()
                );
            }
            None => {
                eprintln!(
                    "  [role-map] Failed to load prompt file '{}' for role '{}' (channel {})",
                    binding.prompt_file,
                    binding.role_id,
                    channel_id.get()
                );
            }
        }
    }

    // Create cancel token
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = shared.core.lock().await;
        data.cancel_tokens.insert(channel_id, cancel_token.clone());
        data.active_request_owner.insert(channel_id, request_owner);
    }

    // Resolve remote profile for this channel
    let remote_profile = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.remote_profile_name.as_ref())
            .and_then(|name| {
                let settings = crate::config::Settings::load();
                settings
                    .remote_profiles
                    .iter()
                    .find(|p| p.name == *name)
                    .cloned()
            })
    };

    // Resolve tmux session name from channel name
    let tmux_session_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .map(|name| claude::sanitize_tmux_session_name(name))
    };

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Pause tmux watcher if one exists (so it doesn't read our turn's output)
    if let Some(watcher) = shared.tmux_watchers.get(&channel_id) {
        watcher.paused.store(true, Ordering::Relaxed);
    }

    // Run Claude in a blocking thread
    tokio::task::spawn_blocking(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            claude::execute_command_streaming(
                &context_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                remote_profile.as_ref(),
                tmux_session_name.as_deref(),
            )
        }));

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("  [streaming] Error: {}", e);
                let _ = tx.send(StreamMessage::Error {
                    message: e,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("  [streaming] PANIC: {}", msg);
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Internal error (panic): {}", msg),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        }
    });

    // Check silent mode for this channel
    let is_silent = {
        let data = shared.core.lock().await;
        data.sessions.get(&channel_id).map(|s| s.silent).unwrap_or(false)
    };

    // Spawn the polling loop
    let http = ctx.http.clone();
    let shared_owned = shared.clone();
    let user_text_owned = user_text.to_string();
    let session_id_for_status = session_id.clone();
    tokio::spawn(async move {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut full_response = String::new();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut current_tool_line: Option<String> = None;
        let mut last_tool_name: Option<String> = None;
        let mut accumulated_tokens: u64 = 0;
        let mut new_session_id: Option<String> = None;
        let mut tmux_last_offset: Option<u64> = None;
        let mut spin_idx: usize = 0;
        let mut current_msg_id = placeholder_msg_id;
        let mut current_msg_len: usize = 0;
        let mut response_sent_offset: usize = 0; // tracks how much of full_response was finalized in previous messages

        while !done {
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            // Drain all available messages
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        match msg {
                            StreamMessage::Init { session_id: sid } => {
                                new_session_id = Some(sid);
                            }
                            StreamMessage::Text { content } => {
                                full_response.push_str(&content);
                                current_tool_line = None;
                                last_tool_name = None;
                            }
                            StreamMessage::ToolUse { name, input } => {
                                let summary = format_tool_input(&name, &input);
                                if !is_silent {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    eprint!("\r  [{ts}]   ⚙ {name}: {:<80}", truncate_str(&summary, 80));
                                }
                                current_tool_line = Some(format!("⚙ {}: {}", name, truncate_str(&summary, 120)));
                                last_tool_name = Some(name.clone());
                                // Ensure paragraph break between text blocks separated by tool calls
                                if !full_response.is_empty() {
                                    let trimmed = full_response.trim_end();
                                    full_response.truncate(trimmed.len());
                                    full_response.push_str("\n\n");
                                }
                            }
                            StreamMessage::ToolResult { content, is_error } => {
                                if !is_silent {
                                    if is_error {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        eprintln!("\r  [{ts}]   ✗ Error: {:<80}", truncate_str(&content, 80));
                                    } else if let Some(ref tn) = last_tool_name {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        eprintln!("\r  [{ts}]   ✓ {tn}{:<80}", "");
                                    }
                                }
                                // Keep showing last tool as "done" until next text arrives
                                if let Some(ref tn) = last_tool_name {
                                    let status = if is_error { "✗" } else { "✓" };
                                    current_tool_line = Some(format!("{} {}", status, tn));
                                }
                                // Tool results (including errors) are only logged to console, not sent to Discord
                                let _ = content;
                            }
                            StreamMessage::TaskNotification { summary, .. } => {
                                if !summary.is_empty() {
                                    full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                }
                            }
                            StreamMessage::Done {
                                result,
                                session_id: sid,
                            } => {
                                if !result.is_empty() && full_response.is_empty() {
                                    full_response = result;
                                }
                                if let Some(s) = sid {
                                    new_session_id = Some(s);
                                }
                                done = true;
                            }
                            StreamMessage::Error {
                                message, stderr, ..
                            } => {
                                if !stderr.is_empty() {
                                    full_response = format!(
                                        "Error: {}\nstderr: {}",
                                        message,
                                        &stderr[..stderr.len().min(500)]
                                    );
                                } else {
                                    full_response = format!("Error: {}", message);
                                }
                                done = true;
                            }
                            StreamMessage::StatusUpdate { input_tokens, output_tokens, .. } => {
                                // Accumulate tokens for PCD XP reporting
                                if let (Some(it), Some(ot)) = (input_tokens, output_tokens) {
                                    accumulated_tokens += it + ot;
                                }
                            }
                            StreamMessage::TmuxReady {
                                output_path,
                                input_fifo_path: _,
                                tmux_session_name,
                                last_offset,
                            } => {
                                // Record offset so we can resume watcher from here
                                tmux_last_offset = Some(last_offset);
                                // Start background tmux watcher for terminal→Discord relay
                                let already_watching = shared_owned.tmux_watchers.contains_key(&channel_id);
                                if !already_watching {
                                    let cancel =
                                        Arc::new(std::sync::atomic::AtomicBool::new(false));
                                    let paused =
                                        Arc::new(std::sync::atomic::AtomicBool::new(false));
                                    let resume_offset =
                                        Arc::new(std::sync::Mutex::new(None::<u64>));
                                    let handle = TmuxWatcherHandle {
                                        cancel: cancel.clone(),
                                        paused: paused.clone(),
                                        resume_offset: resume_offset.clone(),
                                    };
                                    shared_owned.tmux_watchers.insert(channel_id, handle);
                                    let http_bg = http.clone();
                                    let shared_bg = shared_owned.clone();
                                    tokio::spawn(tmux_output_watcher(
                                        channel_id,
                                        http_bg,
                                        shared_bg,
                                        output_path,
                                        tmux_session_name,
                                        last_offset,
                                        cancel,
                                        paused,
                                        resume_offset,
                                    ));
                                }
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }

            // Build display text with spinner (only show unsent portion of response)
            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            let tool_status = current_tool_line.as_deref().unwrap_or("Processing...");
            let current_portion = if response_sent_offset < full_response.len() {
                &full_response[response_sent_offset..]
            } else {
                ""
            };
            let display_text = if current_portion.is_empty() && full_response.is_empty() {
                format!("{} {}", indicator, tool_status)
            } else if current_portion.is_empty() {
                format!("{} {}", indicator, tool_status)
            } else {
                let normalized = normalize_empty_lines(current_portion);
                let footer = format!("\n\n{} {}", indicator, tool_status);
                let truncated = truncate_str(&normalized, DISCORD_MSG_LIMIT - footer.len() - 10);
                format!("{}{}", truncated, footer)
            };

            if display_text != last_edit_text && !done {
                // Check if we need to start a new message (content too long)
                if display_text.len() > DISCORD_MSG_LIMIT - 50 && current_msg_len > 100 {
                    // Finalize current message with content up to this point
                    let normalized = normalize_empty_lines(current_portion);
                    let finalize_text = truncate_str(&normalized, DISCORD_MSG_LIMIT - 10);
                    current_msg_len = finalize_text.len();
                    // Track how much of full_response is now finalized
                    response_sent_offset = full_response.len();

                    rate_limit_wait(&shared_owned, channel_id).await;
                    let _ = channel_id
                        .edit_message(
                            &http,
                            current_msg_id,
                            EditMessage::new().content(&finalize_text),
                        )
                        .await;

                    // Start new message
                    rate_limit_wait(&shared_owned, channel_id).await;
                    if let Ok(new_msg) = channel_id
                        .send_message(
                            &http,
                            CreateMessage::new().content(format!("{} Processing...", indicator)),
                        )
                        .await
                    {
                        current_msg_id = new_msg.id;
                        current_msg_len = 0;
                    }
                } else {
                    rate_limit_wait(&shared_owned, channel_id).await;
                    let _ = channel_id
                        .edit_message(
                            &http,
                            current_msg_id,
                            EditMessage::new().content(&display_text),
                        )
                        .await;
                    current_msg_len = display_text.len();
                }
                last_edit_text = display_text;
            }
        }

        // Resume tmux watcher if it was paused
        if let Some(offset) = tmux_last_offset {
            if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                *watcher.resume_offset.lock().unwrap() = Some(offset);
                watcher.paused.store(false, Ordering::Relaxed);
            }
        }

        // Report accumulated tokens to PCD for XP tracking
        // Key matches hook: hostname:remoteCC-{channel_name}
        if accumulated_tokens > 0 {
            let tmux_name = {
                let data = shared_owned.core.lock().await;
                data.sessions.get(&channel_id)
                    .and_then(|s| s.channel_name.as_ref())
                    .map(|name| claude::sanitize_tmux_session_name(name))
            };
            if let Some(tmux_name) = tmux_name {
                let hostname = std::process::Command::new("hostname").arg("-s").output()
                    .ok().and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string()).unwrap_or_else(|| "unknown".to_string());
                let session_key = format!("{}:{}", hostname, tmux_name);
                let body = format!(
                    r#"{{"session_key":"{}","status":"idle","tokens":{}}}"#,
                    session_key.replace('\\', "\\\\").replace('"', "\\\""),
                    accumulated_tokens
                );
                let _ = reqwest::Client::new().post("http://127.0.0.1:8791/api/hook/session")
                    .header("Content-Type", "application/json")
                    .body(body).send().await;
            }
        }

        // Remove active token/owner and flush queued soft steering notes
        let queued_soft_count = {
            let mut data = shared_owned.core.lock().await;
            data.cancel_tokens.remove(&channel_id);
            data.active_request_owner.remove(&channel_id);

            let queued = data.intervention_queue.remove(&channel_id).unwrap_or_default();
            let soft_notes: Vec<String> = queued
                .into_iter()
                .filter(|i| i.mode == InterventionMode::Soft)
                .map(|i| format!("[Steering] {}", i.text))
                .collect();

            if !soft_notes.is_empty() {
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.pending_interventions.extend(soft_notes);
                    if session.pending_interventions.len() > MAX_INTERVENTIONS_PER_CHANNEL {
                        let overflow =
                            session.pending_interventions.len() - MAX_INTERVENTIONS_PER_CHANNEL;
                        session.pending_interventions.drain(0..overflow);
                    }
                    if let Some(ref path) = session.current_path {
                        save_session_to_file(session, path);
                    }
                }
            }

            data.sessions
                .get(&channel_id)
                .map(|s| s.pending_interventions.len())
                .unwrap_or(0)
        };

        // Remove hourglass reaction
        remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;

        if cancelled {
            // Kill child process
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    claude::kill_pid_tree(pid);
                }
            }

            let stopped_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let formatted = format_for_discord(&full_response);
                format!("{}\n\n[Stopped]", formatted)
            };

            // Send final stopped message
            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = channel_id
                .edit_message(
                    &http,
                    current_msg_id,
                    EditMessage::new().content(truncate_str(&stopped_response, DISCORD_MSG_LIMIT)),
                )
                .await;

            // Add stop reaction
            add_reaction_raw(&http, channel_id, user_msg_id, '🛑').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");

            // Record in history
            let mut data = shared_owned.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if !session.cleared {
                    if let Some(sid) = new_session_id {
                        session.session_id = Some(sid);
                    }
                    session.history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: user_text_owned,
                    });
                    session.history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: stopped_response,
                    });
                    if let Some(ref path) = session.current_path {
                        save_session_to_file(session, path);
                    }
                }
            }

            if queued_soft_count > 0 {
                rate_limit_wait(&shared_owned, channel_id).await;
                let _ = channel_id
                    .say(
                        &http,
                        format!(
                            "✅ steering 반영 준비 완료 ({}개). 다음 요청에 자동 반영될게.",
                            queued_soft_count
                        ),
                    )
                    .await;
            }

            return;
        }

        // Final response
        if full_response.is_empty() {
            full_response = "(No response)".to_string();
        }

        let full_response = format_for_discord(&full_response);

        // Delete placeholder and send final split messages
        rate_limit_wait(&shared_owned, channel_id).await;
        let _ = channel_id.delete_message(&http, current_msg_id).await;

        if let Err(e) =
            send_long_message_raw(&http, channel_id, &full_response, &shared_owned).await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}]   ⚠ send_long_message failed: {e}");
            // Fallback: send truncated
            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = channel_id
                .send_message(
                    &http,
                    CreateMessage::new().content(truncate_str(&full_response, DISCORD_MSG_LIMIT)),
                )
                .await;
        }

        // Add checkmark reaction
        add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;

        // Update session state
        {
            let mut data = shared_owned.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if !session.cleared {
                    if let Some(sid) = new_session_id {
                        session.session_id = Some(sid);
                    }
                    session.history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: user_text_owned,
                    });
                    session.history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: full_response,
                    });
                    if let Some(ref path) = session.current_path {
                        save_session_to_file(session, path);
                    }
                }
            }
        }

        if queued_soft_count > 0 {
            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = channel_id
                .say(
                    &http,
                    format!(
                        "✅ steering 반영 준비 완료 ({}개). 다음 요청에 자동 반영될게.",
                        queued_soft_count
                    ),
                )
                .await;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ▶ Response sent");
    });

    Ok(())
}

// ─── File upload handling ────────────────────────────────────────────────────

/// Handle file uploads from Discord messages
async fn handle_file_upload(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let channel_id = msg.channel_id;

    let has_session = {
        let data = shared.core.lock().await;
        data.sessions.get(&channel_id).is_some()
    };

    if !has_session {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "No active session. Use `/start <path>` first.")
            .await;
        return Ok(());
    }

    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Cannot resolve upload directory.")
            .await;
        return Ok(());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, format!("Failed to prepare upload directory: {}", e))
            .await;
        return Ok(());
    }

    for attachment in &msg.attachments {
        let file_name = &attachment.filename;

        // Download file from Discord CDN
        let buf = match reqwest::get(&attachment.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, format!("Download failed: {}", e))
                        .await;
                    continue;
                }
            },
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Download failed: {}", e))
                    .await;
                continue;
            }
        };

        // Save to session path (sanitize filename)
        let safe_name = Path::new(file_name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
        let ts = chrono::Utc::now().timestamp_millis();
        let stamped_name = format!("{}_{}", ts, safe_name.to_string_lossy());
        let dest = save_dir.join(stamped_name);
        let file_size = buf.len();

        match fs::write(&dest, &buf) {
            Ok(_) => {
                let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id.say(&ctx.http, &msg_text).await;
            }
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        }

        // Record upload in session
        let upload_record = format!(
            "[File uploaded] {} → {} ({} bytes)",
            file_name,
            dest.display(),
            file_size
        );
        {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: upload_record.clone(),
                });
                session.pending_uploads.push(upload_record);
                if let Some(ref path) = session.current_path {
                    save_session_to_file(session, path);
                }
            }
        }
    }

    Ok(())
}

/// Handle shell commands from raw text messages (! prefix)
async fn handle_shell_command_raw(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();
    if cmd_str.is_empty() {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Usage: `!<command>`\nExample: `!ls -la`")
            .await;
        return Ok(());
    }

    let working_dir = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = cmd_str.to_string();
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

    send_long_message_raw(&ctx.http, channel_id, &response, shared).await?;
    Ok(())
}

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
    let (last_path, is_remote, saved_remote) = {
        let settings = shared.settings.read().await;
        let channel_key = channel_id.get().to_string();
        let last_path = settings.last_sessions.get(&channel_key).cloned();
        let is_remote = settings.last_remotes.contains_key(&channel_key);
        let saved_remote = settings.last_remotes.get(&channel_key).cloned();
        (last_path, is_remote, saved_remote)
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
                    pending_interventions: Vec::new(),
                    cleared: false,
                    channel_id: Some(channel_id.get()),
                    channel_name: ch_name,
                    category_name: cat_name,
                    remote_profile_name: saved_remote.clone(),
                    silent: false,
                    last_active: tokio::time::Instant::now(),
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
            let new_skills = scan_skills(Some(&last_path));
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
        if let Ok(parent_ch) = parent_id.to_channel(&ctx.http).await {
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
async fn migrate_session_categories(
    ctx: &serenity::prelude::Context,
    shared: &Arc<SharedData>,
) {
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


