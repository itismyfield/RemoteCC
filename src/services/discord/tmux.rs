use std::sync::atomic::Ordering;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::claude;

use super::formatting::{format_for_discord, send_long_message_raw};
use super::{SharedData, TmuxWatcherHandle};

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
pub(super) async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
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
    shared.tmux_watchers.remove(&channel_id);
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content and detects result events.
/// Returns true if a "result" event was found.
pub(super) fn process_watcher_lines(
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
pub(super) async fn restore_tmux_watchers(http: &Arc<serenity::Http>, shared: &Arc<SharedData>) {
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
        let data = shared.core.lock().await;
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
            if shared.tmux_watchers.contains_key(&channel_id) {
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

        shared.tmux_watchers.insert(
            pw.channel_id,
            TmuxWatcherHandle {
                cancel: cancel.clone(),
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
            },
        );

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

/// Kill orphan tmux sessions (remoteCC-*) that don't map to any known channel.
/// Called after restore_tmux_watchers to clean up sessions from renamed/deleted channels.
pub(super) async fn cleanup_orphan_tmux_sessions(shared: &Arc<SharedData>) {
    let output = match tokio::task::spawn_blocking(|| {
        std::process::Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
    })
    .await
    {
        Ok(Ok(o)) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return,
    };

    let orphans: Vec<String> = {
        let data = shared.core.lock().await;
        let mut result = Vec::new();

        for session_name in output.lines() {
            let session_name = session_name.trim();
            if !session_name.starts_with("remoteCC-") {
                continue;
            }

            // Check if any active channel maps to this session
            let has_owner = data.sessions.iter().any(|(_, session)| {
                session
                    .channel_name
                    .as_ref()
                    .map(|ch_name| claude::sanitize_tmux_session_name(ch_name) == session_name)
                    .unwrap_or(false)
            });

            if !has_owner {
                result.push(session_name.to_string());
            }
        }

        result
    };

    if orphans.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 🧹 Cleaning {} orphan tmux session(s)...",
        orphans.len()
    );

    for name in &orphans {
        let name_clone = name.clone();
        let killed = tokio::task::spawn_blocking(move || {
            std::process::Command::new("tmux")
                .args(["kill-session", "-t", &name_clone])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);

        if killed {
            println!("  [{ts}]   killed orphan: {}", name);
            // Also clean associated temp files
            let _ = std::fs::remove_file(format!("/tmp/remotecc-{}.jsonl", name));
            let _ = std::fs::remove_file(format!("/tmp/remotecc-{}.input", name));
            let _ = std::fs::remove_file(format!("/tmp/remotecc-{}.prompt", name));
        }
    }
}
