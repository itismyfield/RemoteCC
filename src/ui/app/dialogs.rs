use super::*;

use std::fs;

use crate::ui::file_editor::EditorState;
use crate::ui::file_info::FileInfoState;
use crate::ui::file_viewer::ViewerState;

impl App {
    /// Show settings dialog
    pub fn show_settings_dialog(&mut self) {
        self.settings_state = Some(SettingsState::new(&self.settings));
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Settings,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    /// Apply settings from dialog and save
    pub fn apply_settings_from_dialog(&mut self) {
        if let Some(ref state) = self.settings_state {
            let new_theme_name = state.current_theme().to_string();

            // Update theme if changed
            if new_theme_name != self.settings.theme.name {
                self.settings.theme.name = new_theme_name.clone();
                self.theme = crate::ui::theme::Theme::load(&new_theme_name);
                self.theme_watch_state.update_theme(&new_theme_name);
            }

            // Update diff compare method
            let new_diff_method = state.current_diff_method().to_string();
            self.settings.diff_compare_method = new_diff_method;

            // Save settings
            let _ = self.settings.save();
            self.show_message("Settings saved!");
        }

        self.settings_state = None;
        self.dialog = None;
    }

    /// Cancel settings dialog and restore original theme
    pub fn cancel_settings_dialog(&mut self) {
        // Restore original theme if it was changed during preview
        self.theme = crate::ui::theme::Theme::load(&self.settings.theme.name);
        self.settings_state = None;
        self.dialog = None;
    }

    /// Show extension handler error dialog
    pub fn show_extension_handler_error(&mut self, error_message: &str) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::ExtensionHandlerError,
            input: String::new(),
            cursor_pos: 0,
            message: error_message.to_string(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    /// Show handler setup dialog for current file (u key)
    pub fn show_handler_dialog(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Extension handlers are not available for remote files");
            return;
        }
        let panel = self.active_panel();
        if panel.files.is_empty() {
            return;
        }

        let file = &panel.files[panel.selected_index];
        if file.is_directory {
            return; // No handler for directories
        }

        let path = panel.path.join(&file.name);
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();

        if extension.is_empty() {
            self.message = Some("No extension - cannot set handler".to_string());
            self.message_timer = 30;
            return;
        }

        // Check if handler already exists
        let existing_handler = self
            .settings
            .get_extension_handler(&extension)
            .and_then(|handlers| handlers.first().cloned())
            .unwrap_or_default();

        let is_edit_mode = !existing_handler.is_empty();
        let cursor_pos = existing_handler.chars().count();

        // Edit 모드일 때 전체 선택
        let selection = if is_edit_mode {
            Some((0, cursor_pos))
        } else {
            None
        };

        self.pending_binary_file = Some((path, extension.clone()));
        self.dialog = Some(Dialog {
            dialog_type: DialogType::BinaryFileHandler,
            input: existing_handler,
            cursor_pos,
            message: extension,
            completion: None,
            selected_button: if is_edit_mode { 1 } else { 0 }, // 0: Set, 1: Edit
            selection,
            use_md5: false,
        });
    }

    // Dialog methods
    pub fn show_help(&mut self) {
        self.current_screen = Screen::Help;
    }

    pub fn show_file_info(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("File info is not available for remote files");
            return;
        }
        // Clone necessary data first to avoid borrow issues
        let (file_path, is_directory, is_dotdot) = {
            let panel = self.active_panel();
            if let Some(file) = panel.current_file() {
                (
                    panel.path.join(&file.name),
                    file.is_directory,
                    file.name == "..",
                )
            } else {
                return;
            }
        };

        if is_dotdot {
            self.show_message("Select a file for info");
            return;
        }

        self.info_file_path = file_path.clone();

        // For directories, start async size calculation
        if is_directory {
            let mut state = FileInfoState::new();
            state.start_calculation(&file_path);
            self.file_info_state = Some(state);
        } else {
            self.file_info_state = None;
        }

        self.current_screen = Screen::FileInfo;
    }

