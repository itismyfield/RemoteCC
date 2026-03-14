use super::restart_report::{clear_restart_report, save_restart_report, RestartCompletionReport};
use super::*;

fn tail_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    if max_chars <= 1 {
        return "…".to_string();
    }

    let keep = max_chars.saturating_sub(1);
    let tail: String = text
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{}", tail)
}

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

fn is_dcserver_restart_command(input: &str) -> bool {
    let lower = input.to_lowercase();

    if lower.contains("--restart-dcserver") || lower.contains("restart_remotecc.sh") {
        return true;
    }

    if lower.contains("remotecc-discord-smoke.sh") && lower.contains("--deploy-live") {
        return true;
    }

    lower.contains("launchctl")
        && lower.contains("com.itismyfield.remotecc.dcserver")
        && (lower.contains("kickstart") || lower.contains("bootstrap") || lower.contains("bootout"))
}

fn should_resume_watcher_after_turn(
    defer_watcher_resume: bool,
    has_local_queued_turns: bool,
    can_chain_locally: bool,
) -> bool {
    !defer_watcher_resume && !(has_local_queued_turns && can_chain_locally)
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
    pub(super) pcd_session_name: Option<String>,
    pub(super) pcd_session_info: Option<String>,
    pub(super) pcd_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) current_msg_id: MessageId,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) new_session_id: Option<String>,
    pub(super) defer_watcher_resume: bool,
    pub(super) completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
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
        let pcd_session_name = bridge.pcd_session_name.clone();
        let pcd_session_info = bridge.pcd_session_info.clone();
        let pcd_cwd = bridge.pcd_cwd.clone();
        let dispatch_id = bridge.dispatch_id.clone();

        let mut full_response = bridge.full_response.clone();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut rx_disconnected = false;
        let mut current_tool_line: Option<String> = bridge.inflight_state.current_tool_line.clone();
        let mut last_tool_name: Option<String> = None;
        let mut last_tool_summary: Option<String> = None;
        let mut accumulated_tokens: u64 = 0;
        let mut spin_idx: usize = 0;
        let mut restart_followup_pending = false;
        let mut tmux_handed_off = false;
        let mut last_pcd_heartbeat = std::time::Instant::now();
        let current_msg_id = bridge.current_msg_id;
        let response_sent_offset = bridge.response_sent_offset;
        let mut tmux_last_offset = bridge.tmux_last_offset;
        let mut new_session_id = bridge.new_session_id.clone();
        let defer_watcher_resume = bridge.defer_watcher_resume;
        let completion_tx = bridge.completion_tx;
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
                            last_tool_summary = None;
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                        }
                        StreamMessage::Thinking => {
                            current_tool_line = Some("💭 Thinking...".to_string());
                            last_tool_name = None;
                            last_tool_summary = None;
                        }
                        StreamMessage::ToolUse { name, input } => {
                            let summary = format_tool_input(&name, &input);
                            let display_summary = if summary.trim().is_empty() {
                                "…".to_string()
                            } else {
                                truncate_str(&summary, 120).to_string()
                            };
                            current_tool_line =
                                Some(format!("⚙ {}: {}", name, display_summary));
                            last_tool_name = Some(name.clone());
                            last_tool_summary = Some(display_summary);
                            if !restart_followup_pending && is_dcserver_restart_command(&input) {
                                let mut report = RestartCompletionReport::new(
                                    provider,
                                    channel_id.get(),
                                    "pending",
                                    format!(
                                        "dcserver restart requested by `{}`; 새 프로세스가 후속 보고를 이어받을 예정입니다.",
                                        request_owner_name
                                    ),
                                );
                                report.current_msg_id = Some(current_msg_id.get());
                                report.channel_name = pcd_session_name.clone();
                                if save_restart_report(&report).is_ok() {
                                    restart_followup_pending = true;
                                    let handoff_text =
                                        "♻️ dcserver 재시작 중...\n\n새 dcserver가 이 메시지를 이어받는 중입니다.";
                                    rate_limit_wait(&shared_owned, channel_id).await;
                                    let _ = channel_id
                                        .edit_message(
                                            &http,
                                            current_msg_id,
                                            EditMessage::new().content(handoff_text),
                                        )
                                        .await;
                                    last_edit_text = handoff_text.to_string();
                                    inflight_state.current_msg_id = current_msg_id.get();
                                    inflight_state.current_msg_len = handoff_text.len();
                                    state_dirty = true;
                                }
                            }
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
                                let detail = last_tool_summary
                                    .as_deref()
                                    .filter(|s| !s.is_empty() && *s != "…")
                                    .map(|s| format!("{} {}: {}", status, tn, s))
                                    .unwrap_or_else(|| format!("{} {}", status, tn));
                                current_tool_line = Some(detail);
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
                            // Only use result as fallback when streaming didn't accumulate text.
                            // The result event contains only the last assistant message's text,
                            // so overwriting would lose earlier text from multi-tool turns
                            // (e.g. text A → tool call → text B would lose text A).
                            if full_response.trim().is_empty() && !result.is_empty() {
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
                            let combined = format!("{} {}", message, stderr).to_lowercase();
                            if combined.contains("prompt is too long")
                                || combined.contains("prompt too long")
                                || combined.contains("context_length_exceeded")
                                || combined.contains("max_tokens")
                                || combined.contains("context window")
                                || combined.contains("token limit")
                            {
                                full_response = "⚠️ __prompt too long__".to_string();
                            } else if !stderr.is_empty() {
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
                            tmux_handed_off = true;
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
                                    cancel: cancel.clone(),
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
                        rx_disconnected = true;
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
            let footer = format!("\n\n{} {}", indicator, tool_status);
            let body_budget = DISCORD_MSG_LIMIT.saturating_sub(footer.len() + 10);
            let normalized = normalize_empty_lines(current_portion);
            let stable_display_text = if current_portion.is_empty() {
                format!("{} {}", indicator, tool_status)
            } else {
                let body = tail_with_ellipsis(&normalized, body_budget.max(1));
                format!("{}{}", body, footer)
            };

            if stable_display_text != last_edit_text && !done {
                rate_limit_wait(&shared_owned, channel_id).await;
                let _ = channel_id
                    .edit_message(
                        &http,
                        current_msg_id,
                        EditMessage::new().content(&stable_display_text),
                    )
                    .await;
                last_edit_text = stable_display_text;
                inflight_state.current_msg_id = current_msg_id.get();
                inflight_state.current_msg_len = last_edit_text.len();
                inflight_state.response_sent_offset = response_sent_offset;
                inflight_state.full_response = full_response.clone();
                state_dirty = true;
            }

            if state_dirty || inflight_state.current_tool_line != current_tool_line {
                inflight_state.current_tool_line = current_tool_line.clone();
                let _ = save_inflight_state(&inflight_state);
            }

            if last_pcd_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
                post_pcd_session_status(
                    pcd_session_key.as_deref(),
                    pcd_session_name.as_deref(),
                    Some(provider.as_str()),
                    "working",
                    provider,
                    pcd_session_info.as_deref(),
                    None,
                    pcd_cwd.as_deref(),
                    dispatch_id.as_deref(),
                )
                .await;
                last_pcd_heartbeat = std::time::Instant::now();
            }
        }

        post_pcd_session_status(
            pcd_session_key.as_deref(),
            pcd_session_name.as_deref(),
            Some(provider.as_str()),
            "idle",
            provider,
            pcd_session_info.as_deref(),
            (accumulated_tokens > 0).then_some(accumulated_tokens),
            pcd_cwd.as_deref(),
            dispatch_id.as_deref(),
        )
        .await;

        let can_chain_locally =
            serenity_ctx.is_some() && request_owner.is_some() && token.is_some();
        // Mark this turn as finalizing — deferred restart must wait until we finish
        // sending the Discord response and cleaning up state.
        shared_owned
            .finalizing_turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let has_queued_turns = {
            let mut data = shared_owned.core.lock().await;
            data.cancel_tokens.remove(&channel_id);
            data.active_request_owner.remove(&channel_id);
            let mut remove_queue = false;
            let has_pending = if let Some(queue) = data.intervention_queue.get_mut(&channel_id) {
                let has_pending = super::has_soft_intervention(queue);
                remove_queue = queue.is_empty();
                has_pending
            } else {
                false
            };
            if remove_queue {
                data.intervention_queue.remove(&channel_id);
            }
            drop(data);
            has_pending
        };

        // Remove ⏳ only if NOT handing off to tmux watcher.
        // When tmux watcher is handling the response, it will do ⏳→✅ after delivery.
        let tmux_handoff_path = rx_disconnected && tmux_handed_off && full_response.is_empty();
        if !tmux_handoff_path {
            remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
        }

        let is_prompt_too_long = full_response.contains("__prompt too long__");

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
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '🛑').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");
        } else if is_prompt_too_long {
            let mention = request_owner
                .map(|uid| format!("<@{}>", uid.get()))
                .unwrap_or_default();
            full_response = format!(
                "{} ⚠️ 프롬프트가 너무 깁니다. 대화 컨텍스트가 모델 한도를 초과했습니다.\n\n\
                 다음 메시지를 보내면 자동으로 새 턴이 시작됩니다.\n\
                 컨텍스트를 줄이려면 `/compact` 또는 `/clear`를 사용해 주세요.",
                mention
            );
            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⚠ Prompt too long (channel {})", channel_id);
        } else if rx_disconnected && tmux_handed_off && full_response.is_empty() {
            // Tmux watcher is handling response delivery — this is normal.
            // Delete the turn bridge placeholder so it doesn't linger as a zombie spinner.
            // The watcher creates its own placeholder when it has output to display.
            let _ = channel_id.delete_message(&http, current_msg_id).await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ✓ tmux handoff complete, placeholder cleaned up, watcher handles response (channel {})",
                channel_id
            );
        } else {
            if full_response.is_empty() {
                // Fallback: try to extract response from tmux output file
                if let Some(ref path) = inflight_state.output_path {
                    let recovered = super::recovery::extract_response_from_output_pub(
                        path,
                        inflight_state.last_offset,
                    );
                    if !recovered.trim().is_empty() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] ↻ Recovered {} chars from output file for channel {}",
                            recovered.len(),
                            channel_id
                        );
                        full_response = recovered;
                    }
                }

                if full_response.is_empty() {
                    if rx_disconnected {
                        full_response =
                            "(No response — 프로세스가 응답 없이 종료됨)".to_string();
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                             (channel {}, output_path={:?}, last_offset={})",
                            channel_id,
                            inflight_state.output_path,
                            inflight_state.last_offset
                        );
                    } else {
                        full_response = "(No response)".to_string();
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] ⚠ Empty response: done without text (channel {})",
                            channel_id
                        );
                    }
                }
            }

            full_response = format_for_discord(&full_response);
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Response sent");
        }

        if should_resume_watcher_after_turn(
            defer_watcher_resume,
            has_queued_turns,
            can_chain_locally,
        ) {
            if let Some(offset) = tmux_last_offset {
                if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                    if let Ok(mut guard) = watcher.resume_offset.lock() {
                        *guard = Some(offset);
                    }
                    watcher.paused.store(false, Ordering::Relaxed);
                }
            }
        }

        {
            let mut data = shared_owned.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if !session.cleared && !is_prompt_too_long {
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

        // Clear restart report BEFORE clearing inflight state (which removes
        // the cancel token) to prevent the flush loop from processing the
        // report in the gap between cancel token removal and report deletion.
        if restart_followup_pending {
            clear_restart_report(provider, channel_id.get());
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ Cleared restart report for channel {} (turn completed normally)",
                channel_id
            );
        }

        clear_inflight_state(provider, channel_id.get());
        shared_owned.recovering_channels.remove(&channel_id);

        // Finalization complete — decrement counter and check deferred restart
        let finalizing_remaining = shared_owned
            .finalizing_turns
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
            - 1;
        {
            let active = shared_owned.core.lock().await.cancel_tokens.len();
            super::check_deferred_restart(active, finalizing_remaining);
        }

        if has_queued_turns {
            if let (Some(ctx), Some(owner), Some(tok)) =
                (serenity_ctx.as_ref(), request_owner, token.as_deref())
            {
                let (next_intervention, has_more_queued_turns) = {
                    let mut data = shared_owned.core.lock().await;
                    let mut remove_queue = false;
                    let next = if let Some(queue) = data.intervention_queue.get_mut(&channel_id) {
                        let next = super::dequeue_next_soft_intervention(queue);
                        let has_more = super::has_soft_intervention(queue);
                        remove_queue = queue.is_empty();
                        (next, has_more)
                    } else {
                        (None, false)
                    };
                    if remove_queue {
                        data.intervention_queue.remove(&channel_id);
                    }
                    next
                };

                if let Some(intervention) = next_intervention {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 📋 Processing next queued command");
                    if let Err(e) = handle_text_message(
                        ctx,
                        channel_id,
                        intervention.message_id,
                        owner,
                        &request_owner_name,
                        &intervention.text,
                        &shared_owned,
                        tok,
                        true,
                        has_more_queued_turns,
                        true,
                        None,
                    )
                    .await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}]   ⚠ queued command failed: {e}");
                        let mut data = shared_owned.core.lock().await;
                        let queue = data.intervention_queue.entry(channel_id).or_default();
                        super::requeue_intervention_front(queue, intervention);
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 📦 preserving queued command(s): missing live Discord context");
                if let Some(offset) = tmux_last_offset {
                    if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                        if let Ok(mut guard) = watcher.resume_offset.lock() {
                            *guard = Some(offset);
                        }
                        watcher.paused.store(false, Ordering::Relaxed);
                    }
                }
            }
        }

        if let Some(tx) = completion_tx {
            let _ = tx.send(());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::should_resume_watcher_after_turn;

    #[test]
    fn chained_batch_mid_turn_keeps_watcher_paused() {
        assert!(!should_resume_watcher_after_turn(true, false, false));
    }

    #[test]
    fn locally_chainable_queue_keeps_watcher_paused() {
        assert!(!should_resume_watcher_after_turn(false, true, true));
    }

    #[test]
    fn final_turn_without_remaining_queue_resumes_watcher() {
        assert!(should_resume_watcher_after_turn(false, false, true));
    }
}
