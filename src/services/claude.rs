use regex::Regex;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use crate::services::remote::RemoteProfile;

/// Cached path to the claude binary.
/// Once resolved, reused for all subsequent calls.
static CLAUDE_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Resolve the path to the claude binary.
/// First tries `which claude`, then falls back to `bash -lc "which claude"`
/// (for non-interactive SSH sessions where ~/.profile isn't loaded).
fn resolve_claude_path() -> Option<String> {
    // Try direct `which claude` first
    if let Ok(output) = Command::new("which").arg("claude").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    // Fallback: use login shell to resolve PATH
    if let Ok(output) = Command::new("bash").args(["-lc", "which claude"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    None
}

/// Get the cached claude binary path, resolving it on first call.
fn get_claude_path() -> Option<&'static str> {
    CLAUDE_PATH.get_or_init(|| resolve_claude_path()).as_deref()
}

/// Global runtime debug flag — togglable via `/debug` command or COKACDIR_DEBUG=1 env var.
static DEBUG_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Initialize debug flag from environment variable (call once at startup).
pub fn init_debug_from_env() {
    let enabled = std::env::var("COKACDIR_DEBUG")
        .map(|v| v == "1")
        .unwrap_or(false);
    if enabled {
        DEBUG_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Toggle debug mode at runtime. Returns the new state.
pub fn toggle_debug() -> bool {
    let prev = DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    DEBUG_ENABLED.store(!prev, std::sync::atomic::Ordering::Relaxed);
    !prev
}

/// Check if debug mode is currently enabled.
pub fn is_debug_enabled() -> bool {
    DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Debug logging helper — active when DEBUG_ENABLED is true.
fn debug_log(msg: &str) {
    if !DEBUG_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    debug_log_to("claude.log", msg);
}

/// Write a debug message to a specific log file under ~/.remotecc/debug/.
pub fn debug_log_to(filename: &str, msg: &str) {
    if let Some(home) = dirs::home_dir() {
        let debug_dir = home.join(".remotecc").join("debug");
        let _ = std::fs::create_dir_all(&debug_dir);
        let log_path = debug_dir.join(filename);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
            let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
            let _ = writeln!(file, "[{}] {}", timestamp, msg);
        }
    }
}

/// Kill a process tree by PID.
/// On Unix, sends SIGTERM to the process group, then SIGKILL as fallback.
pub fn kill_pid_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Send SIGTERM to the process group (negative PID)
        let ret = libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
        if ret != 0 {
            // Fallback: kill just the process
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        // On Windows, use taskkill /T to kill the tree
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
}

/// Kill a child process and its entire process tree.
/// On Unix, sends SIGTERM to the process group first, then SIGKILL as fallback.
pub fn kill_child_tree(child: &mut std::process::Child) {
    kill_pid_tree(child.id());
    // Give processes a moment to clean up, then force kill if needed
    std::thread::sleep(std::time::Duration::from_millis(200));
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill(); // SIGKILL
    }
    let _ = child.wait();
}

#[derive(Debug, Clone)]
pub struct ClaudeResponse {
    pub success: bool,
    pub response: Option<String>,
    pub session_id: Option<String>,
    pub error: Option<String>,
}

/// Streaming message types for real-time Claude responses
#[derive(Debug, Clone)]
pub enum StreamMessage {
    /// Initialization - contains session_id
    Init { session_id: String },
    /// Text response chunk
    Text { content: String },
    /// Tool use started
    ToolUse { name: String, input: String },
    /// Tool execution result
    ToolResult { content: String, is_error: bool },
    /// Background task notification
    TaskNotification {
        task_id: String,
        status: String,
        summary: String,
    },
    /// Completion
    Done {
        result: String,
        session_id: Option<String>,
    },
    /// Error
    Error {
        message: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    },
    /// Statusline info extracted from result/assistant events
    StatusUpdate {
        model: Option<String>,
        cost_usd: Option<f64>,
        total_cost_usd: Option<f64>,
        duration_ms: Option<u64>,
        num_turns: Option<u32>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    /// tmux session is ready for background monitoring (first turn completed)
    TmuxReady {
        output_path: String,
        input_fifo_path: String,
        tmux_session_name: String,
        last_offset: u64,
    },
}

/// Result from reading a tmux output file until completion or session death.
pub enum ReadOutputResult {
    /// Normal completion (result event received)
    Completed { offset: u64 },
    /// Session died without producing a result
    #[allow(dead_code)]
    SessionDied { offset: u64 },
    /// User cancelled the operation
    Cancelled { offset: u64 },
}

/// Token for cooperative cancellation of streaming requests.
/// Holds a flag and the child process PID so the caller can kill it externally.
pub struct CancelToken {
    pub cancelled: std::sync::atomic::AtomicBool,
    pub child_pid: std::sync::Mutex<Option<u32>>,
    /// SSH cancel flag — set to true to signal remote execution to close the channel
    pub ssh_cancel: std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    /// tmux session name for cleanup on cancel
    pub tmux_session: std::sync::Mutex<Option<String>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            child_pid: std::sync::Mutex::new(None),
            ssh_cancel: std::sync::Mutex::new(None),
            tmux_session: std::sync::Mutex::new(None),
        }
    }

    /// Cancel and clean up any associated tmux session
    pub fn cancel_with_tmux_cleanup(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(name) = self.tmux_session.lock().unwrap().take() {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", &name])
                .output();
        }
    }
}

/// Cached regex pattern for session ID validation
fn session_id_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("Invalid session ID regex pattern"))
}

/// Validate session ID format (alphanumeric, dashes, underscores only)
/// Max length reduced to 64 characters for security
fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty() && session_id.len() <= 64 && session_id_regex().is_match(session_id)
}

/// Default allowed tools for Claude CLI
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "Task",
    "TaskOutput",
    "TaskStop",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
];

/// Execute a command using Claude CLI
pub fn execute_command(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    allowed_tools: Option<&[String]>,
) -> ClaudeResponse {
    let tools_str = match allowed_tools {
        Some(tools) => tools.join(","),
        None => DEFAULT_ALLOWED_TOOLS.join(","),
    };
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--tools".to_string(),
        tools_str,
        "--output-format".to_string(),
        "json".to_string(),
        "--append-system-prompt".to_string(),
        r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#.to_string(),
    ];

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Invalid session ID format".to_string()),
            };
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let claude_bin = match get_claude_path() {
        Some(path) => path,
        None => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some("Claude CLI not found. Is Claude CLI installed?".to_string()),
            };
        }
    };

    let mut child = match Command::new(claude_bin)
        .args(&args)
        .current_dir(working_dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env("BASH_MAX_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env_remove("CLAUDECODE") // Allow running from within Claude Code sessions
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ClaudeResponse {
                success: false,
                response: None,
                session_id: None,
                error: Some(format!(
                    "Failed to start Claude: {}. Is Claude CLI installed?",
                    e
                )),
            };
        }
    };

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    // Wait for output
    match child.wait_with_output() {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                parse_claude_output(&stdout)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                ClaudeResponse {
                    success: false,
                    response: None,
                    session_id: None,
                    error: Some(if stderr.is_empty() {
                        format!("Process exited with code {:?}", output.status.code())
                    } else {
                        stderr
                    }),
                }
            }
        }
        Err(e) => ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some(format!("Failed to read output: {}", e)),
        },
    }
}

/// Parse Claude CLI JSON output
fn parse_claude_output(output: &str) -> ClaudeResponse {
    let mut session_id: Option<String> = None;
    let mut response_text = String::new();

    for line in output.trim().lines() {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            // Extract session ID
            if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                session_id = Some(sid.to_string());
            }

            // Extract response text
            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                response_text = result.to_string();
            } else if let Some(message) = json.get("message").and_then(|v| v.as_str()) {
                response_text = message.to_string();
            } else if let Some(content) = json.get("content").and_then(|v| v.as_str()) {
                response_text = content.to_string();
            }
        } else if !line.trim().is_empty() && !line.starts_with('{') {
            response_text.push_str(line);
            response_text.push('\n');
        }
    }

    // If no structured response, use raw output
    if response_text.is_empty() {
        response_text = output.trim().to_string();
    }

    ClaudeResponse {
        success: true,
        response: Some(response_text.trim().to_string()),
        session_id,
        error: None,
    }
}

