mod config;
mod enc;
mod error;
mod keybindings;
mod services;
mod ui;
mod utils;

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::io;
use std::time::Duration;

use crate::keybindings::PanelAction;
use crate::services::claude;
use crate::ui::app::{App, Screen};
use crate::utils::markdown::{is_line_empty, render_markdown, MarkdownTheme};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("RemoteCC {} - Multi-panel terminal file manager", VERSION);
    println!();
    println!("USAGE:");
    println!("    remotecc [OPTIONS] [PATH...]");
    println!();
    println!("ARGS:");
    println!("    [PATH...]               Open panels at given paths (max 10)");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help              Print help information");
    println!("    -v, --version           Print version information");
    println!("    --prompt <TEXT>         Send prompt to AI and print rendered response");
    println!("    --design                Enable theme hot-reload (for theme development)");
    println!("    --base64 <TEXT>         Decode base64 and print (internal use)");
    println!("    --dcserver [TOKEN]      Start Discord bot server (or set REMOTECC_TOKEN env)");
    println!("    --restart-dcserver       Restart Discord bot (reads token from bot_settings.json)");
    println!("    --discord-sendfile <PATH> --channel <ID> --key <HASH>");
    println!(
        "                            Send file via Discord bot (internal use, HASH = token hash)"
    );
    println!("    --reset-tmux             Kill all remoteCC-* tmux sessions (local + remote profiles)");
    println!("    --ismcptool <TOOL>...    Check if MCP tool(s) are registered in .claude/settings.json (CWD)");
    println!(
        "    --addmcptool <TOOL>...   Add MCP tool permission(s) to .claude/settings.json (CWD)"
    );
    println!();
    println!("HOMEPAGE: https://github.com/itismyfield/RemoteCC");
}

fn handle_base64(encoded: &str) {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    match BASE64.decode(encoded) {
        Ok(decoded) => {
            if let Ok(text) = String::from_utf8(decoded) {
                print!("{}", text);
            } else {
                std::process::exit(1);
            }
        }
        Err(_) => {
            std::process::exit(1);
        }
    }
}

fn handle_ismcptool(tool_names: &[String]) {
    let cwd = std::env::current_dir().expect("Cannot determine current directory");
    let settings_path = cwd.join(".claude").join("settings.json");

    let allow_list: Vec<String> = if settings_path.exists() {
        let content =
            std::fs::read_to_string(&settings_path).expect("Failed to read .claude/settings.json");
        let json: serde_json::Value =
            serde_json::from_str(&content).expect("Failed to parse .claude/settings.json");
        json.get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    for tool_name in tool_names {
        if allow_list.iter().any(|v| v == tool_name) {
            println!("{}: registered", tool_name);
        } else {
            println!("{}: not registered", tool_name);
        }
    }
}

fn handle_addmcptool(tool_names: &[String]) {
    let cwd = std::env::current_dir().expect("Cannot determine current directory");
    let settings_path = cwd.join(".claude").join("settings.json");

    // Read existing file or start with empty object
    let mut json: serde_json::Value = if settings_path.exists() {
        let content =
            std::fs::read_to_string(&settings_path).expect("Failed to read .claude/settings.json");
        serde_json::from_str(&content).expect("Failed to parse .claude/settings.json")
    } else {
        let _ = std::fs::create_dir_all(settings_path.parent().unwrap());
        serde_json::json!({})
    };

    let obj = json
        .as_object_mut()
        .expect("settings.json is not a JSON object");

    // Add tool to permissions.allow array
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let allow = permissions
        .as_object_mut()
        .expect("permissions is not an object")
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let allow_arr = allow.as_array_mut().expect("allow is not an array");

    // Add each tool, skipping duplicates
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for tool_name in tool_names {
        let already_exists = allow_arr
            .iter()
            .any(|v| v.as_str() == Some(tool_name.as_str()));
        if already_exists {
            skipped.push(tool_name.as_str());
        } else {
            allow_arr.push(serde_json::json!(tool_name));
            added.push(tool_name.as_str());
        }
    }

    // Save
    let content = serde_json::to_string_pretty(&json).expect("Failed to serialize JSON");
    std::fs::write(&settings_path, content).expect("Failed to write .claude/settings.json");

    for name in &added {
        println!("Added: {}", name);
    }
    for name in &skipped {
        println!("Already registered: {}", name);
    }
}

fn print_version() {
    println!("RemoteCC {}", VERSION);
}

fn handle_reset_tmux() {
    let hostname = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "local".to_string());

    // Kill local remoteCC-* sessions
    println!("🧹 [{}] Cleaning remoteCC-* tmux sessions...", hostname);
    let killed = kill_remotecc_tmux_sessions_local();
    if killed == 0 {
        println!("   No remoteCC-* sessions found.");
    } else {
        println!("   Killed {} session(s).", killed);
    }

    // Also clean /tmp/remotecc-* temp files
    let cleaned = clean_remotecc_tmp_files();
    if cleaned > 0 {
        println!("   Cleaned {} temp file(s).", cleaned);
    }

    // Kill on remote profiles
    let settings = config::Settings::load();
    for profile in &settings.remote_profiles {
        println!("🧹 [{}] Cleaning remoteCC-* tmux sessions...", profile.name);
        let killed = kill_remotecc_tmux_sessions_remote(profile);
        if killed == 0 {
            println!("   No remoteCC-* sessions found.");
        } else {
            println!("   Killed {} session(s).", killed);
        }
    }

    println!("✅ Done.");
}

