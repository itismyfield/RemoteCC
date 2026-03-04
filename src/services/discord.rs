use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, EditMessage, MessageId, UserId};

use crate::services::claude::{self, CancelToken, StreamMessage, DEFAULT_ALLOWED_TOOLS};
use crate::ui::ai_screen::{self, HistoryItem, HistoryType, SessionData};

/// Discord message length limit
const DISCORD_MSG_LIMIT: usize = 2000;
const MAX_INTERVENTIONS_PER_CHANNEL: usize = 3;
const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// Per-channel session state
struct DiscordSession {
    session_id: Option<String>,
    current_path: Option<String>,
    history: Vec<HistoryItem>,
    pending_uploads: Vec<String>,
    pending_interventions: Vec<String>,
    cleared: bool,
    /// Remote profile name for SSH execution (None = local)
    remote_profile_name: Option<String>,
    channel_name: Option<String>,
    category_name: Option<String>,
    /// Silent mode — when true, tool call details are suppressed from Discord messages
    silent: bool,
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
struct DiscordBotSettings {
    allowed_tools: Vec<String>,
    /// channel_id (string) → last working directory path
    last_sessions: std::collections::HashMap<String, String>,
    /// channel_id (string) → last remote profile name
    last_remotes: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (imprinting auth)
    owner_user_id: Option<u64>,
    /// Additional authorized user IDs (added by owner via /adduser)
    allowed_user_ids: Vec<u64>,
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
struct TmuxWatcherHandle {
    /// Signal to stop the watcher
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Signal to pause monitoring (while Discord handler reads its own turn)
    paused: Arc<std::sync::atomic::AtomicBool>,
    /// After Discord handler finishes its turn, set this offset so watcher resumes from here
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
}

struct SharedData {
    /// Per-channel sessions (each Discord channel can have its own Claude Code session)
    sessions: HashMap<ChannelId, DiscordSession>,
    /// Bot settings
    settings: DiscordBotSettings,
    /// Per-channel cancel tokens for in-progress AI requests
    cancel_tokens: HashMap<ChannelId, Arc<CancelToken>>,
    /// Per-channel owner of the currently running request
    active_request_owner: HashMap<ChannelId, UserId>,
    /// Per-channel steering interventions collected while a request is in progress
    intervention_queue: HashMap<ChannelId, Vec<Intervention>>,
    /// Per-channel timestamps of the last Discord API call (for rate limiting)
    api_timestamps: HashMap<ChannelId, tokio::time::Instant>,
    /// Cached skill list: (name, description)
    skills_cache: Vec<(String, String)>,
    /// Per-channel tmux output watchers for terminal→Discord relay
    tmux_watchers: HashMap<ChannelId, TmuxWatcherHandle>,
}

/// Poise user data type
struct Data {
    shared: Arc<Mutex<SharedData>>,
    token: String,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

/// Compute a short hash key from the bot token (first 16 chars of SHA-256 hex)
/// Uses "discord_" prefix to namespace Discord bot entries in settings.
fn discord_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    format!("discord_{}", hex::encode(&result[..8]))
}

/// Path to bot settings file: ~/.remotecc/bot_settings.json
fn bot_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".remotecc").join("bot_settings.json"))
}

fn discord_uploads_root() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".openclaw").join("remotecc_uploads").join("discord"))
}

fn channel_upload_dir(channel_id: ChannelId) -> Option<std::path::PathBuf> {
    discord_uploads_root().map(|p| p.join(channel_id.get().to_string()))
}

fn cleanup_old_uploads(max_age: Duration) {
    let Some(root) = discord_uploads_root() else {
        return;
    };
    if !root.exists() {
        return;
    }

    let now = SystemTime::now();
    let Ok(channels) = fs::read_dir(&root) else {
        return;
    };

    for ch in channels.filter_map(|e| e.ok()) {
        let ch_path = ch.path();
        if !ch_path.is_dir() {
            continue;
        }

        let Ok(files) = fs::read_dir(&ch_path) else {
            continue;
        };

        for f in files.filter_map(|e| e.ok()) {
            let f_path = f.path();
            if !f_path.is_file() {
                continue;
            }

            let should_delete = fs::metadata(&f_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);

            if should_delete {
                let _ = fs::remove_file(&f_path);
            }
        }

        // Remove empty channel dir
        if fs::read_dir(&ch_path)
            .ok()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&ch_path);
        }
    }
}

fn cleanup_channel_uploads(channel_id: ChannelId) {
    if let Some(dir) = channel_upload_dir(channel_id) {
        let _ = fs::remove_dir_all(dir);
    }
}

