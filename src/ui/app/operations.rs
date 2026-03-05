use super::*;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::services::file_ops::{self, FileOperationType, ProgressMessage};
use crate::services::remote;
use crate::services::remote_transfer;
use crate::ui::file_editor::EditorState;

impl App {
    /// Start diff comparison between panels
    /// With 2 panels: immediately enter diff screen
    /// With 3+ panels: first call selects first panel, second call selects second panel
    pub fn start_diff(&mut self) {
        if self.panels.iter().any(|p| p.is_remote()) {
            self.show_message("Diff is not supported for remote panels");
            return;
        }

        // Priority: if exactly 2 directories are selected in active panel, diff them
        let panel = &self.panels[self.active_panel_index];
        let selected_dirs: Vec<PathBuf> = panel
            .files
            .iter()
            .filter(|f| f.is_directory && panel.selected_files.contains(&f.name))
            .map(|f| panel.path.join(&f.name))
            .collect();
        if selected_dirs.len() == 2 {
            let left = selected_dirs[0].clone();
            let right = selected_dirs[1].clone();
            self.panels[self.active_panel_index].selected_files.clear();
            self.enter_diff_screen(left, right);
            return;
        }

        if self.panels.len() < 2 {
            self.show_message("Need at least 2 panels for diff");
            return;
        }

        if self.panels.len() == 2 {
            // 2 panels: immediate diff
            let left = self.panels[0].path.clone();
            let right = self.panels[1].path.clone();
            self.enter_diff_screen(left, right);
        } else {
            // 3+ panels: 2-stage selection
            if let Some(first) = self.diff_first_panel {
                // Second selection
                let second = self.active_panel_index;
                if first == second {
                    self.show_message("Select a different panel for diff");
                    return;
                }
                let left = self.panels[first].path.clone();
                let right = self.panels[second].path.clone();
                self.diff_first_panel = None;
                self.enter_diff_screen(left, right);
            } else {
                // First selection
                self.diff_first_panel = Some(self.active_panel_index);
                let diff_key = self
                    .keybindings
                    .panel_first_key(crate::keybindings::PanelAction::StartDiff);
                let cancel_key = self
                    .keybindings
                    .panel_first_key(crate::keybindings::PanelAction::ParentDir);
                self.show_message(&format!(
                    "Select second panel for diff ({}) or {} to cancel",
                    diff_key, cancel_key
                ));
            }
        }
    }

    /// Enter diff screen with two directory paths
    pub fn enter_diff_screen(&mut self, left: PathBuf, right: PathBuf) {
        if left == right {
            self.show_message("Both paths are the same");
            return;
        }
        let compare_method =
            crate::ui::diff_screen::parse_compare_method(&self.settings.diff_compare_method);
        let sort_by = self.active_panel().sort_by;
        let sort_order = self.active_panel().sort_order;
        let mut state = crate::ui::diff_screen::DiffState::new(
            left,
            right,
            compare_method,
            sort_by,
            sort_order,
        );
        state.start_comparison();
        self.diff_state = Some(state);
        self.current_screen = Screen::DiffScreen;
    }

    /// Enter file content diff view from the diff screen
    pub fn enter_diff_file_view(
        &mut self,
        left_path: PathBuf,
        right_path: PathBuf,
        file_name: String,
    ) {
        self.diff_file_view_state = Some(crate::ui::diff_file_view::DiffFileViewState::new(
            left_path, right_path, file_name,
        ));
        self.current_screen = Screen::DiffFileView;
    }

    /// Calculate total size and build file size map for tar progress
    fn calculate_tar_sizes(
        base_dir: &Path,
        files: &[String],
    ) -> (u64, std::collections::HashMap<String, u64>) {
        use std::collections::HashMap;
        let mut total_size = 0u64;
        let mut size_map = HashMap::new();

        for file in files {
            let path = base_dir.join(file);
            Self::collect_file_sizes(
                &path,
                &format!("./{}", file),
                &mut size_map,
                &mut total_size,
            );
        }

        (total_size, size_map)
    }