fn kill_remotecc_tmux_sessions_local() -> usize {
    let output = match std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return 0,
    };

    let mut count = 0;
    for line in output.lines() {
        let name = line.trim();
        if name.starts_with("remoteCC-") {
            if std::process::Command::new("tmux")
                .args(["kill-session", "-t", name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                println!("   killed: {}", name);
                count += 1;
            }
        }
    }
    count
}

fn clean_remotecc_tmp_files() -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("remotecc-") && (name_str.ends_with(".jsonl") || name_str.ends_with(".input") || name_str.ends_with(".prompt")) {
                if std::fs::remove_file(entry.path()).is_ok() {
                    count += 1;
                }
            }
        }
    }
    count
}

fn kill_remotecc_tmux_sessions_remote(profile: &services::remote::RemoteProfile) -> usize {
    let ssh_cmd = format!(
        "tmux list-sessions -F '#{{session_name}}' 2>/dev/null | grep '^remoteCC-' | while read s; do tmux kill-session -t \"$s\" && echo \"killed:$s\"; done; rm -f /tmp/remotecc-*.jsonl /tmp/remotecc-*.input /tmp/remotecc-*.prompt 2>/dev/null; true"
    );

    let mut cmd = std::process::Command::new("ssh");
    cmd.arg("-o").arg("ConnectTimeout=5")
        .arg("-o").arg("StrictHostKeyChecking=no")
        .arg("-p").arg(profile.port.to_string())
        .arg(format!("{}@{}", profile.user, profile.host))
        .arg(&ssh_cmd);

    match cmd.output() {
        Ok(o) if o.status.success() => {
            let out = String::from_utf8_lossy(&o.stdout);
            let mut count = 0;
            for line in out.lines() {
                if let Some(name) = line.strip_prefix("killed:") {
                    println!("   killed: {}", name);
                    count += 1;
                }
            }
            count
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.trim().is_empty() {
                eprintln!("   SSH error: {}", stderr.trim());
            }
            0
        }
        Err(e) => {
            eprintln!("   SSH failed: {}", e);
            0
        }
    }
}

fn handle_restart_dcserver() {
    use services::discord::resolve_discord_token_by_hash;

    // Read bot_settings.json to find stored token(s)
    let settings_path = dirs::home_dir()
        .map(|h| h.join(".remotecc").join("bot_settings.json"))
        .expect("Cannot determine home directory");

    let content = match std::fs::read_to_string(&settings_path) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Error: ~/.remotecc/bot_settings.json not found.");
            eprintln!("Run 'remotecc --dcserver <TOKEN>' at least once first.");
            return;
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing bot_settings.json: {}", e);
            return;
        }
    };

    // Find first hash key with a token
    let obj = match json.as_object() {
        Some(o) => o,
        None => {
            eprintln!("Error: bot_settings.json is not a JSON object");
            return;
        }
    };

    let (hash_key, token) = match obj.iter().find_map(|(k, v)| {
        v.get("token")
            .and_then(|t| t.as_str())
            .map(|t| (k.clone(), t.to_string()))
    }) {
        Some(pair) => pair,
        None => {
            eprintln!("Error: no token found in bot_settings.json");
            return;
        }
    };

    println!("🔄 Restarting Discord bot server...");
    println!("   Token key: {}", hash_key);

    // Kill existing dcserver processes (match any binary name with --dcserver arg)
    let pgrep_output = std::process::Command::new("pgrep")
        .args(["-f", " --dcserver"])
        .output();

    if let Ok(output) = pgrep_output {
        if output.status.success() {
            let pids = String::from_utf8_lossy(&output.stdout);
            let my_pid = std::process::id();
            for pid_str in pids.lines() {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    if pid != my_pid {
                        println!("   Killing existing dcserver (PID {})", pid);
                        let _ = std::process::Command::new("kill")
                            .arg(pid.to_string())
                            .status();
                    }
                }
            }
            // Wait for old process to die
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    // NOTE: We intentionally do NOT kill remoteCC-* Claude work sessions here.
    // They will be reconnected by restore_tmux_watchers() after the new dcserver starts.
    // Orphan sessions (channels renamed/deleted) are cleaned up inside the bot event loop.

    // Launch new dcserver inside tmux session "remoteCC"
    // Write a launcher script to avoid token exposure in ps aux
    let launcher_path = dirs::home_dir()
        .map(|h| h.join(".remotecc").join("_launch_dcserver.sh"))
        .expect("Cannot determine home directory");

    // Use production binary at ~/.remotecc/bin/remotecc (trunk-based: separate from build output)
    let prod_bin = dirs::home_dir()
        .map(|h| h.join(".remotecc").join("bin").join("remotecc"))
        .expect("Cannot determine home directory");
    let exe = if prod_bin.exists() {
        prod_bin.display().to_string()
    } else {
        // Fallback: project build output or current exe
        let project_exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("release")
            .join("remotecc");
        if project_exe.exists() {
            project_exe.display().to_string()
        } else {
            std::env::current_exe()
                .expect("Cannot determine executable path")
                .display()
                .to_string()
        }
    };

    let script = format!(
        "#!/bin/bash\nexport REMOTECC_TOKEN='{}'\nunset CLAUDECODE\nexec {} --dcserver\n",
        token.replace('\'', "'\\''"),
        exe
    );
    std::fs::write(&launcher_path, &script).expect("Failed to write launcher script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o700))
            .expect("Failed to set script permissions");
    }

    let tmux_session = "remoteCC";

    // Kill existing tmux session if it exists
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", tmux_session])
        .output();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let child = std::process::Command::new("tmux")
        .args(["new-session", "-d", "-s", tmux_session, launcher_path.to_str().unwrap()])
        .spawn();

    // Clean up launcher script after tmux reads it
    std::thread::sleep(std::time::Duration::from_secs(1));
    let _ = std::fs::remove_file(&launcher_path);

    match child {
        Ok(_) => {
            // Verify the session exists
            let check = std::process::Command::new("tmux")
                .args(["has-session", "-t", tmux_session])
                .status();
            if check.map(|s| s.success()).unwrap_or(false) {
                println!("✅ Discord bot started in tmux session '{}'", tmux_session);
            } else {
                eprintln!("❌ tmux session '{}' failed to start. Check with: tmux a -t {}", tmux_session, tmux_session);
            }
        }
        Err(e) => {
            eprintln!("❌ Failed to start tmux session: {}", e);
        }
    }
}

