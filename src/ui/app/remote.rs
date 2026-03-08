use super::*;

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::services::file_ops;
use crate::services::remote::{self, ConnectionStatus, RemoteContext};

impl App {
    /// 원격 파일의 로컬 tmp 경로 생성
    pub(crate) fn remote_tmp_path(&self, file_name: &str) -> Option<PathBuf> {
        let panel = self.active_panel();
        let remote_path = format!("{}/{}", panel.path.display(), file_name);
        if let Some(ref ctx) = panel.remote_ctx {
            let tmp_base = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".remotecc")
                .join("tmp")
                .join(format!("{}@{}", ctx.profile.user, ctx.profile.host));
            Some(tmp_base.join(remote_path.trim_start_matches('/')))
        } else {
            None
        }
    }

    /// 원격 파일을 tmp로 다운로드 (프로그레스 표시) 후 편집기/뷰어로 열기
    pub(crate) fn download_for_remote_open(
        &mut self,
        file_name: &str,
        file_size: u64,
        open_action: PendingRemoteOpen,
    ) {
        let panel_index = self.active_panel_index;
        let panel = &self.panels[panel_index];
        let remote_path = format!("{}/{}", panel.path.display(), file_name);

        let (profile, tmp_path) = if let Some(ref ctx) = panel.remote_ctx {
            let tmp_base = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".remotecc")
                .join("tmp")
                .join(format!("{}@{}", ctx.profile.user, ctx.profile.host));
            let tmp_path = tmp_base.join(remote_path.trim_start_matches('/'));
            (ctx.profile.clone(), tmp_path)
        } else {
            return;
        };

        // 디렉토리 생성
        if let Some(parent) = tmp_path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                self.show_message(&format!("Cannot create tmp dir: {}", e));
                return;
            }
        }

        // 프로그레스 설정
        let mut progress = FileOperationProgress::new(file_ops::FileOperationType::Download);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        let tmp_path_clone = tmp_path.clone();
        let remote_path_clone = remote_path.clone();
        let file_name_owned = file_name.to_string();

        thread::spawn(move || {
            let _ = tx.send(file_ops::ProgressMessage::Preparing(format!(
                "Connecting to {}...",
                profile.host
            )));

            // 새 SFTP 세션 연결
            let session = match remote::SftpSession::connect(&profile) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(file_ops::ProgressMessage::Error(
                        file_name_owned.clone(),
                        format!("Connection failed: {}", e),
                    ));
                    let _ = tx.send(file_ops::ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            let _ = tx.send(file_ops::ProgressMessage::PrepareComplete);
            let _ = tx.send(file_ops::ProgressMessage::FileStarted(
                file_name_owned.clone(),
            ));
            let _ = tx.send(file_ops::ProgressMessage::TotalProgress(0, 1, 0, file_size));

            // 프로그레스 콜백과 함께 다운로드
            let local_path_str = tmp_path_clone.display().to_string();
            match session.download_file_with_progress(
                &remote_path_clone,
                &local_path_str,
                file_size,
                &cancel_flag,
                |downloaded, total| {
                    let _ = tx.send(file_ops::ProgressMessage::FileProgress(downloaded, total));
                    let _ = tx.send(file_ops::ProgressMessage::TotalProgress(
                        0, 1, downloaded, total,
                    ));
                },
            ) {
                Ok(_) => {
                    let _ = tx.send(file_ops::ProgressMessage::FileCompleted(file_name_owned));
                    let _ = tx.send(file_ops::ProgressMessage::TotalProgress(
                        1, 1, file_size, file_size,
                    ));
                    let _ = tx.send(file_ops::ProgressMessage::Completed(1, 0));
                }
                Err(e) => {
                    let _ = tx.send(file_ops::ProgressMessage::Error(
                        file_name_owned,
                        e.to_string(),
                    ));
                    let _ = tx.send(file_ops::ProgressMessage::Completed(0, 1));
                }
            }
        });

        self.pending_remote_open = Some(open_action);
        self.file_operation_progress = Some(progress);
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Progress,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    /// Handle goto for remote path (user@host:/path)
    pub(crate) fn execute_goto_remote(
        &mut self,
        user: &str,
        host: &str,
        port: u16,
        remote_path: &str,
    ) {
        // Check if we have a matching saved profile
        if let Some(profile) =
            remote::find_matching_profile(&self.settings.remote_profiles, user, host, port)
        {
            // Use saved profile credentials to connect
            let profile = profile.clone();
            let path = if remote_path == "/" && !profile.default_path.is_empty() {
                profile.default_path.clone()
            } else {
                remote_path.to_string()
            };
            self.connect_remote_panel(&profile, &path);
        } else {
            // No saved profile — show remote connect dialog for auth
            let state = RemoteConnectState::from_parsed(user, host, port, remote_path);
            self.remote_connect_state = Some(state);
            self.dialog = Some(Dialog {
                dialog_type: DialogType::RemoteConnect,
                input: String::new(),
                cursor_pos: 0,
                message: format!("Connect to {}@{}:{}", user, host, port),
                completion: None,
                selected_button: 0,
                selection: None,
                use_md5: false,
            });
        }
    }

    /// Handle goto for relative path on a remote panel (async with spinner)
    pub(crate) fn execute_goto_remote_relative(&mut self, path_str: &str) {
        if self.remote_spinner.is_some() {
            return;
        }

        let current = self.active_panel().path.display().to_string();
        let new_path = if path_str == ".." {
            // Go to parent directory
            if current == "/" {
                return;
            }
            let parent = std::path::Path::new(&current)
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/".to_string());
            parent
        } else if path_str.starts_with('/') {
            path_str.to_string()
        } else {
            format!("{}/{}", current.trim_end_matches('/'), path_str)
        };

        self.spawn_remote_list_dir(&new_path);
    }

    /// Connect a panel to a remote server (async with spinner)
    pub fn connect_remote_panel(&mut self, profile: &remote::RemoteProfile, path: &str) {
        if self.remote_spinner.is_some() {
            return;
        }

        let (tx, rx) = mpsc::channel();
        let profile_clone = profile.clone();
        let path_clone = path.to_string();
        let panel_idx = self.active_panel_index;

        thread::spawn(move || {
            let result = match remote::SftpSession::connect(&profile_clone) {
                Ok(session) => {
                    let mut ctx = RemoteContext {
                        profile: profile_clone.clone(),
                        session,
                        status: ConnectionStatus::Connected,
                    };
                    // Try listing the requested path
                    match ctx.session.list_dir(&path_clone) {
                        Ok(entries) => Ok(ConnectSuccess {
                            ctx: Box::new(ctx),
                            entries,
                            path: path_clone,
                            fallback_msg: None,
                            profile: profile_clone,
                        }),
                        Err(_) => {
                            // Fallback to /
                            match ctx.session.list_dir("/") {
                                Ok(entries) => Ok(ConnectSuccess {
                                    ctx: Box::new(ctx),
                                    entries,
                                    path: "/".to_string(),
                                    fallback_msg: Some(format!(
                                        "Path not found: {} — moved to /",
                                        path_clone
                                    )),
                                    profile: profile_clone,
                                }),
                                Err(e2) => Err(format!("Connection failed: {}", e2)),
                            }
                        }
                    }
                }
                Err(e) => Err(format!("Connection failed: {}", e)),
            };
            let _ = tx.send(RemoteSpinnerResult::Connected { result, panel_idx });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: format!("Connecting to {}@{}...", profile.user, profile.host),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    /// Disconnect remote panel and switch back to local
    pub fn disconnect_remote_panel(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(mut ctx) = panel.remote_ctx.take() {
            ctx.session.disconnect();
        }
        panel.remote_display = None;
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        panel.path = home;
        panel.selected_index = 0;
        panel.selected_files.clear();
        panel.load_files();
        self.show_message("Disconnected from remote server");
    }

    /// Spawn a background thread for remote list_dir operation
    pub(crate) fn spawn_remote_list_dir(&mut self, new_path: &str) {
        if self.remote_spinner.is_some() {
            return;
        }
        let panel_idx = self.active_panel_index;
        let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
            Some(ctx) => ctx,
            None => return,
        };
        // Save old path for rollback on failure
        let old_path = self.panels[panel_idx].path.clone();
        // Update panel path now so header shows the new remote path during loading
        self.panels[panel_idx].path = PathBuf::from(new_path);
        let path = new_path.to_string();
        let path_for_result = PathBuf::from(new_path);
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let entries = ctx.session.list_dir(&path).map_err(|e| e.to_string());
            let _ = tx.send(RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome: PanelOpOutcome::ListDir {
                    entries,
                    path: path_for_result,
                    old_path: Some(old_path),
                },
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Loading...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    /// Spawn a background thread for remote list_dir (for panel refresh)
    pub fn spawn_remote_refresh(&mut self, panel_idx: usize) {
        if self.remote_spinner.is_some() {
            return;
        }
        let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
            Some(ctx) => ctx,
            None => return,
        };
        let path = self.panels[panel_idx].path.display().to_string();
        let path_for_result = self.panels[panel_idx].path.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let entries = ctx.session.list_dir(&path).map_err(|e| e.to_string());
            let _ = tx.send(RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome: PanelOpOutcome::ListDir {
                    entries,
                    path: path_for_result,
                    old_path: None,
                },
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Loading...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    /// Poll the remote spinner for completion
    pub fn poll_remote_spinner(&mut self) {
        let result = if let Some(ref spinner) = self.remote_spinner {
            match spinner.receiver.try_recv() {
                Ok(result) => Some(Ok(result)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(())),
            }
        } else {
            return;
        };

        let result = match result {
            Some(Ok(r)) => r,
            Some(Err(())) => {
                // Thread panicked or sender dropped — cancel spinner
                self.remote_spinner = None;
                self.show_message("Remote operation failed unexpectedly");
                return;
            }
            None => return,
        };

        // Spinner completed — remove it
        self.remote_spinner = None;

        match result {
            RemoteSpinnerResult::Connected { result, panel_idx } => {
                match result {
                    Ok(success) => {
                        let panel = &mut self.panels[panel_idx];
                        panel.remote_display = Some((
                            success.ctx.profile.user.clone(),
                            success.ctx.profile.host.clone(),
                            success.ctx.profile.port,
                        ));
                        panel.remote_ctx = Some(success.ctx);
                        panel.selected_index = 0;
                        panel.selected_files.clear();
                        // Update connection status
                        if let Some(ref mut ctx) = panel.remote_ctx {
                            ctx.status = ConnectionStatus::Connected;
                        }
                        panel.apply_remote_entries(success.entries, &PathBuf::from(&success.path));

                        // Auto-save profile and bookmark on first connection to this server
                        let already_has_profile = self.settings.remote_profiles.iter().any(|p| {
                            p.user == success.profile.user
                                && p.host == success.profile.host
                                && p.port == success.profile.port
                        });
                        let already_bookmarked = self.settings.bookmarked_path.iter().any(|bm| {
                            if let Some((bu, bh, bp, _)) = remote::parse_remote_path(bm) {
                                bu == success.profile.user
                                    && bh == success.profile.host
                                    && bp == success.profile.port
                            } else {
                                false
                            }
                        });
                        let mut settings_changed = false;
                        if !already_has_profile {
                            self.settings.remote_profiles.push(success.profile.clone());
                            settings_changed = true;
                        }
                        if !already_bookmarked {
                            let bookmark_path =
                                remote::format_remote_display(&success.profile, &success.path);
                            self.settings.bookmarked_path.push(bookmark_path);
                            settings_changed = true;
                        }
                        if settings_changed {
                            let _ = self.settings.save();
                        }

                        if let Some(msg) = success.fallback_msg {
                            self.show_extension_handler_error(&msg);
                        } else {
                            self.show_message(&format!(
                                "Connected to {}@{}",
                                success.profile.user, success.profile.host
                            ));
                        }
                    }
                    Err(e) => {
                        self.show_message(&e);
                    }
                }
            }
            RemoteSpinnerResult::PanelOp {
                ctx,
                panel_idx,
                outcome,
            } => {
                // Return ctx to panel
                self.panels[panel_idx].remote_ctx = Some(ctx);

                match outcome {
                    PanelOpOutcome::Simple {
                        message,
                        pending_focus,
                        reload,
                    } => {
                        let (msg_text, is_err) = match &message {
                            Ok(msg) => (msg.clone(), false),
                            Err(e) => (format!("Error: {}", e), true),
                        };
                        if !is_err {
                            if let Some(focus) = pending_focus {
                                self.panels[panel_idx].pending_focus = Some(focus);
                            }
                        }
                        // If in editor, set editor message; otherwise show app message
                        if self.current_screen == Screen::FileEditor {
                            if let Some(ref mut editor) = self.editor_state {
                                let duration = if is_err { 50 } else { 30 };
                                editor.set_message(msg_text, duration);
                            }
                        } else {
                            self.show_message(&msg_text);
                        }
                        if reload {
                            // Refresh local panels synchronously
                            for i in 0..self.panels.len() {
                                if !self.panels[i].is_remote() {
                                    self.panels[i].selected_files.clear();
                                    self.panels[i].load_files();
                                }
                            }
                            // For the remote panel, spawn another list_dir
                            if self.panels[panel_idx].is_remote() {
                                self.spawn_remote_refresh(panel_idx);
                            }
                        }
                    }
                    PanelOpOutcome::ListDir {
                        entries,
                        path,
                        old_path,
                    } => {
                        match entries {
                            Ok(sftp_entries) => {
                                let panel = &mut self.panels[panel_idx];
                                panel.selected_index = 0;
                                panel.selected_files.clear();
                                if let Some(ref mut ctx) = panel.remote_ctx {
                                    ctx.status = ConnectionStatus::Connected;
                                }
                                panel.apply_remote_entries(sftp_entries, &path);
                            }
                            Err(e) => {
                                // Rollback path on failure
                                if let Some(prev) = old_path {
                                    self.panels[panel_idx].path = prev;
                                }
                                if let Some(ref mut ctx) = self.panels[panel_idx].remote_ctx {
                                    ctx.status = ConnectionStatus::Disconnected(e.clone());
                                }
                                self.show_message(&format!("Error: {}", e));
                            }
                        }
                    }
                    PanelOpOutcome::DirExists {
                        exists,
                        target_entry,
                    } => {
                        if exists {
                            self.execute_goto(&target_entry);
                        } else {
                            self.show_extension_handler_error(&format!(
                                "Path not found: {}",
                                target_entry
                            ));
                        }
                    }
                }
            }
            RemoteSpinnerResult::LocalOp { message, reload } => {
                match &message {
                    Ok(msg) => self.show_message(msg),
                    Err(e) => self.show_message(e),
                }
                if reload {
                    self.refresh_panels();
                }
            }
            RemoteSpinnerResult::SearchComplete {
                results,
                search_term,
                base_path,
            } => {
                if results.is_empty() {
                    self.show_message(&format!("No files found matching \"{}\"", search_term));
                } else {
                    self.search_result_state.results = results;
                    self.search_result_state.selected_index = 0;
                    self.search_result_state.scroll_offset = 0;
                    self.search_result_state.search_term = search_term;
                    self.search_result_state.base_path = base_path;
                    self.search_result_state.active = true;
                    self.current_screen = Screen::SearchResult;
                }
            }
            RemoteSpinnerResult::GitDiffComplete { result } => match result {
                Ok((dir1, dir2)) => {
                    self.enter_diff_screen(dir1, dir2);
                }
                Err(e) => {
                    self.show_message(&e);
                }
            },
        }
    }
}
