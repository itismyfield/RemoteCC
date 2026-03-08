use super::*;

pub(super) fn cancel_active_token(token: &Arc<CancelToken>, cleanup_tmux: bool) {
    token.cancelled.store(true, Ordering::Relaxed);

    let child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    if let Some(pid) = child_pid {
        claude::kill_pid_tree(pid);
    }

    if cleanup_tmux {
        if child_pid.is_some() {
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                let _ = std::process::Command::new("tmux")
                    .args(["kill-session", "-t", &name])
                    .output();
            }
        } else {
            token.cancel_with_tmux_cleanup();
        }
    }
}

pub(super) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    (
        format!("/tmp/remotecc-{}.jsonl", tmux_session_name),
        format!("/tmp/remotecc-{}.input", tmux_session_name),
    )
}

pub(super) fn stale_inflight_message(saved_response: &str) -> String {
    let trimmed = saved_response.trim();
    if trimmed.is_empty() {
        "⚠️ RemoteCC가 재시작되어 진행 중이던 응답을 이어붙이지 못했습니다.".to_string()
    } else {
        let formatted = format_for_discord(trimmed);
        format!("{}\n\n[Interrupted by restart]", formatted)
    }
}

pub(super) struct TurnBridgeContext {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: MessageId,
    pub(super) user_text_owned: String,
    pub(super) request_owner_name: String,
    pub(super) request_owner: Option<UserId>,
    pub(super) serenity_ctx: Option<serenity::Context>,
    pub(super) token: Option<String>,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) pcd_session_key: Option<String>,
    pub(super) current_msg_id: MessageId,
    pub(super) current_msg_len: usize,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) new_session_id: Option<String>,
    pub(super) inflight_state: InflightTurnState,
}

