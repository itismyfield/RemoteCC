use super::*;

pub(super) async fn handle_event(
    ctx: &serenity::Context,
    event: &serenity::FullEvent,
    data: &Data,
) -> Result<(), Error> {
    maybe_cleanup_sessions(&data.shared).await;
    match event {
        serenity::FullEvent::Message { new_message } => {
            // Ignore bot messages, unless the bot is in the allowed_bot_ids list
            if new_message.author.bot {
                let allowed = {
                    let settings = data.shared.settings.read().await;
                    settings
                        .allowed_bot_ids
                        .contains(&new_message.author.id.get())
                };
                if !allowed {
                    return Ok(());
                }
            }

            // Ignore messages that look like slash commands (but allow from trusted bots)
            if new_message.content.starts_with('/') && !new_message.author.bot {
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
            let is_dm = new_message.guild_id.is_none();
            let (channel_name, _) = resolve_channel_category(ctx, channel_id).await;
            // For threads, inherit role binding from the parent channel
            let (effective_channel_id, effective_channel_name) =
                if let Some((parent_id, parent_name)) =
                    resolve_thread_parent(ctx, channel_id).await
                {
                    (parent_id, parent_name.or_else(|| channel_name.clone()))
                } else {
                    (channel_id, channel_name.clone())
                };
            let role_binding = resolve_role_binding(
                effective_channel_id,
                effective_channel_name.as_deref(),
            );
            if !channel_supports_provider(
                data.provider,
                effective_channel_name.as_deref(),
                is_dm,
                role_binding.as_ref(),
            ) {
                return Ok(());
            }

            let text = new_message.content.trim();
            if !text.is_empty()
                && try_handle_family_profile_probe_reply(
                    ctx,
                    new_message,
                    &data.shared,
                    data.provider,
                )
                .await?
            {
                return Ok(());
            }

            // Auth check (allowed bots bypass auth)
            let is_allowed_bot = new_message.author.bot && {
                let settings = data.shared.settings.read().await;
                settings.allowed_bot_ids.contains(&user_id.get())
            };
            if !is_allowed_bot && !check_auth(user_id, user_name, &data.shared, &data.token).await {
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

            if text.is_empty() {
                return Ok(());
            }

            // Auto-restore session (for threads, fall back to parent channel's session)
            auto_restore_session(&data.shared, channel_id, ctx).await;
            if effective_channel_id != channel_id {
                // Thread: if no session found for thread, try to bootstrap from parent
                let needs_parent = {
                    let d = data.shared.core.lock().await;
                    !d.sessions.contains_key(&channel_id)
                };
                if needs_parent {
                    auto_restore_session(&data.shared, effective_channel_id, ctx).await;
                    // Clone parent session's path for the thread
                    let parent_path = {
                        let d = data.shared.core.lock().await;
                        d.sessions
                            .get(&effective_channel_id)
                            .and_then(|s| s.current_path.clone())
                    };
                    if let Some(path) = parent_path {
                        bootstrap_thread_session(
                            &data.shared,
                            channel_id,
                            &path,
                            ctx,
                        )
                        .await;
                    }
                }
            }

            // Queue messages while AI is in progress (executed as next turn after current finishes)
            {
                let mut d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id) {
                    let inserted = {
                        let queue = d.intervention_queue.entry(channel_id).or_default();
                        enqueue_intervention(
                            queue,
                            Intervention {
                                author_id: user_id,
                                message_id: new_message.id,
                                text: text.to_string(),
                                mode: InterventionMode::Soft,
                                created_at: Instant::now(),
                            },
                        )
                    };

                    drop(d);

                    if !inserted {
                        rate_limit_wait(&data.shared, channel_id).await;
                        let _ = channel_id
                            .say(&ctx.http, "↪ 같은 메시지가 방금 이미 큐잉되어서 무시했어.")
                            .await;
                        return Ok(());
                    }

                    return Ok(());
                }
            }

            // Meeting command from text (e.g. announce bot sending "/meeting start ...")
            if text.starts_with("/meeting ") {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ◀ [{user_name}] Meeting cmd: {text}");
                let http = ctx.http.clone();
                if meeting::handle_meeting_command(
                    http,
                    channel_id,
                    text,
                    data.provider,
                    &data.shared,
                )
                .await?
                {
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

            // Extract reply context if user replied to another message
            let reply_context = if let Some(ref_msg) = new_message.referenced_message.as_ref() {
                let ref_author = &ref_msg.author.name;
                let ref_content = ref_msg.content.trim();
                let ref_text = if ref_content.is_empty() {
                    format!("[Reply to {}'s message (no text content)]", ref_author)
                } else {
                    let truncated = truncate_str(ref_content, 500);
                    format!(
                        "[Reply context]\nAuthor: {}\nContent: {}",
                        ref_author, truncated
                    )
                };

                // Fetch preceding messages for Q&A context (best-effort)
                let mut context_parts = Vec::new();
                if let Ok(preceding) = channel_id
                    .messages(
                        &ctx.http,
                        serenity::builder::GetMessages::new()
                            .before(ref_msg.id)
                            .limit(4),
                    )
                    .await
                {
                    // preceding comes newest-first; reverse for chronological order
                    let mut msgs: Vec<_> = preceding
                        .iter()
                        .filter(|m| !m.content.trim().is_empty())
                        .collect();
                    msgs.reverse();
                    // Keep last 2 Q&A-style messages (budget: ~1000 chars total)
                    let mut budget: usize = 1000;
                    for m in msgs.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
                        let entry = format!(
                            "{}: {}",
                            m.author.name,
                            truncate_str(m.content.trim(), 300)
                        );
                        if entry.len() > budget {
                            break;
                        }
                        budget -= entry.len();
                        context_parts.push(entry);
                    }
                }

                if context_parts.is_empty() {
                    Some(ref_text)
                } else {
                    let preceding_ctx = context_parts.join("\n");
                    Some(format!(
                        "[Reply context — preceding conversation]\n{}\n\n{}",
                        preceding_ctx, ref_text
                    ))
                }
            } else {
                None
            };

            // Add ⏳ reaction to user's message to indicate processing
            let _ = ctx
                .http
                .create_reaction(
                    channel_id,
                    new_message.id,
                    &serenity::model::channel::ReactionType::Unicode("⏳".to_string()),
                )
                .await;

            handle_text_message(
                ctx,
                channel_id,
                new_message.id,
                user_id,
                user_name,
                text,
                &data.shared,
                &data.token,
                false,
                false,
                false,
                reply_context,
            )
            .await?;

            // Replace ⏳ with ✅ after turn completes
            let _ = ctx
                .http
                .delete_reaction_me(
                    channel_id,
                    new_message.id,
                    &serenity::model::channel::ReactionType::Unicode("⏳".to_string()),
                )
                .await;
            let _ = ctx
                .http
                .create_reaction(
                    channel_id,
                    new_message.id,
                    &serenity::model::channel::ReactionType::Unicode("✅".to_string()),
                )
                .await;
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn handle_text_message(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    request_owner_name: &str,
    user_text: &str,
    shared: &Arc<SharedData>,
    token: &str,
    reply_to_user_message: bool,
    defer_watcher_resume: bool,
    wait_for_completion: bool,
    reply_context: Option<String>,
) -> Result<(), Error> {
    // Get session info, allowed tools, and pending uploads
    let (session_info, provider, allowed_tools, pending_uploads, last_shared_mem_ts) = {
        let mut data = shared.core.lock().await;
        let info = data.sessions.get(&channel_id).and_then(|session| {
            session.current_path.as_ref().map(|_| {
                (
                    session.session_id.clone(),
                    session.current_path.clone().unwrap_or_default(),
                )
            })
        });
        let (uploads, shared_ts) = data
            .sessions
            .get_mut(&channel_id)
            .map(|s| {
                s.cleared = false;
                (std::mem::take(&mut s.pending_uploads), s.last_shared_memory_ts.clone())
            })
            .unwrap_or_default();
        drop(data);
        let settings = shared.settings.read().await;
        (
            info,
            settings.provider,
            settings.allowed_tools.clone(),
            uploads,
            shared_ts,
        )
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            // Try auto-start from role_map workspace
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let workspace = settings::resolve_workspace(channel_id, ch_name.as_deref());
            if let Some(ws_path) = workspace {
                let ws = std::path::Path::new(&ws_path);
                if ws.is_dir() {
                    let canonical = ws
                        .canonicalize()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| ws_path.clone());
                    // Check worktree conflict
                    let wt_info = {
                        let data = shared.core.lock().await;
                        let conflict =
                            detect_worktree_conflict(&data.sessions, &canonical, channel_id);
                        drop(data);
                        if let Some(conflicting) = conflict {
                            let ch = ch_name.as_deref().unwrap_or("unknown");
                            match create_git_worktree(&canonical, ch, provider.as_str()) {
                                Ok((wt_path, branch)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] 🌿 Auto-start worktree: {} uses {}",
                                        conflicting, canonical
                                    );
                                    Some(WorktreeInfo {
                                        original_path: canonical.clone(),
                                        worktree_path: wt_path,
                                        branch_name: branch,
                                    })
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    };
                    let eff_path = wt_info
                        .as_ref()
                        .map(|wt| wt.worktree_path.clone())
                        .unwrap_or_else(|| canonical.clone());
                    let (ch_name_resolved, cat_name) =
                        resolve_channel_category(ctx, channel_id).await;
                    let existing = load_existing_session(&eff_path, Some(channel_id.get()));
                    {
                        let mut data = shared.core.lock().await;
                        let session =
                            data.sessions
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
                                    last_shared_memory_ts: None,
                                });
                        session.current_path = Some(eff_path.clone());
                        session.channel_name = ch_name_resolved;
                        session.category_name = cat_name;
                        session.channel_id = Some(channel_id.get());
                        session.last_active = tokio::time::Instant::now();
                        session.worktree = wt_info;
                        if let Some((session_data, _)) = &existing {
                            session.history = session_data.history.clone();
                            session.session_id = if session_data.session_id.is_empty() {
                                None
                            } else {
                                Some(session_data.session_id.clone())
                            };
                        }
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ▶ Auto-started session from workspace: {eff_path}");
                    let sid = {
                        let data = shared.core.lock().await;
                        data.sessions
                            .get(&channel_id)
                            .and_then(|s| s.session_id.clone())
                    };
                    (sid, eff_path)
                } else {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, "No active session. Use `/start <path>` first.")
                        .await;
                    return Ok(());
                }
            } else {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, "No active session. Use `/start <path>` first.")
                    .await;
                return Ok(());
            }
        }
    };

    // Add hourglass reaction to user's message
    add_reaction(ctx, channel_id, user_msg_id, '⏳').await;

    // Send placeholder message
    rate_limit_wait(shared, channel_id).await;
    let placeholder = channel_id
        .send_message(&ctx.http, {
            let builder = CreateMessage::new().content("...");
            if reply_to_user_message {
                builder.reference_message((channel_id, user_msg_id))
            } else {
                builder
            }
        })
        .await?;
    let placeholder_msg_id = placeholder.id;

    // Sanitize input
    let sanitized_input = ai_screen::sanitize_user_input(user_text);

    let role_binding = {
        let data = shared.core.lock().await;
        let ch_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_deref());
        resolve_role_binding(channel_id, ch_name)
    };

    // Prepend pending file uploads
    let mut context_chunks = Vec::new();
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if let Some(shared_memory) = role_binding.as_ref().and_then(|binding| {
        build_shared_memory_context(
            &binding.role_id,
            provider,
            channel_id,
            session_id.is_some(),
            last_shared_mem_ts.as_deref(),
        )
    }) {
        context_chunks.push(shared_memory);
        // Update last_shared_memory_ts for dedup in next turn
        if let Some(binding) = role_binding.as_ref() {
            if let Some(ts) = latest_shared_memory_ts(&binding.role_id) {
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.last_shared_memory_ts = Some(ts);
                }
            }
        }
    }
    if let Some(ref reply_ctx) = reply_context {
        context_chunks.push(reply_ctx.clone());
    }
    // Re-inject compact formatting reminder for interactive follow-up turns.
    // System prompt is only sent at session creation; after context compaction
    // these rules can be lost.
    if session_id.is_some() {
        context_chunks.push(
            "<system-reminder>\n\
             Discord formatting: minimize code blocks, keep messages concise.\n\
             </system-reminder>"
                .to_string(),
        );
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
            match provider {
                ProviderKind::Claude => format!(
                    "\n\nAvailable skills (invoke via the Skill tool):\n{}",
                    list.join("\n")
                ),
                ProviderKind::Codex => format!(
                    "\n\nAvailable local Codex skills (use them by name when relevant):\n{}",
                    list.join("\n")
                ),
            }
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
                let cat_part = cat_name
                    .map(|c| format!(" (category: {})", c))
                    .unwrap_or_default();
                format!(
                    "Discord context: channel #{} (ID: {}){}, user: {} (ID: {})",
                    name,
                    channel_id.get(),
                    cat_part,
                    request_owner_name,
                    request_owner.get()
                )
            }
            None => format!(
                "Discord context: DM, user: {} (ID: {})",
                request_owner_name,
                request_owner.get()
            ),
        }
    };

    let system_prompt_owned = build_system_prompt(
        &discord_context,
        &current_path,
        channel_id,
        token,
        &disabled_notice,
        &skills_notice,
        role_binding.as_ref(),
        reply_to_user_message,
    );

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

    // Resolve channel/tmux session name from current session state
    let (channel_name, tmux_session_name) = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone());
        let tmux_session_name = channel_name
            .as_ref()
            .map(|name| provider.build_tmux_session_name(name));
        (channel_name, tmux_session_name)
    };
    let pcd_session_key = build_pcd_session_key(shared, channel_id, provider).await;
    let pcd_session_name = channel_name.clone();
    let pcd_session_info = derive_pcd_session_info(
        Some(user_text),
        channel_name.as_deref(),
        Some(&current_path),
    );
    let dispatch_id = parse_dispatch_id(user_text);
    post_pcd_session_status(
        pcd_session_key.as_deref(),
        pcd_session_name.as_deref(),
        Some(provider.as_str()),
        "working",
        provider,
        Some(&pcd_session_info),
        None,
        Some(&current_path),
        dispatch_id.as_deref(),
    )
    .await;

    let (inflight_tmux_name, inflight_output_path, inflight_input_fifo, inflight_offset) =
        if remote_profile.is_none() && claude::is_tmux_available() {
            if let Some(ref tmux_name) = tmux_session_name {
                let (output_path, input_fifo_path) = tmux_runtime_paths(tmux_name);
                let session_exists = std::process::Command::new("tmux")
                    .args(["has-session", "-t", tmux_name])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let last_offset = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                (
                    Some(tmux_name.clone()),
                    Some(output_path),
                    Some(input_fifo_path),
                    if session_exists { last_offset } else { 0 },
                )
            } else {
                (None, None, None, 0)
            }
        } else {
            (None, None, None, 0)
        };

    let inflight_state = InflightTurnState::new(
        provider,
        channel_id.get(),
        channel_name.clone(),
        request_owner.get(),
        user_msg_id.get(),
        placeholder_msg_id.get(),
        user_text.to_string(),
        session_id.clone(),
        inflight_tmux_name,
        inflight_output_path,
        inflight_input_fifo.clone(),
        inflight_offset,
    );
    if let Err(e) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}]   ⚠ inflight state save failed: {e}");
    }

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = if wait_for_completion {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Pause tmux watcher if one exists (so it doesn't read our turn's output)
    if let Some(watcher) = shared.tmux_watchers.get(&channel_id) {
        watcher.paused.store(true, Ordering::Relaxed);
    }

    // Auto-sync worktree before sending message to session
    {
        let script = dirs::home_dir()
            .unwrap_or_default()
            .join(".remotecc/scripts/worktree-autosync.sh");
        if script.exists() {
            let ws = current_path.clone();
            let ts = chrono::Local::now().format("%H:%M:%S");
            match std::process::Command::new(&script)
                .arg(&ws)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
            {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let msg = stdout.trim();
                    match out.status.code() {
                        Some(0) => println!("  [{ts}] 🔄 worktree-autosync [{ws}]: {msg}"),
                        Some(1) => println!("  [{ts}] ⏭ worktree-autosync [{ws}]: skipped — {msg}"),
                        _ => eprintln!("  [{ts}] ⚠ worktree-autosync [{ws}]: error — {msg}"),
                    }
                }
                Err(e) => eprintln!("  [{ts}] ⚠ worktree-autosync: failed to run — {e}"),
            }
        }
    }

    // Run the provider in a blocking thread
    tokio::task::spawn_blocking(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match provider {
            ProviderKind::Claude => claude::execute_command_streaming(
                &context_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                remote_profile.as_ref(),
                tmux_session_name.as_deref(),
                Some(channel_id.get()),
                Some(provider),
            ),
            ProviderKind::Codex => codex::execute_command_streaming(
                &context_prompt,
                session_id_clone.as_deref(),
                &current_path_clone,
                tx.clone(),
                Some(&system_prompt_owned),
                Some(&allowed_tools),
                Some(cancel_token_clone),
                remote_profile.as_ref(),
                tmux_session_name.as_deref(),
                Some(channel_id.get()),
                Some(provider),
            ),
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

    spawn_turn_bridge(
        ctx.http.clone(),
        shared.clone(),
        cancel_token.clone(),
        rx,
        TurnBridgeContext {
            provider,
            channel_id,
            user_msg_id,
            user_text_owned: user_text.to_string(),
            request_owner_name: request_owner_name.to_string(),
            request_owner: Some(request_owner),
            serenity_ctx: Some(ctx.clone()),
            token: Some(token.to_string()),
            role_binding: role_binding.clone(),
            pcd_session_key,
            pcd_session_name,
            pcd_session_info: Some(pcd_session_info),
            pcd_cwd: Some(current_path.clone()),
            dispatch_id,
            current_msg_id: placeholder_msg_id,
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id.clone(),
            defer_watcher_resume,
            completion_tx,
            inflight_state,
        },
    );

    if let Some(rx) = completion_rx {
        rx.await
            .map_err(|_| "queued turn completion wait failed".to_string())?;
    }

    Ok(())
}

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
            .say(
                &ctx.http,
                format!("Failed to prepare upload directory: {}", e),
            )
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