fn handle_dcserver(token: String) {
    // Prevent CLAUDECODE from leaking into child tmux sessions
    std::env::remove_var("CLAUDECODE");

    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    let title = format!("  RemoteCC v{}  |  Discord Bot Server  ", VERSION);
    let width = title.chars().count();
    println!();
    println!("  ┌{}┐", "─".repeat(width));
    println!("  │{}│", title);
    println!("  └{}┘", "─".repeat(width));
    println!();
    println!("  ▸ Status : Connecting...");

    rt.block_on(async {
        println!();
        services::discord::run_bot(&token).await;
    });
}

fn handle_discord_sendfile(path: &str, channel_id: u64, hash_key: &str) {
    use crate::services::discord::resolve_discord_token_by_hash;
    let token = match resolve_discord_token_by_hash(hash_key) {
        Some(t) => t,
        None => {
            eprintln!(
                "Error: no Discord bot token found for hash key: {}",
                hash_key
            );
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async {
        match services::discord::send_file_to_channel(&token, channel_id, path).await {
            Ok(_) => println!("File sent: {}", path),
            Err(e) => {
                eprintln!("Failed to send file: {}", e);
                std::process::exit(1);
            }
        }
    });
}

fn handle_prompt(prompt: &str) {
    use crate::ui::theme::Theme;

    // Check if Claude is available
    if !claude::is_claude_available() {
        eprintln!("Error: Claude CLI is not available.");
        eprintln!("Please install Claude CLI: https://claude.ai/cli");
        return;
    }

    // Execute Claude command
    let current_dir = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let response = claude::execute_command(prompt, None, &current_dir, None);

    if !response.success {
        eprintln!(
            "Error: {}",
            response
                .error
                .unwrap_or_else(|| "Unknown error".to_string())
        );
        return;
    }

    let content = response.response.unwrap_or_default();

    // Normalize empty lines first
    let normalized = normalize_consecutive_empty_lines(&content);

    // Render markdown
    let theme = Theme::default();
    let md_theme = MarkdownTheme::from_theme(&theme);
    let lines = render_markdown(&normalized, md_theme);

    // Remove consecutive empty lines from rendered output
    let mut prev_was_empty = false;
    for line in lines {
        let is_empty = is_line_empty(&line);
        if is_empty {
            if !prev_was_empty {
                println!();
            }
            prev_was_empty = true;
        } else {
            let content: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("{}", content);
            prev_was_empty = false;
        }
    }
}

/// Normalize consecutive empty lines to maximum of one
fn normalize_consecutive_empty_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result_lines: Vec<&str> = Vec::new();
    let mut prev_was_empty = false;

    for line in lines {
        let is_empty = line.chars().all(|c| c.is_whitespace());
        if is_empty {
            if !prev_was_empty {
                result_lines.push("");
            }
            prev_was_empty = true;
        } else {
            result_lines.push(line);
            prev_was_empty = false;
        }
    }

    result_lines.join("\n")
}

fn migrate_config_dir() {
    if let Some(home) = dirs::home_dir() {
        let old_dir = home.join(".cokacdir");
        let new_dir = home.join(".remotecc");
        if old_dir.exists() && !new_dir.exists() {
            if let Err(e) = std::fs::rename(&old_dir, &new_dir) {
                eprintln!(
                    "Warning: failed to migrate ~/.cokacdir to ~/.remotecc: {}",
                    e
                );
            }
        }
    }
}