/// Load Discord bot settings from bot_settings.json
fn load_bot_settings(token: &str) -> DiscordBotSettings {
    let Some(path) = bot_settings_path() else {
        return DiscordBotSettings::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return DiscordBotSettings::default();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return DiscordBotSettings::default();
    };
    let key = discord_token_hash(token);
    let Some(entry) = json.get(&key) else {
        return DiscordBotSettings::default();
    };
    let owner_user_id = entry.get("owner_user_id").and_then(|v| v.as_u64());
    let Some(tools_arr) = entry.get("allowed_tools").and_then(|v| v.as_array()) else {
        return DiscordBotSettings {
            owner_user_id,
            ..DiscordBotSettings::default()
        };
    };
    let tools: Vec<String> = tools_arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if tools.is_empty() {
        return DiscordBotSettings {
            owner_user_id,
            ..DiscordBotSettings::default()
        };
    }
    let last_sessions = entry
        .get("last_sessions")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let last_remotes = entry
        .get("last_remotes")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let allowed_user_ids = entry
        .get("allowed_user_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default();
    DiscordBotSettings {
        allowed_tools: tools,
        last_sessions,
        last_remotes,
        owner_user_id,
        allowed_user_ids,
    }
}

/// Save Discord bot settings to bot_settings.json
fn save_bot_settings(token: &str, settings: &DiscordBotSettings) {
    let Some(path) = bot_settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut json: serde_json::Value = if let Ok(content) = fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    let key = discord_token_hash(token);
    let mut entry = serde_json::json!({
        "token": token,
        "allowed_tools": settings.allowed_tools,
        "last_sessions": settings.last_sessions,
        "last_remotes": settings.last_remotes,
        "allowed_user_ids": settings.allowed_user_ids,
    });
    if let Some(owner_id) = settings.owner_user_id {
        entry["owner_user_id"] = serde_json::json!(owner_id);
    }
    json[key] = entry;
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let _ = fs::write(&path, s);
    }
}

/// Resolve a Discord bot token from its hash by searching bot_settings.json
pub fn resolve_discord_token_by_hash(hash: &str) -> Option<String> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = json.as_object()?;
    let entry = obj.get(hash)?;
    entry
        .get("token")
        .and_then(|v| v.as_str())
        .map(String::from)
}

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

/// All available tools with (name, description, is_destructive)
const ALL_TOOLS: &[(&str, &str, bool)] = &[
    ("Bash", "Execute shell commands", true),
    ("Read", "Read file contents from the filesystem", false),
    ("Edit", "Perform find-and-replace edits in files", true),
    ("Write", "Create or overwrite files", true),
    ("Glob", "Find files by name pattern", false),
    ("Grep", "Search file contents with regex", false),
    (
        "Task",
        "Launch autonomous sub-agents for complex tasks",
        true,
    ),
    ("TaskOutput", "Retrieve output from background tasks", false),
    ("TaskStop", "Stop a running background task", false),
    ("WebFetch", "Fetch and process web page content", true),
    (
        "WebSearch",
        "Search the web for up-to-date information",
        true,
    ),
    ("NotebookEdit", "Edit Jupyter notebook cells", true),
    ("Skill", "Invoke slash-command skills", false),
    (
        "TaskCreate",
        "Create a structured task in the task list",
        false,
    ),
    ("TaskGet", "Retrieve task details by ID", false),
    ("TaskUpdate", "Update task status or details", false),
    ("TaskList", "List all tasks and their status", false),
    (
        "AskUserQuestion",
        "Ask the user a question (interactive)",
        false,
    ),
    ("EnterPlanMode", "Enter planning mode (interactive)", false),
    ("ExitPlanMode", "Exit planning mode (interactive)", false),
];

/// Tool info: (description, is_destructive)
fn tool_info(name: &str) -> (&'static str, bool) {
    ALL_TOOLS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, desc, destr)| (*desc, *destr))
        .unwrap_or(("Custom tool", false))
}

/// Format a risk badge for display
fn risk_badge(destructive: bool) -> &'static str {
    if destructive {
        "⚠️"
    } else {
        ""
    }
}

/// Claude Code built-in slash commands
const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("clear", "Clear conversation context and start fresh"),
    ("compact", "Compact conversation to reduce context"),
    ("context", "Visualize current context usage"),
    ("cost", "Show token usage and cost for this session"),
    ("diff", "View uncommitted changes and per-turn diffs"),
    ("doctor", "Check Claude Code health and configuration"),
    ("export", "Export conversation to file"),
    ("fast", "Toggle fast output mode"),
    ("files", "List all files currently in context"),
    ("fork", "Create a fork of the current conversation"),
    ("init", "Initialize project with CLAUDE.md guide"),
    ("memory", "Edit CLAUDE.md memory files"),
    ("model", "Switch AI model"),
    ("permissions", "View and manage tool permissions"),
    ("plan", "Enable plan mode or view current plan"),
    ("pr-comments", "View PR comments for current branch"),
    ("rename", "Rename the current conversation"),
    ("review", "Code review for uncommitted changes"),
    ("skills", "List available skills"),
    ("stats", "Show usage statistics"),
    ("status", "Show session status and git info"),
    ("todos", "List current todo items"),
    ("usage", "Show plan usage limits"),
];