/// Check if Claude CLI is available
pub fn is_claude_available() -> bool {
    #[cfg(not(unix))]
    {
        false
    }

    #[cfg(unix)]
    {
        get_claude_path().is_some()
    }
}

/// Check if platform supports AI features
pub fn is_ai_supported() -> bool {
    cfg!(unix)
}

/// Execute a simple Claude CLI call with `--print` flag (no tools, text-only response).
/// Used for short synchronous tasks like meeting participant selection.
/// This is a blocking function — call from tokio::task::spawn_blocking.
pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let claude_bin = get_claude_path().ok_or("Claude CLI not found")?;

    let mut child = Command::new(claude_bin)
        .args(["-p", "--output-format", "text"])
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "4096")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Claude: {}", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to read output: {}", e))?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Err("Empty response from Claude".to_string())
        } else {
            Ok(text)
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(if stderr.is_empty() {
            format!("Process exited with code {:?}", output.status.code())
        } else {
            stderr
        })
    }
}

/// Execute a command using Claude CLI with streaming output
/// If `system_prompt` is None, uses the default file manager system prompt.
/// If `system_prompt` is Some(""), no system prompt is appended.
pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    tmux_session_name: Option<&str>,
) -> Result<(), String> {
    debug_log("========================================");
    debug_log("=== execute_command_streaming START ===");
    debug_log("========================================");
    debug_log(&format!("prompt_len: {} chars", prompt.len()));
    let prompt_preview: String = prompt.chars().take(200).collect();
    debug_log(&format!("prompt_preview: {:?}", prompt_preview));
    debug_log(&format!("session_id: {:?}", session_id));
    debug_log(&format!("working_dir: {}", working_dir));
    debug_log(&format!("timestamp: {:?}", std::time::SystemTime::now()));

    let default_system_prompt = r#"You are a terminal file manager assistant. Be concise. Focus on file operations. Respond in the same language as the user.

SECURITY RULES (MUST FOLLOW):
- NEVER execute destructive commands like rm -rf, format, mkfs, dd, etc.
- NEVER modify system files in /etc, /sys, /proc, /boot
- NEVER access or modify files outside the current working directory without explicit user path
- NEVER execute commands that could harm the system or compromise security
- ONLY suggest safe file operations: copy, move, rename, create directory, view, edit
- If a request seems dangerous, explain the risk and suggest a safer alternative

BASH EXECUTION RULES (MUST FOLLOW):
- All commands MUST run non-interactively without user input
- Use -y, --yes, or --non-interactive flags (e.g., apt install -y, npm init -y)
- Use -m flag for commit messages (e.g., git commit -m "message")
- Disable pagers with --no-pager or pipe to cat (e.g., git --no-pager log)
- NEVER use commands that open editors (vim, nano, etc.)
- NEVER use commands that wait for stdin without arguments
- NEVER use interactive flags like -i

IMPORTANT: Format your responses using Markdown for better readability:
- Use **bold** for important terms or commands
- Use `code` for file paths, commands, and technical terms
- Use bullet lists (- item) for multiple items
- Use numbered lists (1. item) for sequential steps
- Use code blocks (```language) for multi-line code or command examples
- Use headers (## Title) to organize longer responses
- Keep formatting minimal and terminal-friendly"#;

    let tools_str = match allowed_tools {
        Some(tools) => tools.join(","),
        None => DEFAULT_ALLOWED_TOOLS.join(","),
    };
    let mut args = vec![
        "-p".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--tools".to_string(),
        tools_str,
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];

    // Append system prompt based on parameter
    let effective_prompt = match system_prompt {
        None => Some(default_system_prompt),
        Some("") => None,
        Some(p) => Some(p),
    };
    if let Some(sp) = effective_prompt {
        args.push("--append-system-prompt".to_string());
        args.push(sp.to_string());
    }

    // Resume session if available
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            debug_log("ERROR: Invalid session ID format");
            return Err("Invalid session ID format".to_string());
        }
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    // tmux execution path: wrap Claude in a tmux session for terminal attach
    if let Some(tmux_name) = tmux_session_name {
        if is_tmux_available() {
            debug_log(&format!("tmux session requested: {}", tmux_name));
            // Add stream-json input format for bidirectional communication
            args.push("--input-format".to_string());
            args.push("stream-json".to_string());
            if let Some(profile) = remote_profile {
                return execute_streaming_remote_tmux(
                    profile,
                    &args,
                    prompt,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                );
            } else {
                return execute_streaming_local_tmux(
                    &args,
                    prompt,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                );
            }
        } else {
            debug_log("tmux requested but not available, falling back to direct execution");
        }
    }

    // Remote execution path: SSH to remote host
    if let Some(profile) = remote_profile {
        debug_log("Remote profile detected — delegating to execute_streaming_remote()");
        return execute_streaming_remote(profile, &args, prompt, working_dir, sender, cancel_token);
    }

    let claude_bin = get_claude_path().ok_or_else(|| {
        debug_log("ERROR: Claude CLI not found");
        "Claude CLI not found. Is Claude CLI installed?".to_string()
    })?;

    debug_log("--- Spawning claude process ---");
    debug_log(&format!("Command: {}", claude_bin));
    debug_log(&format!("Args count: {}", args.len()));
    for (i, arg) in args.iter().enumerate() {
        if arg.len() > 100 {
            debug_log(&format!(
                "  arg[{}]: {}... (truncated, {} chars total)",
                i,
                &arg[..100],
                arg.len()
            ));
        } else {
            debug_log(&format!("  arg[{}]: {}", i, arg));
        }
    }
    debug_log("Env: CLAUDE_CODE_MAX_OUTPUT_TOKENS=64000");
    debug_log("Env: BASH_DEFAULT_TIMEOUT_MS=86400000");
    debug_log("Env: BASH_MAX_TIMEOUT_MS=86400000");

    let spawn_start = std::time::Instant::now();
    let mut child = Command::new(claude_bin)
        .args(&args)
        .current_dir(working_dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env("BASH_MAX_TIMEOUT_MS", "86400000") // 24 hours (no practical timeout)
        .env_remove("CLAUDECODE") // Allow running from within Claude Code sessions
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            debug_log(&format!(
                "ERROR: Failed to spawn after {:?}: {}",
                spawn_start.elapsed(),
                e
            ));
            format!("Failed to start Claude: {}. Is Claude CLI installed?", e)
        })?;
    debug_log(&format!(
        "Claude process spawned successfully in {:?}, pid={:?}",
        spawn_start.elapsed(),
        child.id()
    ));

    // Store child PID in cancel token so the caller can kill it externally
    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
    }

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        debug_log(&format!(
            "Writing prompt to stdin ({} bytes)...",
            prompt.len()
        ));
        let write_start = std::time::Instant::now();
        let write_result = stdin.write_all(prompt.as_bytes());
        debug_log(&format!(
            "stdin.write_all completed in {:?}, result={:?}",
            write_start.elapsed(),
            write_result.is_ok()
        ));
        // stdin is dropped here, which closes it - this signals end of input to claude
        debug_log("stdin handle dropped (closed)");
    } else {
        debug_log("WARNING: Could not get stdin handle!");
    }

    // Read stdout line by line for streaming
    debug_log("Taking stdout handle...");
    let stdout = child.stdout.take().ok_or_else(|| {
        debug_log("ERROR: Failed to capture stdout");
        "Failed to capture stdout".to_string()
    })?;
    let reader = BufReader::new(stdout);
    debug_log("BufReader created, ready to read lines...");

    let mut last_session_id: Option<String> = None;
    let mut last_model: Option<String> = None;
    let mut accum_input_tokens: u64 = 0;
    let mut accum_output_tokens: u64 = 0;
    let mut final_result: Option<String> = None;
    let mut stdout_error: Option<(String, String)> = None; // (message, raw_line)
    let mut line_count = 0;

    debug_log("Entering lines loop - will block until first line arrives...");
    for line in reader.lines() {
        // Check cancel token before processing each line
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                debug_log("Cancel detected — killing child process tree");
                kill_child_tree(&mut child);
                return Ok(());
            }
        }

        debug_log(&format!("Line {} - read started", line_count + 1));
        let line = match line {
            Ok(l) => {
                debug_log(&format!(
                    "Line {} - read completed: {} chars",
                    line_count + 1,
                    l.len()
                ));
                l
            }
            Err(e) => {
                debug_log(&format!("ERROR: Failed to read line: {}", e));
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to read output: {}", e),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                break;
            }
        };

        line_count += 1;
        debug_log(&format!("Line {}: {} chars", line_count, line.len()));

        if line.trim().is_empty() {
            debug_log("  (empty line, skipping)");
            continue;
        }

        let line_preview: String = line.chars().take(200).collect();
        debug_log(&format!("  Raw line preview: {}", line_preview));

        if let Ok(json) = serde_json::from_str::<Value>(&line) {
            let msg_type = json
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let msg_subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("-");
            debug_log(&format!(
                "  JSON parsed: type={}, subtype={}",
                msg_type, msg_subtype
            ));

            // Log more details for specific message types
            if msg_type == "assistant" {
                if let Some(content) = json.get("message").and_then(|m| m.get("content")) {
                    debug_log(&format!("  Assistant content array: {}", content));
                }
                // Extract model name and token usage from assistant messages
                if let Some(msg_obj) = json.get("message") {
                    if let Some(model) = msg_obj.get("model").and_then(|v| v.as_str()) {
                        last_model = Some(model.to_string());
                    }
                    if let Some(usage) = msg_obj.get("usage") {
                        if let Some(inp) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                            accum_input_tokens += inp;
                        }
                        if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                            accum_output_tokens += out;
                        }
                    }
                }
            }

            // Extract statusline info from result events
            if msg_type == "result" {
                let cost_usd = json.get("cost_usd").and_then(|v| v.as_f64());
                let total_cost_usd = json.get("total_cost_usd").and_then(|v| v.as_f64());
                let duration_ms = json.get("duration_ms").and_then(|v| v.as_u64());
                let num_turns = json
                    .get("num_turns")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                if cost_usd.is_some() || total_cost_usd.is_some() || last_model.is_some() {
                    let _ = sender.send(StreamMessage::StatusUpdate {
                        model: last_model.clone(),
                        cost_usd,
                        total_cost_usd,
                        duration_ms,
                        num_turns,
                        input_tokens: if accum_input_tokens > 0 {
                            Some(accum_input_tokens)
                        } else {
                            None
                        },
                        output_tokens: if accum_output_tokens > 0 {
                            Some(accum_output_tokens)
                        } else {
                            None
                        },
                    });
                }
            }

            debug_log("  Calling parse_stream_message...");
            if let Some(msg) = parse_stream_message(&json) {
                debug_log(&format!(
                    "  Parsed message variant: {:?}",
                    std::mem::discriminant(&msg)
                ));

                // Track session_id and final result for Done message
                match &msg {
                    StreamMessage::Init { session_id } => {
                        debug_log(&format!("  >>> Init: session_id={}", session_id));
                        last_session_id = Some(session_id.clone());
                    }
                    StreamMessage::Text { content } => {
                        let preview: String = content.chars().take(100).collect();
                        debug_log(&format!(
                            "  >>> Text: {} chars, preview: {:?}",
                            content.len(),
                            preview
                        ));
                    }
                    StreamMessage::ToolUse { name, input } => {
                        let input_preview: String = input.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolUse: name={}, input_preview={:?}",
                            name, input_preview
                        ));
                    }
                    StreamMessage::ToolResult { content, is_error } => {
                        let content_preview: String = content.chars().take(200).collect();
                        debug_log(&format!(
                            "  >>> ToolResult: is_error={}, content_len={}, preview={:?}",
                            is_error,
                            content.len(),
                            content_preview
                        ));
                    }
                    StreamMessage::Done { result, session_id } => {
                        let result_preview: String = result.chars().take(100).collect();
                        debug_log(&format!(
                            "  >>> Done: result_len={}, session_id={:?}, preview={:?}",
                            result.len(),
                            session_id,
                            result_preview
                        ));
                        final_result = Some(result.clone());
                        if session_id.is_some() {
                            last_session_id = session_id.clone();
                        }
                    }
                    StreamMessage::Error { ref message, .. } => {
                        debug_log(&format!("  >>> Error: {}", message));
                        stdout_error = Some((message.clone(), line.clone()));
                        continue; // don't send yet; will combine with stderr after process exits
                    }
                    StreamMessage::TaskNotification {
                        task_id,
                        status,
                        summary,
                    } => {
                        debug_log(&format!(
                            "  >>> TaskNotification: task_id={}, status={}, summary={}",
                            task_id, status, summary
                        ));
                    }
                    StreamMessage::StatusUpdate {
                        ref model,
                        cost_usd,
                        total_cost_usd,
                        ..
                    } => {
                        debug_log(&format!(
                            "  >>> StatusUpdate: model={:?}, cost={:?}, total_cost={:?}",
                            model, cost_usd, total_cost_usd
                        ));
                    }
                    StreamMessage::TmuxReady { .. } => {
                        debug_log("  >>> TmuxReady (ignored in direct execution)");
                    }
                }

                // Send message to channel
                debug_log("  Sending message to channel...");
                let send_result = sender.send(msg);
                if send_result.is_err() {
                    debug_log("  ERROR: Channel send failed (receiver dropped)");
                    break;
                }
                debug_log("  Message sent to channel successfully");
            } else {
                debug_log(&format!(
                    "  parse_stream_message returned None for type={}",
                    msg_type
                ));
            }
        } else {
            let invalid_preview: String = line.chars().take(200).collect();
            debug_log(&format!("  NOT valid JSON: {}", invalid_preview));
        }
    }

    debug_log("--- Exited lines loop ---");
    debug_log(&format!("Total lines read: {}", line_count));
    debug_log(&format!("final_result present: {}", final_result.is_some()));
    debug_log(&format!("last_session_id: {:?}", last_session_id));

    // Check cancel token after exiting the loop
    if let Some(ref token) = cancel_token {
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            debug_log("Cancel detected after loop — killing child process tree");
            kill_child_tree(&mut child);
            return Ok(());
        }
    }

    // Wait for process to finish
    debug_log("Waiting for child process to finish (child.wait())...");
    let wait_start = std::time::Instant::now();
    let status = child.wait().map_err(|e| {
        debug_log(&format!(
            "ERROR: Process wait failed after {:?}: {}",
            wait_start.elapsed(),
            e
        ));
        format!("Process error: {}", e)
    })?;
    debug_log(&format!(
        "Process finished in {:?}, status: {:?}, exit_code: {:?}",
        wait_start.elapsed(),
        status,
        status.code()
    ));

    // Handle stdout error or non-zero exit code
    if stdout_error.is_some() || !status.success() {
        let stderr_msg = child
            .stderr
            .take()
            .and_then(|s| std::io::read_to_string(s).ok())
            .unwrap_or_default();

        let (message, stdout_raw) = if let Some((msg, raw)) = stdout_error {
            (msg, raw)
        } else {
            (
                format!("Process exited with code {:?}", status.code()),
                String::new(),
            )
        };

        debug_log(&format!(
            "Sending error: message={}, exit_code={:?}",
            message,
            status.code()
        ));
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: stdout_raw,
            stderr: stderr_msg,
            exit_code: status.code(),
        });
        return Ok(());
    }

    // If we didn't get a proper Done message, send one now
    if final_result.is_none() {
        debug_log("No Done message received, sending synthetic Done message...");
        let send_result = sender.send(StreamMessage::Done {
            result: String::new(),
            session_id: last_session_id.clone(),
        });
        debug_log(&format!(
            "Synthetic Done message sent, result={:?}",
            send_result.is_ok()
        ));
    } else {
        debug_log("Done message was already received, not sending synthetic one");
    }

    debug_log("========================================");
    debug_log("=== execute_command_streaming END (success) ===");
    debug_log("========================================");
    Ok(())
}

/// Shared state for processing stream-json lines from Claude.
/// Used by both local and remote execution paths.
pub struct StreamLineState {
    pub last_session_id: Option<String>,
    pub last_model: Option<String>,
    pub accum_input_tokens: u64,
    pub accum_output_tokens: u64,
    pub final_result: Option<String>,
    pub stdout_error: Option<(String, String)>,
}

impl StreamLineState {
    pub fn new() -> Self {
        Self {
            last_session_id: None,
            last_model: None,
            accum_input_tokens: 0,
            accum_output_tokens: 0,
            final_result: None,
            stdout_error: None,
        }
    }
}

/// Process a single stream-json line. Returns false if the sender channel is disconnected.
/// Sets `stdout_error` in state for error messages (these are deferred until process exit).
fn process_stream_line(
    line: &str,
    sender: &Sender<StreamMessage>,
    state: &mut StreamLineState,
) -> bool {
    if line.trim().is_empty() {
        return true;
    }

    let json = match serde_json::from_str::<Value>(line) {
        Ok(j) => j,
        Err(_) => return true,
    };

    let msg_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Extract model name and token usage from assistant messages
    if msg_type == "assistant" {
        if let Some(msg_obj) = json.get("message") {
            if let Some(model) = msg_obj.get("model").and_then(|v| v.as_str()) {
                state.last_model = Some(model.to_string());
            }
            if let Some(usage) = msg_obj.get("usage") {
                if let Some(inp) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                    state.accum_input_tokens += inp;
                }
                if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                    state.accum_output_tokens += out;
                }
            }
        }
    }

    // Extract statusline info from result events
    if msg_type == "result" {
        let cost_usd = json.get("cost_usd").and_then(|v| v.as_f64());
        let total_cost_usd = json.get("total_cost_usd").and_then(|v| v.as_f64());
        let duration_ms = json.get("duration_ms").and_then(|v| v.as_u64());
        let num_turns = json
            .get("num_turns")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        if cost_usd.is_some() || total_cost_usd.is_some() || state.last_model.is_some() {
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: state.last_model.clone(),
                cost_usd,
                total_cost_usd,
                duration_ms,
                num_turns,
                input_tokens: if state.accum_input_tokens > 0 {
                    Some(state.accum_input_tokens)
                } else {
                    None
                },
                output_tokens: if state.accum_output_tokens > 0 {
                    Some(state.accum_output_tokens)
                } else {
                    None
                },
            });
        }
    }

    if let Some(msg) = parse_stream_message(&json) {
        // Track session_id and final result
        match &msg {
            StreamMessage::Init { session_id } => {
                state.last_session_id = Some(session_id.clone());
            }
            StreamMessage::Done { result, session_id } => {
                state.final_result = Some(result.clone());
                if session_id.is_some() {
                    state.last_session_id = session_id.clone();
                }
            }
            StreamMessage::Error { ref message, .. } => {
                state.stdout_error = Some((message.clone(), line.to_string()));
                return true; // don't send yet; will combine with stderr after process exits
            }
            _ => {}
        }

        if sender.send(msg).is_err() {
            return false; // channel disconnected
        }
    }

    true
}

/// Shell-escape a string using single quotes (POSIX safe).
/// Internal single quotes are replaced with `'\''`.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Execute claude command on a remote host via SSH, streaming stdout lines
/// back through the sender channel.
fn execute_streaming_remote(
    profile: &RemoteProfile,
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    use crate::services::remote::{RemoteAuth, SshHandler};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    debug_log("=== execute_streaming_remote START ===");
    debug_log(&format!(
        "Remote host: {}@{}:{}",
        profile.user, profile.host, profile.port
    ));

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

    let profile = profile.clone();
    let args = args.to_vec();
    let prompt = prompt.to_string();
    let working_dir = working_dir.to_string();

    // Shared cancel flag for SSH
    let ssh_cancel_flag = Arc::new(AtomicBool::new(false));
    if let Some(ref token) = cancel_token {
        *token.ssh_cancel.lock().unwrap() = Some(ssh_cancel_flag.clone());
    }

    let ssh_cancel = ssh_cancel_flag.clone();
    let cancel_token_inner = cancel_token.clone();

    runtime.block_on(async move {
        // Connect
        let config = russh::client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 10,
            ..Default::default()
        };

        let mut ssh = russh::client::connect(
            Arc::new(config),
            (profile.host.as_str(), profile.port),
            SshHandler,
        )
        .await
        .map_err(|e| format!("SSH connection failed: {}", e))?;

        // Authenticate
        let auth_result = match &profile.auth {
            RemoteAuth::Password { password } => {
                ssh.authenticate_password(&profile.user, password)
                    .await
                    .map_err(|e| format!("Password auth failed: {}", e))?
            }
            RemoteAuth::KeyFile { path, passphrase } => {
                let key_path = if path.starts_with('~') {
                    if let Some(home) = dirs::home_dir() {
                        home.join(path.trim_start_matches('~').trim_start_matches('/'))
                    } else {
                        std::path::PathBuf::from(path)
                    }
                } else {
                    std::path::PathBuf::from(path)
                };

                let key_pair = if let Some(pass) = passphrase {
                    russh_keys::load_secret_key(&key_path, Some(pass))
                        .map_err(|e| format!("Failed to load key: {}", e))?
                } else {
                    russh_keys::load_secret_key(&key_path, None)
                        .map_err(|e| format!("Failed to load key: {}", e))?
                };

                ssh.authenticate_publickey(&profile.user, Arc::new(key_pair))
                    .await
                    .map_err(|e| format!("Key auth failed: {}", e))?
            }
        };

        if !auth_result {
            return Err("Authentication rejected by server".to_string());
        }

        debug_log("SSH authenticated, opening session channel...");

        let mut channel = ssh.channel_open_session()
            .await
            .map_err(|e| format!("Failed to open channel: {}", e))?;

        // Build remote command string
        let claude_bin = profile.claude_path.as_deref().unwrap_or("claude");

        // Build escaped args
        let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();

        // Source shell profile to get PATH (SSH non-login shell doesn't load it)
        // For tilde paths (~ or ~/...), don't shell-escape so tilde expansion works
        let cd_part = if working_dir == "~" {
            String::new()
        } else if working_dir.starts_with("~/") {
            format!("cd {} && ", working_dir)
        } else {
            format!("cd {} && ", shell_escape(&working_dir))
        };
        let cmd = format!(
            "{{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null; {}CLAUDE_CODE_MAX_OUTPUT_TOKENS=64000 BASH_DEFAULT_TIMEOUT_MS=86400000 BASH_MAX_TIMEOUT_MS=86400000 {} {}",
            cd_part,
            claude_bin,
            escaped_args.join(" ")
        );

        debug_log(&format!("Remote command: {}", &cmd[..cmd.len().min(300)]));

        channel.exec(true, cmd)
            .await
            .map_err(|e| format!("Failed to exec command: {}", e))?;

        // Write prompt to stdin, then close stdin
        channel.data(&prompt.into_bytes()[..])
            .await
            .map_err(|e| format!("Failed to send stdin: {}", e))?;
        channel.eof()
            .await
            .map_err(|e| format!("Failed to close stdin: {}", e))?;

        debug_log("Prompt written to remote stdin, reading output...");

        // Read output
        let mut line_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let mut exit_status: Option<u32> = None;
        let mut line_state = StreamLineState::new();

        while let Some(msg) = channel.wait().await {
            // Check cancellation
            if let Some(ref token) = cancel_token_inner {
                if token.cancelled.load(Ordering::Relaxed) {
                    debug_log("Cancel detected — closing SSH channel");
                    ssh_cancel.store(true, Ordering::Relaxed);
                    let _ = channel.close().await;
                    return Ok(());
                }
            }

            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    // Accumulate stdout bytes and process complete lines
                    line_buf.extend_from_slice(data);

                    // Process complete lines
                    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = line_buf.drain(..=pos).collect();
                        if let Ok(line) = String::from_utf8(line_bytes) {
                            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                            if !process_stream_line(trimmed, &sender, &mut line_state) {
                                debug_log("Channel disconnected, stopping remote read");
                                let _ = channel.close().await;
                                return Ok(());
                            }
                        }
                    }
                }
                russh::ChannelMsg::ExtendedData { data, ext } => {
                    if ext == 1 {
                        stderr_buf.extend_from_slice(&data);
                    }
                }
                russh::ChannelMsg::ExitStatus { exit_status: s } => {
                    exit_status = Some(s);
                }
                _ => {}
            }
        }

        // Process any remaining data in the buffer
        if !line_buf.is_empty() {
            if let Ok(line) = String::from_utf8(line_buf) {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                let _ = process_stream_line(trimmed, &sender, &mut line_state);
            }
        }

        debug_log(&format!("Remote process exit_status: {:?}", exit_status));

        // Handle errors
        let success = exit_status.map_or(false, |s| s == 0);
        if line_state.stdout_error.is_some() || !success {
            let stderr_msg = String::from_utf8_lossy(&stderr_buf).to_string();
            let (message, stdout_raw) = if let Some((msg, raw)) = line_state.stdout_error {
                (msg, raw)
            } else {
                (format!("Remote process exited with code {:?}", exit_status), String::new())
            };
            let _ = sender.send(StreamMessage::Error {
                message,
                stdout: stdout_raw,
                stderr: stderr_msg,
                exit_code: exit_status.map(|s| s as i32),
            });
            return Ok(());
        }

        // If we didn't get a proper Done message, send one
        if line_state.final_result.is_none() {
            let _ = sender.send(StreamMessage::Done {
                result: String::new(),
                session_id: line_state.last_session_id,
            });
        }

        debug_log("=== execute_streaming_remote END (success) ===");
        Ok(())
    })
}

/// Parse a stream-json line into a StreamMessage
fn parse_stream_message(json: &Value) -> Option<StreamMessage> {
    let msg_type = json.get("type")?.as_str()?;

    match msg_type {
        "system" => {
            // {"type":"system","subtype":"init","session_id":"..."}
            // {"type":"system","subtype":"task_notification","task_id":"...","status":"...","summary":"..."}
            let subtype = json.get("subtype").and_then(|v| v.as_str())?;
            match subtype {
                "init" => {
                    let session_id = json.get("session_id")?.as_str()?.to_string();
                    Some(StreamMessage::Init { session_id })
                }
                "task_notification" => {
                    let task_id = json
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = json
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let summary = json
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(StreamMessage::TaskNotification {
                        task_id,
                        status,
                        summary,
                    })
                }
                _ => None,
            }
        }
        "assistant" => {
            // {"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}
            // or {"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{...}}]}}
            let content = json.get("message")?.get("content")?.as_array()?;

            for item in content {
                let item_type = item.get("type")?.as_str()?;
                match item_type {
                    "text" => {
                        let text = item.get("text")?.as_str()?.to_string();
                        return Some(StreamMessage::Text { content: text });
                    }
                    "tool_use" => {
                        let name = item.get("name")?.as_str()?.to_string();
                        let input = item
                            .get("input")
                            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                            .unwrap_or_default();
                        return Some(StreamMessage::ToolUse { name, input });
                    }
                    _ => {}
                }
            }
            None
        }
        "user" => {
            // {"type":"user","message":{"content":[{"type":"tool_result","content":"..." or [array]}]}}
            let content = json.get("message")?.get("content")?.as_array()?;

            for item in content {
                let item_type = item.get("type")?.as_str()?;
                if item_type == "tool_result" {
                    // content can be a string or an array of text items
                    let content_text = if let Some(s) = item.get("content").and_then(|v| v.as_str())
                    {
                        s.to_string()
                    } else if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
                        // Extract text from array: [{"type":"text","text":"..."},...]
                        arr.iter()
                            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    };
                    let is_error = item
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    return Some(StreamMessage::ToolResult {
                        content: content_text,
                        is_error,
                    });
                }
            }
            None
        }
        "result" => {
            // {"type":"result","subtype":"error_during_execution","is_error":true,"errors":["..."]}
            // {"type":"result","subtype":"success","result":"...","session_id":"..."}
            let is_error = json
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_error {
                let errors_raw = json.get("errors");
                let result_raw = json.get("result").and_then(|v| v.as_str());
                // Try "errors" array first, then fall back to "result" field
                let error_msg = errors_raw
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .or_else(|| result_raw.map(|s| s.to_string()))
                    .unwrap_or_else(|| "Unknown error".to_string());
                return Some(StreamMessage::Error {
                    message: error_msg,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            let result = json
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(StreamMessage::Done { result, session_id })
        }
        _ => None,
    }
}

/// Check if tmux is available on the system
pub fn is_tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Sanitize a name for use as a tmux session name.
/// Replaces non-alphanumeric characters (except - and _) with -.
pub fn sanitize_tmux_session_name(channel_name: &str) -> String {
    let sanitized: String = channel_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = &sanitized[..sanitized.len().min(50)];
    format!("remoteCC-{}", trimmed)
}

/// Execute Claude inside a local tmux session with bidirectional input.
///
/// If a tmux session with this name already exists, sends the prompt as a
/// follow-up message to the running Claude process. Otherwise creates a new session.
///
/// Communication:
/// - Output: wrapper appends JSON lines to a file; parent reads with polling
/// - Input (Discord→Claude): parent writes stream-json to INPUT_FIFO
/// - Input (terminal→Claude): wrapper reads stdin directly
fn execute_streaming_local_tmux(
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<(), String> {
    debug_log(&format!(
        "=== execute_streaming_local_tmux START: {} ===",
        tmux_session_name
    ));

    let output_path = format!("/tmp/remotecc-{}.jsonl", tmux_session_name);
    let input_fifo_path = format!("/tmp/remotecc-{}.input", tmux_session_name);
    let prompt_path = format!("/tmp/remotecc-{}.prompt", tmux_session_name);

    // Check if tmux session already exists (follow-up to running session)
    let session_exists = Command::new("tmux")
        .args(["has-session", "-t", tmux_session_name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if session_exists {
        debug_log("Existing tmux session found — sending follow-up message");
        return send_followup_to_tmux(
            prompt,
            &output_path,
            &input_fifo_path,
            sender,
            cancel_token,
            tmux_session_name,
        );
    }

    // === Create new tmux session ===
    debug_log("No existing tmux session — creating new one");

    // Clean up any leftover files
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&input_fifo_path);
    let _ = std::fs::remove_file(&prompt_path);

    // Create output file (empty)
    std::fs::write(&output_path, "").map_err(|e| format!("Failed to create output file: {}", e))?;

    // Create input FIFO
    let mkfifo = Command::new("mkfifo")
        .arg(&input_fifo_path)
        .output()
        .map_err(|e| format!("Failed to create input FIFO: {}", e))?;
    if !mkfifo.status.success() {
        let _ = std::fs::remove_file(&output_path);
        return Err(format!(
            "mkfifo failed: {}",
            String::from_utf8_lossy(&mkfifo.stderr)
        ));
    }

    // Write prompt to temp file
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    // Get paths
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let claude_bin = get_claude_path().ok_or_else(|| "Claude CLI not found".to_string())?;

    // Build wrapper command
    let mut wrapper_parts = vec![
        shell_escape(&exe.display().to_string()),
        "--tmux-wrapper".to_string(),
        "--output-file".to_string(),
        shell_escape(&output_path),
        "--input-fifo".to_string(),
        shell_escape(&input_fifo_path),
        "--prompt-file".to_string(),
        shell_escape(&prompt_path),
        "--cwd".to_string(),
        shell_escape(working_dir),
        "--".to_string(),
        claude_bin.to_string(),
    ];
    for arg in args {
        wrapper_parts.push(shell_escape(arg));
    }
    let wrapper_cmd = wrapper_parts.join(" ");

    debug_log(&format!(
        "Wrapper cmd: {}",
        &wrapper_cmd[..wrapper_cmd.len().min(300)]
    ));

    // Launch tmux session (remove CLAUDECODE so nested claude invocations work)
    let wrapper_cmd_with_env = format!("env -u CLAUDECODE {}", wrapper_cmd);
    let tmux_result = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux_session_name,
            "-c",
            working_dir,
            &wrapper_cmd_with_env,
        ])
        .env_remove("CLAUDECODE")
        .output()
        .map_err(|e| format!("Failed to create tmux session: {}", e))?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        let _ = std::fs::remove_file(&output_path);
        let _ = std::fs::remove_file(&input_fifo_path);
        let _ = std::fs::remove_file(&prompt_path);
        return Err(format!("tmux error: {}", stderr));
    }

    debug_log("tmux session created, storing in cancel token...");

    // Store tmux session name in cancel token
    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    // Read output file from beginning (new session), with retry on session death
    const MAX_RETRIES: u32 = 2;
    let mut attempt = 0u32;

    loop {
        let read_result = read_output_file_until_result(
            &output_path,
            0,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
        )?;

        match read_result {
            ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
                // Normal completion or user cancel — notify caller
                let _ = sender.send(StreamMessage::TmuxReady {
                    output_path,
                    input_fifo_path,
                    tmux_session_name: tmux_session_name.to_string(),
                    last_offset: offset,
                });
                return Ok(());
            }
            ReadOutputResult::SessionDied { .. } => {
                attempt += 1;
                if attempt > MAX_RETRIES {
                    debug_log(&format!(
                        "tmux session died {} times, giving up",
                        attempt
                    ));
                    let _ = sender.send(StreamMessage::Done {
                        result: "⚠ tmux 세션이 반복 종료되었습니다. 다시 시도해 주세요."
                            .to_string(),
                        session_id: None,
                    });
                    return Ok(());
                }

                debug_log(&format!(
                    "tmux session died, retrying ({}/{})",
                    attempt, MAX_RETRIES
                ));

                // Wait before retry
                std::thread::sleep(std::time::Duration::from_secs(2));

                // Kill stale session if lingering
                let _ = Command::new("tmux")
                    .args(["kill-session", "-t", tmux_session_name])
                    .output();

                // Clean up and recreate temp files
                let _ = std::fs::remove_file(&output_path);
                let _ = std::fs::remove_file(&input_fifo_path);
                let _ = std::fs::remove_file(&prompt_path);

                std::fs::write(&output_path, "")
                    .map_err(|e| format!("Failed to recreate output file: {}", e))?;

                let mkfifo = Command::new("mkfifo")
                    .arg(&input_fifo_path)
                    .output()
                    .map_err(|e| format!("Failed to recreate input FIFO: {}", e))?;
                if !mkfifo.status.success() {
                    return Err(format!(
                        "mkfifo failed on retry: {}",
                        String::from_utf8_lossy(&mkfifo.stderr)
                    ));
                }

                std::fs::write(&prompt_path, prompt)
                    .map_err(|e| format!("Failed to rewrite prompt file: {}", e))?;

                // Re-launch tmux session
                let wrapper_cmd_retry = format!("env -u CLAUDECODE {}", wrapper_cmd);
                let tmux_retry = Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        tmux_session_name,
                        "-c",
                        working_dir,
                        &wrapper_cmd_retry,
                    ])
                    .env_remove("CLAUDECODE")
                    .output()
                    .map_err(|e| format!("Failed to recreate tmux session: {}", e))?;

                if !tmux_retry.status.success() {
                    let stderr = String::from_utf8_lossy(&tmux_retry.stderr);
                    return Err(format!("tmux retry error: {}", stderr));
                }

                debug_log("tmux session re-created, retrying read...");
            }
        }
    }
}

/// Send a follow-up message to an existing tmux Claude session.
fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<(), String> {
    use std::io::Write;

    debug_log(&format!(
        "=== send_followup_to_tmux: {} ===",
        tmux_session_name
    ));

    // Get current output file size (we'll read from this offset)
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    debug_log(&format!("Output file offset: {}", start_offset));

    // Format prompt as stream-json
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": prompt
        }
    });

    // Write to input FIFO (blocks briefly until wrapper's reader is ready)
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))?;

    writeln!(fifo, "{}", msg).map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
    fifo.flush()
        .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
    drop(fifo);

    debug_log("Follow-up message sent to input FIFO");

    // Store tmux session name in cancel token
    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    // Read output file from the offset
    let read_result = read_output_file_until_result(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        tmux_session_name,
    )?;

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            // Notify caller that tmux session is ready for background monitoring
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            debug_log("tmux session died during follow-up");
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

    Ok(())
}

/// Poll-read the output file from a given offset until a "result" event is received.
/// Uses raw File::read to handle growing file (not BufReader which caches EOF).
/// Returns ReadOutputResult indicating how the read ended.
fn read_output_file_until_result(
    output_path: &str,
    start_offset: u64,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<ReadOutputResult, String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    debug_log(&format!(
        "=== read_output_file_until_result: offset={} ===",
        start_offset
    ));

    // Wait for output file to exist (wrapper might not have created it yet)
    let wait_start = std::time::Instant::now();
    loop {
        if std::fs::metadata(output_path).is_ok() {
            break;
        }
        if wait_start.elapsed() > Duration::from_secs(30) {
            return Err("Timeout waiting for output file".to_string());
        }
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(Ordering::Relaxed) {
                return Ok(ReadOutputResult::Cancelled { offset: start_offset });
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut file = std::fs::File::open(output_path)
        .map_err(|e| format!("Failed to open output file: {}", e))?;
    file.seek(SeekFrom::Start(start_offset))
        .map_err(|e| format!("Failed to seek output file: {}", e))?;

    let mut current_offset = start_offset;
    let mut partial_line = String::new();
    let mut state = StreamLineState::new();
    let mut buf = [0u8; 8192];
    let mut no_data_count: u32 = 0;

    loop {
        // Check cancellation
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(Ordering::Relaxed) {
                debug_log("Cancel detected during output file read");
                return Ok(ReadOutputResult::Cancelled { offset: current_offset });
            }
        }

        match file.read(&mut buf) {
            Ok(0) => {
                // No new data — check if tmux session is still alive
                no_data_count += 1;
                if no_data_count % 50 == 0 {
                    // Every ~5 seconds
                    let alive = Command::new("tmux")
                        .args(["has-session", "-t", tmux_session_name])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !alive {
                        debug_log("tmux session ended while reading output");
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                no_data_count = 0;
                current_offset += n as u64;
                partial_line.push_str(&String::from_utf8_lossy(&buf[..n]));

                // Process complete lines
                while let Some(pos) = partial_line.find('\n') {
                    let line: String = partial_line.drain(..=pos).collect();
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    if !process_stream_line(trimmed, &sender, &mut state) {
                        debug_log("Channel disconnected during output file read");
                        return Ok(ReadOutputResult::Cancelled { offset: current_offset });
                    }

                    // Check if we got a result (turn complete)
                    if state.final_result.is_some() {
                        debug_log("Result received — returning from output file read");
                        return Ok(ReadOutputResult::Completed { offset: current_offset });
                    }
                }
            }
            Err(e) => {
                debug_log(&format!("Error reading output file: {}", e));
                break;
            }
        }
    }

    // Handle deferred error or missing Done message
    if let Some((message, stdout_raw)) = state.stdout_error {
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: stdout_raw,
            stderr: String::new(),
            exit_code: None,
        });
    }

    debug_log("=== read_output_file_until_result END (session died) ===");
    Ok(ReadOutputResult::SessionDied { offset: current_offset })
}

/// Execute Claude inside a tmux session on a remote host via SSH.
///
/// For new sessions: SSH carries prompt file, creates tmux, tails output file.
/// For follow-ups: SSH writes to input FIFO, tails output file from offset.
fn execute_streaming_remote_tmux(
    profile: &RemoteProfile,
    args: &[String],
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<(), String> {
    use crate::services::remote::{RemoteAuth, SshHandler};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    debug_log(&format!(
        "=== execute_streaming_remote_tmux START: {} ===",
        tmux_session_name
    ));

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

    let profile = profile.clone();
    let args = args.to_vec();
    let prompt = prompt.to_string();
    let working_dir = working_dir.to_string();
    let tmux_name = tmux_session_name.to_string();

    let ssh_cancel_flag = Arc::new(AtomicBool::new(false));
    if let Some(ref token) = cancel_token {
        *token.ssh_cancel.lock().unwrap() = Some(ssh_cancel_flag.clone());
        *token.tmux_session.lock().unwrap() = Some(tmux_name.clone());
    }

    let ssh_cancel = ssh_cancel_flag.clone();
    let cancel_token_inner = cancel_token.clone();

    runtime.block_on(async move {
        // Connect & authenticate
        let config = russh::client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 10,
            ..Default::default()
        };

        let mut ssh = russh::client::connect(
            Arc::new(config),
            (profile.host.as_str(), profile.port),
            SshHandler,
        )
        .await
        .map_err(|e| format!("SSH connection failed: {}", e))?;

        let auth_result = match &profile.auth {
            RemoteAuth::Password { password } => {
                ssh.authenticate_password(&profile.user, password)
                    .await
                    .map_err(|e| format!("Password auth failed: {}", e))?
            }
            RemoteAuth::KeyFile { path, passphrase } => {
                let key_path = if path.starts_with('~') {
                    if let Some(home) = dirs::home_dir() {
                        home.join(path.trim_start_matches('~').trim_start_matches('/'))
                    } else {
                        std::path::PathBuf::from(path)
                    }
                } else {
                    std::path::PathBuf::from(path)
                };

                let key_pair = if let Some(pass) = passphrase {
                    russh_keys::load_secret_key(&key_path, Some(pass))
                        .map_err(|e| format!("Failed to load key: {}", e))?
                } else {
                    russh_keys::load_secret_key(&key_path, None)
                        .map_err(|e| format!("Failed to load key: {}", e))?
                };

                ssh.authenticate_publickey(&profile.user, Arc::new(key_pair))
                    .await
                    .map_err(|e| format!("Key auth failed: {}", e))?
            }
        };

        if !auth_result {
            return Err("Authentication rejected by server".to_string());
        }

        debug_log("SSH authenticated for tmux, opening channel...");
        eprintln!("  [remote-tmux] SSH authenticated");

        let claude_bin = profile.claude_path.as_deref().unwrap_or("claude");
        let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();

        let cd_part = if working_dir == "~" {
            String::new()
        } else if working_dir.starts_with("~/") {
            format!("cd {} && ", working_dir)
        } else {
            format!("cd {} && ", shell_escape(&working_dir))
        };

        let output_path = format!("/tmp/remotecc-{}.jsonl", tmux_name);
        let input_fifo_path = format!("/tmp/remotecc-{}.input", tmux_name);
        let prompt_path = format!("/tmp/remotecc-{}.prompt", tmux_name);

        // Build wrapper command parts for the launch script.
        // We write a shell script to avoid nested shell-escaping issues with tmux.
        let escaped_args: Vec<String> = args.iter().map(|a| shell_escape(a)).collect();
        let script_path = format!("/tmp/remotecc-{}.sh", tmux_name);

        // Script content: source profile for PATH, then exec the wrapper
        let script_content = format!(
            "#!/bin/bash\n\
            {{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null\n\
            exec remotecc --tmux-wrapper \\\n  \
            --output-file {output} \\\n  \
            --input-fifo {input_fifo} \\\n  \
            --prompt-file {prompt} \\\n  \
            --cwd {wd} \\\n  \
            -- {claude_bin} {claude_args}\n",
            output = shell_escape(&output_path),
            input_fifo = shell_escape(&input_fifo_path),
            prompt = shell_escape(&prompt_path),
            wd = shell_escape(&working_dir),
            claude_bin = claude_bin,
            claude_args = escaped_args.join(" "),
        );

        // Encode prompt and script as base64 to embed in command (avoids stdin/escaping issues)
        use base64::Engine;
        let prompt_b64 = base64::engine::general_purpose::STANDARD.encode(prompt.as_bytes());
        let script_b64 = base64::engine::general_purpose::STANDARD.encode(script_content.as_bytes());

        // === PHASE 1: Setup channel — create/resume tmux session ===
        let is_followup;
        {
            let mut setup_channel = ssh.channel_open_session()
                .await
                .map_err(|e| format!("Failed to open setup channel: {}", e))?;

            // Setup command: check tmux, create session if needed, report status.
            // The wrapper command is written as a script file to avoid nested shell-escaping.
            // First block: detect and clean up stale sessions (dead pane or auth failure).
            // Second block: proceed with FOLLOWUP or NEW as before.
            let setup_cmd = format!(
                r#"{{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null; \
                {cd}if tmux has-session -t {name} 2>/dev/null; then \
                    _PANE_DEAD=$(tmux list-panes -t {name} -F '#{{pane_dead}}' 2>/dev/null | head -1); \
                    if [ "$_PANE_DEAD" = "1" ] || grep -q '"error":"authentication_failed"' {output} 2>/dev/null; then \
                        tmux kill-session -t {name} 2>/dev/null; \
                        pkill -f 'tail -f {output}' 2>/dev/null; \
                        rm -f {output} {input_fifo} {script} 2>/dev/null; \
                    fi; \
                fi; \
                if tmux has-session -t {name} 2>/dev/null; then \
                    echo 'FOLLOWUP'; \
                    OFFSET=$(wc -c < {output} 2>/dev/null || echo 0); \
                    echo "$OFFSET"; \
                    echo '{prompt_b64}' | base64 -d > {input_fifo}; \
                else \
                    echo '{prompt_b64}' | base64 -d > {prompt} && \
                    rm -f {output} {input_fifo} && touch {output} && mkfifo {input_fifo} && \
                    echo '{script_b64}' | base64 -d > {script} && chmod +x {script} && \
                    tmux new-session -d -s {name} {script} && \
                    echo 'NEW' || echo 'FAILED'; \
                fi"#,
                cd = cd_part,
                name = shell_escape(&tmux_name),
                output = shell_escape(&output_path),
                input_fifo = shell_escape(&input_fifo_path),
                prompt = shell_escape(&prompt_path),
                prompt_b64 = prompt_b64,
                script_b64 = script_b64,
                script = shell_escape(&script_path),
            );

            eprintln!("  [remote-tmux] Phase 1: setup ({} bytes)...", setup_cmd.len());
            setup_channel.exec(true, setup_cmd)
                .await
                .map_err(|e| format!("Failed to exec setup: {}", e))?;
            // Close stdin immediately (not needed)
            let _ = setup_channel.eof().await;

            // Read setup response
            let mut setup_output = Vec::new();
            while let Some(msg) = setup_channel.wait().await {
                if let russh::ChannelMsg::Data { ref data } = msg {
                    setup_output.extend_from_slice(data);
                }
            }
            let setup_str = String::from_utf8_lossy(&setup_output).to_string();
            let setup_lines: Vec<&str> = setup_str.trim().lines().collect();
            eprintln!("  [remote-tmux] Setup result: {:?}", setup_lines);

            is_followup = setup_lines.first().map_or(false, |l| *l == "FOLLOWUP");
            if setup_lines.first().map_or(true, |l| *l == "FAILED") && !is_followup {
                return Err("Failed to create tmux session on remote".to_string());
            }
        }

        // === PHASE 2: Streaming channel — read output via Claude directly ===
        // Use the same proven pattern as execute_streaming_remote:
        // run a single long-lived process whose stdout IS the data stream.
        let mut stream_channel = ssh.channel_open_session()
            .await
            .map_err(|e| format!("Failed to open stream channel: {}", e))?;

        // Use a simple script: source profile for PATH, then exec tail -f
        // The 'exec' replaces the shell with tail, so tail's stdout goes directly to SSH channel.
        let stream_cmd = format!(
            r#"{{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null; exec tail -f {output}"#,
            output = shell_escape(&output_path),
        );

        eprintln!("  [remote-tmux] Phase 2: streaming tail -f ...");
        stream_channel.exec(true, stream_cmd)
            .await
            .map_err(|e| format!("Failed to exec stream: {}", e))?;
        let _ = stream_channel.eof().await;

        // Read output (same pattern as execute_streaming_remote)
        let mut line_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let mut exit_status: Option<u32> = None;
        let mut line_state = StreamLineState::new();

        while let Some(msg) = stream_channel.wait().await {
            if let Some(ref token) = cancel_token_inner {
                if token.cancelled.load(Ordering::Relaxed) {
                    debug_log("Cancel detected — closing SSH channel");
                    ssh_cancel.store(true, Ordering::Relaxed);
                    let _ = stream_channel.close().await;
                    return Ok(());
                }
            }

            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    line_buf.extend_from_slice(data);
                    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = line_buf.drain(..=pos).collect();
                        if let Ok(line) = String::from_utf8(line_bytes) {
                            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                            if !process_stream_line(trimmed, &sender, &mut line_state) {
                                let _ = stream_channel.close().await;
                                return Ok(());
                            }
                            if line_state.final_result.is_some() {
                                let _ = stream_channel.close().await;
                                return Ok(());
                            }
                        }
                    }
                }
                russh::ChannelMsg::ExtendedData { data, ext } => {
                    if ext == 1 {
                        stderr_buf.extend_from_slice(&data);
                    }
                }
                russh::ChannelMsg::ExitStatus { exit_status: s } => {
                    exit_status = Some(s);
                }
                _ => {}
            }
        }

        // Process remaining buffer
        if !line_buf.is_empty() {
            if let Ok(line) = String::from_utf8(line_buf) {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                let _ = process_stream_line(trimmed, &sender, &mut line_state);
            }
        }

        // Handle errors
        let stderr_msg = String::from_utf8_lossy(&stderr_buf).to_string();
        eprintln!("  [remote-tmux] Stream ended. exit_status={:?}, stderr_len={}, tokens_in={}, has_result={}",
            exit_status, stderr_msg.len(), line_state.accum_input_tokens, line_state.final_result.is_some());
        if !stderr_msg.is_empty() {
            eprintln!("  [remote-tmux] stderr: {}", &stderr_msg[..stderr_msg.len().min(500)]);
        }
        let success = exit_status.map_or(true, |s| s == 0);
        if line_state.stdout_error.is_some() || (!success && line_state.final_result.is_none()) {
            let (message, stdout_raw) = if let Some((msg, raw)) = line_state.stdout_error {
                (msg, raw)
            } else {
                (format!("Remote tmux process exited with code {:?}", exit_status), String::new())
            };
            let _ = sender.send(StreamMessage::Error {
                message,
                stdout: stdout_raw,
                stderr: stderr_msg,
                exit_code: exit_status.map(|s| s as i32),
            });
            return Ok(());
        }

        if line_state.final_result.is_none() {
            eprintln!("  [remote-tmux] No result received, sending empty Done");
            let _ = sender.send(StreamMessage::Done {
                result: String::new(),
                session_id: line_state.last_session_id,
            });
        }

        debug_log("=== execute_streaming_remote_tmux END (success) ===");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========== is_valid_session_id tests ==========

    #[test]
    fn test_session_id_valid() {
        assert!(is_valid_session_id("abc123"));
        assert!(is_valid_session_id("session-1"));
        assert!(is_valid_session_id("session_2"));
        assert!(is_valid_session_id("ABC-XYZ_123"));
        assert!(is_valid_session_id("a")); // Single char
    }

    #[test]
    fn test_session_id_empty_rejected() {
        assert!(!is_valid_session_id(""));
    }

    #[test]
    fn test_session_id_too_long_rejected() {
        // 64 characters should be valid
        let max_len = "a".repeat(64);
        assert!(is_valid_session_id(&max_len));

        // 65 characters should be rejected
        let too_long = "a".repeat(65);
        assert!(!is_valid_session_id(&too_long));
    }

    #[test]
    fn test_session_id_special_chars_rejected() {
        assert!(!is_valid_session_id("session;rm -rf"));
        assert!(!is_valid_session_id("session'OR'1=1"));
        assert!(!is_valid_session_id("session`cmd`"));
        assert!(!is_valid_session_id("session$(cmd)"));
        assert!(!is_valid_session_id("session\nline2"));
        assert!(!is_valid_session_id("session\0null"));
        assert!(!is_valid_session_id("path/traversal"));
        assert!(!is_valid_session_id("session with space"));
        assert!(!is_valid_session_id("session.dot"));
        assert!(!is_valid_session_id("session@email"));
    }

    #[test]
    fn test_session_id_unicode_rejected() {
        assert!(!is_valid_session_id("세션아이디"));
        assert!(!is_valid_session_id("session_日本語"));
        assert!(!is_valid_session_id("émoji🎉"));
    }

    // ========== ClaudeResponse tests ==========

    #[test]
    fn test_claude_response_struct() {
        let response = ClaudeResponse {
            success: true,
            response: Some("Hello".to_string()),
            session_id: Some("abc123".to_string()),
            error: None,
        };

        assert!(response.success);
        assert_eq!(response.response, Some("Hello".to_string()));
        assert_eq!(response.session_id, Some("abc123".to_string()));
        assert!(response.error.is_none());
    }

    #[test]
    fn test_claude_response_error() {
        let response = ClaudeResponse {
            success: false,
            response: None,
            session_id: None,
            error: Some("Connection failed".to_string()),
        };

        assert!(!response.success);
        assert!(response.response.is_none());
        assert_eq!(response.error, Some("Connection failed".to_string()));
    }

    // ========== parse_claude_output tests ==========

    #[test]
    fn test_parse_claude_output_json_result() {
        let output = r#"{"session_id": "test-123", "result": "Hello, world!"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.response, Some("Hello, world!".to_string()));
        assert_eq!(response.session_id, Some("test-123".to_string()));
    }

    #[test]
    fn test_parse_claude_output_json_message() {
        let output = r#"{"session_id": "sess-456", "message": "This is a message"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.response, Some("This is a message".to_string()));
    }

    #[test]
    fn test_parse_claude_output_plain_text() {
        let output = "Just plain text response";
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(
            response.response,
            Some("Just plain text response".to_string())
        );
    }

    #[test]
    fn test_parse_claude_output_multiline() {
        let output = r#"{"session_id": "s1"}
{"result": "Final result"}"#;
        let response = parse_claude_output(output);

        assert!(response.success);
        assert_eq!(response.session_id, Some("s1".to_string()));
        assert_eq!(response.response, Some("Final result".to_string()));
    }

    #[test]
    fn test_parse_claude_output_empty() {
        let output = "";
        let response = parse_claude_output(output);

        assert!(response.success);
        // Empty output should return empty response
        assert_eq!(response.response, Some("".to_string()));
    }

    // ========== is_ai_supported tests ==========

    #[test]
    fn test_is_ai_supported() {
        #[cfg(unix)]
        assert!(is_ai_supported());

        #[cfg(not(unix))]
        assert!(!is_ai_supported());
    }

    // ========== session_id_regex tests ==========

    #[test]
    fn test_session_id_regex_caching() {
        // Multiple calls should return the same cached regex
        let regex1 = session_id_regex();
        let regex2 = session_id_regex();

        // Both should point to the same static instance
        assert!(std::ptr::eq(regex1, regex2));
    }

    // ========== parse_stream_message tests ==========

    #[test]
    fn test_parse_stream_message_init() {
        let json: Value =
            serde_json::from_str(r#"{"type":"system","subtype":"init","session_id":"test-123"}"#)
                .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Init { session_id }) => {
                assert_eq!(session_id, "test-123");
            }
            _ => panic!("Expected Init message"),
        }
    }

    #[test]
    fn test_parse_stream_message_text() {
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello world"}]}}"#,
        )
        .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Text { content }) => {
                assert_eq!(content, "Hello world");
            }
            _ => panic!("Expected Text message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_use() {
        let json: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolUse { name, input }) => {
                assert_eq!(name, "Bash");
                assert!(input.contains("ls"));
            }
            _ => panic!("Expected ToolUse message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_result() {
        let json: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"file.txt","is_error":false}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "file.txt");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolResult message"),
        }
    }

    #[test]
    fn test_parse_stream_message_tool_result_error() {
        let json: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"Error: not found","is_error":true}]}}"#
        ).unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::ToolResult { content, is_error }) => {
                assert_eq!(content, "Error: not found");
                assert!(is_error);
            }
            _ => panic!("Expected ToolResult message with error"),
        }
    }

    #[test]
    fn test_parse_stream_message_result() {
        let json: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","result":"Done!","session_id":"sess-456"}"#,
        )
        .unwrap();

        match parse_stream_message(&json) {
            Some(StreamMessage::Done { result, session_id }) => {
                assert_eq!(result, "Done!");
                assert_eq!(session_id, Some("sess-456".to_string()));
            }
            _ => panic!("Expected Done message"),
        }
    }

    #[test]
    fn test_parse_stream_message_unknown_type() {
        let json: Value = serde_json::from_str(r#"{"type":"unknown","data":"something"}"#).unwrap();

        let msg = parse_stream_message(&json);
        assert!(msg.is_none());
    }
}
