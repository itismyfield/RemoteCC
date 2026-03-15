use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use crate::services::claude::{
    self, process_stream_line, read_output_file_until_result, shell_escape, CancelToken,
    ReadOutputResult, StreamLineState, StreamMessage,
};
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::provider::ProviderKind;
use crate::services::remote::RemoteProfile;
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, tmux_session_exists,
    tmux_session_has_live_pane,
};

static CODEX_PATH: OnceLock<Option<String>> = OnceLock::new();
const TMUX_PROMPT_B64_PREFIX: &str = "__REMOTECC_B64__:";

fn resolve_codex_path() -> Option<String> {
    if let Ok(output) = Command::new("which").arg("codex").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    if let Ok(output) = Command::new("bash").args(["-lc", "which codex"]).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }

    None
}

fn get_codex_path() -> Option<&'static str> {
    CODEX_PATH.get_or_init(resolve_codex_path).as_deref()
}

use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;
    let args = base_exec_args(None, prompt);
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let output = Command::new(codex_bin)
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("Codex exited with code {:?}", output.status.code())
        } else {
            stderr
        });
    }

    let mut final_text = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if json.get("type").and_then(|v| v.as_str()) != Some("item.completed") {
            continue;
        }
        let Some(item) = json.get("item") else {
            continue;
        };
        if item.get("type").and_then(|v| v.as_str()) != Some("agent_message") {
            continue;
        }
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        if !final_text.is_empty() {
            final_text.push_str("\n\n");
        }
        final_text.push_str(text);
    }

    let final_text = final_text.trim().to_string();
    if final_text.is_empty() {
        Err("Empty response from Codex".to_string())
    } else {
        Ok(final_text)
    }
}

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
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    let prompt = compose_codex_prompt(prompt, system_prompt, allowed_tools);

    if let Some(profile) = remote_profile {
        let use_remote_tmux = tmux_session_name.is_some()
            && std::env::var("REMOTECC_CODEX_REMOTE_TMUX")
                .map(|value| {
                    let normalized = value.trim().to_ascii_lowercase();
                    normalized == "1" || normalized == "true" || normalized == "yes"
                })
                .unwrap_or(false);
        if use_remote_tmux {
            let tmux_name = tmux_session_name.expect("checked is_some above");
            return execute_streaming_remote_tmux(
                profile,
                &prompt,
                working_dir,
                sender,
                cancel_token,
                tmux_name,
                report_channel_id,
                report_provider,
            );
        }
        return execute_streaming_remote_direct(
            profile,
            session_id,
            &prompt,
            working_dir,
            sender,
            cancel_token,
        );
    }

    if let Some(tmux_name) = tmux_session_name {
        if claude::is_tmux_available() {
            return execute_streaming_local_tmux(
                &prompt,
                working_dir,
                sender,
                cancel_token,
                tmux_name,
                report_channel_id,
                report_provider,
            );
        }
    }

    execute_streaming_direct(
        &prompt,
        session_id,
        working_dir,
        sender,
        cancel_token,
        report_channel_id,
        report_provider,
    )
}

fn compose_codex_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    let mut sections = Vec::new();

    if let Some(system_prompt) = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!(
            "[Authoritative Instructions]\n{}\n\nThese instructions are authoritative for this turn. \
Follow them over any generic assistant persona unless the user explicitly asks to inspect or compare them.",
            system_prompt
        ));
    }

    if let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) {
        sections.push(format!(
            "[Tool Policy]\nIf tools are needed, stay within this allowlist unless the user explicitly asks to change it: {}",
            allowed_tools.join(", ")
        ));
    }

    if sections.is_empty() {
        return prompt.to_string();
    }

    sections.push(format!("[User Request]\n{}", prompt));
    sections.join("\n\n")
}