/// Extract a description from a skill .md file.
/// Priority: 1) frontmatter `description:` field  2) first meaningful text line
fn extract_skill_description(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Check for YAML frontmatter (starts with ---)
    if lines.first().map(|l| l.trim()) == Some("---") {
        // Find closing ---
        for (i, line) in lines.iter().enumerate().skip(1) {
            let trimmed = line.trim();
            if trimmed == "---" {
                // Look for description: inside frontmatter
                for fm_line in &lines[1..i] {
                    let fm_trimmed = fm_line.trim();
                    if let Some(desc) = fm_trimmed.strip_prefix("description:") {
                        let desc = desc.trim();
                        if !desc.is_empty() {
                            return desc.chars().take(80).collect();
                        }
                    }
                }
                // No description in frontmatter, use first line after frontmatter
                for after_line in &lines[(i + 1)..] {
                    let t = after_line.trim().trim_start_matches('#').trim();
                    if !t.is_empty() {
                        return t.chars().take(80).collect();
                    }
                }
                break;
            }
        }
    }

    // No frontmatter: skip heading lines like "# 역할", use first non-heading meaningful line
    let mut found_heading = false;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            found_heading = true;
            continue;
        }
        // Use this line as description
        return trimmed.chars().take(80).collect();
    }

    // Fallback: if only heading exists, use heading text
    if found_heading {
        for line in &lines {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                let t = trimmed.trim_start_matches('#').trim();
                if !t.is_empty() {
                    return t.chars().take(80).collect();
                }
            }
        }
    }

    "Custom skill".to_string()
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

    let shared = Arc::new(Mutex::new(SharedData {
        sessions: HashMap::new(),
        settings: bot_settings,
        cancel_tokens: HashMap::new(),
        active_request_owner: HashMap::new(),
        intervention_queue: HashMap::new(),
        api_timestamps: HashMap::new(),
        skills_cache: initial_skills,
        tmux_watchers: HashMap::new(),
    }));

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

                // Background: restore tmux watchers for surviving tmux sessions
                let http_for_tmux = ctx.http.clone();
                let shared_for_tmux2 = shared_for_tmux.clone();
                tokio::spawn(async move {
                    restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
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
    shared: &Arc<Mutex<SharedData>>,
    token: &str,
) -> bool {
    let mut data = shared.lock().await;
    match data.settings.owner_user_id {
        None => {
            // Imprint: register first user as owner
            data.settings.owner_user_id = Some(user_id.get());
            save_bot_settings(token, &data.settings);
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ★ Owner registered: {user_name} (id:{})",
                user_id.get()
            );
            true
        }
        Some(owner_id) => {
            let uid = user_id.get();
            if uid == owner_id || data.settings.allowed_user_ids.contains(&uid) {
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
async fn check_owner(user_id: UserId, shared: &Arc<Mutex<SharedData>>) -> bool {
    let data = shared.lock().await;
    data.settings.owner_user_id == Some(user_id.get())
}

/// Rate limit helper — ensures minimum 1s gap between API calls per channel
async fn rate_limit_wait(shared: &Arc<Mutex<SharedData>>, channel_id: ChannelId) {
    let min_gap = tokio::time::Duration::from_millis(1000);
    let sleep_until = {
        let mut data = shared.lock().await;
        let last = data
            .api_timestamps
            .entry(channel_id)
            .or_insert_with(|| tokio::time::Instant::now() - tokio::time::Duration::from_secs(10));
        let earliest_next = *last + min_gap;
        let now = tokio::time::Instant::now();
        let target = if earliest_next > now {
            earliest_next
        } else {
            now
        };
        *last = target;
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

/// Handle raw Discord events (non-slash-command messages, file uploads)
async fn handle_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    match event {
        serenity::FullEvent::Message { new_message } => {
            // Ignore bot messages and messages starting with / (slash commands)
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
                let mut d = data.shared.lock().await;
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
            let data = ctx.data().shared.lock().await;
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
    let existing = load_existing_session(&canonical_path);

    // Resolve channel/category names before taking the lock
    let (ch_name, cat_name) =
        resolve_channel_category(ctx.serenity_context(), ctx.channel_id()).await;

    let mut response_lines = Vec::new();

    {
        let mut data = ctx.data().shared.lock().await;
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
                silent: false,
            });
        session.channel_name = ch_name;
        session.category_name = cat_name;

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
        data.settings
            .last_sessions
            .insert(ch_key.clone(), canonical_path.clone());
        // Persist remote profile: store if active, remove if cleared
        match &remote_override {
            Some(Some(name)) => {
                data.settings.last_remotes.insert(ch_key, name.clone());
            }
            Some(None) => {
                data.settings.last_remotes.remove(&ch_key);
            }
            None => {
                // No explicit override — persist current session state
                let current_remote = data
                    .sessions
                    .get(&channel_id)
                    .and_then(|s| s.remote_profile_name.clone());
                if let Some(name) = current_remote {
                    data.settings.last_remotes.insert(ch_key, name);
                }
            }
        }
        save_bot_settings(&ctx.data().token, &data.settings);

        // Rescan skills with project path to pick up project-level commands
        data.skills_cache = scan_skills(Some(&canonical_path));
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
        let data = ctx.data().shared.lock().await;
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
        let data = ctx.data().shared.lock().await;
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
        let mut data = ctx.data().shared.lock().await;
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
        let data = ctx.data().shared.lock().await;
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
            let data = ctx.data().shared.lock().await;
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
        let data = ctx.data().shared.lock().await;
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
        let data = ctx.data().shared.lock().await;
        data.settings.allowed_tools.clone()
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
        let mut data = ctx.data().shared.lock().await;
        match op {
            '+' => {
                if data.settings.allowed_tools.iter().any(|t| t == &tool_name) {
                    format!("`{}` is already in the list.", tool_name)
                } else {
                    data.settings.allowed_tools.push(tool_name.clone());
                    save_bot_settings(&ctx.data().token, &data.settings);
                    format!("Added `{}`", tool_name)
                }
            }
            '-' => {
                let before_len = data.settings.allowed_tools.len();
                data.settings.allowed_tools.retain(|t| t != &tool_name);
                if data.settings.allowed_tools.len() < before_len {
                    save_bot_settings(&ctx.data().token, &data.settings);
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
        let mut data = ctx.data().shared.lock().await;
        if data.settings.allowed_user_ids.contains(&target_id) {
            ctx.say(format!("`{}` is already authorized.", target_name))
                .await?;
            return Ok(());
        }
        data.settings.allowed_user_ids.push(target_id);
        save_bot_settings(&ctx.data().token, &data.settings);
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
        let mut data = ctx.data().shared.lock().await;
        let before_len = data.settings.allowed_user_ids.len();
        data.settings.allowed_user_ids.retain(|&id| id != target_id);
        if data.settings.allowed_user_ids.len() == before_len {
            ctx.say(format!("`{}` is not in the authorized list.", target_name))
                .await?;
            return Ok(());
        }
        save_bot_settings(&ctx.data().token, &data.settings);
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
        let mut data = ctx.data().shared.lock().await;
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
    let data = ctx.data().shared.lock().await;
    let partial_lower = partial.to_lowercase();
    data.skills_cache
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
                let data = ctx.data().shared.lock().await;
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
                let mut data = ctx.data().shared.lock().await;
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
                let data = ctx.data().shared.lock().await;
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
                let data = ctx.data().shared.lock().await;
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
        let data = ctx.data().shared.lock().await;
        data.skills_cache.iter().any(|(name, _)| name == &skill)
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
        let data = ctx.data().shared.lock().await;
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
        let d = ctx.data().shared.lock().await;
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
    shared: &Arc<Mutex<SharedData>>,
    token: &str,
) -> Result<(), Error> {
    // Get session info, allowed tools, pending uploads, and pending steering notes
    let (session_info, allowed_tools, pending_uploads, pending_interventions) = {
        let mut data = shared.lock().await;
        let info = data.sessions.get(&channel_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (
                    session.session_id.clone(),
                    session.current_path.clone().unwrap_or_default(),
                )
            })
        });
        let tools = data.settings.allowed_tools.clone();
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
        let data = shared.lock().await;
        if data.skills_cache.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = data
                .skills_cache
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
        let data = shared.lock().await;
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
    let system_prompt_owned = format!(
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

    // Create cancel token
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = shared.lock().await;
        data.cancel_tokens.insert(channel_id, cancel_token.clone());
        data.active_request_owner.insert(channel_id, request_owner);
    }

    // Resolve remote profile for this channel
    let remote_profile = {
        let data = shared.lock().await;
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
        let data = shared.lock().await;
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
    {
        let data = shared.lock().await;
        if let Some(watcher) = data.tmux_watchers.get(&channel_id) {
            watcher.paused.store(true, Ordering::Relaxed);
        }
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
        let data = shared.lock().await;
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

        while !done {
            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;

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
                                    println!("  [{ts}]   ⚙ {name}: {}", truncate_str(&summary, 80));
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
                                if is_error && !is_silent {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}]   ✗ Error: {}", truncate_str(&content, 80));
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
                                let already_watching = {
                                    let data = shared_owned.lock().await;
                                    data.tmux_watchers.contains_key(&channel_id)
                                };
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
                                    {
                                        let mut data = shared_owned.lock().await;
                                        data.tmux_watchers.insert(channel_id, handle);
                                    }
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

            // Build display text with spinner
            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            let tool_status = current_tool_line.as_deref().unwrap_or("Processing...");
            let display_text = if full_response.is_empty() {
                format!("{} {}", indicator, tool_status)
            } else {
                let normalized = normalize_empty_lines(&full_response);
                let footer = format!("\n\n{} {}", indicator, tool_status);
                let truncated = truncate_str(&normalized, DISCORD_MSG_LIMIT - footer.len() - 10);
                format!("{}{}", truncated, footer)
            };

            if display_text != last_edit_text && !done {
                // Check if we need to start a new message (content too long)
                if display_text.len() > DISCORD_MSG_LIMIT - 50 && current_msg_len > 100 {
                    // Finalize current message with content up to this point
                    let normalized = normalize_empty_lines(&full_response);
                    let finalize_text = truncate_str(&normalized, DISCORD_MSG_LIMIT - 10);
                    current_msg_len = finalize_text.len();

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
            let data = shared_owned.lock().await;
            if let Some(watcher) = data.tmux_watchers.get(&channel_id) {
                *watcher.resume_offset.lock().unwrap() = Some(offset);
                watcher.paused.store(false, Ordering::Relaxed);
            }
        }

        // Report accumulated tokens to PCD for XP tracking
        // Key matches hook: hostname:remoteCC-{channel_name}
        if accumulated_tokens > 0 {
            let tmux_name = {
                let data = shared_owned.lock().await;
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
            let mut data = shared_owned.lock().await;
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
            let mut data = shared_owned.lock().await;
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
            let mut data = shared_owned.lock().await;
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
    shared: &Arc<Mutex<SharedData>>,
) -> Result<(), Error> {
    let channel_id = msg.channel_id;

    let has_session = {
        let data = shared.lock().await;
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
            let mut data = shared.lock().await;
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
    shared: &Arc<Mutex<SharedData>>,
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
        let data = shared.lock().await;
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
    shared: &Arc<Mutex<SharedData>>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    {
        let data = shared.lock().await;
        if data.sessions.contains_key(&channel_id) {
            return;
        }
    }

    // Resolve channel/category before taking the lock for mutation
    let (ch_name, cat_name) = resolve_channel_category(serenity_ctx, channel_id).await;

    let mut data = shared.lock().await;
    if data.sessions.contains_key(&channel_id) {
        return; // Double-check after re-acquiring lock
    }

    let channel_key = channel_id.get().to_string();
    if let Some(last_path) = data.settings.last_sessions.get(&channel_key).cloned() {
        let is_remote = data.settings.last_remotes.contains_key(&channel_key);
        if is_remote || Path::new(&last_path).is_dir() {
            let existing = load_existing_session(&last_path);
            let saved_remote = data.settings.last_remotes.get(&channel_key).cloned();
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
                    channel_name: ch_name,
                    category_name: cat_name,
                    remote_profile_name: saved_remote.clone(),
                    silent: false,
                });
            session.current_path = Some(last_path.clone());
            if let Some((session_data, _)) = existing {
                session.session_id = Some(session_data.session_id.clone());
                session.history = session_data.history.clone();
            }
            // Rescan skills with project path
            data.skills_cache = scan_skills(Some(&last_path));
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
fn load_existing_session(current_path: &str) -> Option<(SessionData, std::time::SystemTime)> {
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
                            if let Ok(metadata) = path.metadata() {
                                if let Ok(modified) = metadata.modified() {
                                    let has_id = !session_data.session_id.is_empty();
                                    let target = if has_id { &mut best_with_id } else { &mut best_without_id };
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
    shared: &Arc<Mutex<SharedData>>,
) {
    let sessions_dir = match ai_screen::ai_sessions_dir() {
        Some(d) if d.exists() => d,
        _ => return,
    };

    // Collect channel IDs from bot_settings.last_sessions
    let channel_keys: Vec<(String, String)> = {
        let data = shared.lock().await;
        data.settings
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
        let existing = load_existing_session(session_path);
        if let Some((session_data, _)) = existing {
            let file_path = sessions_dir.join(format!("{}.json", session_data.session_id));
            if file_path.exists() {
                // Read, update category fields, write back
                if let Ok(content) = fs::read_to_string(&file_path) {
                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(obj) = val.as_object_mut() {
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

    // Clean up old session files for the same channel (different session_id)
    if let Some(ref ch_name) = effective_channel_name {
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
                            if old.discord_channel_name.as_deref() == Some(ch_name) {
                                let _ = fs::remove_file(&path);
                            }
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
        discord_channel_id: None,
        discord_channel_name: effective_channel_name,
        discord_category_name: effective_category_name,
        remote_profile_name: session.remote_profile_name.clone(),
    };

    if let Ok(json) = serde_json::to_string_pretty(&session_data) {
        let _ = fs::write(file_path, json);
    }
}

// ─── Message utilities ──────────────────────────────────────────────────────

/// Find the largest byte index <= `index` that is a valid UTF-8 char boundary
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else {
        let mut i = index;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

/// Truncate a string to max_len bytes at a safe UTF-8 and line boundary
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let safe_end = floor_char_boundary(s, max_len);
    let truncated = &s[..safe_end];
    if let Some(pos) = truncated.rfind('\n') {
        truncated[..pos].to_string()
    } else {
        truncated.to_string()
    }
}

/// Normalize consecutive empty lines to maximum of one
fn normalize_empty_lines(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_empty = false;

    for line in s.lines() {
        let is_empty = line.is_empty();
        if is_empty {
            if !prev_was_empty {
                result.push('\n');
            }
            prev_was_empty = true;
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
            prev_was_empty = false;
        }
    }

    result
}

/// Format tool input JSON into a human-readable summary
fn format_tool_input(name: &str, input: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return format!("{} {}", name, truncate_str(input, 200));
    };

    match name {
        "Bash" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let cmd = v.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if !desc.is_empty() {
                format!("{}: `{}`", desc, truncate_str(cmd, 150))
            } else {
                format!("`{}`", truncate_str(cmd, 200))
            }
        }
        "Read" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            format!("Read {}", fp)
        }
        "Write" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let lines = content.lines().count();
            if lines > 0 {
                format!("Write {} ({} lines)", fp, lines)
            } else {
                format!("Write {}", fp)
            }
        }
        "Edit" => {
            let fp = v.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let replace_all = v
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if replace_all {
                format!("Edit {} (replace all)", fp)
            } else {
                format!("Edit {}", fp)
            }
        }
        "Glob" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                format!("Glob {} in {}", pattern, path)
            } else {
                format!("Glob {}", pattern)
            }
        }
        "Grep" => {
            let pattern = v.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let output_mode = v.get("output_mode").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                if !output_mode.is_empty() {
                    format!("Grep \"{}\" in {} ({})", pattern, path, output_mode)
                } else {
                    format!("Grep \"{}\" in {}", pattern, path)
                }
            } else {
                format!("Grep \"{}\"", pattern)
            }
        }
        "NotebookEdit" => {
            let nb_path = v
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let cell_id = v.get("cell_id").and_then(|v| v.as_str()).unwrap_or("");
            if !cell_id.is_empty() {
                format!("Notebook {} ({})", nb_path, cell_id)
            } else {
                format!("Notebook {}", nb_path)
            }
        }
        "WebSearch" => {
            let query = v.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Search: {}", query)
        }
        "WebFetch" => {
            let url = v.get("url").and_then(|v| v.as_str()).unwrap_or("");
            format!("Fetch {}", url)
        }
        "Task" => {
            let desc = v.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let subagent_type = v
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !subagent_type.is_empty() {
                format!("Task [{}]: {}", subagent_type, desc)
            } else {
                format!("Task: {}", desc)
            }
        }
        "TaskOutput" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            format!("Get task output: {}", task_id)
        }
        "TaskStop" => {
            let task_id = v.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            format!("Stop task: {}", task_id)
        }
        "TodoWrite" => {
            if let Some(todos) = v.get("todos").and_then(|v| v.as_array()) {
                let pending = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("pending"))
                    .count();
                let in_progress = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("in_progress"))
                    .count();
                let completed = todos
                    .iter()
                    .filter(|t| t.get("status").and_then(|s| s.as_str()) == Some("completed"))
                    .count();
                format!(
                    "Todo: {} pending, {} in progress, {} completed",
                    pending, in_progress, completed
                )
            } else {
                "Update todos".to_string()
            }
        }
        "Skill" => {
            let skill = v.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            format!("Skill: {}", skill)
        }
        "AskUserQuestion" => {
            if let Some(questions) = v.get("questions").and_then(|v| v.as_array()) {
                if let Some(q) = questions.first() {
                    let question = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    truncate_str(question, 200)
                } else {
                    "Ask user question".to_string()
                }
            } else {
                "Ask user question".to_string()
            }
        }
        "ExitPlanMode" => "Exit plan mode".to_string(),
        "EnterPlanMode" => "Enter plan mode".to_string(),
        "TaskCreate" => {
            let subject = v.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            format!("Create task: {}", subject)
        }
        "TaskUpdate" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if !status.is_empty() {
                format!("Update task {}: {}", task_id, status)
            } else {
                format!("Update task {}", task_id)
            }
        }
        "TaskGet" => {
            let task_id = v.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            format!("Get task: {}", task_id)
        }
        "TaskList" => "List tasks".to_string(),
        _ => format!("{} {}", name, truncate_str(input, 200)),
    }
}

/// Mechanical formatting for Discord readability.
/// Converts markdown headers to bold, ensures spacing around lists, etc.
fn format_for_discord(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_code_block = false;

    for line in s.lines() {
        // Don't touch anything inside code blocks
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(line.to_string());
            continue;
        }
        if in_code_block {
            lines.push(line.to_string());
            continue;
        }

        let trimmed = line.trim_start();

        // Convert # headers to **bold** (Discord doesn't render headers in bot messages)
        if let Some(rest) = trimmed.strip_prefix("### ") {
            // Ensure blank line before header
            if let Some(prev) = lines.last() {
                if !prev.trim().is_empty() {
                    lines.push(String::new());
                }
            }
            lines.push(format!("**{}**", rest));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            if let Some(prev) = lines.last() {
                if !prev.trim().is_empty() {
                    lines.push(String::new());
                }
            }
            lines.push(format!("**{}**", rest));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            if let Some(prev) = lines.last() {
                if !prev.trim().is_empty() {
                    lines.push(String::new());
                }
            }
            lines.push(format!("**{}**", rest));
            continue;
        }

        // Ensure blank line before the first item of a list block
        let is_list_item = trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || (trimmed.len() > 2
                && trimmed.as_bytes()[0].is_ascii_digit()
                && trimmed.contains(". "));

        if is_list_item {
            if let Some(prev) = lines.last() {
                let prev_trimmed = prev.trim();
                // Add blank line only if previous line is non-empty and not itself a list item
                let prev_is_list = prev_trimmed.starts_with("- ")
                    || prev_trimmed.starts_with("* ")
                    || (prev_trimmed.len() > 2
                        && prev_trimmed.as_bytes()[0].is_ascii_digit()
                        && prev_trimmed.contains(". "));
                if !prev_trimmed.is_empty() && !prev_is_list {
                    lines.push(String::new());
                }
            }
        }

        lines.push(line.to_string());
    }

    // Collapse consecutive blank lines (max 1)
    let mut result = String::with_capacity(s.len());
    let mut prev_was_empty = false;
    for line in &lines {
        let is_empty = line.trim().is_empty();
        if is_empty {
            if !prev_was_empty && !result.is_empty() {
                result.push('\n');
            }
            prev_was_empty = true;
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
            prev_was_empty = false;
        }
    }

    result
}

/// Send a message using poise Context, splitting if necessary
async fn send_long_message_ctx(ctx: Context<'_>, text: &str) -> Result<(), Error> {
    if text.len() <= DISCORD_MSG_LIMIT {
        ctx.say(text).await?;
        return Ok(());
    }

    let chunks = split_message(text);
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            ctx.say(chunk).await?;
        } else {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            ctx.channel_id().say(ctx.serenity_context(), chunk).await?;
        }
    }

    Ok(())
}

/// Send a long message using raw HTTP, splitting if necessary
async fn send_long_message_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<Mutex<SharedData>>,
) -> Result<(), Error> {
    if text.len() <= DISCORD_MSG_LIMIT {
        rate_limit_wait(shared, channel_id).await;
        channel_id
            .send_message(http, CreateMessage::new().content(text))
            .await?;
        return Ok(());
    }

    let chunks = split_message(text);
    for chunk in &chunks {
        rate_limit_wait(shared, channel_id).await;
        channel_id
            .send_message(http, CreateMessage::new().content(chunk))
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    Ok(())
}

/// Split a message into chunks that fit within Discord's 2000 char limit.
/// Handles code block boundaries correctly.
fn split_message(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut in_code_block = false;
    let mut code_block_lang = String::new();

    while !remaining.is_empty() {
        // Reserve space for code block tags we may need to add
        let tag_overhead = if in_code_block {
            // closing ``` + opening ```lang\n
            3 + 3 + code_block_lang.len() + 1
        } else {
            0
        };
        let effective_limit = DISCORD_MSG_LIMIT
            .saturating_sub(tag_overhead)
            .saturating_sub(10);

        if remaining.len() <= effective_limit {
            let mut chunk = String::new();
            if in_code_block {
                chunk.push_str("```");
                chunk.push_str(&code_block_lang);
                chunk.push('\n');
            }
            chunk.push_str(remaining);
            chunks.push(chunk);
            break;
        }

        // Find a safe split point
        let safe_end = floor_char_boundary(remaining, effective_limit);
        let split_at = remaining[..safe_end].rfind('\n').unwrap_or(safe_end);

        let (raw_chunk, rest) = remaining.split_at(split_at);

        let mut chunk = String::new();
        if in_code_block {
            chunk.push_str("```");
            chunk.push_str(&code_block_lang);
            chunk.push('\n');
        }
        chunk.push_str(raw_chunk);

        // Track code blocks across chunk boundaries
        for line in raw_chunk.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_code_block {
                    in_code_block = false;
                    code_block_lang.clear();
                } else {
                    in_code_block = true;
                    code_block_lang = trimmed.strip_prefix("```").unwrap_or("").to_string();
                }
            }
        }

        // Close unclosed code block at end of chunk
        if in_code_block {
            chunk.push_str("\n```");
        }

        chunks.push(chunk);
        remaining = rest.strip_prefix('\n').unwrap_or(rest);
    }

    chunks
}

/// Add reaction using raw HTTP reference
async fn add_reaction_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    let _ = channel_id.create_reaction(http, message_id, reaction).await;
}

/// Remove reaction using raw HTTP reference
async fn remove_reaction_raw(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    let _ = channel_id
        .delete_reaction(http, message_id, None, reaction)
        .await;
}

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<Mutex<SharedData>>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
) {
    use claude::StreamLineState;
    use std::io::{Read, Seek, SeekFrom};

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset}");

    let mut current_offset = initial_offset;

    loop {
        // Check cancel
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        // If paused (Discord handler is processing its own turn), wait
        if paused.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            // Check if resumed with a new offset
            if let Some(new_offset) = resume_offset.lock().unwrap().take() {
                current_offset = new_offset;
            }
            continue;
        }

        // Check if tmux session is still alive
        let alive = tokio::task::spawn_blocking({
            let name = tmux_session_name.clone();
            move || {
                std::process::Command::new("tmux")
                    .args(["has-session", "-t", &name])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            }
        })
        .await
        .unwrap_or(false);

        if !alive {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping");
            break;
        }

        // Try to read new data from output file
        let read_result = tokio::task::spawn_blocking({
            let path = output_path.clone();
            let offset = current_offset;
            move || -> Result<(Vec<u8>, u64), String> {
                let mut file = std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| format!("seek: {}", e))?;
                let mut buf = vec![0u8; 16384];
                let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                buf.truncate(n);
                Ok((buf, offset + n as u64))
            }
        })
        .await;

        let (data, new_offset) = match read_result {
            Ok(Ok((data, off))) => (data, off),
            _ => {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        if data.is_empty() {
            // No new data, sleep and retry
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            continue;
        }

        // We got new data while not paused — this means terminal input triggered a response
        current_offset = new_offset;

        // Collect the full turn: keep reading until we see a "result" event
        let mut all_data = String::from_utf8_lossy(&data).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();

        // Process any complete lines we already have
        let mut found_result = process_watcher_lines(&mut all_data, &mut state, &mut full_response);

        // Keep reading until result or timeout
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = tokio::time::Duration::from_secs(600); // 10 min max

            while !found_result && turn_start.elapsed() < turn_timeout {
                if cancel.load(Ordering::Relaxed) || paused.load(Ordering::Relaxed) {
                    break;
                }

                let read_more = tokio::task::spawn_blocking({
                    let path = output_path.clone();
                    let offset = current_offset;
                    move || -> Result<(Vec<u8>, u64), String> {
                        let mut file =
                            std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                        file.seek(SeekFrom::Start(offset))
                            .map_err(|e| format!("seek: {}", e))?;
                        let mut buf = vec![0u8; 16384];
                        let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                        buf.truncate(n);
                        Ok((buf, offset + n as u64))
                    }
                })
                .await;

                match read_more {
                    Ok(Ok((chunk, off))) if !chunk.is_empty() => {
                        current_offset = off;
                        all_data.push_str(&String::from_utf8_lossy(&chunk));
                        found_result =
                            process_watcher_lines(&mut all_data, &mut state, &mut full_response);
                    }
                    _ => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }

        // If paused was set while we were reading, discard — Discord handler will handle it
        if paused.load(Ordering::Relaxed) {
            continue;
        }

        // Send the terminal response to Discord
        if !full_response.trim().is_empty() {
            let formatted = format_for_discord(&full_response);
            let prefixed = format!("🖥 **[Terminal]**\n{}", formatted);
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 👁 Relaying terminal response to Discord ({} chars)",
                prefixed.len()
            );
            if let Err(e) = send_long_message_raw(&http, channel_id, &prefixed, &shared).await {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 👁 Failed to relay: {e}");
            }
        }
    }

    // Cleanup
    {
        let mut data = shared.lock().await;
        data.tmux_watchers.remove(&channel_id);
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content and detects result events.
/// Returns true if a "result" event was found.
fn process_watcher_lines(
    buffer: &mut String,
    state: &mut claude::StreamLineState,
    full_response: &mut String,
) -> bool {
    let mut found_result = false;

    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "assistant" => {
                    // Text content from assistant message
                    if let Some(message) = val.get("message") {
                        if let Some(content) = message.get("content") {
                            if let Some(arr) = content.as_array() {
                                for block in arr {
                                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                        if let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                        {
                                            full_response.push_str(text);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = val.get("delta") {
                        if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            full_response.push_str(text);
                        }
                    }
                }
                "result" => {
                    // Extract text from result if full_response is still empty
                    if full_response.is_empty() {
                        if let Some(result_str) = val.get("result").and_then(|r| r.as_str()) {
                            full_response.push_str(result_str);
                        }
                    }
                    state.final_result = Some(String::new());
                    found_result = true;
                }
                _ => {}
            }
        }
    }

    found_result
}

/// On startup, scan for surviving tmux sessions (remoteCC-*) and restore watchers.
/// This handles the case where RemoteCC was restarted but tmux sessions are still alive.
async fn restore_tmux_watchers(http: &Arc<serenity::Http>, shared: &Arc<Mutex<SharedData>>) {
    // List tmux sessions matching our naming convention
    let output = match tokio::task::spawn_blocking(|| {
        std::process::Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
    })
    .await
    {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return, // No tmux or no sessions
    };

    // Collect sessions to restore (under lock), then spawn watchers outside lock
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
    }

    let pending: Vec<PendingWatcher> = {
        let data = shared.lock().await;
        let mut result = Vec::new();

        for session_name in output.lines() {
            let session_name = session_name.trim();
            if !session_name.starts_with("remoteCC-") {
                continue;
            }

            // Find the channel that maps to this tmux session name
            let mut found_channel: Option<ChannelId> = None;
            for (&ch_id, session) in &data.sessions {
                if let Some(ref ch_name) = session.channel_name {
                    if claude::sanitize_tmux_session_name(ch_name) == session_name {
                        found_channel = Some(ch_id);
                        break;
                    }
                }
            }

            let Some(channel_id) = found_channel else {
                continue;
            };
            if data.tmux_watchers.contains_key(&channel_id) {
                continue;
            }

            let output_path = format!("/tmp/remotecc-{}.jsonl", session_name);
            if std::fs::metadata(&output_path).is_err() {
                continue;
            }

            let initial_offset = std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0);

            result.push(PendingWatcher {
                channel_id,
                output_path,
                session_name: session_name.to_string(),
                initial_offset,
            });
        }

        result
    }; // lock dropped here

    // Now spawn watchers outside the lock
    for pw in pending {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name, pw.initial_offset
        );

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));

        {
            let mut data = shared.lock().await;
            data.tmux_watchers.insert(
                pw.channel_id,
                TmuxWatcherHandle {
                    cancel: cancel.clone(),
                    paused: paused.clone(),
                    resume_offset: resume_offset.clone(),
                },
            );
        }

        tokio::spawn(tmux_output_watcher(
            pw.channel_id,
            http.clone(),
            shared.clone(),
            pw.output_path,
            pw.session_name,
            pw.initial_offset,
            cancel,
            paused,
            resume_offset,
        ));
    }
}