fn main() -> io::Result<()> {
    // Migrate config directory from old name
    migrate_config_dir();

    // Handle command line arguments
    let args: Vec<String> = env::args().collect();
    let mut design_mode = false;
    let mut start_paths: Vec<std::path::PathBuf> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-v" | "--version" => {
                print_version();
                return Ok(());
            }
            "--prompt" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --prompt requires a text argument");
                    eprintln!("Usage: remotecc --prompt \"your question\"");
                    return Ok(());
                }
                handle_prompt(&args[i + 1]);
                return Ok(());
            }
            "--base64" => {
                if i + 1 >= args.len() {
                    std::process::exit(1);
                }
                handle_base64(&args[i + 1]);
                return Ok(());
            }
            "--dcserver" => {
                let token = if i + 1 < args.len() {
                    args[i + 1].clone()
                } else if let Ok(t) = std::env::var("REMOTECC_TOKEN") {
                    t
                } else {
                    eprintln!("Error: --dcserver requires a token (argument or REMOTECC_TOKEN env)");
                    eprintln!("Usage: remotecc --dcserver <TOKEN>");
                    return Ok(());
                };
                handle_dcserver(token);
                return Ok(());
            }
            "--restart-dcserver" => {
                handle_restart_dcserver();
                return Ok(());
            }
            "--discord-sendfile" => {
                // Parse: --discord-sendfile <PATH> --channel <ID> --key <HASH>
                let mut file_path: Option<String> = None;
                let mut channel_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--channel" => {
                            if j + 1 < args.len() {
                                channel_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" => {
                            if j + 1 < args.len() {
                                key = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        _ if file_path.is_none() && !args[j].starts_with("--") => {
                            file_path = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (file_path, channel_id, key) {
                    (Some(fp), Some(cid), Some(k)) => {
                        handle_discord_sendfile(&fp, cid, &k);
                    }
                    _ => {
                        eprintln!("Error: --discord-sendfile requires <PATH>, --channel <ID>, and --key <HASH>");
                        eprintln!(
                            "Usage: remotecc --discord-sendfile <PATH> --channel <ID> --key <HASH>"
                        );
                    }
                }
                return Ok(());
            }
            "--ismcptool" => {
                let tool_names: Vec<String> = args[i + 1..]
                    .iter()
                    .take_while(|a| !a.starts_with('-'))
                    .cloned()
                    .collect();
                if tool_names.is_empty() {
                    eprintln!("Error: --ismcptool requires at least one tool name");
                    eprintln!("Usage: remotecc --ismcptool \"TOOL1\" \"TOOL2\" ...");
                    return Ok(());
                }
                handle_ismcptool(&tool_names);
                return Ok(());
            }
            "--addmcptool" => {
                let tool_names: Vec<String> = args[i + 1..]
                    .iter()
                    .take_while(|a| !a.starts_with('-'))
                    .cloned()
                    .collect();
                if tool_names.is_empty() {
                    eprintln!("Error: --addmcptool requires at least one tool name");
                    eprintln!("Usage: remotecc --addmcptool \"TOOL1\" \"TOOL2\" ...");
                    return Ok(());
                }
                handle_addmcptool(&tool_names);
                return Ok(());
            }
            "--reset-tmux" => {
                handle_reset_tmux();
                return Ok(());
            }
            "--tmux-wrapper" => {
                // Internal: runs inside tmux session as bidirectional Claude wrapper
                // Usage: remotecc --tmux-wrapper --output-file <PATH> --input-fifo <PATH> --prompt-file <PATH> --cwd <PATH> -- <claude-cmd...>
                let mut output_file: Option<String> = None;
                let mut input_fifo: Option<String> = None;
                let mut prompt_file: Option<String> = None;
                let mut cwd: Option<String> = None;
                let mut claude_cmd: Vec<String> = Vec::new();
                let mut j = i + 1;
                let mut after_separator = false;
                while j < args.len() {
                    if after_separator {
                        claude_cmd.push(args[j].clone());
                        j += 1;
                        continue;
                    }
                    match args[j].as_str() {
                        "--" => {
                            after_separator = true;
                            j += 1;
                        }
                        "--output-file" => {
                            output_file = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--input-fifo" => {
                            input_fifo = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--prompt-file" => {
                            prompt_file = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--cwd" => {
                            cwd = args.get(j + 1).cloned();
                            j += 2;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (output_file, input_fifo, prompt_file) {
                    (Some(of), Some(inf), Some(pf)) => {
                        let wd = cwd.unwrap_or_else(|| ".".to_string());
                        services::tmux_wrapper::run(&of, &inf, &pf, &wd, &claude_cmd);
                    }
                    _ => {
                        eprintln!("Error: --tmux-wrapper requires --output-file, --input-fifo, and --prompt-file");
                    }
                }
                return Ok(());
            }
            "--design" => {
                design_mode = true;
            }
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {}", arg);
                eprintln!("Use --help for usage information");
                return Ok(());
            }
            path => {
                // Treat as a directory path
                let p = std::path::PathBuf::from(path);
                let resolved = if p.is_absolute() {
                    p
                } else {
                    env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
                        .join(p)
                };
                start_paths.push(resolved);
            }
        }
        i += 1;
    }

    // Setup panic hook to restore terminal on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Clear screen before entering alternate screen
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Detect terminal image protocol (must be after alternate screen, before event loop)
    let picker = {
        let mut p = ratatui_image::picker::Picker::from_termios()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::new((8, 16)));
        p.guess_protocol();
        p
    };

    // Load settings and create app state
    let (settings, settings_error) = match config::Settings::load_with_error() {
        Ok(s) => (s, None),
        Err(e) => (config::Settings::default(), Some(e)),
    };
    let mut app = App::with_settings(settings);
    app.image_picker = Some(picker);
    app.design_mode = design_mode;

    // Override panels with command-line paths if provided
    if !start_paths.is_empty() {
        app.set_panels_from_paths(start_paths);
    }

    // Show settings load error if any
    if let Some(err) = settings_error {
        app.show_message(&format!("Settings error: {} (using defaults)", err));
    }

    // Show design mode message if active
    if design_mode {
        app.show_message("Design mode: theme hot-reload enabled");
    }

    // Run app
    let result = run_app(&mut terminal, &mut app);

    // Save settings before exit
    app.save_settings();

    // Save last directory for shell cd (skip remote paths)
    if !app.active_panel().is_remote() {
        let last_dir = app.active_panel().path.display().to_string();
        if let Some(config_dir) = config::Settings::config_dir() {
            let lastdir_path = config_dir.join("lastdir");
            let _ = std::fs::write(&lastdir_path, &last_dir);
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Show
    )?;

    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }

    // Print goodbye message
    print_goodbye_message();

    Ok(())
}

fn print_goodbye_message() {
    // Check for updates
    check_for_updates();

    println!("Thank you for using RemoteCC! 🙏");
    println!();
    println!("If you found this useful, consider checking out my other content:");
    println!("  📺 YouTube: https://www.youtube.com/@코드깎는노인");
    println!("  📚 Classes: https://github.com/itismyfield/RemoteCC");
    println!();
    println!("Happy coding!");
}

fn check_for_updates() {
    let current_version = env!("CARGO_PKG_VERSION");

    // Fetch latest version from GitHub (with timeout)
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "3",
            "https://raw.githubusercontent.com/itismyfield/RemoteCC/refs/heads/main/Cargo.toml",
        ])
        .output();

    let latest_version = match output {
        Ok(output) if output.status.success() => {
            let content = String::from_utf8_lossy(&output.stdout);
            parse_version_from_cargo_toml(&content)
        }
        _ => None,
    };

    if let Some(latest) = latest_version {
        if is_newer_version(&latest, current_version) {
            println!(
                "┌──────────────────────────────────────────────────────────────────────────┐"
            );
            println!(
                "│  🚀 New version available: v{} (current: v{})                            ",
                latest, current_version
            );
            println!(
                "│                                                                          │"
            );
            println!(
                "│  Update with:                                                            │"
            );
            println!("│  /bin/bash -c \"$(curl -fsSL https://github.com/itismyfield/RemoteCC/releases/latest/download/install.sh)\"      │");
            println!(
                "└──────────────────────────────────────────────────────────────────────────┘"
            );
            println!();
        }
    }
}

