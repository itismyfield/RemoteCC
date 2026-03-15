use crate::config;
use crate::services;
use crate::services::claude;
use crate::utils::markdown::{is_line_empty, render_markdown, MarkdownTheme};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn print_help() {
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
    println!("    --dcserver [TOKEN]      Start Discord bot server(s); without TOKEN uses bot_settings.json");
    println!(
        "    --restart-dcserver [--report-channel-id <ID> --report-provider <claude|codex> [--report-message-id <ID>]]"
    );
    println!("    --discord-sendfile <PATH> --channel <ID> --key <HASH>");
    println!("    --discord-sendmessage --channel <ID> --message <TEXT> [--key <HASH>]");
    println!("    --discord-senddm --user <ID> --message <TEXT> [--key <HASH>]");
    println!(
        "                            Send file via Discord bot (internal use, HASH = token hash)"
    );
    println!(
        "    --reset-tmux             Kill all remoteCC-* tmux sessions (local + remote profiles)"
    );
    println!("    --ismcptool <TOOL>...    Check if MCP tool(s) are registered in .claude/settings.json (CWD)");
    println!(
        "    --addmcptool <TOOL>...   Add MCP tool permission(s) to .claude/settings.json (CWD)"
    );
    println!();
    println!("HOMEPAGE: https://github.com/itismyfield/RemoteCC");
}

pub fn print_version() {
    println!("RemoteCC {}", VERSION);
}

pub fn handle_base64(encoded: &str) {
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

pub fn handle_ismcptool(tool_names: &[String]) {
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

pub fn handle_addmcptool(tool_names: &[String]) {
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

pub fn handle_reset_tmux() {
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
            if name_str.starts_with("remotecc-")
                && (name_str.ends_with(".jsonl")
                    || name_str.ends_with(".input")
                    || name_str.ends_with(".prompt"))
            {
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
    cmd.arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-p")
        .arg(profile.port.to_string())
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

pub fn handle_prompt(prompt: &str) {
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
pub fn normalize_consecutive_empty_lines(text: &str) -> String {
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

pub fn migrate_config_dir() {
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

pub fn print_goodbye_message() {
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

pub fn check_for_updates() {
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

pub fn parse_version_from_cargo_toml(content: &str) -> Option<String> {
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

pub fn is_newer_version(latest: &str, current: &str) -> bool {
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