    /// Collect file sizes recursively, matching tar's output format
    fn collect_file_sizes(
        path: &Path,
        tar_path: &str,
        size_map: &mut std::collections::HashMap<String, u64>,
        total_size: &mut u64,
    ) {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if metadata.is_dir() {
                // Directory itself (tar lists directories too)
                size_map.insert(tar_path.to_string(), 0);

                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let entry_name = entry.file_name().to_string_lossy().to_string();
                        let child_tar_path = format!("{}/{}", tar_path, entry_name);
                        Self::collect_file_sizes(
                            &entry.path(),
                            &child_tar_path,
                            size_map,
                            total_size,
                        );
                    }
                }
            } else {
                // Regular file or symlink
                let size = metadata.len();
                size_map.insert(tar_path.to_string(), size);
                *total_size += size;
            }
        }
    }

    pub fn execute_encrypt(&mut self, split_size_mb: u64, use_md5: bool) {
        // Remember split size for next time
        self.settings.encrypt_split_size = split_size_mb;

        let key_path = match crate::enc::ensure_key() {
            Ok(p) => p,
            Err(e) => {
                self.show_message(&format!("Key error: {}", e));
                return;
            }
        };

        let dir = self.active_panel().path.clone();

        let mut progress = FileOperationProgress::new(FileOperationType::Encrypt);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        thread::spawn(move || {
            crate::enc::pack_directory_with_progress(
                &dir,
                &key_path,
                tx,
                cancel_flag,
                split_size_mb,
                use_md5,
            );
        });

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

    pub fn execute_decrypt(&mut self) {
        let key_path = match crate::enc::ensure_key() {
            Ok(p) => p,
            Err(e) => {
                self.show_message(&format!("Key error: {}", e));
                return;
            }
        };

        let dir = self.active_panel().path.clone();

        let mut progress = FileOperationProgress::new(FileOperationType::Decrypt);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        thread::spawn(move || {
            crate::enc::unpack_directory_with_progress(&dir, &key_path, tx, cancel_flag);
        });

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

    pub fn execute_dedup(&mut self) {
        let path = self.active_panel().path.clone();
        self.dedup_screen_state = Some(crate::ui::dedup_screen::DedupScreenState::new(path));
        self.current_screen = Screen::DedupScreen;
    }

    pub fn execute_git_log_diff(&mut self) {
        self.dialog = None;

        let state = match self.git_log_diff_state.take() {
            Some(s) => s,
            None => return,
        };
        if state.selected_commits.len() != 2 {
            return;
        }
        let hash1 = state.selected_commits[0].clone();
        let hash2 = state.selected_commits[1].clone();

        // Validate hashes
        if !hash1.chars().all(|c| c.is_ascii_alphanumeric())
            || !hash2.chars().all(|c| c.is_ascii_alphanumeric())
        {
            self.show_message("Invalid commit hash");
            return;
        }

        if self.remote_spinner.is_some() {
            return;
        }

        let project_name = state.project_name.clone();
        let repo_path = state.repo_path.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let diff_base = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".remotecc")
                .join("diff");

            let _ = std::fs::remove_dir_all(&diff_base);
            if std::fs::create_dir_all(&diff_base).is_err() {
                let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                    result: Err("Failed to create diff directory".to_string()),
                });
                return;
            }

            let dir1 = diff_base.join(format!("{}_{}", project_name, hash1));
            let dir2 = diff_base.join(format!("{}_{}", project_name, hash2));

            for (dir, hash) in [(&dir1, &hash1), (&dir2, &hash2)] {
                let repo_str = repo_path.display().to_string();
                let dir_str = dir.display().to_string();
                let status = std::process::Command::new("cp")
                    .args(["-a", &repo_str, &dir_str])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if status.map(|s| !s.success()).unwrap_or(true) {
                    let _ = std::fs::remove_dir_all(&diff_base);
                    let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                        result: Err("Failed to copy repository".to_string()),
                    });
                    return;
                }

                let checkout_status = crate::ui::git_screen::git_cmd_public(dir)
                    .args(["checkout", hash.as_str()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if checkout_status.map(|s| !s.success()).unwrap_or(true) {
                    let _ = std::fs::remove_dir_all(&diff_base);
                    let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                        result: Err(format!("Failed to checkout {}", hash)),
                    });
                    return;
                }

                let _ = std::fs::remove_dir_all(dir.join(".git"));
            }

            let _ = tx.send(RemoteSpinnerResult::GitDiffComplete {
                result: Ok((dir1, dir2)),
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Preparing diff...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    pub fn execute_advanced_search(
        &mut self,
        criteria: &crate::ui::advanced_search::SearchCriteria,
    ) {
        let panel = self.active_panel_mut();
        let mut matched_count = 0;

        panel.selected_files.clear();

        for file in &panel.files {
            if file.name == ".." {
                continue;
            }

            if crate::ui::advanced_search::matches_criteria(
                &file.name,
                file.size,
                file.modified,
                criteria,
            ) {
                panel.selected_files.insert(file.name.clone());
                matched_count += 1;
            }
        }

        if matched_count > 0 {
            self.show_message(&format!("Found {} matching file(s)", matched_count));
        } else {
            self.show_message("No files match the criteria");
        }
    }

    pub fn execute_delete(&mut self) {
        // 이미지 뷰어에서 삭제 시 현재 보고 있는 이미지 삭제
        if self.current_screen == Screen::ImageViewer {
            if let Some(ref state) = self.image_viewer_state {
                let path = state.path.clone();
                match file_ops::delete_file(&path) {
                    Ok(_) => {
                        self.show_message("Deleted image");
                        // 이미지 뷰어 닫기
                        self.current_screen = Screen::FilePanel;
                        self.image_viewer_state = None;
                    }
                    Err(e) => {
                        self.show_message(&format!("Delete failed: {}", e));
                    }
                }
                self.refresh_panels();
            }
            return;
        }

        let files = self.get_operation_files();
        let source_path = self.active_panel().path.clone();
        let is_remote = self.active_panel().is_remote();

        let mut success_count = 0;
        let mut last_error = String::new();

        if is_remote {
            // Remote delete via SFTP (async with spinner)
            if self.remote_spinner.is_some() {
                return;
            }
            let panel_idx = self.active_panel_index;
            let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                Some(ctx) => ctx,
                None => return,
            };
            let remote_base = source_path.display().to_string();
            // Collect file info before spawning thread
            let file_infos: Vec<(String, bool)> = files
                .iter()
                .map(|file_name| {
                    let is_dir = self
                        .active_panel()
                        .files
                        .iter()
                        .find(|f| f.name == *file_name)
                        .map(|f| f.is_directory)
                        .unwrap_or(false);
                    (file_name.clone(), is_dir)
                })
                .collect();
            let total = file_infos.len();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let mut success_count = 0;
                let mut last_error = String::new();
                for (file_name, is_dir) in &file_infos {
                    let remote_path =
                        format!("{}/{}", remote_base.trim_end_matches('/'), file_name);
                    match ctx.session.remove(&remote_path, *is_dir) {
                        Ok(_) => success_count += 1,
                        Err(e) => last_error = e.to_string(),
                    }
                }
                let msg = if success_count == total {
                    Ok(format!("Deleted {} file(s)", success_count))
                } else {
                    Err(format!(
                        "Deleted {}/{}. Error: {}",
                        success_count, total, last_error
                    ))
                };
                let _ = tx.send(RemoteSpinnerResult::PanelOp {
                    ctx,
                    panel_idx,
                    outcome: PanelOpOutcome::Simple {
                        message: msg,
                        pending_focus: None,
                        reload: true,
                    },
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Deleting...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
            return;
        } else {
            // Local delete in background thread with spinner
            if self.remote_spinner.is_some() {
                return;
            }
            let files_to_delete: Vec<PathBuf> = files.iter().map(|f| source_path.join(f)).collect();
            let total = files_to_delete.len();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let mut success_count = 0;
                let mut last_error = String::new();
                for path in &files_to_delete {
                    match file_ops::delete_file(path) {
                        Ok(_) => success_count += 1,
                        Err(e) => last_error = e.to_string(),
                    }
                }
                let msg = if success_count == total {
                    Ok(format!("Deleted {} file(s)", success_count))
                } else {
                    Err(format!(
                        "Deleted {}/{}. Error: {}",
                        success_count, total, last_error
                    ))
                };
                let _ = tx.send(RemoteSpinnerResult::LocalOp {
                    message: msg,
                    reload: true,
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Deleting...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
        }
    }

    // ========== Clipboard operations (Ctrl+C/X/V) ==========

    /// Copy selected files to clipboard (Ctrl+C)
    pub fn clipboard_copy(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        let source_path = self.active_panel().path.clone();
        let source_remote_profile = self
            .active_panel()
            .remote_ctx
            .as_ref()
            .map(|c| c.profile.clone());
        let count = files.len();

        self.clipboard = Some(Clipboard {
            files,
            source_path,
            operation: ClipboardOperation::Copy,
            source_remote_profile,
        });

        self.show_message(&format!("{} file(s) copied to clipboard", count));
    }

    /// Cut selected files to clipboard (Ctrl+X)
    pub fn clipboard_cut(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        let source_path = self.active_panel().path.clone();
        let source_remote_profile = self
            .active_panel()
            .remote_ctx
            .as_ref()
            .map(|c| c.profile.clone());
        let count = files.len();

        self.clipboard = Some(Clipboard {
            files,
            source_path,
            operation: ClipboardOperation::Cut,
            source_remote_profile,
        });

        self.show_message(&format!("{} file(s) cut to clipboard", count));
    }

    /// Paste files from clipboard to current panel (Ctrl+V)
    pub fn clipboard_paste(&mut self) {
        let clipboard = match self.clipboard.take() {
            Some(cb) => cb,
            None => {
                self.show_message("Clipboard is empty");
                return;
            }
        };

        let source_is_remote = clipboard.source_remote_profile.is_some();
        let target_is_remote = self.active_panel().is_remote();
        let target_remote_profile = self
            .active_panel()
            .remote_ctx
            .as_ref()
            .map(|c| c.profile.clone());

        // Remote involved — use remote transfer path (no conflict detection for remote)
        if source_is_remote || target_is_remote {
            let is_cut = clipboard.operation == ClipboardOperation::Cut;
            let op_type = if is_cut {
                FileOperationType::Move
            } else {
                FileOperationType::Copy
            };

            // Remote-to-remote: download to local temp, then upload
            if source_is_remote && target_is_remote {
                let source_profile = match clipboard.source_remote_profile.clone() {
                    Some(p) => p,
                    None => {
                        self.clipboard = Some(clipboard);
                        self.show_message("Source remote profile not found");
                        return;
                    }
                };
                let target_profile = match target_remote_profile {
                    Some(p) => p,
                    None => {
                        self.clipboard = Some(clipboard);
                        self.show_message("Target remote profile not found");
                        return;
                    }
                };

                let target_path = self.active_panel().path.clone();
                let file_paths: Vec<PathBuf> = clipboard.files.iter().map(PathBuf::from).collect();
                let source_base = clipboard.source_path.display().to_string();
                let target = target_path.display().to_string();

                // Set pending focus to pasted file names
                if !clipboard.files.is_empty() {
                    self.pending_paste_focus = Some(clipboard.files.clone());
                }

                let mut progress = FileOperationProgress::new(op_type);
                progress.is_active = true;
                progress.total_files = file_paths.len();
                let cancel_flag = progress.cancel_flag.clone();
                let (tx, rx) = mpsc::channel();
                progress.receiver = Some(rx);

                thread::spawn(move || {
                    remote_transfer::transfer_remote_to_remote_with_progress(
                        source_profile,
                        target_profile,
                        file_paths,
                        source_base,
                        target,
                        cancel_flag,
                        tx,
                        is_cut,
                    );
                });

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

                // Keep clipboard for copy, consume for cut
                if !is_cut {
                    self.clipboard = Some(clipboard);
                }
                return;
            }

            let profile = if source_is_remote {
                clipboard.source_remote_profile.clone()
            } else {
                target_remote_profile
            };

            let Some(profile) = profile else {
                self.clipboard = Some(clipboard);
                self.show_message("Remote profile not found");
                return;
            };

            let direction = if source_is_remote {
                remote_transfer::TransferDirection::RemoteToLocal
            } else {
                remote_transfer::TransferDirection::LocalToRemote
            };

            // For cut: determine source_profile for deletion
            // RemoteToLocal: source is remote → pass source remote profile
            // LocalToRemote: source is local → None
            let source_profile_for_delete = if is_cut && source_is_remote {
                clipboard.source_remote_profile.clone()
            } else {
                None
            };

            let target_path = self.active_panel().path.clone();
            let valid_files: Vec<String> = clipboard.files.clone();
            let file_paths: Vec<PathBuf> = valid_files.iter().map(PathBuf::from).collect();
            let source_base = clipboard.source_path.display().to_string();
            let target = target_path.display().to_string();

            // Set pending focus to pasted file names
            if !valid_files.is_empty() {
                self.pending_paste_focus = Some(valid_files.clone());
            }

            let mut progress = FileOperationProgress::new(op_type);
            progress.is_active = true;
            progress.total_files = file_paths.len();
            let cancel_flag = progress.cancel_flag.clone();
            let (tx, rx) = mpsc::channel();
            progress.receiver = Some(rx);

            let config = remote_transfer::TransferConfig {
                direction,
                profile,
                source_files: file_paths,
                source_base,
                target_path: target,
            };

            thread::spawn(move || {
                remote_transfer::transfer_files_with_progress(
                    config,
                    cancel_flag,
                    tx,
                    is_cut,
                    source_profile_for_delete,
                );
            });

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

            // Keep clipboard for copy, consume for cut
            if !is_cut {
                self.clipboard = Some(clipboard);
            }
            return;
        }

        // Both local — existing local paste logic
        let target_path = self.active_panel().path.clone();

        // Check if source and target are the same (use canonical paths for robustness)
        let is_same_folder = match (
            clipboard.source_path.canonicalize(),
            target_path.canonicalize(),
        ) {
            (Ok(src), Ok(dest)) => src == dest,
            _ => clipboard.source_path == target_path, // Fallback to direct comparison
        };

        if is_same_folder {
            // For Cut operation in same folder, it doesn't make sense
            if clipboard.operation == ClipboardOperation::Cut {
                self.clipboard = Some(clipboard);
                self.show_message("Cannot move files to the same folder");
                return;
            }
            // For Copy operation in same folder, create duplicate with _dup suffix
            self.execute_same_folder_paste(clipboard);
            return;
        }

        // Verify source path still exists
        if !clipboard.source_path.exists() {
            self.show_message("Source folder no longer exists");
            return; // Don't restore clipboard - source is gone
        }

        // Verify target is a valid directory
        if !target_path.is_dir() {
            self.clipboard = Some(clipboard);
            self.show_message("Target is not a valid directory");
            return;
        }

        // Get canonical target path for cycle detection
        let canonical_target = target_path.canonicalize().ok();

        // Filter out files that would cause cycle
        let mut valid_files: Vec<String> = Vec::new();
        for file_name in &clipboard.files {
            let src = clipboard.source_path.join(file_name);

            // Check for copying/moving directory into itself
            if let (Some(ref target_canon), Ok(src_canon)) = (&canonical_target, src.canonicalize())
            {
                if src.is_dir() && target_canon.starts_with(&src_canon) {
                    self.show_message(&format!("Cannot copy '{}' into itself", file_name));
                    continue;
                }
            }
            valid_files.push(file_name.clone());
        }

        if valid_files.is_empty() {
            self.clipboard = Some(clipboard);
            return;
        }

        // Detect conflicts (files that already exist at destination)
        let conflicts = self.detect_paste_conflicts(&clipboard, &target_path, &valid_files);

        if !conflicts.is_empty() {
            // Has conflicts - show conflict dialog
            let is_move = clipboard.operation == ClipboardOperation::Cut;
            self.conflict_state = Some(ConflictState {
                conflicts,
                current_index: 0,
                files_to_overwrite: Vec::new(),
                files_to_skip: Vec::new(),
                clipboard_backup: Some(clipboard),
                is_move_operation: is_move,
                target_path: target_path.clone(),
            });
            self.show_duplicate_conflict_dialog();
            return;
        }

        // No conflicts - proceed with normal paste
        self.execute_paste_operation(clipboard, valid_files, target_path);
    }

    /// Detect files that would conflict (already exist) at paste destination
    fn detect_paste_conflicts(
        &self,
        clipboard: &Clipboard,
        target_dir: &Path,
        valid_files: &[String],
    ) -> Vec<(PathBuf, PathBuf, String)> {
        let mut conflicts = Vec::new();

        for file_name in valid_files {
            let src = clipboard.source_path.join(file_name);
            let dest = target_dir.join(file_name);

            if dest.exists() {
                conflicts.push((src, dest, file_name.clone()));
            }
        }

        conflicts
    }

    /// Generate a duplicate filename with _dup suffix, checking for existence
    /// e.g., "file.txt" -> "file_dup.txt", if exists -> "file_dup2.txt", etc.
    fn generate_dup_filename(name: &str, target_dir: &Path) -> String {
        let generate_name = |base: &str, ext: &str, suffix: &str| -> String {
            if ext.is_empty() {
                format!("{}{}", base, suffix)
            } else {
                format!("{}{}{}", base, suffix, ext)
            }
        };

        let (base, ext) = if let Some(dot_pos) = name.rfind('.') {
            let (b, e) = name.split_at(dot_pos);
            (b.to_string(), e.to_string())
        } else {
            (name.to_string(), String::new())
        };

        // Try _dup first
        let dup_name = generate_name(&base, &ext, "_dup");
        if !target_dir.join(&dup_name).exists() {
            return dup_name;
        }

        // If _dup exists, try _dup2, _dup3, etc.
        let mut counter = 2;
        loop {
            let suffix = format!("_dup{}", counter);
            let dup_name = generate_name(&base, &ext, &suffix);
            if !target_dir.join(&dup_name).exists() {
                return dup_name;
            }
            counter += 1;
            // Safety limit to prevent infinite loop
            if counter > 10000 {
                return generate_name(
                    &base,
                    &ext,
                    &format!(
                        "_dup{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0)
                    ),
                );
            }
        }
    }

    /// Execute paste operation (internal, called after conflict resolution or when no conflicts)
    fn execute_paste_operation(
        &mut self,
        clipboard: Clipboard,
        valid_files: Vec<String>,
        target_path: PathBuf,
    ) {
        // Set pending focus to pasted file names (will find first match in sorted file list)
        if !valid_files.is_empty() {
            self.pending_paste_focus = Some(valid_files.clone());
        }

        // Determine operation type for progress
        let operation_type = match clipboard.operation {
            ClipboardOperation::Copy => FileOperationType::Copy,
            ClipboardOperation::Cut => FileOperationType::Move,
        };

        // Create progress state
        let mut progress = FileOperationProgress::new(operation_type);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Convert files to PathBuf
        let file_paths: Vec<PathBuf> = valid_files.iter().map(PathBuf::from).collect();
        let source_path = clipboard.source_path.clone();

        // Start operation in background thread
        let clipboard_operation = clipboard.operation;
        thread::spawn(move || match clipboard_operation {
            ClipboardOperation::Copy => {
                file_ops::copy_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    HashSet::new(),
                    HashSet::new(),
                    cancel_flag,
                    tx,
                );
            }
            ClipboardOperation::Cut => {
                file_ops::move_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    HashSet::new(),
                    HashSet::new(),
                    cancel_flag,
                    tx,
                );
            }
        });

        // Store progress state and show dialog
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

        // Keep clipboard for copy operations (can paste multiple times)
        // Clear clipboard for cut operations (files are moved)
        if clipboard.operation == ClipboardOperation::Copy {
            self.clipboard = Some(clipboard);
        }
    }

    /// Execute paste operation for same folder (creates _dup copies)
    fn execute_same_folder_paste(&mut self, clipboard: Clipboard) {
        let source_path = clipboard.source_path.clone();

        // Filter valid files (skip ".." and non-existent)
        let valid_files: Vec<String> = clipboard
            .files
            .iter()
            .filter(|f| *f != ".." && source_path.join(f).exists())
            .cloned()
            .collect();

        if valid_files.is_empty() {
            self.clipboard = Some(clipboard);
            self.show_message("No valid files to duplicate");
            return;
        }

        // Create progress state
        let mut progress = FileOperationProgress::new(FileOperationType::Copy);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Build rename map: original name -> dup name
        let mut rename_map: Vec<(PathBuf, PathBuf)> = Vec::new();
        for file_name in &valid_files {
            let dup_name = Self::generate_dup_filename(file_name, &source_path);
            let src = source_path.join(file_name);
            let dest = source_path.join(&dup_name);
            rename_map.push((src, dest));
        }

        let file_count = rename_map.len();

        // Set pending focus to all dup file names (will find first match in sorted file list)
        let dup_names: Vec<String> = rename_map
            .iter()
            .filter_map(|(_, dest)| dest.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect();
        if !dup_names.is_empty() {
            self.pending_paste_focus = Some(dup_names);
        }

        // Start operation in background thread
        thread::spawn(move || {
            let mut completed = 0;
            let mut failed = 0;

            for (src, dest) in rename_map {
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }

                let file_name = src
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();

                // Safety check: never overwrite existing files
                if dest.exists() {
                    let _ = tx.send(crate::services::file_ops::ProgressMessage::Error(
                        file_name.clone(),
                        "destination already exists".to_string(),
                    ));
                    failed += 1;
                    continue;
                }

                let _ = tx.send(crate::services::file_ops::ProgressMessage::FileStarted(
                    file_name.clone(),
                ));

                let result = if src.is_dir() {
                    // Use create_dir (not create_dir_all) to fail if already exists
                    std::fs::create_dir(&dest).and_then(|_| {
                        // Now copy contents into the newly created directory
                        for entry in std::fs::read_dir(&src)? {
                            let entry = entry?;
                            let entry_src = entry.path();
                            let entry_dest = dest.join(entry.file_name());
                            if entry_src.is_dir() {
                                crate::services::file_ops::copy_dir_recursive(
                                    &entry_src,
                                    &entry_dest,
                                )?;
                            } else {
                                std::fs::copy(&entry_src, &entry_dest)?;
                            }
                        }
                        Ok(())
                    })
                } else {
                    // Use create_new to ensure we never overwrite
                    std::fs::File::create_new(&dest)
                        .and_then(|_| std::fs::copy(&src, &dest))
                        .map(|_| ())
                };

                match result {
                    Ok(_) => {
                        completed += 1;
                        let _ = tx.send(crate::services::file_ops::ProgressMessage::FileCompleted(
                            file_name,
                        ));
                    }
                    Err(e) => {
                        failed += 1;
                        let _ = tx.send(crate::services::file_ops::ProgressMessage::Error(
                            file_name,
                            e.to_string(),
                        ));
                    }
                }
            }

            let _ = tx.send(crate::services::file_ops::ProgressMessage::Completed(
                completed, failed,
            ));
        });

        // Store progress state and show dialog
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

        // Keep clipboard for copy operations
        self.clipboard = Some(clipboard);
    }

    /// Execute paste operation with conflict resolution (overwrite/skip sets)
    pub fn execute_paste_with_conflicts(&mut self) {
        let conflict_state = match self.conflict_state.take() {
            Some(state) => state,
            None => return,
        };

        let clipboard = match conflict_state.clipboard_backup {
            Some(cb) => cb,
            None => return,
        };

        let target_path = conflict_state.target_path;

        // Build all files to process (from original clipboard)
        let valid_files: Vec<String> = clipboard.files.clone();

        // Build overwrite and skip sets from source paths
        let files_to_overwrite: HashSet<PathBuf> =
            conflict_state.files_to_overwrite.into_iter().collect();
        let files_to_skip: HashSet<PathBuf> = conflict_state.files_to_skip.into_iter().collect();

        // Check if all files would be skipped
        let files_to_process: Vec<&String> = valid_files
            .iter()
            .filter(|f| {
                let src = clipboard.source_path.join(f);
                !files_to_skip.contains(&src)
            })
            .collect();

        // Set pending focus to all non-skipped file names (will find first match in sorted file list)
        if !files_to_process.is_empty() {
            self.pending_paste_focus =
                Some(files_to_process.iter().map(|f| (*f).clone()).collect());
        }

        if files_to_process.is_empty() {
            // All files were skipped - show message and restore clipboard if copy
            if clipboard.operation == ClipboardOperation::Copy {
                self.clipboard = Some(clipboard);
            }
            self.show_message("All files skipped");
            self.refresh_panels();
            return;
        }

        // Determine operation type for progress
        let operation_type = match clipboard.operation {
            ClipboardOperation::Copy => FileOperationType::Copy,
            ClipboardOperation::Cut => FileOperationType::Move,
        };

        // Create progress state
        let mut progress = FileOperationProgress::new(operation_type);
        progress.is_active = true;
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Convert files to PathBuf
        let file_paths: Vec<PathBuf> = valid_files.iter().map(PathBuf::from).collect();
        let source_path = clipboard.source_path.clone();

        // Start operation in background thread
        let clipboard_operation = clipboard.operation;
        thread::spawn(move || match clipboard_operation {
            ClipboardOperation::Copy => {
                file_ops::copy_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    files_to_overwrite,
                    files_to_skip,
                    cancel_flag,
                    tx,
                );
            }
            ClipboardOperation::Cut => {
                file_ops::move_files_with_progress(
                    file_paths,
                    &source_path,
                    &target_path,
                    files_to_overwrite,
                    files_to_skip,
                    cancel_flag,
                    tx,
                );
            }
        });

        // Store progress state and show dialog
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

        // Keep clipboard for copy operations (can paste multiple times)
        // Clear clipboard for cut operations (files are moved)
        if clipboard.operation == ClipboardOperation::Copy {
            self.clipboard = Some(clipboard);
        }
    }

    /// Check if clipboard has content
    pub fn has_clipboard(&self) -> bool {
        self.clipboard.is_some()
    }

    /// Get clipboard info for status display
    pub fn clipboard_info(&self) -> Option<(usize, &str)> {
        self.clipboard.as_ref().map(|cb| {
            let op = match cb.operation {
                ClipboardOperation::Copy => "copy",
                ClipboardOperation::Cut => "cut",
            };
            (cb.files.len(), op)
        })
    }

    pub fn execute_mkdir(&mut self, name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if self.active_panel().is_remote() {
            // Remote mkdir via SFTP (async with spinner)
            if self.remote_spinner.is_some() {
                return;
            }
            let panel_idx = self.active_panel_index;
            let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                Some(ctx) => ctx,
                None => return,
            };
            let remote_base = self.active_panel().path.display().to_string();
            let remote_path = format!("{}/{}", remote_base.trim_end_matches('/'), name);
            let focus_name = name.to_string();
            let display_name = name.to_string();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let msg = match ctx.session.mkdir(&remote_path) {
                    Ok(_) => Ok(format!("Created directory: {}", display_name)),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(RemoteSpinnerResult::PanelOp {
                    ctx,
                    panel_idx,
                    outcome: PanelOpOutcome::Simple {
                        message: msg,
                        pending_focus: Some(focus_name),
                        reload: true,
                    },
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Creating directory...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
            return;
        }

        let path = self.active_panel().path.join(name);

        // Additional check: ensure the resulting path is within the current directory
        if let Ok(canonical_parent) = self.active_panel().path.canonicalize() {
            if let Ok(canonical_new) = path.canonicalize().or_else(|_| {
                // For new directories, check the parent path
                path.parent()
                    .and_then(|p| p.canonicalize().ok())
                    .map(|p| p.join(name))
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, ""))
            }) {
                if !canonical_new.starts_with(&canonical_parent) {
                    self.show_message("Error: Path traversal attempt detected");
                    return;
                }
            }
        }

        match file_ops::create_directory(&path) {
            Ok(_) => {
                self.active_panel_mut().pending_focus = Some(name.to_string());
                self.show_message(&format!("Created directory: {}", name));
            }
            Err(e) => self.show_message(&format!("Error: {}", e)),
        }
        self.refresh_panels();
    }

    pub fn execute_mkfile(&mut self, name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if self.active_panel().is_remote() {
            // Remote file creation via SFTP (async with spinner)
            if self.remote_spinner.is_some() {
                return;
            }
            let panel_idx = self.active_panel_index;
            let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                Some(ctx) => ctx,
                None => return,
            };
            let remote_base = self.active_panel().path.display().to_string();
            let remote_path = format!("{}/{}", remote_base.trim_end_matches('/'), name);
            let focus_name = name.to_string();
            let display_name = name.to_string();
            let (tx, rx) = mpsc::channel();

            thread::spawn(move || {
                let msg = match ctx.session.create_file(&remote_path) {
                    Ok(_) => Ok(format!("Created file: {}", display_name)),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(RemoteSpinnerResult::PanelOp {
                    ctx,
                    panel_idx,
                    outcome: PanelOpOutcome::Simple {
                        message: msg,
                        pending_focus: Some(focus_name),
                        reload: true,
                    },
                });
            });

            self.remote_spinner = Some(RemoteSpinner {
                message: "Creating file...".to_string(),
                started_at: Instant::now(),
                receiver: rx,
            });
            return;
        }

        let path = self.active_panel().path.join(name);

        // Check if file already exists
        if path.exists() {
            self.show_message(&format!("'{}' already exists!", name));
            return;
        }

        // Create empty file
        match std::fs::File::create(&path) {
            Ok(_) => {
                self.active_panel_mut().pending_focus = Some(name.to_string());
                self.refresh_panels();

                // Open the file in editor
                let mut editor = EditorState::new();
                editor.set_syntax_colors(self.theme.syntax);
                match editor.load_file(&path) {
                    Ok(_) => {
                        self.editor_state = Some(editor);
                        self.current_screen = Screen::FileEditor;
                    }
                    Err(e) => {
                        self.show_message(&format!("File created but cannot open: {}", e));
                    }
                }
            }
            Err(e) => self.show_message(&format!("Error: {}", e)),
        }
    }

    pub fn execute_rename(&mut self, new_name: &str) {
        // Validate filename to prevent path traversal attacks
        if let Err(e) = file_ops::is_valid_filename(new_name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        if let Some(file) = self.active_panel().current_file() {
            let old_name = file.name.clone();

            if self.active_panel().is_remote() {
                // Remote rename via SFTP (async with spinner)
                if self.remote_spinner.is_some() {
                    return;
                }
                let panel_idx = self.active_panel_index;
                let mut ctx = match self.panels[panel_idx].remote_ctx.take() {
                    Some(ctx) => ctx,
                    None => return,
                };
                let remote_base = self.active_panel().path.display().to_string();
                let old_remote = format!("{}/{}", remote_base.trim_end_matches('/'), old_name);
                let new_remote = format!("{}/{}", remote_base.trim_end_matches('/'), new_name);
                let focus_name = new_name.to_string();
                let display_name = new_name.to_string();
                let (tx, rx) = mpsc::channel();

                thread::spawn(move || {
                    let msg = match ctx.session.rename(&old_remote, &new_remote) {
                        Ok(_) => Ok(format!("Renamed to: {}", display_name)),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(RemoteSpinnerResult::PanelOp {
                        ctx,
                        panel_idx,
                        outcome: PanelOpOutcome::Simple {
                            message: msg,
                            pending_focus: Some(focus_name),
                            reload: true,
                        },
                    });
                });

                self.remote_spinner = Some(RemoteSpinner {
                    message: "Renaming...".to_string(),
                    started_at: Instant::now(),
                    receiver: rx,
                });
                return;
            }

            let old_path = self.active_panel().path.join(&old_name);
            let new_path = self.active_panel().path.join(new_name);

            // Additional check: ensure the new path stays within the current directory
            if let Ok(canonical_parent) = self.active_panel().path.canonicalize() {
                // For rename, we verify against parent directory
                if let Some(new_parent) = new_path.parent() {
                    if let Ok(canonical_new_parent) = new_parent.canonicalize() {
                        if canonical_new_parent != canonical_parent {
                            self.show_message("Error: Path traversal attempt detected");
                            return;
                        }
                    }
                }
            }

            match file_ops::rename_file(&old_path, &new_path) {
                Ok(_) => {
                    self.active_panel_mut().pending_focus = Some(new_name.to_string());
                    self.show_message(&format!("Renamed to: {}", new_name));
                }
                Err(e) => self.show_message(&format!("Error: {}", e)),
            }
            self.refresh_panels();
        }
    }

    pub fn execute_tar(&mut self, archive_name: &str) {
        if self.active_panel().is_remote() {
            self.show_message("Archive creation is not supported on remote panels");
            return;
        }
        // Fast validations only (no I/O or external processes)
        if let Err(e) = file_ops::is_valid_filename(archive_name) {
            self.show_message(&format!("Error: {}", e));
            return;
        }

        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files to archive");
            return;
        }

        // Validate each filename to prevent argument injection
        for file in &files {
            if let Err(e) = file_ops::is_valid_filename(file) {
                self.show_message(&format!("Invalid filename '{}': {}", file, e));
                return;
            }
        }

        let current_dir = self.active_panel().path.clone();
        let archive_path = current_dir.join(archive_name);

        // Check if archive already exists (fast check)
        if archive_path.exists() {
            self.show_message(&format!("Error: {} already exists", archive_name));
            return;
        }

        // Check for unsafe symlinks BEFORE starting background work
        let (_, excluded_paths) = file_ops::filter_symlinks_for_tar(&current_dir, &files);

        // If there are files to exclude, show confirmation dialog
        if !excluded_paths.is_empty() {
            self.tar_exclude_state = Some(TarExcludeState {
                archive_name: archive_name.to_string(),
                files: files.clone(),
                excluded_paths,
                scroll_offset: 0,
            });
            self.dialog = Some(Dialog {
                dialog_type: DialogType::TarExcludeConfirm,
                input: String::new(),
                cursor_pos: 0,
                message: String::new(),
                completion: None,
                selected_button: 0,
                selection: None,
                use_md5: false,
            });
            return;
        }

        // No exclusions needed - proceed directly
        self.execute_tar_with_excludes(archive_name, &files, &[]);
    }

    /// Execute tar with specified exclusions (called after confirmation or when no exclusions needed)
    pub fn execute_tar_with_excludes(
        &mut self,
        archive_name: &str,
        files: &[String],
        excluded_paths: &[String],
    ) {
        use std::io::BufReader;
        use std::process::{Command, Stdio};

        let current_dir = self.active_panel().path.clone();

        // Determine compression option based on extension
        let tar_options = if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            "cvfpz"
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            "cvfpj"
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            "cvfpJ"
        } else {
            "cvfp"
        };

        let tar_options_owned = tar_options.to_string();
        let archive_name_owned = archive_name.to_string();
        let archive_path_clone = current_dir.join(archive_name);
        let files_owned = files.to_vec();
        let excluded_owned = excluded_paths.to_vec();

        // Create progress state with preparing flag - show dialog immediately
        let mut progress = FileOperationProgress::new(FileOperationType::Tar);
        progress.is_active = true;
        progress.is_preparing = true;
        progress.preparing_message = "Preparing...".to_string();
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Clear selection before starting
        self.active_panel_mut().selected_files.clear();

        // Store progress state and show dialog IMMEDIATELY
        self.file_operation_progress = Some(progress);
        self.pending_tar_archive = Some(archive_name.to_string());
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

        // Clone tar_path from settings for use in background thread
        let tar_path = self.settings.tar_path.clone();

        // Start all preparation and execution in background thread
        thread::spawn(move || {
            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Build tar_args with --exclude options for unsafe symlinks
            // Note: archive name must come right after options (e.g., cvfpz archive.tar.gz)
            let mut tar_args = vec![tar_options_owned.clone(), archive_name_owned.clone()];
            for excluded in &excluded_owned {
                tar_args.push(format!("--exclude=./{}", excluded));
            }
            tar_args.extend(files_owned.iter().map(|f| format!("./{}", f)));

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Determine tar command (in background)
            let _ = tx.send(ProgressMessage::Preparing(
                "Checking tar command...".to_string(),
            ));
            let tar_cmd = if let Some(ref custom_tar) = tar_path {
                // Use custom tar path from settings
                match Command::new(custom_tar).arg("--version").output() {
                    Ok(output) if output.status.success() => Some(custom_tar.clone()),
                    _ => None,
                }
            } else {
                // Default: try gtar first, then tar
                match Command::new("gtar").arg("--version").output() {
                    Ok(output) if output.status.success() => Some("gtar".to_string()),
                    _ => match Command::new("tar").arg("--version").output() {
                        Ok(output) if output.status.success() => Some("tar".to_string()),
                        _ => None,
                    },
                }
            };

            let tar_cmd = match tar_cmd {
                Some(cmd) => cmd,
                None => {
                    let _ = tx.send(ProgressMessage::Error(
                        archive_name_owned,
                        "tar command not found".to_string(),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            // Check if stdbuf is available (in background)
            let has_stdbuf = Command::new("stdbuf")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Calculate file sizes
            let _ = tx.send(ProgressMessage::Preparing(
                "Calculating file sizes...".to_string(),
            ));

            // Check for cancellation during preparation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Calculate total size and file size map (in background)
            let (total_bytes, size_map) = Self::calculate_tar_sizes(&current_dir, &files_owned);
            let total_file_count = size_map.len();

            // Check for cancellation after preparation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    archive_name_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Preparation complete, send initial totals
            let _ = tx.send(ProgressMessage::PrepareComplete);
            let _ = tx.send(ProgressMessage::TotalProgress(
                0,
                total_file_count,
                0,
                total_bytes,
            ));

            // Helper function to cleanup partial archive
            let cleanup_archive = |path: &PathBuf| {
                let _ = std::fs::remove_file(path);
            };

            // Use stdbuf to disable buffering if available
            let child = if has_stdbuf {
                let mut args = vec!["-o0".to_string(), "-e0".to_string(), tar_cmd.clone()];
                args.extend(tar_args);
                Command::new("stdbuf")
                    .current_dir(&current_dir)
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            } else {
                Command::new(&tar_cmd)
                    .current_dir(&current_dir)
                    .args(&tar_args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            };

            match child {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let mut completed_files = 0usize;
                    let mut completed_bytes = 0u64;
                    let mut last_error_line: Option<String> = None;

                    // Collect stderr in background for error messages
                    let stderr_handle = stderr.map(|stderr| {
                        thread::spawn(move || {
                            use std::io::Read;
                            let mut err_str = String::new();
                            let mut stderr = stderr;
                            let _ = stderr.read_to_string(&mut err_str);
                            err_str
                        })
                    });

                    // Read stdout line by line for progress updates
                    // (tar outputs verbose listing to stdout on most systems)
                    if let Some(stdout) = stdout {
                        use std::io::BufRead;
                        let mut reader = BufReader::with_capacity(64, stdout);
                        let mut line = String::new();

                        loop {
                            // Check for cancellation
                            if cancel_flag.load(Ordering::Relaxed) {
                                let _ = child.kill();
                                // Cleanup partial archive on cancellation
                                cleanup_archive(&archive_path_clone);
                                let _ = tx.send(ProgressMessage::Error(
                                    archive_name_owned.clone(),
                                    "Cancelled".to_string(),
                                ));
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 1));
                                return;
                            }

                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) => break, // EOF
                                Ok(_) => {
                                    let filename = line.trim_end();
                                    // Check if this looks like an error line (starts with "tar:")
                                    if filename.starts_with("tar:") || filename.starts_with("gtar:")
                                    {
                                        last_error_line = Some(filename.to_string());
                                    } else if !filename.is_empty() {
                                        completed_files += 1;
                                        // Look up file size from the map
                                        if let Some(&file_size) = size_map.get(filename) {
                                            completed_bytes += file_size;
                                        }
                                        let _ = tx.send(ProgressMessage::FileStarted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::FileCompleted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::TotalProgress(
                                            completed_files,
                                            total_file_count,
                                            completed_bytes,
                                            total_bytes,
                                        ));
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }

                    // Wait for completion
                    match child.wait() {
                        Ok(status) => {
                            if status.success() {
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 0));
                            } else {
                                // Cleanup partial archive on failure
                                cleanup_archive(&archive_path_clone);
                                // Get error from stderr or last_error_line
                                let error_msg = last_error_line
                                    .or_else(|| {
                                        stderr_handle
                                            .and_then(|h| h.join().ok())
                                            .filter(|s| !s.trim().is_empty())
                                            .map(|s| {
                                                s.lines()
                                                    .next()
                                                    .unwrap_or("tar command failed")
                                                    .to_string()
                                            })
                                    })
                                    .unwrap_or_else(|| "tar command failed".to_string());
                                let _ =
                                    tx.send(ProgressMessage::Error(archive_name_owned, error_msg));
                                let _ = tx.send(ProgressMessage::Completed(0, 1));
                            }
                        }
                        Err(e) => {
                            // Cleanup partial archive on error
                            cleanup_archive(&archive_path_clone);
                            let _ =
                                tx.send(ProgressMessage::Error(archive_name_owned, e.to_string()));
                            let _ = tx.send(ProgressMessage::Completed(0, 1));
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(ProgressMessage::Error(
                        archive_name_owned,
                        format!("Failed to run tar: {}", e),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                }
            }
        });
    }

    /// List archive contents to get total file count and sizes
    fn list_archive_contents(
        tar_cmd: &str,
        archive_path: &std::path::Path,
        archive_name: &str,
    ) -> (usize, u64, std::collections::HashMap<String, u64>) {
        use std::collections::HashMap;
        use std::process::Command;

        // Determine list option based on extension
        let list_options = if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            "tvfz"
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            "tvfj"
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            "tvfJ"
        } else {
            "tvf"
        };

        let output = Command::new(tar_cmd)
            .args(&[list_options, &archive_path.to_string_lossy()])
            .output();

        let mut total_files = 0usize;
        let mut total_bytes = 0u64;
        let mut size_map = HashMap::new();

        if let Ok(output) = output {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    // tar -tvf output format: -rw-r--r-- user/group    1234 2024-01-01 12:00 filename
                    // Parse the line to extract size and filename
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 6 {
                        // Size is typically the 3rd field (index 2)
                        if let Ok(size) = parts[2].parse::<u64>() {
                            // Filename is everything after the date/time (index 5+)
                            let filename = parts[5..].join(" ");
                            size_map.insert(filename, size);
                            total_bytes += size;
                        }
                        total_files += 1;
                    }
                }
            }
        }

        (total_files, total_bytes, size_map)
    }

    /// Execute archive extraction with progress display
    pub fn execute_untar(&mut self, archive_path: &std::path::Path) {
        if self.active_panel().is_remote() {
            self.show_message("Archive extraction is not supported on remote panels");
            return;
        }
        use std::io::BufReader;
        use std::process::{Command, Stdio};

        let archive_name = match archive_path.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => {
                self.show_message("Invalid archive path");
                return;
            }
        };

        // Fast validations only
        if !archive_path.exists() {
            self.show_message(&format!("Archive not found: {}", archive_name));
            return;
        }

        let current_dir = match archive_path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => {
                self.show_message("Invalid archive path");
                return;
            }
        };

        // Determine extraction directory name (remove archive extensions)
        let extract_dir_name = archive_name
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".tgz")
            .trim_end_matches(".tar.bz2")
            .trim_end_matches(".tbz2")
            .trim_end_matches(".tar.xz")
            .trim_end_matches(".txz")
            .trim_end_matches(".tar")
            .to_string();

        let extract_path = current_dir.join(&extract_dir_name);

        // Check if extraction directory already exists (fast check)
        if extract_path.exists() {
            self.show_message(&format!("Error: {} already exists", extract_dir_name));
            return;
        }

        // Determine decompression option based on extension
        let tar_options = if archive_name.ends_with(".tar.gz") || archive_name.ends_with(".tgz") {
            "xvfpz"
        } else if archive_name.ends_with(".tar.bz2") || archive_name.ends_with(".tbz2") {
            "xvfpj"
        } else if archive_name.ends_with(".tar.xz") || archive_name.ends_with(".txz") {
            "xvfpJ"
        } else {
            "xvfp"
        };

        let archive_path_owned = archive_path.to_path_buf();
        let archive_name_owned = archive_name.clone();
        let extract_dir_owned = extract_dir_name.clone();
        let extract_path_clone = extract_path.clone();

        // Create progress state with preparing flag - show dialog immediately
        let mut progress = FileOperationProgress::new(FileOperationType::Untar);
        progress.is_active = true;
        progress.is_preparing = true;
        progress.preparing_message = "Preparing...".to_string();
        let cancel_flag = progress.cancel_flag.clone();

        // Create channel for progress messages
        let (tx, rx) = mpsc::channel();
        progress.receiver = Some(rx);

        // Store progress state and show dialog IMMEDIATELY
        self.file_operation_progress = Some(progress);
        self.pending_extract_dir = Some(extract_dir_name);
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

        // Clone tar_path from settings for use in background thread
        let tar_path = self.settings.tar_path.clone();

        // Start all preparation and execution in background thread
        thread::spawn(move || {
            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Determine tar command (in background)
            let _ = tx.send(ProgressMessage::Preparing(
                "Checking tar command...".to_string(),
            ));
            let tar_cmd = if let Some(ref custom_tar) = tar_path {
                // Use custom tar path from settings
                match Command::new(custom_tar).arg("--version").output() {
                    Ok(output) if output.status.success() => Some(custom_tar.clone()),
                    _ => None,
                }
            } else {
                // Default: try gtar first, then tar
                match Command::new("gtar").arg("--version").output() {
                    Ok(output) if output.status.success() => Some("gtar".to_string()),
                    _ => match Command::new("tar").arg("--version").output() {
                        Ok(output) if output.status.success() => Some("tar".to_string()),
                        _ => None,
                    },
                }
            };

            let tar_cmd = match tar_cmd {
                Some(cmd) => cmd,
                None => {
                    let _ = tx.send(ProgressMessage::Error(
                        extract_dir_owned,
                        "tar command not found".to_string(),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                    return;
                }
            };

            // Check if stdbuf is available (in background)
            let has_stdbuf = Command::new("stdbuf")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            // Check for cancellation
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // List archive contents
            let _ = tx.send(ProgressMessage::Preparing(
                "Reading archive contents...".to_string(),
            ));
            let (total_file_count, total_bytes, size_map) =
                Self::list_archive_contents(&tar_cmd, &archive_path_owned, &archive_name_owned);

            // Check for cancellation after listing
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Cancelled".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            if total_file_count == 0 {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    "Archive appears to be empty or corrupted".to_string(),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Create extraction directory
            if let Err(e) = std::fs::create_dir(&extract_path_clone) {
                let _ = tx.send(ProgressMessage::Error(
                    extract_dir_owned,
                    format!("Failed to create directory: {}", e),
                ));
                let _ = tx.send(ProgressMessage::Completed(0, 1));
                return;
            }

            // Preparation complete, send initial totals
            let _ = tx.send(ProgressMessage::PrepareComplete);
            let _ = tx.send(ProgressMessage::TotalProgress(
                0,
                total_file_count,
                0,
                total_bytes,
            ));

            // Build command arguments
            let archive_path_str = archive_path_owned.to_string_lossy().to_string();
            let tar_args = vec![tar_options.to_string(), archive_path_str];

            // Execute tar extraction
            let child = if has_stdbuf {
                let mut args = vec!["-oL".to_string(), "-eL".to_string(), tar_cmd.clone()];
                args.extend(tar_args);
                Command::new("stdbuf")
                    .current_dir(&extract_path_clone)
                    .args(&args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            } else {
                Command::new(&tar_cmd)
                    .current_dir(&extract_path_clone)
                    .args(&tar_args)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            };

            // Cleanup helper for failed extraction
            let cleanup_extract_dir = |path: &std::path::PathBuf| {
                let _ = std::fs::remove_dir_all(path);
            };

            match child {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let mut completed_files = 0usize;
                    let mut completed_bytes = 0u64;
                    let mut last_error_line: Option<String> = None;

                    // Collect stderr in background for error messages
                    let stderr_handle = stderr.map(|stderr| {
                        thread::spawn(move || {
                            use std::io::Read;
                            let mut err_str = String::new();
                            let mut stderr = stderr;
                            let _ = stderr.read_to_string(&mut err_str);
                            err_str
                        })
                    });

                    // Read stdout line by line for progress updates
                    if let Some(stdout) = stdout {
                        use std::io::BufRead;
                        let mut reader = BufReader::with_capacity(256, stdout);
                        let mut line = String::new();

                        loop {
                            // Check for cancellation
                            if cancel_flag.load(Ordering::Relaxed) {
                                let _ = child.kill();
                                cleanup_extract_dir(&extract_path_clone);
                                let _ = tx.send(ProgressMessage::Error(
                                    extract_dir_owned.clone(),
                                    "Cancelled".to_string(),
                                ));
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 1));
                                return;
                            }

                            line.clear();
                            match reader.read_line(&mut line) {
                                Ok(0) => break, // EOF
                                Ok(_) => {
                                    let filename = line.trim_end();
                                    if filename.starts_with("tar:") || filename.starts_with("gtar:")
                                    {
                                        last_error_line = Some(filename.to_string());
                                    } else if !filename.is_empty() {
                                        completed_files += 1;
                                        // Look up file size from the map
                                        if let Some(&file_size) = size_map.get(filename) {
                                            completed_bytes += file_size;
                                        }
                                        let _ = tx.send(ProgressMessage::FileStarted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::FileCompleted(
                                            filename.to_string(),
                                        ));
                                        let _ = tx.send(ProgressMessage::TotalProgress(
                                            completed_files,
                                            total_file_count,
                                            completed_bytes,
                                            total_bytes,
                                        ));
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }

                    // Wait for completion
                    match child.wait() {
                        Ok(status) => {
                            if status.success() {
                                let _ = tx.send(ProgressMessage::Completed(completed_files, 0));
                            } else {
                                cleanup_extract_dir(&extract_path_clone);
                                let error_msg = last_error_line
                                    .or_else(|| {
                                        stderr_handle
                                            .and_then(|h| h.join().ok())
                                            .filter(|s| !s.trim().is_empty())
                                            .map(|s| {
                                                s.lines()
                                                    .next()
                                                    .unwrap_or("tar extraction failed")
                                                    .to_string()
                                            })
                                    })
                                    .unwrap_or_else(|| "tar extraction failed".to_string());
                                let _ =
                                    tx.send(ProgressMessage::Error(extract_dir_owned, error_msg));
                                let _ = tx.send(ProgressMessage::Completed(0, 1));
                            }
                        }
                        Err(e) => {
                            cleanup_extract_dir(&extract_path_clone);
                            let _ =
                                tx.send(ProgressMessage::Error(extract_dir_owned, e.to_string()));
                            let _ = tx.send(ProgressMessage::Completed(0, 1));
                        }
                    }
                }
                Err(e) => {
                    cleanup_extract_dir(&extract_path_clone);
                    let _ = tx.send(ProgressMessage::Error(
                        extract_dir_owned,
                        format!("Failed to run tar: {}", e),
                    ));
                    let _ = tx.send(ProgressMessage::Completed(0, 1));
                }
            }
        });
    }

    pub fn execute_search(&mut self, term: &str) {
        if self.active_panel().is_remote() {
            self.show_message("Search is not supported on remote panels");
            return;
        }
        if term.trim().is_empty() {
            self.show_message("Please enter a search term");
            return;
        }
        if self.remote_spinner.is_some() {
            return;
        }

        let base_path = self.active_panel().path.clone();
        let search_term = term.to_string();
        let base_path_clone = base_path.clone();
        let term_clone = search_term.clone();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let results = crate::ui::search_result::execute_recursive_search(
                &base_path_clone,
                &term_clone,
                1000,
            );
            let _ = tx.send(RemoteSpinnerResult::SearchComplete {
                results,
                search_term: term_clone,
                base_path: base_path_clone,
            });
        });

        self.remote_spinner = Some(RemoteSpinner {
            message: "Searching...".to_string(),
            started_at: Instant::now(),
            receiver: rx,
        });
    }

    pub fn execute_goto(&mut self, path_str: &str) {
        // Check if this is a remote path (user@host:/path)
        if let Some((user, host, port, remote_path)) = remote::parse_remote_path(path_str) {
            self.execute_goto_remote(&user, &host, port, &remote_path);
            return;
        }

        // If the current panel is remote:
        // - ~ should disconnect and go local home
        // - ~/subdir should disconnect and go local ~/subdir
        // - /absolute/path: if exists locally → disconnect and go local, otherwise remote navigation
        // - Relative paths are remote navigation
        if self.active_panel().is_remote() {
            if self.remote_spinner.is_some() {
                // Don't disconnect while a background operation is using remote_ctx
                return;
            }
            if path_str == "~" {
                // Just go to local home - disconnect handles navigation
                self.disconnect_remote_panel();
                return;
            } else if path_str.starts_with("~/") {
                // Disconnect and fall through to local goto for ~/subdir
                self.disconnect_remote_panel();
            } else if path_str.starts_with('/') {
                // Absolute path: check if it exists on the local filesystem
                let local_path = PathBuf::from(path_str);
                if local_path.exists() {
                    // Path exists locally → disconnect from remote and navigate locally
                    self.disconnect_remote_panel();
                    // fall through to local goto
                } else {
                    // Not a local path → navigate within remote
                    self.execute_goto_remote_relative(path_str);
                    return;
                }
            } else {
                // Relative path → navigate within remote
                self.execute_goto_remote_relative(path_str);
                return;
            }
        }

        // Security: Check for path traversal attempts
        if path_str.contains("..") {
            // Normalize the path to resolve .. components
            let normalized = if path_str.starts_with('~') {
                dirs::home_dir()
                    .map(|h| h.join(path_str[1..].trim_start_matches('/')))
                    .unwrap_or_else(|| PathBuf::from(path_str))
            } else if PathBuf::from(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                self.active_panel().path.join(path_str)
            };

            // Canonicalize to resolve all .. components
            match normalized.canonicalize() {
                Ok(canonical) => {
                    let fallback = self.active_panel().path.clone();
                    let valid_path = get_valid_path(&canonical, &fallback);
                    if valid_path != fallback {
                        let panel = self.active_panel_mut();
                        panel.path = valid_path.clone();
                        panel.selected_index = 0;
                        panel.selected_files.clear();
                        panel.load_files();
                        self.show_message(&format!("Moved to: {}", valid_path.display()));
                    } else {
                        self.show_message("Error: Path not found or not accessible");
                    }
                    return;
                }
                Err(_) => {
                    self.show_message("Error: Invalid path");
                    return;
                }
            }
        }

        let path = if path_str.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(path_str[1..].trim_start_matches('/')))
                .unwrap_or_else(|| PathBuf::from(path_str))
        } else {
            let p = PathBuf::from(path_str);
            if p.is_absolute() {
                p
            } else {
                self.active_panel().path.join(path_str)
            }
        };

        // Validate path and find nearest valid parent if necessary
        let fallback = self.active_panel().path.clone();
        let valid_path = get_valid_path(&path, &fallback);

        if valid_path == path && valid_path == fallback {
            // 이미 해당 경로에 있음
            self.show_message(&format!("Already at: {}", valid_path.display()));
        } else if valid_path != fallback {
            let panel = self.active_panel_mut();
            panel.path = valid_path.clone();
            panel.selected_index = 0;
            panel.selected_files.clear();
            panel.load_files();

            if valid_path == path {
                self.show_message(&format!("Moved to: {}", valid_path.display()));
            } else {
                self.show_message(&format!("Moved to nearest valid: {}", valid_path.display()));
            }
        } else {
            self.show_message("Error: Path not found or not accessible");
        }
    }
}