pub(super) fn spawn_turn_bridge(
    http: Arc<serenity::Http>,
    shared_owned: Arc<SharedData>,
    cancel_token: Arc<CancelToken>,
    rx: mpsc::Receiver<StreamMessage>,
    bridge: TurnBridgeContext,
) {
    tokio::spawn(async move {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let channel_id = bridge.channel_id;
        let provider = bridge.provider;
        let user_msg_id = bridge.user_msg_id;
        let user_text_owned = bridge.user_text_owned.clone();
        let request_owner_name = bridge.request_owner_name.clone();
        let request_owner = bridge.request_owner;
        let serenity_ctx = bridge.serenity_ctx.clone();
        let token = bridge.token.clone();
        let role_binding = bridge.role_binding.clone();
        let pcd_session_key = bridge.pcd_session_key.clone();

        let mut full_response = bridge.full_response.clone();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut current_tool_line: Option<String> = None;
        let mut last_tool_name: Option<String> = None;
        let mut accumulated_tokens: u64 = 0;
        let mut spin_idx: usize = 0;
        let mut current_msg_id = bridge.current_msg_id;
        let mut current_msg_len = bridge.current_msg_len;
        let mut response_sent_offset = bridge.response_sent_offset;
        let mut tmux_last_offset = bridge.tmux_last_offset;
        let mut new_session_id = bridge.new_session_id.clone();
        let mut inflight_state = bridge.inflight_state.clone();

        let _ = save_inflight_state(&inflight_state);

        while !done {
            let mut state_dirty = false;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            loop {
                match rx.try_recv() {
                    Ok(msg) => match msg {
                        StreamMessage::Init { session_id: sid } => {
                            new_session_id = Some(sid.clone());
                            inflight_state.session_id = Some(sid);
                            state_dirty = true;
                        }
                        StreamMessage::Text { content } => {
                            full_response.push_str(&content);
                            current_tool_line = None;
                            last_tool_name = None;
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                        }
                        StreamMessage::ToolUse { name, input } => {
                            let summary = format_tool_input(&name, &input);
                            current_tool_line =
                                Some(format!("⚙ {}: {}", name, truncate_str(&summary, 120)));
                            last_tool_name = Some(name.clone());
                            if !full_response.is_empty() {
                                let trimmed = full_response.trim_end();
                                full_response.truncate(trimmed.len());
                                full_response.push_str("\n\n");
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::ToolResult { content, is_error } => {
                            if let Some(ref tn) = last_tool_name {
                                let status = if is_error { "✗" } else { "✓" };
                                current_tool_line = Some(format!("{} {}", status, tn));
                            }
                            let _ = content;
                        }
                        StreamMessage::TaskNotification { summary, .. } => {
                            if !summary.is_empty() {
                                full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::Done {
                            result,
                            session_id: sid,
                        } => {
                            if !result.is_empty() && full_response.is_empty() {
                                full_response = result;
                                inflight_state.full_response = full_response.clone();
                            }
                            if let Some(s) = sid {
                                new_session_id = Some(s.clone());
                                inflight_state.session_id = Some(s);
                            }
                            state_dirty = true;
                            done = true;
                        }
                        StreamMessage::Error {
                            message, stderr, ..
                        } => {
                            if !stderr.is_empty() {
                                full_response = format!(
                                    "Error: {}\nstderr: {}",
                                    message,
                                    truncate_str(&stderr, 500)
                                );
                            } else {
                                full_response = format!("Error: {}", message);
                            }
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                            done = true;
                        }
                        StreamMessage::StatusUpdate {
                            input_tokens,
                            output_tokens,
                            ..
                        } => {
                            if let (Some(it), Some(ot)) = (input_tokens, output_tokens) {
                                accumulated_tokens += it + ot;
                            }
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
                            inflight_state.output_path = Some(output_path.clone());
                            inflight_state.input_fifo_path = Some(input_fifo_path);
                            inflight_state.last_offset = last_offset;

                            let already_watching =
                                shared_owned.tmux_watchers.contains_key(&channel_id);
                            if !already_watching {
                                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
                                let handle = TmuxWatcherHandle {
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
                            state_dirty = true;
                        }
                        StreamMessage::OutputOffset { offset } => {
                            tmux_last_offset = Some(offset);
                            inflight_state.last_offset = offset;
                            state_dirty = true;
                        }
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }

            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            let tool_status = current_tool_line.as_deref().unwrap_or("Processing...");
            let current_portion = if response_sent_offset < full_response.len() {
                &full_response[response_sent_offset..]
            } else {
                ""
            };
            let display_text = if current_portion.is_empty() {
                format!("{} {}", indicator, tool_status)
            } else {
                let normalized = normalize_empty_lines(current_portion);
                let footer = format!("\n\n{} {}", indicator, tool_status);
                let truncated = truncate_str(&normalized, DISCORD_MSG_LIMIT - footer.len() - 10);
                format!("{}{}", truncated, footer)
            };

            if display_text != last_edit_text && !done {
                if display_text.len() > DISCORD_MSG_LIMIT - 50 && current_msg_len > 100 {
                    let normalized = normalize_empty_lines(current_portion);
                    let finalize_text = truncate_str(&normalized, DISCORD_MSG_LIMIT - 10);
                    current_msg_len = finalize_text.len();
                    response_sent_offset = full_response.len();

                    rate_limit_wait(&shared_owned, channel_id).await;
                    let _ = channel_id
                        .edit_message(
                            &http,
                            current_msg_id,
                            EditMessage::new().content(&finalize_text),
                        )
                        .await;

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
                inflight_state.current_msg_id = current_msg_id.get();
                inflight_state.current_msg_len = current_msg_len;
                inflight_state.response_sent_offset = response_sent_offset;
                inflight_state.full_response = full_response.clone();
                state_dirty = true;
            }

            if state_dirty {
                let _ = save_inflight_state(&inflight_state);
            }
        }

        if let Some(offset) = tmux_last_offset {
            if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                if let Ok(mut guard) = watcher.resume_offset.lock() {
                    *guard = Some(offset);
                }
                watcher.paused.store(false, Ordering::Relaxed);
            }
        }

        post_pcd_session_status(
            pcd_session_key.as_deref(),
            "idle",
            provider,
            (accumulated_tokens > 0).then_some(accumulated_tokens),
        )
        .await;

        let queued_commands: Vec<String> = {
            let mut data = shared_owned.core.lock().await;
            data.cancel_tokens.remove(&channel_id);
            data.active_request_owner.remove(&channel_id);

            let queued = data
                .intervention_queue
                .remove(&channel_id)
                .unwrap_or_default();
            queued
                .into_iter()
                .filter(|i| i.mode == InterventionMode::Soft)
                .map(|i| i.text)
                .collect()
        };

        remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;

        if cancelled {
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    claude::kill_pid_tree(pid);
                }
            }

            full_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let formatted = format_for_discord(&full_response);
                format!("{}\n\n[Stopped]", formatted)
            };

            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = channel_id
                .edit_message(
                    &http,
                    current_msg_id,
                    EditMessage::new().content(truncate_str(&full_response, DISCORD_MSG_LIMIT)),
                )
                .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '🛑').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");
        } else {
            if full_response.is_empty() {
                full_response = "(No response)".to_string();
            }

            full_response = format_for_discord(&full_response);

            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = channel_id.delete_message(&http, current_msg_id).await;

            if let Err(e) =
                send_long_message_raw(&http, channel_id, &full_response, &shared_owned).await
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}]   ⚠ send_long_message failed: {e}");
                rate_limit_wait(&shared_owned, channel_id).await;
                let _ = channel_id
                    .send_message(
                        &http,
                        CreateMessage::new().content(truncate_str(&full_response, DISCORD_MSG_LIMIT)),
                    )
                    .await;
            }

            add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Response sent");
        }

        {
            let mut data = shared_owned.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if !session.cleared {
                    if let Some(sid) = new_session_id {
                        session.session_id = Some(sid);
                    }
                    session.history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: user_text_owned.clone(),
                    });
                    session.history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: full_response.clone(),
                    });
                    let current_path = session.current_path.clone();
                    let channel_name = session.channel_name.clone();
                    if let Some(ref path) = current_path {
                        if let Some(binding) = role_binding.as_ref() {
                            if let Err(e) = append_shared_memory_turn(
                                &binding.role_id,
                                provider,
                                channel_id,
                                channel_name.as_deref(),
                                path,
                                Some(request_owner_name.as_str()),
                                &user_text_owned,
                                &full_response,
                            ) {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}]   ⚠ shared memory save failed: {e}");
                            }
                        }
                        save_session_to_file(session, path);
                    }
                }
            }
        }

        clear_inflight_state(provider, channel_id.get());
        shared_owned.recovering_channels.remove(&channel_id);

        if !queued_commands.is_empty() {
            if let (Some(ctx), Some(owner), Some(tok)) =
                (serenity_ctx.as_ref(), request_owner, token.as_deref())
            {
                let cmd_count = queued_commands.len();
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 📋 Processing {cmd_count} queued command(s)");
                for cmd in queued_commands {
                    if let Err(e) = handle_text_message(
                        ctx,
                        channel_id,
                        user_msg_id,
                        owner,
                        &request_owner_name,
                        &cmd,
                        &shared_owned,
                        tok,
                    )
                    .await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}]   ⚠ queued command failed: {e}");
                    }
                }
            }
        }
    });
}