    pub fn view_file(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Cannot view remote files directly. Use copy to download first.");
            return;
        }
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if !file.is_directory {
                let path = panel.path.join(&file.name);

                // Check if it's an image file
                if crate::ui::image_viewer::is_image_file(&path) {
                    // Skip true color check if inline image protocol is available
                    let has_inline = self
                        .image_picker
                        .as_ref()
                        .map(|p| p.protocol_type != ratatui_image::picker::ProtocolType::Halfblocks)
                        .unwrap_or(false);
                    if !has_inline && !crate::ui::image_viewer::supports_true_color() {
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::TrueColorWarning,
                            input: String::new(),
                            cursor_pos: 0,
                            message: "Terminal doesn't support true color. Open anyway?"
                                .to_string(),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                        return;
                    }

                    // Check file size (threshold: 50MB)
                    const LARGE_IMAGE_THRESHOLD: u64 = 50 * 1024 * 1024;
                    let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

                    if file_size > LARGE_IMAGE_THRESHOLD {
                        // Show confirmation dialog for large image
                        let size_mb = file_size as f64 / (1024.0 * 1024.0);
                        self.pending_large_image = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::LargeImageConfirm,
                            input: String::new(),
                            cursor_pos: 0,
                            message: format!("This image is {:.1}MB. Open anyway?", size_mb),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                        return;
                    }

                    self.image_viewer_state =
                        Some(crate::ui::image_viewer::ImageViewerState::new(&path));
                    self.current_screen = Screen::ImageViewer;
                    return;
                }

                // 새로운 고급 뷰어 사용
                let mut viewer = ViewerState::new();
                viewer.set_syntax_colors(self.theme.syntax);
                match viewer.load_file(&path) {
                    Ok(_) => {
                        self.viewer_state = Some(viewer);
                        self.current_screen = Screen::FileViewer;
                    }
                    Err(e) => {
                        self.show_message(&format!("Cannot read file: {}", e));
                    }
                }
            } else {
                self.show_message("Select a file to view");
            }
        }
    }

    pub fn edit_file(&mut self) {
        if self.active_panel().is_remote() {
            let panel = self.active_panel();
            let file = match panel.current_file() {
                Some(f) if !f.is_directory => f.clone(),
                Some(_) => {
                    self.show_message("Select a file to edit");
                    return;
                }
                None => return,
            };
            let remote_path = format!("{}/{}", panel.path.display(), file.name);
            let panel_index = self.active_panel_index;
            let tmp_path = self.remote_tmp_path(&file.name);
            let tmp_path = match tmp_path {
                Some(p) => p,
                None => return,
            };
            self.download_for_remote_open(
                &file.name,
                file.size,
                PendingRemoteOpen::Editor {
                    tmp_path,
                    panel_index,
                    remote_path,
                },
            );
        } else {
            // 로컬 파일: 기존 로직
            let panel = self.active_panel();
            if let Some(file) = panel.current_file() {
                if !file.is_directory {
                    let path = panel.path.join(&file.name);

                    let mut editor = EditorState::new();
                    editor.set_syntax_colors(self.theme.syntax);
                    match editor.load_file(&path) {
                        Ok(_) => {
                            self.editor_state = Some(editor);
                            self.current_screen = Screen::FileEditor;
                        }
                        Err(e) => {
                            self.show_message(&format!("Cannot open file: {}", e));
                        }
                    }
                } else {
                    self.show_message("Select a file to edit");
                }
            }
        }
    }

    pub fn show_delete_dialog(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }
        let file_list = if files.len() <= 3 {
            files.join(", ")
        } else {
            format!("{} and {} more", files[..2].join(", "), files.len() - 2)
        };
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Delete,
            input: String::new(),
            cursor_pos: 0,
            message: format!("Delete {}?", file_list),
            completion: None,
            selected_button: 1, // 기본값: No (안전을 위해)
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_encrypt_dialog(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Encryption is not available on remote panels");
            return;
        }

        let dir = self.active_panel().path.clone();
        let count = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    if !path.is_file() {
                        return false;
                    }
                    let name = e.file_name().to_string_lossy().to_string();
                    !name.ends_with(".cokacenc") && !name.starts_with('.')
                })
                .count(),
            Err(_) => 0,
        };

        if count == 0 {
            self.show_message("No files to encrypt");
            return;
        }

        let split_size = self.settings.encrypt_split_size.to_string();
        let cursor = split_size.len();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::EncryptConfirm,
            input: split_size,
            cursor_pos: cursor,
            message: format!("Encrypt {} file(s)? Split size MB (0=no split):", count),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_decrypt_dialog(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("Decryption is not available on remote panels");
            return;
        }

        let dir = self.active_panel().path.clone();
        let count = match fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let path = e.path();
                    path.is_file() && e.file_name().to_string_lossy().ends_with(".cokacenc")
                })
                .count(),
            Err(_) => 0,
        };

        if count == 0 {
            self.show_message("No .cokacenc files to decrypt");
            return;
        }

        self.dialog = Some(Dialog {
            dialog_type: DialogType::DecryptConfirm,
            input: String::new(),
            cursor_pos: 0,
            message: format!("Decrypt {} .cokacenc file(s) in {}?", count, dir.display()),
            completion: None,
            selected_button: 1, // Default: No
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_mkdir_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Mkdir,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_mkfile_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Mkfile,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_rename_dialog(&mut self) {
        let panel = self.active_panel();
        if let Some(file) = panel.current_file() {
            if file.name != ".." {
                let name = &file.name;
                let len = name.chars().count();

                // 확장자 제외한 선택 범위 계산
                // 디렉토리: 전체 선택
                // 파일: 마지막 '.' 앞까지 선택 (숨김파일 고려)
                let selection_end = if file.is_directory {
                    len
                } else {
                    // 숨김 파일(.으로 시작)의 경우 첫 번째 점 이후의 확장자만 찾음
                    let search_start = if name.starts_with('.') { 1 } else { 0 };
                    if let Some(dot_pos) = name[search_start..].rfind('.') {
                        // 확장자가 있으면 그 앞까지
                        name[..search_start].chars().count()
                            + name[search_start..search_start + dot_pos].chars().count()
                    } else {
                        // 확장자 없으면 전체
                        len
                    }
                };

                self.dialog = Some(Dialog {
                    dialog_type: DialogType::Rename,
                    input: file.name.clone(),
                    cursor_pos: selection_end,
                    message: String::new(),
                    completion: None,
                    selected_button: 0,
                    selection: Some((0, selection_end)),
                    use_md5: false,
                });
            } else {
                self.show_message("Select a file to rename");
            }
        }
    }

    pub fn show_tar_dialog(&mut self) {
        let files = self.get_operation_files();
        if files.is_empty() {
            self.show_message("No files selected");
            return;
        }

        // Generate default archive name based on first file
        let first_file = &files[0];
        let archive_name = format!("{}.tar.gz", first_file);

        let file_list = if files.len() <= 3 {
            files.join(", ")
        } else {
            format!("{} and {} more", files[..2].join(", "), files.len() - 2)
        };

        let cursor_pos = archive_name.chars().count();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Tar,
            input: archive_name,
            cursor_pos,
            message: file_list,
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_search_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Search,
            input: String::new(),
            cursor_pos: 0,
            message: "Search for:".to_string(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_goto_dialog(&mut self) {
        let current_path = self.active_panel().display_path();
        let len = current_path.chars().count();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::Goto,
            input: current_path,
            cursor_pos: len,
            message: "Go to path:".to_string(),
            completion: Some(PathCompletion::default()),
            selected_button: 0,
            selection: Some((0, len)), // 전체 선택
            use_md5: false,
        });
    }

    pub fn show_process_manager(&mut self) {
        self.processes = crate::services::process::get_process_list();
        self.process_selected_index = 0;
        self.process_confirm_kill = None;
        self.current_screen = Screen::ProcessManager;
    }

    pub fn show_ai_screen(&mut self) {
        if self.active_panel().is_remote() {
            self.show_message("AI features are not available for remote panels");
            return;
        }
        // 1패널이면 AI용 패널 자동 추가
        if self.panels.len() == 1 {
            let path = self.active_panel().path.clone();
            self.panels.push(PanelState::new(path));
        }
        let current_path = self.active_panel().path.display().to_string();
        // Try to load the most recent session, fall back to new session
        // Note: claude availability is checked inside AIScreenState (displays error in UI if unavailable)
        self.ai_state = Some(
            crate::ui::ai_screen::AIScreenState::load_latest_session(current_path.clone())
                .unwrap_or_else(|| crate::ui::ai_screen::AIScreenState::new(current_path)),
        );
        // 원래 포커스 위치 저장
        self.ai_previous_panel = Some(self.active_panel_index);
        // AI 화면을 비활성 패널(다음 패널)에 표시
        let ai_idx = (self.active_panel_index + 1) % self.panels.len();
        self.ai_panel_index = Some(ai_idx);
        // 포커스를 AI 화면으로 이동
        self.active_panel_index = ai_idx;
    }

    /// AI 화면을 닫고 상태 초기화
    pub fn close_ai_screen(&mut self) {
        if let Some(ref mut state) = self.ai_state {
            state.save_session_to_file();
        }
        // 원래 포커스 위치로 복원
        if let Some(prev) = self.ai_previous_panel {
            if prev < self.panels.len() {
                self.active_panel_index = prev;
            }
        }
        self.ai_panel_index = None;
        self.ai_previous_panel = None;
        self.ai_state = None;
        self.refresh_panels();
    }

    pub fn show_system_info(&mut self) {
        self.system_info_state = crate::ui::system_info::SystemInfoState::default();
        self.current_screen = Screen::SystemInfo;
    }

    pub fn show_git_screen(&mut self) {
        let path = self.active_panel().path.clone();
        if !crate::ui::git_screen::is_git_repo(&path) {
            self.show_message("Not a git repository");
            return;
        }
        self.git_screen_state = Some(crate::ui::git_screen::GitScreenState::new(path));
        self.current_screen = Screen::GitScreen;
    }

    pub fn show_dedup_screen(&mut self) {
        let path = self.active_panel().path.clone();
        self.dialog = Some(Dialog {
            dialog_type: DialogType::DedupConfirm,
            input: String::new(),
            cursor_pos: 0,
            message: format!("WARNING: This will PERMANENTLY DELETE duplicate files in {}. This action cannot be undone. Proceed?", path.display()),
            completion: None,
            selected_button: 1,  // Default: No
            selection: None,
            use_md5: false,
        });
    }

    pub fn show_git_log_diff_dialog(&mut self) {
        let path = self.active_panel().path.clone();
        if !crate::ui::git_screen::is_git_repo(&path) {
            self.show_message("Not a git repository");
            return;
        }
        let repo_root = match crate::ui::git_screen::get_repo_root(&path) {
            Some(r) => r,
            None => {
                self.show_message("Failed to get git repo root");
                return;
            }
        };
        let project_name = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        let log_entries = crate::ui::git_screen::get_log_public(&repo_root, 200);
        if log_entries.is_empty() {
            self.show_message("No git commits found");
            return;
        }
        self.git_log_diff_state = Some(GitLogDiffState {
            repo_path: repo_root,
            project_name,
            log_entries,
            selected_index: 0,
            scroll_offset: 0,
            selected_commits: Vec::new(),
            visible_height: 20,
        });
        self.dialog = Some(Dialog {
            dialog_type: DialogType::GitLogDiff,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    #[allow(dead_code)]
    pub fn show_advanced_search_dialog(&mut self) {
        self.advanced_search_state.active = true;
        self.advanced_search_state.reset();
    }

    /// Show the duplicate conflict dialog
    pub fn show_duplicate_conflict_dialog(&mut self) {
        self.dialog = Some(Dialog {
            dialog_type: DialogType::DuplicateConflict,
            input: String::new(),
            cursor_pos: 0,
            message: String::new(),
            completion: None,
            selected_button: 0,
            selection: None,
            use_md5: false,
        });
    }

    pub fn execute_open_large_image(&mut self) {
        if let Some(path) = self.pending_large_image.take() {
            self.image_viewer_state = Some(crate::ui::image_viewer::ImageViewerState::new(&path));
            self.current_screen = Screen::ImageViewer;
        }
    }

    pub fn execute_open_large_file(&mut self) {
        if let Some(path) = self.pending_large_file.take() {
            let mut editor = EditorState::new();
            editor.set_syntax_colors(self.theme.syntax);
            match editor.load_file(&path) {
                Ok(_) => {
                    self.editor_state = Some(editor);
                    self.current_screen = Screen::FileEditor;
                }
                Err(e) => {
                    self.show_message(&format!("Cannot open file: {}", e));
                }
            }
        }
    }
}