fn execute_streaming_direct(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;
    let args = base_exec_args(session_id, prompt);

    let mut command = Command::new(codex_bin);
    command
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(channel_id) = report_channel_id {
        command.env(RESTART_REPORT_CHANNEL_ENV, channel_id.to_string());
    }
    if let Some(provider) = report_provider {
        command.env(RESTART_REPORT_PROVIDER_ENV, provider.as_str());
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
        // Race condition fix: if /stop arrived before PID was stored, kill now
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            claude::kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Codex stdout".to_string())?;
    let reader = BufReader::new(stdout);

    let mut current_thread_id = session_id.map(str::to_string);
    let mut final_text = String::new();
    let mut saw_done = false;
    let started_at = std::time::Instant::now();

    for line in reader.lines() {
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                claude::kill_child_tree(&mut child);
                return Ok(());
            }
        }

        let line = match line {
            Ok(line) => line,
            Err(e) => return Err(format!("Failed to read Codex output: {}", e)),
        };

        if let Some(done) = handle_codex_json_line(
            &line,
            &sender,
            &mut current_thread_id,
            &mut final_text,
            started_at,
        )? {
            saw_done = saw_done || done;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for Codex: {}", e))?;

    if !output.status.success() && !saw_done {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("Codex exited with code {:?}", output.status.code())
        } else {
            stderr
        };
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: String::new(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        });
        return Ok(());
    }

    if !saw_done {
        let _ = sender.send(StreamMessage::Done {
            result: final_text,
            session_id: current_thread_id,
        });
    }

    Ok(())
}