fn parse_version_from_cargo_toml(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("version") {
            // Parse: version = "x.x.x"
            if let Some(start) = line.find('"') {
                if let Some(end) = line.rfind('"') {
                    if start < end {
                        return Some(line[start + 1..end].to_string());
                    }
                }
            }
        }
    }
    None
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse().ok()).collect() };

    let latest_parts = parse(latest);
    let current_parts = parse(current);

    for i in 0..latest_parts.len().max(current_parts.len()) {
        let l = latest_parts.get(i).copied().unwrap_or(0);
        let c = current_parts.get(i).copied().unwrap_or(0);
        if l > c {
            return true;
        } else if l < c {
            return false;
        }
    }
    false
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        // Check if full redraw is needed (after terminal mode command like vim)
        if app.needs_full_redraw {
            terminal.clear()?;
            app.needs_full_redraw = false;
        }

        terminal.draw(|f| ui::draw::draw(f, app))?;

        // For AI screen, FileInfo with calculation, ImageViewer loading, diff comparing, file operation progress, or remote spinner, use fast polling
        let is_file_info_calculating = app.current_screen == Screen::FileInfo
            && app
                .file_info_state
                .as_ref()
                .map(|s| s.is_calculating)
                .unwrap_or(false);
        let is_image_loading = app.current_screen == Screen::ImageViewer
            && app
                .image_viewer_state
                .as_ref()
                .map(|s| s.is_loading)
                .unwrap_or(false);
        let is_diff_comparing = app.current_screen == Screen::DiffScreen
            && app
                .diff_state
                .as_ref()
                .map(|s| s.is_comparing)
                .unwrap_or(false);
        let is_dedup_active = app.current_screen == Screen::DedupScreen
            && app
                .dedup_screen_state
                .as_ref()
                .map(|s| !s.is_complete)
                .unwrap_or(false);
        let is_progress_active = app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false);
        let is_remote_spinner = app.remote_spinner.is_some();

        let poll_timeout = if is_progress_active || is_dedup_active {
            Duration::from_millis(16) // ~60fps for smooth real-time updates
        } else if is_remote_spinner {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else if app.current_screen == Screen::AIScreen
            || app.is_ai_mode()
            || is_file_info_calculating
            || is_image_loading
            || is_diff_comparing
        {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else {
            Duration::from_millis(250)
        };

        // Poll for AI responses if on AI screen or AI mode (panel)
        if app.current_screen == Screen::AIScreen || app.is_ai_mode() {
            if let Some(ref mut state) = app.ai_state {
                // poll_response()가 true를 반환하면 새 내용이 추가된 것
                let has_new_content = state.poll_response();
                if has_new_content {
                    app.refresh_panels();
                }
            }
        }

        // Poll for file info calculation if on FileInfo screen
        if app.current_screen == Screen::FileInfo {
            if let Some(ref mut state) = app.file_info_state {
                state.poll();
            }
        }

        // Poll for image loading if on ImageViewer screen
        if app.current_screen == Screen::ImageViewer {
            if let Some(ref mut state) = app.image_viewer_state {
                let was_loading = state.is_loading;
                state.poll();
                // Create inline protocol when loading completes
                if was_loading && !state.is_loading && state.image.is_some() {
                    if let Some(ref mut picker) = app.image_picker {
                        if picker.protocol_type != ratatui_image::picker::ProtocolType::Halfblocks {
                            let img = state.image.as_ref().expect("checked above").clone();
                            state.inline_protocol = Some(picker.new_resize_protocol(img));
                            state.use_inline = true;
                        }
                    }
                }
            }
        }

        // Poll for diff comparison progress if on DiffScreen
        if app.current_screen == Screen::DiffScreen {
            if let Some(ref mut state) = app.diff_state {
                let just_completed = state.poll();
                if just_completed && !state.has_differences() {
                    app.diff_state = None;
                    app.current_screen = Screen::FilePanel;
                    app.show_message("No differences found");
                }
            }
        }

        // Poll for remote spinner completion
        app.poll_remote_spinner();

        // Check for theme file changes (hot-reload, only in design mode)
        if app.design_mode && app.theme_watch_state.check_for_changes() {
            app.reload_theme();
        }

        // Poll for file operation progress
        let progress_message: Option<String> = if let Some(ref mut progress) =
            app.file_operation_progress
        {
            let still_active = progress.poll();
            if !still_active {
                // Operation completed - extract result info before releasing borrow
                let msg = if let Some(ref result) = progress.result {
                    // Special handling for Tar - show archive name
                    if progress.operation_type == crate::services::file_ops::FileOperationType::Tar
                    {
                        if result.failure_count == 0 {
                            if let Some(ref archive_name) = app.pending_tar_archive {
                                Some(format!("Created: {}", archive_name))
                            } else {
                                Some(format!("Archived {} file(s)", result.success_count))
                            }
                        } else {
                            Some(format!(
                                "Error: {}",
                                result.last_error.as_deref().unwrap_or("Archive failed")
                            ))
                        }
                    } else if progress.operation_type
                        == crate::services::file_ops::FileOperationType::Untar
                    {
                        if result.failure_count == 0 {
                            if let Some(ref extract_dir) = app.pending_extract_dir {
                                Some(format!("Extracted to: {}", extract_dir))
                            } else {
                                Some(format!("Extracted {} file(s)", result.success_count))
                            }
                        } else {
                            Some(format!(
                                "Error: {}",
                                result.last_error.as_deref().unwrap_or("Extract failed")
                            ))
                        }
                    } else {
                        let op_name = match progress.operation_type {
                            crate::services::file_ops::FileOperationType::Copy => "Copied",
                            crate::services::file_ops::FileOperationType::Move => "Moved",
                            crate::services::file_ops::FileOperationType::Tar => "Archived",
                            crate::services::file_ops::FileOperationType::Untar => "Extracted",
                            crate::services::file_ops::FileOperationType::Download => "Downloaded",
                            crate::services::file_ops::FileOperationType::Encrypt => "Encrypted",
                            crate::services::file_ops::FileOperationType::Decrypt => "Decrypted",
                        };
                        let total = result.success_count + result.failure_count;
                        if result.failure_count == 0 {
                            Some(format!("{} {} file(s)", op_name, result.success_count))
                        } else {
                            Some(format!(
                                "{} {}/{}. Error: {}",
                                op_name,
                                result.success_count,
                                total,
                                result.last_error.as_deref().unwrap_or("Unknown error")
                            ))
                        }
                    }
                } else {
                    None
                };
                msg
            } else {
                None
            }
        } else {
            None
        };

        // Handle progress completion (outside of borrow)
        if progress_message.is_some() {
            // 원격 다운로드 완료 → 편집기/뷰어 열기
            if let Some(pending) = app.pending_remote_open.take() {
                app.file_operation_progress = None;
                app.dialog = None;

                // tmp 파일 존재 확인으로 성공/실패 판단
                let tmp_exists = match &pending {
                    crate::ui::app::PendingRemoteOpen::Editor { tmp_path, .. } => tmp_path.exists(),
                    crate::ui::app::PendingRemoteOpen::ImageViewer { tmp_path } => {
                        tmp_path.exists()
                    }
                };

                if !tmp_exists {
                    if let Some(msg) = progress_message {
                        app.show_message(&msg);
                    } else {
                        app.show_message("Download failed");
                    }
                } else {
                    match pending {
                        crate::ui::app::PendingRemoteOpen::Editor {
                            tmp_path,
                            panel_index,
                            remote_path,
                        } => {
                            let mut editor = crate::ui::file_editor::EditorState::new();
                            editor.set_syntax_colors(app.theme.syntax);
                            match editor.load_file(&tmp_path) {
                                Ok(_) => {
                                    editor.remote_origin =
                                        Some(crate::ui::file_editor::RemoteEditOrigin {
                                            panel_index,
                                            remote_path,
                                        });
                                    app.editor_state = Some(editor);
                                    app.current_screen = Screen::FileEditor;
                                }
                                Err(e) => {
                                    app.show_message(&format!("Cannot open file: {}", e));
                                }
                            }
                        }
                        crate::ui::app::PendingRemoteOpen::ImageViewer { tmp_path } => {
                            if !crate::ui::image_viewer::supports_true_color() {
                                app.pending_large_image = Some(tmp_path);
                                app.dialog = Some(crate::ui::app::Dialog {
                                    dialog_type: crate::ui::app::DialogType::TrueColorWarning,
                                    input: String::new(),
                                    cursor_pos: 0,
                                    message: "Terminal doesn't support true color. Open anyway?"
                                        .to_string(),
                                    completion: None,
                                    selected_button: 1,
                                    selection: None,
                                    use_md5: false,
                                });
                            } else {
                                app.image_viewer_state =
                                    Some(crate::ui::image_viewer::ImageViewerState::new(&tmp_path));
                                app.current_screen = Screen::ImageViewer;
                            }
                        }
                    }
                }
            } else {
                if let Some(msg) = progress_message {
                    app.show_message(&msg);
                }
                // Focus on created tar archive if applicable
                if let Some(archive_name) = app.pending_tar_archive.take() {
                    app.refresh_panels();
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| f.name == archive_name)
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                // Focus on extracted directory if applicable
                } else if let Some(extract_dir) = app.pending_extract_dir.take() {
                    app.refresh_panels();
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| f.name == extract_dir)
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                // Focus on first pasted file (by panel's sorted order) if applicable
                } else if let Some(paste_names) = app.pending_paste_focus.take() {
                    app.refresh_panels();
                    // Find the first file in the panel's sorted list that matches any pasted name
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| paste_names.contains(&f.name))
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                } else {
                    app.refresh_panels();
                }
                app.file_operation_progress = None;
                app.dialog = None;
            }
        }

        // Check for key events with timeout
        if event::poll(poll_timeout)? {
            // Block all input while remote spinner is active
            if app.remote_spinner.is_some() {
                let ev = event::read()?;
                if let Event::Key(key) = ev {
                    if key.code == KeyCode::Esc {
                        app.remote_spinner = None;
                        app.show_message("Connection cancelled");
                    }
                }
                continue;
            }
            match event::read()? {
                Event::Key(key) => {
                    match app.current_screen {
                        Screen::FilePanel => {
                            if handle_panel_input(app, key.code, key.modifiers) {
                                return Ok(());
                            }
                        }
                        Screen::FileViewer => {
                            ui::file_viewer::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::FileEditor => {
                            ui::file_editor::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::FileInfo => {
                            ui::file_info::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::ProcessManager => {
                            ui::process_manager::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::Help => {
                            if ui::help::handle_input(app, key.code) {
                                app.current_screen = Screen::FilePanel;
                            }
                        }
                        Screen::AIScreen => {
                            if let Some(ref mut state) = app.ai_state {
                                if ui::ai_screen::handle_input(
                                    state,
                                    key.code,
                                    key.modifiers,
                                    &app.keybindings,
                                ) {
                                    // Save session to file before leaving
                                    state.save_session_to_file();
                                    app.current_screen = Screen::FilePanel;
                                    app.ai_state = None;
                                    // Refresh panels in case AI modified files
                                    app.refresh_panels();
                                }
                            }
                        }
                        Screen::SystemInfo => {
                            if ui::system_info::handle_input(
                                &mut app.system_info_state,
                                key.code,
                                key.modifiers,
                                &app.keybindings,
                            ) {
                                app.current_screen = Screen::FilePanel;
                            }
                        }
                        Screen::ImageViewer => {
                            // 다이얼로그가 열려있으면 다이얼로그 입력 처리
                            if app.dialog.is_some() {
                                ui::dialogs::handle_dialog_input(app, key.code, key.modifiers);
                            } else {
                                ui::image_viewer::handle_input(app, key.code, key.modifiers);
                            }
                        }
                        Screen::SearchResult => {
                            let result = ui::search_result::handle_input(
                                &mut app.search_result_state,
                                key.code,
                                key.modifiers,
                                &app.keybindings,
                            );
                            match result {
                                Some(crate::keybindings::SearchResultAction::Open) => {
                                    app.goto_search_result();
                                }
                                Some(crate::keybindings::SearchResultAction::Close) => {
                                    app.search_result_state.active = false;
                                    app.current_screen = Screen::FilePanel;
                                }
                                _ => {}
                            }
                        }
                        Screen::DiffScreen => {
                            ui::diff_screen::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::DiffFileView => {
                            ui::diff_file_view::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::GitScreen => {
                            ui::git_screen::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::DedupScreen => {
                            if let Some(ref mut state) = app.dedup_screen_state {
                                if ui::dedup_screen::handle_input(state, key.code, key.modifiers) {
                                    app.current_screen = Screen::FilePanel;
                                    app.dedup_screen_state = None;
                                    app.refresh_panels();
                                }
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    match app.current_screen {
                        Screen::AIScreen => {
                            if let Some(ref mut state) = app.ai_state {
                                ui::ai_screen::handle_paste(state, &text);
                            }
                        }
                        Screen::FilePanel => {
                            // AI mode with focus on AI panel
                            if app.is_ai_mode()
                                && app.ai_panel_index == Some(app.active_panel_index)
                            {
                                if let Some(ref mut state) = app.ai_state {
                                    ui::ai_screen::handle_paste(state, &text);
                                }
                            } else if app.dialog.is_some() {
                                ui::dialogs::handle_paste(app, &text);
                            } else if app.advanced_search_state.active {
                                ui::advanced_search::handle_paste(
                                    &mut app.advanced_search_state,
                                    &text,
                                );
                            }
                        }
                        Screen::FileEditor => {
                            ui::file_editor::handle_paste(app, &text);
                        }
                        Screen::ImageViewer => {
                            if app.dialog.is_some() {
                                ui::dialogs::handle_paste(app, &text);
                            }
                        }
                        Screen::GitScreen => {
                            if let Some(ref mut state) = app.git_screen_state {
                                ui::git_screen::handle_paste(state, &text);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn handle_panel_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    // AI 모드일 때: active_panel이 AI 패널 쪽이면 AI로 입력 전달, 아니면 파일 패널 조작
    if app.is_ai_mode() {
        let ai_has_focus = app.ai_panel_index == Some(app.active_panel_index);
        if app.keybindings.panel_action(code, modifiers) == Some(PanelAction::SwitchPanel) {
            // AI fullscreen 모드에서는 패널 전환 차단
            let ai_fullscreen = app.ai_state.as_ref().map_or(false, |s| s.ai_fullscreen);
            if !ai_fullscreen {
                app.switch_panel();
            }
            return false;
        }
        if ai_has_focus {
            if let Some(ref mut state) = app.ai_state {
                if ui::ai_screen::handle_input(state, code, modifiers, &app.keybindings) {
                    // AI 화면 종료 요청
                    app.close_ai_screen();
                }
            }
            return false;
        }
        // ai_has_focus가 false면 아래 파일 패널 로직으로 진행
    }

    // Handle advanced search dialog first
    if app.advanced_search_state.active {
        if let Some(criteria) = ui::advanced_search::handle_input(
            &mut app.advanced_search_state,
            code,
            modifiers,
            &app.keybindings,
        ) {
            app.execute_advanced_search(&criteria);
        }
        return false;
    }

    // Handle dialog input first
    if app.dialog.is_some() {
        return ui::dialogs::handle_dialog_input(app, code, modifiers);
    }

    // Look up action from keybindings
    if let Some(action) = app.keybindings.panel_action(code, modifiers) {
        match action {
            PanelAction::Quit => return true,
            PanelAction::MoveUp => app.move_cursor(-1),
            PanelAction::MoveDown => app.move_cursor(1),
            PanelAction::PageUp => app.move_cursor(-10),
            PanelAction::PageDown => app.move_cursor(10),
            PanelAction::GoHome => app.cursor_to_start(),
            PanelAction::GoEnd => app.cursor_to_end(),
            PanelAction::Open => app.enter_selected(),
            PanelAction::ParentDir => {
                if app.diff_first_panel.is_some() {
                    app.diff_first_panel = None;
                    app.show_message("Diff cancelled");
                } else {
                    app.go_to_parent();
                }
            }
            PanelAction::SwitchPanel => app.switch_panel(),
            PanelAction::SwitchPanelLeft => app.switch_panel_left(),
            PanelAction::SwitchPanelRight => app.switch_panel_right(),
            PanelAction::ToggleSelect => app.toggle_selection(),
            PanelAction::SelectAll => app.toggle_all_selection(),
            PanelAction::SelectByExtension => app.select_by_extension(),
            PanelAction::SelectUp => app.move_cursor_with_selection(-1),
            PanelAction::SelectDown => app.move_cursor_with_selection(1),
            PanelAction::Copy => app.clipboard_copy(),
            PanelAction::Cut => app.clipboard_cut(),
            PanelAction::Paste => app.clipboard_paste(),
            PanelAction::SortByName => app.toggle_sort_by_name(),
            PanelAction::SortByType => app.toggle_sort_by_type(),
            PanelAction::SortBySize => app.toggle_sort_by_size(),
            PanelAction::SortByDate => app.toggle_sort_by_date(),
            PanelAction::Help => app.show_help(),
            PanelAction::FileInfo => app.show_file_info(),
            PanelAction::Edit => app.edit_file(),
            PanelAction::Mkdir => app.show_mkdir_dialog(),
            PanelAction::Mkfile => app.show_mkfile_dialog(),
            PanelAction::Delete => app.show_delete_dialog(),
            PanelAction::ProcessManager => app.show_process_manager(),
            PanelAction::Rename => app.show_rename_dialog(),
            PanelAction::Tar => app.show_tar_dialog(),
            PanelAction::Search => app.show_search_dialog(),
            PanelAction::GoToPath => app.show_goto_dialog(),
            PanelAction::AddPanel => app.add_panel(),
            PanelAction::GoHomeDir => app.goto_home(),
            PanelAction::Refresh => app.refresh_panels(),
            PanelAction::GitLogDiff => app.show_git_log_diff_dialog(),
            PanelAction::StartDiff => app.start_diff(),
            PanelAction::ClosePanel => app.close_panel(),
            PanelAction::AIScreen => app.show_ai_screen(),
            PanelAction::Settings => app.show_settings_dialog(),
            PanelAction::GitScreen => app.show_git_screen(),
            PanelAction::ToggleBookmark => app.toggle_bookmark(),
            PanelAction::SetHandler => app.show_handler_dialog(),
            PanelAction::EncryptAll => app.show_encrypt_dialog(),
            PanelAction::DecryptAll => app.show_decrypt_dialog(),
            PanelAction::RemoveDuplicates => app.show_dedup_screen(),
            #[cfg(target_os = "macos")]
            PanelAction::OpenInFinder => app.open_in_finder(),
            #[cfg(target_os = "macos")]
            PanelAction::OpenInVSCode => app.open_in_vscode(),
        }
    }
    false
}