fn execute_streaming_remote_direct(
    profile: &RemoteProfile,
    session_id: Option<&str>,
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

    let profile = profile.clone();
    let args = base_exec_args(session_id, prompt);
    let working_dir = working_dir.to_string();

    let ssh_cancel_flag = Arc::new(AtomicBool::new(false));
    if let Some(ref token) = cancel_token {
        *token.ssh_cancel.lock().unwrap() = Some(ssh_cancel_flag.clone());
    }

    let ssh_cancel = ssh_cancel_flag.clone();
    let cancel_token_inner = cancel_token.clone();

    runtime.block_on(async move {
        let ssh = crate::services::remote::ssh_connect_and_auth(&profile).await?;

        eprintln!("  [remote-codex] SSH authenticated");

        let mut channel = ssh
            .channel_open_session()
            .await
            .map_err(|e| format!("Failed to open channel: {}", e))?;

        let escaped_args: Vec<String> = args.iter().map(|arg| shell_escape(arg)).collect();
        let cd_part = if working_dir == "~" {
            String::new()
        } else if working_dir.starts_with("~/") {
            format!("cd {} && ", working_dir)
        } else {
            format!("cd {} && ", shell_escape(&working_dir))
        };
        let cmd = format!(
            "{{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null; {}codex {}",
            cd_part,
            escaped_args.join(" ")
        );

        eprintln!("  [remote-codex] exec direct codex over SSH ...");
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| format!("Failed to exec remote Codex: {}", e))?;
        let _ = channel.eof().await;

        let mut line_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let mut exit_status: Option<u32> = None;
        let mut current_thread_id = session_id.map(str::to_string);
        let mut final_text = String::new();
        let mut saw_done = false;
        let started_at = std::time::Instant::now();

        while let Some(msg) = channel.wait().await {
            if let Some(ref token) = cancel_token_inner {
                if token.cancelled.load(Ordering::Relaxed) {
                    ssh_cancel.store(true, Ordering::Relaxed);
                    let _ = channel.close().await;
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
                            if let Some(done) = handle_codex_json_line(
                                trimmed,
                                &sender,
                                &mut current_thread_id,
                                &mut final_text,
                                started_at,
                            )? {
                                saw_done = saw_done || done;
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

        if !line_buf.is_empty() {
            if let Ok(line) = String::from_utf8(line_buf) {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                if let Some(done) = handle_codex_json_line(
                    trimmed,
                    &sender,
                    &mut current_thread_id,
                    &mut final_text,
                    started_at,
                )? {
                    saw_done = saw_done || done;
                }
            }
        }

        let stderr = String::from_utf8_lossy(&stderr_buf).trim().to_string();
        let success = exit_status.map_or(true, |status| status == 0);
        if !success && !saw_done {
            let message = if stderr.is_empty() {
                format!("Remote Codex exited with code {:?}", exit_status)
            } else {
                stderr.clone()
            };
            let _ = sender.send(StreamMessage::Error {
                message,
                stdout: String::new(),
                stderr,
                exit_code: exit_status.map(|status| status as i32),
            });
            return Ok(());
        }

        if !saw_done {
            let _ = sender.send(StreamMessage::Done {
                result: final_text,
                session_id: current_thread_id,
            });
        }

        Ok(())
    })
}

fn execute_streaming_remote_tmux(
    profile: &RemoteProfile,
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| format!("Failed to create tokio runtime: {}", e))?;

    let profile = profile.clone();
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
        let ssh = crate::services::remote::ssh_connect_and_auth(&profile).await?;

        eprintln!("  [remote-codex-tmux] SSH authenticated");

        let codex_bin = "codex";
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
        let script_path = format!("/tmp/remotecc-{}.sh", tmux_name);

        let mut wrapper_env_prefix = vec![
            "env".to_string(),
            "-u".to_string(),
            "CLAUDECODE".to_string(),
        ];
        if let Some(channel_id) = report_channel_id {
            wrapper_env_prefix.push(format!(
                "{}={}",
                RESTART_REPORT_CHANNEL_ENV,
                shell_escape(&channel_id.to_string())
            ));
        }
        if let Some(provider) = report_provider {
            wrapper_env_prefix.push(format!(
                "{}={}",
                RESTART_REPORT_PROVIDER_ENV,
                shell_escape(provider.as_str())
            ));
        }

        let script_content = format!(
            "#!/bin/bash\n\
            {{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null\n\
            exec {} remotecc --codex-tmux-wrapper \\\n  \
            --output-file {output} \\\n  \
            --input-fifo {input_fifo} \\\n  \
            --prompt-file {prompt} \\\n  \
            --cwd {wd} \\\n  \
            --codex-bin {codex_bin}\n",
            wrapper_env_prefix.join(" "),
            output = shell_escape(&output_path),
            input_fifo = shell_escape(&input_fifo_path),
            prompt = shell_escape(&prompt_path),
            wd = shell_escape(&working_dir),
            codex_bin = shell_escape(codex_bin),
        );

        let prompt_b64 = BASE64_STANDARD.encode(prompt.as_bytes());
        let script_b64 = BASE64_STANDARD.encode(script_content.as_bytes());

        let mut force_recreate = false;
        let mut retried_after_missing_output = false;

        loop {
            let is_followup;
            {
                let mut setup_channel = ssh
                    .channel_open_session()
                    .await
                    .map_err(|e| format!("Failed to open setup channel: {}", e))?;

                let force_cleanup = if force_recreate {
                    format!(
                        r#"if tmux has-session -t {name} 2>/dev/null; then \
                            tmux kill-session -t {name} 2>/dev/null; \
                        fi; \
                        pkill -f 'tail -f {output}' 2>/dev/null; \
                        pkill -f 'tail -F {output}' 2>/dev/null; \
                        rm -f {output} {input_fifo} {prompt} {script} 2>/dev/null; \
                        "#,
                        name = shell_escape(&tmux_name),
                        output = shell_escape(&output_path),
                        input_fifo = shell_escape(&input_fifo_path),
                        prompt = shell_escape(&prompt_path),
                        script = shell_escape(&script_path),
                    )
                } else {
                    String::new()
                };

                let setup_cmd = format!(
                    r#"{{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null; \
                    {cd}{force_cleanup}if tmux has-session -t {name} 2>/dev/null; then \
                        _PANE_DEAD=$(tmux list-panes -t {name} -F '#{{pane_dead}}' 2>/dev/null | head -1); \
                        _HAS_OUTPUT=0; [ -f {output} ] && _HAS_OUTPUT=1; \
                        _HAS_FIFO=0; [ -p {input_fifo} ] && _HAS_FIFO=1; \
                        if [ "$_PANE_DEAD" = "1" ] || [ "$_HAS_OUTPUT" != "1" ] || [ "$_HAS_FIFO" != "1" ]; then \
                            tmux kill-session -t {name} 2>/dev/null; \
                            pkill -f 'tail -f {output}' 2>/dev/null; \
                            pkill -f 'tail -F {output}' 2>/dev/null; \
                            rm -f {output} {input_fifo} {prompt} {script} 2>/dev/null; \
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
                    force_cleanup = force_cleanup,
                    name = shell_escape(&tmux_name),
                    output = shell_escape(&output_path),
                    input_fifo = shell_escape(&input_fifo_path),
                    prompt = shell_escape(&prompt_path),
                    prompt_b64 = prompt_b64,
                    script_b64 = script_b64,
                    script = shell_escape(&script_path),
                );

                eprintln!(
                    "  [remote-codex-tmux] Phase 1: setup ({} bytes)...",
                    setup_cmd.len()
                );
                setup_channel
                    .exec(true, setup_cmd)
                    .await
                    .map_err(|e| format!("Failed to exec setup: {}", e))?;
                let _ = setup_channel.eof().await;

                let mut setup_output = Vec::new();
                while let Some(msg) = setup_channel.wait().await {
                    if let russh::ChannelMsg::Data { ref data } = msg {
                        setup_output.extend_from_slice(data);
                    }
                }
                let setup_str = String::from_utf8_lossy(&setup_output).to_string();
                let setup_lines: Vec<&str> = setup_str.trim().lines().collect();
                eprintln!("  [remote-codex-tmux] Setup result: {:?}", setup_lines);

                is_followup = setup_lines.first().map_or(false, |l| *l == "FOLLOWUP");
                if setup_lines.first().map_or(true, |l| *l == "FAILED") && !is_followup {
                    return Err("Failed to create tmux session on remote".to_string());
                }
            }
            let mut stream_channel = ssh
                .channel_open_session()
                .await
                .map_err(|e| format!("Failed to open stream channel: {}", e))?;

            let stream_cmd = format!(
                r#"{{ [ -f ~/.zshrc ] && source ~/.zshrc; [ -f ~/.bashrc ] && source ~/.bashrc; }} 2>/dev/null; \
                _WAIT_OK=0; \
                for _ in $(seq 1 100); do \
                    if [ -f {output} ]; then \
                        _WAIT_OK=1; \
                        break; \
                    fi; \
                    sleep 0.1; \
                done; \
                if [ "$_WAIT_OK" != "1" ]; then \
                    echo "tail: {output}: No such file or directory" >&2; \
                    exit 1; \
                fi; \
                exec tail -f {output}"#,
                output = shell_escape(&output_path),
            );

            eprintln!("  [remote-codex-tmux] Phase 2: waiting for output then tail -f ...");
            stream_channel
                .exec(true, stream_cmd)
                .await
                .map_err(|e| format!("Failed to exec stream: {}", e))?;
            let _ = stream_channel.eof().await;

            let mut line_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            let mut exit_status: Option<u32> = None;
            let mut line_state = StreamLineState::new();

            while let Some(msg) = stream_channel.wait().await {
                if let Some(ref token) = cancel_token_inner {
                    if token.cancelled.load(Ordering::Relaxed) {
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

            if !line_buf.is_empty() {
                if let Ok(line) = String::from_utf8(line_buf) {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    let _ = process_stream_line(trimmed, &sender, &mut line_state);
                }
            }

            let stderr_msg = String::from_utf8_lossy(&stderr_buf).to_string();
            eprintln!(
                "  [remote-codex-tmux] Stream ended. exit_status={:?}, stderr_len={}, has_result={}",
                exit_status,
                stderr_msg.len(),
                line_state.final_result.is_some()
            );
            if !stderr_msg.is_empty() {
                eprintln!("  [remote-codex-tmux] stderr: {}", stderr_msg);
            }

            let missing_output_after_followup = is_followup
                && !retried_after_missing_output
                && line_state.final_result.is_none()
                && line_state.stdout_error.is_none()
                && stderr_msg.contains("No such file or directory");
            if missing_output_after_followup {
                eprintln!(
                    "  [remote-codex-tmux] FOLLOWUP lost output file; forcing NEW retry..."
                );
                force_recreate = true;
                retried_after_missing_output = true;
                continue;
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
                let _ = sender.send(StreamMessage::Done {
                    result: String::new(),
                    session_id: line_state.last_session_id,
                });
            }

            return Ok(());
        }
    })
}

fn execute_streaming_local_tmux(
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    let output_path = format!("/tmp/remotecc-{}.jsonl", tmux_session_name);
    let input_fifo_path = format!("/tmp/remotecc-{}.input", tmux_session_name);
    let prompt_path = format!("/tmp/remotecc-{}.prompt", tmux_session_name);
    let owner_path = tmux_owner_path(tmux_session_name);

    let session_exists = tmux_session_exists(tmux_session_name);
    let session_usable = tmux_session_has_live_pane(tmux_session_name)
        && std::fs::metadata(&output_path).is_ok()
        && std::path::Path::new(&input_fifo_path).exists();

    if session_usable {
        return send_followup_to_tmux(
            prompt,
            &output_path,
            &input_fifo_path,
            sender,
            cancel_token,
            tmux_session_name,
        );
    }

    if session_exists {
        record_tmux_exit_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", tmux_session_name])
            .status();
    }

    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&input_fifo_path);
    let _ = std::fs::remove_file(&prompt_path);
    let _ = std::fs::remove_file(&owner_path);
    let _ = std::fs::remove_file(format!("/tmp/remotecc-{}.sh", tmux_session_name));

    std::fs::write(&output_path, "").map_err(|e| format!("Failed to create output file: {}", e))?;

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

    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;
    write_tmux_owner_marker(tmux_session_name)?;

    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;

    // Write launch script to file to avoid tmux "command too long" errors
    let script_path = format!("/tmp/remotecc-{}.sh", tmux_session_name);

    let mut env_lines = String::from("unset CLAUDECODE\n");
    if let Some(channel_id) = report_channel_id {
        env_lines.push_str(&format!(
            "export {}={}\n",
            RESTART_REPORT_CHANNEL_ENV, channel_id
        ));
    }
    if let Some(provider) = report_provider {
        env_lines.push_str(&format!(
            "export {}={}\n",
            RESTART_REPORT_PROVIDER_ENV,
            provider.as_str()
        ));
    }

    let script_content = format!(
        "#!/bin/bash\n\
        {env}\
        exec {exe} --codex-tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd} \\\n  \
        --codex-bin {codex_bin}\n",
        env = env_lines,
        exe = shell_escape(&exe.display().to_string()),
        output = shell_escape(&output_path),
        input_fifo = shell_escape(&input_fifo_path),
        prompt = shell_escape(&prompt_path),
        wd = shell_escape(working_dir),
        codex_bin = shell_escape(codex_bin),
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launch script: {}", e))?;

    let tmux_result = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux_session_name,
            "-c",
            working_dir,
            &format!("bash {}", shell_escape(&script_path)),
        ])
        .env_remove("CLAUDECODE")
        .output()
        .map_err(|e| format!("Failed to create tmux session: {}", e))?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        let _ = std::fs::remove_file(&output_path);
        let _ = std::fs::remove_file(&input_fifo_path);
        let _ = std::fs::remove_file(&prompt_path);
        let _ = std::fs::remove_file(&owner_path);
        let _ = std::fs::remove_file(&script_path);
        return Err(format!("tmux error: {}", stderr));
    }

    // Keep tmux session alive after process exits for post-mortem analysis
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            tmux_session_name,
            "remain-on-exit",
            "on",
        ])
        .output();

    // Stamp generation marker so post-restart watcher restore can detect old sessions
    let gen_marker_path = format!("/tmp/remotecc-{}.generation", tmux_session_name);
    let current_gen = crate::services::discord::runtime_store::load_generation();
    let _ = std::fs::write(&gen_marker_path, current_gen.to_string());

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token.clone(),
        tmux_session_name,
    )?;

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

    Ok(())
}

fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<(), String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))?;
    let encoded = format!(
        "{}{}",
        TMUX_PROMPT_B64_PREFIX,
        BASE64_STANDARD.encode(prompt.as_bytes())
    );
    writeln!(fifo, "{}", encoded).map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
    fifo.flush()
        .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
    drop(fifo);

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    let read_result = read_output_file_until_result(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        tmux_session_name,
    )?;

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

    Ok(())
}

fn base_exec_args(session_id: Option<&str>, prompt: &str) -> Vec<String> {
    let mut args = vec!["exec".to_string()];
    if let Some(existing_thread_id) = session_id {
        args.push("resume".to_string());
        args.push(existing_thread_id.to_string());
    }
    args.extend([
        "--skip-git-repo-check".to_string(),
        "--json".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        prompt.to_string(),
    ]);
    args
}

fn handle_codex_json_line(
    line: &str,
    sender: &Sender<StreamMessage>,
    current_thread_id: &mut Option<String>,
    final_text: &mut String,
    started_at: std::time::Instant,
) -> Result<Option<bool>, String> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    let json = serde_json::from_str::<Value>(line)
        .map_err(|e| format!("Failed to parse Codex JSON: {}", e))?;

    match json.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "thread.started" => {
            if let Some(thread_id) = json.get("thread_id").and_then(|v| v.as_str()) {
                *current_thread_id = Some(thread_id.to_string());
                let _ = sender.send(StreamMessage::Init {
                    session_id: thread_id.to_string(),
                });
            }
        }
        "item.started" => {
            if let Some(item) = json.get("item") {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "command_execution" => {
                        let command = item.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        let input = serde_json::json!({ "command": command }).to_string();
                        let _ = sender.send(StreamMessage::ToolUse {
                            name: "Bash".to_string(),
                            input,
                        });
                    }
                    "reasoning" => {
                        // Codex reasoning: extract summary text if available
                        let summary = item
                            .get("summary")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|s| s.get("text"))
                            .and_then(|v| v.as_str())
                            .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
                            .map(|l| l.trim().to_string());
                        let _ = sender.send(StreamMessage::Thinking { summary });
                    }
                    _ => {}
                }
            }
        }
        "item.completed" => {
            if let Some(item) = json.get("item") {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "agent_message" => {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            if !final_text.is_empty() {
                                final_text.push_str("\n\n");
                            }
                            final_text.push_str(text);
                            let _ = sender.send(StreamMessage::Text {
                                content: text.to_string(),
                            });
                        }
                    }
                    "command_execution" => {
                        let content = item
                            .get("aggregated_output")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_error = item
                            .get("exit_code")
                            .and_then(|v| v.as_i64())
                            .map(|code| code != 0)
                            .unwrap_or(false);
                        let _ = sender.send(StreamMessage::ToolResult { content, is_error });
                    }
                    "reasoning" => {
                        let summary = item
                            .get("summary")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|s| s.get("text"))
                            .and_then(|v| v.as_str())
                            .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
                            .map(|l| l.trim().to_string());
                        let _ = sender.send(StreamMessage::Thinking { summary });
                    }
                    _ => {}
                }
            }
        }
        "turn.completed" => {
            let usage = json.get("usage").cloned().unwrap_or_default();
            let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64());
            let output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64());
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: Some("codex".to_string()),
                cost_usd: None,
                total_cost_usd: None,
                duration_ms: Some(started_at.elapsed().as_millis() as u64),
                num_turns: None,
                input_tokens,
                output_tokens,
            });
            let _ = sender.send(StreamMessage::Done {
                result: final_text.clone(),
                session_id: current_thread_id.clone(),
            });
            return Ok(Some(true));
        }
        "error" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Codex error");
            let _ = sender.send(StreamMessage::Error {
                message: message.to_string(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(Some(true));
        }
        _ => {}
    }

    Ok(Some(false))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::{compose_codex_prompt, handle_codex_json_line, TMUX_PROMPT_B64_PREFIX};
    use crate::services::claude::StreamMessage;
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

    #[test]
    fn test_handle_codex_json_line_maps_thread_and_turn_completion() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"thread.started","thread_id":"thread-1"}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let _ = handle_codex_json_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}} "#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let done = handle_codex_json_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        assert_eq!(thread_id.as_deref(), Some("thread-1"));
        assert_eq!(done, Some(true));

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert!(matches!(items[0], StreamMessage::Init { .. }));
        assert!(matches!(items[1], StreamMessage::Text { .. }));
        assert!(matches!(items[2], StreamMessage::StatusUpdate { .. }));
        assert!(matches!(items[3], StreamMessage::Done { .. }));
    }

    #[test]
    fn test_compose_codex_prompt_includes_authoritative_sections() {
        let prompt = compose_codex_prompt(
            "role과 mission만 답해줘.",
            Some("role: PMD\nmission: 백로그 관리"),
            Some(&["Bash".to_string(), "Read".to_string()]),
        );

        assert!(prompt.contains("[Authoritative Instructions]"));
        assert!(prompt.contains("role: PMD"));
        assert!(prompt.contains("[Tool Policy]"));
        assert!(prompt.contains("Bash, Read"));
        assert!(prompt.contains("[User Request]\nrole과 mission만 답해줘."));
    }

    #[test]
    fn test_compose_codex_prompt_returns_plain_prompt_without_overrides() {
        let prompt = compose_codex_prompt("just answer", None, None);
        assert_eq!(prompt, "just answer");
    }

    #[test]
    fn test_codex_reasoning_started_sends_thinking() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"item.started","item":{"type":"reasoning","id":"rs_001","summary":[]}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], StreamMessage::Thinking { summary: None }));
    }

    #[test]
    fn test_codex_reasoning_completed_sends_thinking_with_summary() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"item.completed","item":{"type":"reasoning","id":"rs_001","summary":[{"type":"summary_text","text":"Analyzing the code structure"}]}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        match &items[0] {
            StreamMessage::Thinking { summary } => {
                assert_eq!(summary.as_deref(), Some("Analyzing the code structure"));
            }
            _ => panic!("Expected Thinking with summary"),
        }
    }

    #[test]
    fn test_tmux_followup_encoding_is_single_line() {
        let prompt = "line1\nline2\nline3";
        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );

        assert!(!encoded.contains('\n'));
    }
}
