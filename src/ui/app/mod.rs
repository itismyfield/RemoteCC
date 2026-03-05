mod state;
mod panel;
mod remote;
mod operations;
mod dialogs;

pub use state::*;
pub use panel::*;

use std::path::{Path, PathBuf};

use crate::config::Settings;
use crate::keybindings::Keybindings;
use crate::ui::file_editor::EditorState;
use crate::ui::file_info::FileInfoState;
use crate::ui::file_viewer::ViewerState;
use crate::ui::theme::DEFAULT_THEME_NAME;

pub struct App {
    pub panels: Vec<PanelState>,
    pub active_panel_index: usize,
    pub current_screen: Screen,
    pub dialog: Option<Dialog>,
    pub message: Option<String>,
    pub message_timer: u8,

    // Flag to request full screen redraw (after terminal mode command)
    pub needs_full_redraw: bool,

    // Settings
    pub settings: Settings,

    // Theme (loaded from settings)
    pub theme: crate::ui::theme::Theme,

    // Theme hot-reload watcher (only active in design mode)
    pub theme_watch_state: ThemeWatchState,

    // Design mode flag (--design): enables theme hot-reload
    pub design_mode: bool,

    // Keybindings (built from settings)
    pub keybindings: Keybindings,

    // File viewer state (새로운 고급 상태)
    pub viewer_state: Option<ViewerState>,

    // File viewer state (레거시 호환용 - 제거 예정)
    #[allow(dead_code)]
    pub viewer_lines: Vec<String>,
    #[allow(dead_code)]
    pub viewer_scroll: usize,
    #[allow(dead_code)]
    pub viewer_search_term: String,
    #[allow(dead_code)]
    pub viewer_search_mode: bool,
    #[allow(dead_code)]
    pub viewer_search_input: String,
    #[allow(dead_code)]
    pub viewer_match_lines: Vec<usize>,
    #[allow(dead_code)]
    pub viewer_current_match: usize,

    // File editor state (새로운 고급 상태)
    pub editor_state: Option<EditorState>,

    // File editor state (레거시 호환용 - 제거 예정)
    #[allow(dead_code)]
    pub editor_lines: Vec<String>,
    #[allow(dead_code)]
    pub editor_cursor_line: usize,
    #[allow(dead_code)]
    pub editor_cursor_col: usize,
    #[allow(dead_code)]
    pub editor_scroll: usize,
    #[allow(dead_code)]
    pub editor_modified: bool,
    #[allow(dead_code)]
    pub editor_file_path: PathBuf,

    // File info state
    pub info_file_path: PathBuf,
    pub file_info_state: Option<FileInfoState>,

    // Process manager state
    pub processes: Vec<crate::services::process::ProcessInfo>,
    pub process_selected_index: usize,
    pub process_sort_field: crate::services::process::SortField,
    pub process_sort_asc: bool,
    pub process_confirm_kill: Option<i32>,
    pub process_force_kill: bool,

    // AI screen state
    pub ai_state: Option<crate::ui::ai_screen::AIScreenState>,
    pub ai_panel_index: Option<usize>,    // AI가 표시될 패널 인덱스
    pub ai_previous_panel: Option<usize>, // AI 화면 띄우기 전 포커스 인덱스

    // System info state
    pub system_info_state: crate::ui::system_info::SystemInfoState,

    // Advanced search state
    pub advanced_search_state: crate::ui::advanced_search::AdvancedSearchState,

    // Image viewer state
    pub image_viewer_state: Option<crate::ui::image_viewer::ImageViewerState>,

    // Image protocol picker (for inline image rendering: Kitty/iTerm2/Sixel)
    pub image_picker: Option<ratatui_image::picker::Picker>,

    // Pending large image path (for confirmation dialog)
    pub pending_large_image: Option<std::path::PathBuf>,

    // Pending large file path (for confirmation dialog)
    pub pending_large_file: Option<std::path::PathBuf>,

    // Pending binary file path and extension (for handler setup dialog)
    pub pending_binary_file: Option<(std::path::PathBuf, String)>,

    // Search result state (재귀 검색 결과)
    pub search_result_state: crate::ui::search_result::SearchResultState,

    // Track previous screen for back navigation
    pub previous_screen: Option<Screen>,

    // Clipboard state for Ctrl+C/X/V operations
    pub clipboard: Option<Clipboard>,

    // File operation progress state
    pub file_operation_progress: Option<FileOperationProgress>,

    // Pending tar archive name (for focusing after completion)
    pub pending_tar_archive: Option<String>,

    // Pending extract directory name (for focusing after completion)
    pub pending_extract_dir: Option<String>,

    // Pending paste focus names (for focusing on first pasted file after completion)
    pub pending_paste_focus: Option<Vec<String>>,

    // Conflict resolution state for duplicate file handling
    pub conflict_state: Option<ConflictState>,

    // Tar exclude confirmation state
    pub tar_exclude_state: Option<TarExcludeState>,

    // Help screen state
    pub help_state: HelpState,

    // Settings dialog state
    pub settings_state: Option<SettingsState>,

    // Remote connection dialog state
    pub remote_connect_state: Option<RemoteConnectState>,

    // Diff screen state
    pub diff_first_panel: Option<usize>,
    pub diff_state: Option<crate::ui::diff_screen::DiffState>,
    pub diff_file_view_state: Option<crate::ui::diff_file_view::DiffFileViewState>,

    // Git screen state
    pub git_screen_state: Option<crate::ui::git_screen::GitScreenState>,

    // Dedup screen state
    pub dedup_screen_state: Option<crate::ui::dedup_screen::DedupScreenState>,

    // Git log diff state
    pub git_log_diff_state: Option<GitLogDiffState>,

    // Pending remote download → open action
    pub pending_remote_open: Option<PendingRemoteOpen>,

    // Remote operation spinner (SSH/SFTP background task)
    pub remote_spinner: Option<RemoteSpinner>,
}

impl App {
    pub fn new(first_path: PathBuf, second_path: PathBuf) -> Self {
        Self {
            panels: vec![PanelState::new(first_path), PanelState::new(second_path)],
            active_panel_index: 0,
            current_screen: Screen::FilePanel,
            dialog: None,
            message: None,
            message_timer: 0,
            needs_full_redraw: false,
            settings: Settings::default(),
            theme: crate::ui::theme::Theme::default(),
            theme_watch_state: ThemeWatchState::watch_theme(DEFAULT_THEME_NAME),
            design_mode: false,
            keybindings: Keybindings::from_config(&crate::keybindings::KeybindingsConfig::default()),

            // 새로운 고급 상태
            viewer_state: None,
            editor_state: None,

            // 레거시 호환용
            viewer_lines: Vec::new(),
            viewer_scroll: 0,
            viewer_search_term: String::new(),
            viewer_search_mode: false,
            viewer_search_input: String::new(),
            viewer_match_lines: Vec::new(),
            viewer_current_match: 0,

            editor_lines: vec![String::new()],
            editor_cursor_line: 0,
            editor_cursor_col: 0,
            editor_scroll: 0,
            editor_modified: false,
            editor_file_path: PathBuf::new(),

            info_file_path: PathBuf::new(),
            file_info_state: None,

            processes: Vec::new(),
            process_selected_index: 0,
            process_sort_field: crate::services::process::SortField::Cpu,
            process_sort_asc: false,
            process_confirm_kill: None,
            process_force_kill: false,

            ai_state: None,
            ai_panel_index: None,
            ai_previous_panel: None,
            system_info_state: crate::ui::system_info::SystemInfoState::default(),
            advanced_search_state: crate::ui::advanced_search::AdvancedSearchState::default(),
            image_viewer_state: None,
            image_picker: None,
            pending_large_image: None,
            pending_large_file: None,
            pending_binary_file: None,
            search_result_state: crate::ui::search_result::SearchResultState::default(),
            previous_screen: None,
            clipboard: None,
            file_operation_progress: None,
            pending_tar_archive: None,
            pending_extract_dir: None,
            pending_paste_focus: None,
            conflict_state: None,
            tar_exclude_state: None,
            help_state: HelpState::default(),
            settings_state: None,
            remote_connect_state: None,
            diff_first_panel: None,
            diff_state: None,
            diff_file_view_state: None,
            git_screen_state: None,
            dedup_screen_state: None,
            git_log_diff_state: None,
            pending_remote_open: None,
            remote_spinner: None,
        }
    }
    pub fn with_settings(settings: Settings) -> Self {
        // Build panels from settings
        let panels: Vec<PanelState> = if settings.panels.is_empty() {
            // No panels configured, create defaults
            let first = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let second = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            vec![PanelState::new(first), PanelState::new(second)]
        } else {
            settings
                .panels
                .iter()
                .map(|ps| {
                    let path = settings.resolve_path(&ps.start_path, || {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                    });
                    PanelState::with_settings(path, ps)
                })
                .collect()
        };
        let active_panel_index = settings
            .active_panel_index
            .min(panels.len().saturating_sub(1));

        // Load theme from settings
        let theme = crate::ui::theme::Theme::load(&settings.theme.name);
        let theme_watch_state = ThemeWatchState::watch_theme(&settings.theme.name);

        // Build keybindings from settings
        let keybindings = Keybindings::from_config(&settings.keybindings);

        Self {
            panels,
            active_panel_index,
            current_screen: Screen::FilePanel,
            dialog: None,
            message: None,
            message_timer: 0,
            needs_full_redraw: false,
            settings,
            theme,
            theme_watch_state,
            design_mode: false,
            keybindings,

            // 새로운 고급 상태
            viewer_state: None,
            editor_state: None,

            // 레거시 호환용
            viewer_lines: Vec::new(),
            viewer_scroll: 0,
            viewer_search_term: String::new(),
            viewer_search_mode: false,
            viewer_search_input: String::new(),
            viewer_match_lines: Vec::new(),
            viewer_current_match: 0,

            editor_lines: vec![String::new()],
            editor_cursor_line: 0,
            editor_cursor_col: 0,
            editor_scroll: 0,
            editor_modified: false,
            editor_file_path: PathBuf::new(),

            info_file_path: PathBuf::new(),
            file_info_state: None,

            processes: Vec::new(),
            process_selected_index: 0,
            process_sort_field: crate::services::process::SortField::Cpu,
            process_sort_asc: false,
            process_confirm_kill: None,
            process_force_kill: false,

            ai_state: None,
            ai_panel_index: None,
            ai_previous_panel: None,
            system_info_state: crate::ui::system_info::SystemInfoState::default(),
            advanced_search_state: crate::ui::advanced_search::AdvancedSearchState::default(),
            image_viewer_state: None,
            image_picker: None,
            pending_large_image: None,
            pending_large_file: None,
            pending_binary_file: None,
            search_result_state: crate::ui::search_result::SearchResultState::default(),
            previous_screen: None,
            clipboard: None,
            file_operation_progress: None,
            pending_tar_archive: None,
            pending_extract_dir: None,
            pending_paste_focus: None,
            conflict_state: None,
            tar_exclude_state: None,
            help_state: HelpState::default(),
            settings_state: None,
            remote_connect_state: None,
            diff_first_panel: None,
            diff_state: None,
            diff_file_view_state: None,
            git_screen_state: None,
            dedup_screen_state: None,
            git_log_diff_state: None,
            pending_remote_open: None,
            remote_spinner: None,
        }
    }
    pub fn save_settings(&mut self) {
        use crate::config::PanelSettings;

        // Preserve extension_handler from current file (user may have edited it externally)
        // Load current file to get the latest extension_handler
        if let Ok(current_file_settings) = Settings::load_with_error() {
            self.settings.extension_handler = current_file_settings.extension_handler;
        }

        // Update settings from current state - save panels array
        let home_path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        self.settings.panels = self
            .panels
            .iter()
            .map(|p| {
                // Remote panel paths should not be saved — use home directory instead
                let path = if p.is_remote() {
                    home_path.display().to_string()
                } else {
                    p.path.display().to_string()
                };
                PanelSettings {
                    start_path: Some(path),
                    sort_by: sort_by_to_string(p.sort_by),
                    sort_order: sort_order_to_string(p.sort_order),
                }
            })
            .collect();
        self.settings.active_panel_index = self.active_panel_index;

        // Save to file (ignore errors silently)
        let _ = self.settings.save();
    }
    pub fn reload_settings(&mut self) -> bool {
        let new_settings = match Settings::load_with_error() {
            Ok(s) => s,
            Err(e) => {
                self.show_message(&format!("Settings error: {}", e));
                return false;
            }
        };

        // Reload theme if name changed
        if new_settings.theme.name != self.settings.theme.name {
            self.theme = crate::ui::theme::Theme::load(&new_settings.theme.name);
            self.theme_watch_state
                .update_theme(&new_settings.theme.name);
        }

        // Apply panel sort settings from new settings (keep current paths and selection)
        for (i, panel) in self.panels.iter_mut().enumerate() {
            if let Some(ps) = new_settings.panels.get(i) {
                let new_sort_by = parse_sort_by(&ps.sort_by);
                let new_sort_order = parse_sort_order(&ps.sort_order);
                if panel.sort_by != new_sort_by || panel.sort_order != new_sort_order {
                    panel.sort_by = new_sort_by;
                    panel.sort_order = new_sort_order;
                    panel.load_files();
                }
            }
        }

        // Update tar_path setting
        self.settings.tar_path = new_settings.tar_path;

        // Update extension_handler setting
        self.settings.extension_handler = new_settings.extension_handler;

        // Update diff compare method
        self.settings.diff_compare_method = new_settings.diff_compare_method;

        // Update keybindings
        self.keybindings = crate::keybindings::Keybindings::from_config(&new_settings.keybindings);
        self.settings.keybindings = new_settings.keybindings;

        // Update settings
        self.settings.theme = new_settings.theme;
        self.settings.panels = new_settings.panels;

        self.show_message("Settings reloaded");
        true
    }
    pub fn is_settings_file(path: &std::path::Path) -> bool {
        if let Some(config_path) = Settings::config_path() {
            path == config_path
        } else {
            false
        }
    }

    /// Reload current theme from file (for hot-reload)
    pub fn reload_theme(&mut self) {
        self.theme = crate::ui::theme::Theme::load(&self.settings.theme.name);
    }

    pub fn active_panel_mut(&mut self) -> &mut PanelState {
        &mut self.panels[self.active_panel_index]
    }

    pub fn active_panel(&self) -> &PanelState {
        &self.panels[self.active_panel_index]
    }

    pub fn target_panel(&self) -> &PanelState {
        let target_idx = (self.active_panel_index + 1) % self.panels.len();
        &self.panels[target_idx]
    }

    pub fn switch_panel(&mut self) {
        // 현재 패널의 선택 해제
        self.panels[self.active_panel_index].selected_files.clear();
        self.active_panel_index = (self.active_panel_index + 1) % self.panels.len();
    }

    /// 왼쪽 패널로 전환 (화면 위치 유지)
    pub fn switch_panel_left(&mut self) {
        if self.active_panel_index == 0 {
            return;
        }
        self.switch_panel_keep_index_to(self.active_panel_index - 1);
    }

    /// 오른쪽 패널로 전환 (화면 위치 유지)
    pub fn switch_panel_right(&mut self) {
        if self.active_panel_index >= self.panels.len() - 1 {
            return;
        }
        self.switch_panel_keep_index_to(self.active_panel_index + 1);
    }

    /// 패널 전환 시 화면에서의 상대적 위치(줄 번호) 유지, 새 패널의 스크롤은 변경하지 않음
    fn switch_panel_keep_index_to(&mut self, target_idx: usize) {
        // 현재 패널의 스크롤 오프셋과 선택 인덱스로 화면 내 상대 위치 계산
        let current_scroll = self.panels[self.active_panel_index].scroll_offset;
        let current_index = self.panels[self.active_panel_index].selected_index;
        let relative_pos = current_index.saturating_sub(current_scroll);

        // 현재 패널의 선택 해제
        self.panels[self.active_panel_index].selected_files.clear();

        // 패널 전환
        self.active_panel_index = target_idx;

        // 새 패널의 기존 스크롤 오프셋 유지, 같은 화면 위치에 커서 설정
        let new_panel = &mut self.panels[self.active_panel_index];
        if !new_panel.files.is_empty() {
            let new_scroll = new_panel.scroll_offset;
            let new_total = new_panel.files.len();

            // 새 패널의 스크롤 오프셋 + 화면 내 상대 위치 = 새 선택 인덱스
            let new_index = new_scroll + relative_pos;
            new_panel.selected_index = new_index.min(new_total.saturating_sub(1));
        }
    }

    /// 새 패널 추가
    /// Replace all panels with ones created from the given paths (CLI args)
    pub fn set_panels_from_paths(&mut self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = paths.into_iter().take(10).collect();
        let panels: Vec<PanelState> = paths.into_iter().map(|p| PanelState::new(p)).collect();
        if !panels.is_empty() {
            self.panels = panels;
            self.active_panel_index = 0;
        }
    }

    pub fn add_panel(&mut self) {
        if self.panels.len() >= 10 {
            return;
        }
        let path = self.active_panel().path.clone();
        let new_panel = PanelState::new(path);
        self.panels.insert(self.active_panel_index + 1, new_panel);
        // AI 인덱스 보정: 삽입 위치보다 뒤에 있으면 +1
        if let Some(ai_idx) = self.ai_panel_index {
            if ai_idx > self.active_panel_index {
                self.ai_panel_index = Some(ai_idx + 1);
            }
        }
        if let Some(prev_idx) = self.ai_previous_panel {
            if prev_idx > self.active_panel_index {
                self.ai_previous_panel = Some(prev_idx + 1);
            }
        }
        self.active_panel_index += 1;
    }

    /// 현재 패널 닫기
    pub fn close_panel(&mut self) {
        if self.panels.len() <= 1 {
            return;
        }
        let removed_idx = self.active_panel_index;
        // AI가 이 패널에 있으면 AI 상태만 직접 정리 (close_ai_screen은 active_panel_index를 변경하므로 사용하지 않음)
        if self.ai_panel_index == Some(removed_idx) {
            if let Some(ref mut state) = self.ai_state {
                state.save_session_to_file();
            }
            self.ai_panel_index = None;
            self.ai_previous_panel = None;
            self.ai_state = None;
        }
        self.panels.remove(removed_idx);
        // AI 인덱스 보정
        if let Some(ai_idx) = self.ai_panel_index {
            if ai_idx > removed_idx {
                self.ai_panel_index = Some(ai_idx - 1);
            }
        }
        if let Some(prev_idx) = self.ai_previous_panel {
            if prev_idx > removed_idx {
                self.ai_previous_panel = Some(prev_idx - 1);
            } else if prev_idx == removed_idx {
                self.ai_previous_panel = None;
            }
        }
        if self.active_panel_index >= self.panels.len() {
            self.active_panel_index = self.panels.len() - 1;
        }
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let panel = self.active_panel_mut();
        let new_index = (panel.selected_index as i32 + delta)
            .max(0)
            .min(panel.files.len().saturating_sub(1) as i32) as usize;
        panel.selected_index = new_index;
    }

    pub fn cursor_to_start(&mut self) {
        self.active_panel_mut().selected_index = 0;
    }

    pub fn cursor_to_end(&mut self) {
        let panel = self.active_panel_mut();
        if !panel.files.is_empty() {
            panel.selected_index = panel.files.len() - 1;
        }
    }

    /// Shift+방향키: 현재 항목 토글 후 커서 이동
    pub fn move_cursor_with_selection(&mut self, delta: i32) {
        let panel = self.active_panel_mut();

        // 이동할 새 인덱스 계산
        let new_index = (panel.selected_index as i32 + delta)
            .max(0)
            .min(panel.files.len().saturating_sub(1) as i32) as usize;

        // 이동하지 않는 경우 (이미 맨 위나 맨 아래)
        if new_index == panel.selected_index {
            return;
        }

        // 현재 항목 토글 (".." 제외)
        if let Some(file) = panel.files.get(panel.selected_index) {
            if file.name != ".." {
                let name = file.name.clone();
                if panel.selected_files.contains(&name) {
                    panel.selected_files.remove(&name);
                } else {
                    panel.selected_files.insert(name);
                }
            }
        }

        // 커서 이동
        panel.selected_index = new_index;
    }

    pub fn enter_selected(&mut self) {
        // Check for remote directory navigation first (to avoid borrow conflicts)
        let remote_nav = {
            let panel = &self.panels[self.active_panel_index];
            if let Some(file) = panel.current_file().cloned() {
                if file.is_directory && panel.is_remote() {
                    let (new_path, focus) = if file.name == ".." {
                        let focus = panel
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string());
                        let parent = panel
                            .path
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "/".to_string());
                        (parent, focus)
                    } else {
                        (panel.path.join(&file.name).display().to_string(), None)
                    };
                    Some((new_path, focus))
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((new_path, focus)) = remote_nav {
            if let Some(focus_name) = focus {
                self.active_panel_mut().pending_focus = Some(focus_name);
            }
            self.spawn_remote_list_dir(&new_path);
            return;
        }

        let panel = self.active_panel_mut();
        if let Some(file) = panel.current_file().cloned() {
            if file.is_directory {
                if file.name == ".." {
                    // Go to parent - remember current directory name
                    if let Some(current_name) = panel.path.file_name() {
                        panel.pending_focus = Some(current_name.to_string_lossy().to_string());
                    }
                    if let Some(parent) = panel.path.parent() {
                        panel.path = parent.to_path_buf();
                        panel.selected_index = 0;
                        panel.selected_files.clear();
                        panel.load_files();
                    }
                } else {
                    panel.path = panel.path.join(&file.name);
                    panel.selected_index = 0;
                    panel.selected_files.clear();
                    panel.load_files();
                }
            } else {
                // 원격 파일: 이미지는 뷰어, 나머지는 편집기 (프로그레스 표시)
                if panel.is_remote() {
                    let is_image = {
                        let p = std::path::Path::new(&file.name);
                        crate::ui::image_viewer::is_image_file(p)
                    };

                    if is_image {
                        let tmp_path = match self.remote_tmp_path(&file.name) {
                            Some(p) => p,
                            None => return,
                        };
                        self.download_for_remote_open(
                            &file.name,
                            file.size,
                            PendingRemoteOpen::ImageViewer { tmp_path },
                        );
                    } else {
                        self.edit_file();
                    }
                    return;
                }

                // It's a file - check for extension handler first
                let path = panel.path.join(&file.name);

                // Try extension handler first (takes priority over all default behaviors)
                match self.try_extension_handler(&path) {
                    Ok(true) => {
                        // Handler executed successfully, nothing more to do
                        return;
                    }
                    Ok(false) => {
                        // No handler defined, continue with default behavior
                    }
                    Err(error_msg) => {
                        // All handlers failed, show error dialog
                        self.show_extension_handler_error(&error_msg);
                        return;
                    }
                }

                // Default behavior: check file type
                if Self::is_archive_file(&file.name) {
                    // It's an archive file - extract it
                    self.execute_untar(&path);
                    return;
                }

                // Check file size for large file warning
                const LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024; // 50MB
                let file_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

                let is_image = crate::ui::image_viewer::is_image_file(&path);

                if file_size > LARGE_FILE_THRESHOLD {
                    // Show confirmation dialog for large file
                    let size_mb = file_size as f64 / (1024.0 * 1024.0);
                    if is_image {
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
                    } else {
                        self.pending_large_file = Some(path);
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::LargeFileConfirm,
                            input: String::new(),
                            cursor_pos: 0,
                            message: format!("This file is {:.1}MB. Open anyway?", size_mb),
                            completion: None,
                            selected_button: 1, // Default to "No"
                            selection: None,
                            use_md5: false,
                        });
                    }
                } else if is_image {
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
                    } else {
                        self.image_viewer_state =
                            Some(crate::ui::image_viewer::ImageViewerState::new(&path));
                        self.current_screen = Screen::ImageViewer;
                    }
                } else {
                    // Regular file - check if binary
                    if Self::is_binary_file(&path) {
                        // Binary file without handler - show handler setup dialog
                        let extension = path
                            .extension()
                            .map(|e| e.to_string_lossy().to_string())
                            .unwrap_or_default();
                        self.pending_binary_file = Some((path, extension.clone()));
                        self.dialog = Some(Dialog {
                            dialog_type: DialogType::BinaryFileHandler,
                            input: String::new(),
                            cursor_pos: 0,
                            message: extension,
                            completion: None,
                            selected_button: 0, // 0: Set mode (no existing handler)
                            selection: None,
                            use_md5: false,
                        });
                    } else {
                        // Text file - open editor
                        self.edit_file()
                    }
                }
            }
        }
    }

    /// Check if a file is a supported archive format
    fn is_archive_file(filename: &str) -> bool {
        let lower = filename.to_lowercase();
        lower.ends_with(".tar")
            || lower.ends_with(".tar.gz")
            || lower.ends_with(".tgz")
            || lower.ends_with(".tar.bz2")
            || lower.ends_with(".tbz2")
            || lower.ends_with(".tar.xz")
            || lower.ends_with(".txz")
    }

    /// Check if a file is binary (not a text file)
    /// Reads the first 8KB of the file and checks for null bytes or high proportion of non-text bytes
    fn is_binary_file(path: &std::path::Path) -> bool {
        use std::io::Read;

        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return false, // Can't open, assume text
        };

        let mut reader = std::io::BufReader::new(file);
        let mut buffer = [0u8; 8192]; // Read first 8KB

        let bytes_read = match reader.read(&mut buffer) {
            Ok(n) => n,
            Err(_) => return false,
        };

        if bytes_read == 0 {
            return false; // Empty file is text
        }

        // Check for null bytes (strong indicator of binary)
        // Also count non-printable bytes (excluding common whitespace)
        let mut non_text_count = 0;
        for &byte in &buffer[..bytes_read] {
            if byte == 0 {
                return true; // Null byte = definitely binary
            }
            // Non-printable and non-whitespace characters
            // Allow: tab (9), newline (10), carriage return (13), and printable ASCII (32-126)
            // Also allow UTF-8 continuation bytes (128-255) for international text
            if byte < 9 || (byte > 13 && byte < 32) || byte == 127 {
                non_text_count += 1;
            }
        }

        // If more than 10% of bytes are non-text control characters, consider it binary
        let threshold = bytes_read / 10;
        non_text_count > threshold
    }

    /// Try to execute extension handler commands for a file
    /// Returns Ok(true) if a handler was executed successfully
    /// Returns Ok(false) if no handler is defined for this extension
    /// Returns Err(error_message) if all handlers failed
    ///
    /// Handler prefix:
    /// - No prefix: Foreground execution (suspends TUI, runs command, waits for exit, restores TUI)
    ///   Example: "vim {{FILEPATH}}" - hands over terminal, blocks until program exits
    /// - @ prefix: Background execution (spawns detached, returns to RemoteCC immediately)
    ///   Example: "@evince {{FILEPATH}}" - does not wait for program to finish
    pub fn try_extension_handler(&mut self, path: &std::path::Path) -> Result<bool, String> {
        // Get file extension
        let extension = match path.extension() {
            Some(ext) => ext.to_string_lossy().to_string(),
            None => return Ok(false), // No extension, use default behavior
        };

        // Check if there's a handler for this extension
        let handlers = match self.settings.get_extension_handler(&extension) {
            Some(h) => h.clone(),
            None => return Ok(false), // No handler defined, use default behavior
        };

        if handlers.is_empty() {
            return Ok(false);
        }

        // Get the current working directory from active panel
        let cwd = self.active_panel().path.clone();

        let file_path_str = path.to_string_lossy().to_string();
        let mut last_error = String::new();

        // Try each handler in order (fallback mechanism)
        for handler_template in &handlers {
            // Check for background mode prefix (@)
            let (is_background_mode, template) = if handler_template.starts_with('@') {
                (true, &handler_template[1..])
            } else {
                (false, handler_template.as_str())
            };

            // Replace {{FILEPATH}} with actual file path (no escaping needed - will use base64)
            let command = template.replace("{{FILEPATH}}", &file_path_str);

            if is_background_mode {
                // Background mode: spawn and detach (@ prefix)
                match self.execute_background_command(&command, template, &cwd) {
                    Ok(true) => {
                        self.refresh_panels();
                        return Ok(true);
                    }
                    Ok(false) => {
                        // Command failed, error already set in last_error via closure
                        continue;
                    }
                    Err(e) => {
                        last_error = e;
                        continue;
                    }
                }
            } else {
                // Foreground mode: suspend TUI, run command, restore TUI (default)
                match self.execute_terminal_command(&command, &cwd) {
                    Ok(true) => {
                        self.refresh_panels();
                        return Ok(true);
                    }
                    Ok(false) => {
                        last_error = format!("Command failed: {}", template);
                        continue;
                    }
                    Err(e) => {
                        last_error = e;
                        continue;
                    }
                }
            }
        }

        // All handlers failed
        Err(last_error)
    }

    /// Execute a command in terminal mode (blocking, inherits stdio)
    /// Suspends the TUI, runs the command, then restores the TUI
    fn execute_terminal_command(
        &mut self,
        command: &str,
        cwd: &std::path::Path,
    ) -> Result<bool, String> {
        use crossterm::cursor::{Hide, Show};
        use crossterm::execute;
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
        use std::io::{stdout, Write};

        // Show cursor and leave alternate screen
        let _ = execute!(stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();

        // Clear screen for clean command output
        print!("\x1B[2J\x1B[H");
        let _ = stdout().flush();

        // Execute command with inherited stdio and active panel's directory as CWD
        // Use base64 encoding to avoid shell escaping issues
        let encoded = encode_command_base64(command);
        let exe_path = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "remotecc".to_string());
        let wrapped_command = format!("eval \"$('{}' --base64 '{}')\"", exe_path, encoded);

        let result = std::process::Command::new("bash")
            .arg("-c")
            .arg(&wrapped_command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status();

        // Restore: enable raw mode, enter alternate screen, hide cursor
        let _ = enable_raw_mode();
        let _ = execute!(stdout(), EnterAlternateScreen, Hide);

        // Request full redraw on next frame
        self.needs_full_redraw = true;

        match result {
            Ok(status) => Ok(status.success()),
            Err(e) => Err(format!("Failed to execute: {}", e)),
        }
    }

    /// Execute a command in background mode (non-blocking, detached)
    fn execute_background_command(
        &self,
        command: &str,
        template: &str,
        cwd: &std::path::Path,
    ) -> Result<bool, String> {
        // Use base64 encoding to avoid shell escaping issues
        let encoded = encode_command_base64(command);
        let exe_path = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "remotecc".to_string());
        let wrapped_command = format!("eval \"$('{}' --base64 '{}')\"", exe_path, encoded);

        let result = std::process::Command::new("bash")
            .arg("-c")
            .arg(&wrapped_command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match result {
            Ok(mut child) => {
                // Wait briefly to check if command started successfully
                std::thread::sleep(std::time::Duration::from_millis(100));

                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Process exited quickly - likely an error
                        if !status.success() {
                            // Try to get stderr
                            if let Some(mut stderr) = child.stderr.take() {
                                use std::io::Read;
                                let mut err_msg = String::new();
                                let _ = stderr.read_to_string(&mut err_msg);
                                if err_msg.trim().is_empty() {
                                    return Err(format!("Command failed: {}", template));
                                } else {
                                    return Err(err_msg.trim().to_string());
                                }
                            }
                            return Err(format!("Command failed: {}", template));
                        }
                        Ok(true) // Command succeeded quickly
                    }
                    Ok(None) => {
                        // Process still running - consider it successful
                        Ok(true)
                    }
                    Err(e) => Err(format!("Failed to check process: {}", e)),
                }
            }
            Err(e) => Err(format!("Failed to execute '{}': {}", template, e)),
        }
    }

    pub fn go_to_parent(&mut self) {
        if self.active_panel().is_remote() {
            // Remote parent navigation — use spinner
            let focus = self
                .active_panel()
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string());
            let parent = self
                .active_panel()
                .path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/".to_string());
            if let Some(focus_name) = focus {
                self.active_panel_mut().pending_focus = Some(focus_name);
            }
            self.spawn_remote_list_dir(&parent);
            return;
        }
        let panel = self.active_panel_mut();
        if let Some(current_name) = panel.path.file_name() {
            panel.pending_focus = Some(current_name.to_string_lossy().to_string());
        }
        if let Some(parent) = panel.path.parent() {
            panel.path = parent.to_path_buf();
            panel.selected_index = 0;
            panel.selected_files.clear();
            panel.load_files();
        }
    }

    /// 홈 디렉토리로 이동
    pub fn goto_home(&mut self) {
        if let Some(home) = dirs::home_dir() {
            // Disconnect remote if active panel is remote
            if self.active_panel().is_remote() {
                if self.remote_spinner.is_some() {
                    return;
                }
                self.disconnect_remote_panel();
            }
            let panel = self.active_panel_mut();
            panel.path = home;
            panel.selected_index = 0;
            panel.selected_files.clear();
            panel.load_files();
        }
    }

    /// Open current folder in Finder (macOS only)
    #[cfg(target_os = "macos")]
    pub fn open_in_finder(&mut self) {
        let path = self.active_panel().path.clone();
        match std::process::Command::new("open").arg(&path).spawn() {
            Ok(_) => self.show_message(&format!("Opened in Finder: {}", path.display())),
            Err(e) => self.show_message(&format!("Failed to open: {}", e)),
        }
    }

    /// Open current folder in VS Code (macOS only)
    /// Falls back to code-insiders if code is not available
    #[cfg(target_os = "macos")]
    pub fn open_in_vscode(&mut self) {
        use std::process::Command;

        let path = self.active_panel().path.clone();

        // Check which command is available
        let code_cmd = if Command::new("which")
            .arg("code")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            "code"
        } else if Command::new("which")
            .arg("code-insiders")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            "code-insiders"
        } else {
            self.show_message("VS Code not found (tried: code, code-insiders)");
            return;
        };

        match Command::new(code_cmd).arg(&path).spawn() {
            Ok(_) => self.show_message(&format!("Opened in {}: {}", code_cmd, path.display())),
            Err(e) => self.show_message(&format!("Failed to open {}: {}", code_cmd, e)),
        }
    }

    pub fn toggle_selection(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(file) = panel.current_file() {
            if file.name != ".." {
                let name = file.name.clone();
                if panel.selected_files.contains(&name) {
                    panel.selected_files.remove(&name);
                } else {
                    panel.selected_files.insert(name);
                }
                // Move cursor down
                if panel.selected_index < panel.files.len() - 1 {
                    panel.selected_index += 1;
                }
            }
        }
    }

    pub fn toggle_all_selection(&mut self) {
        let panel = self.active_panel_mut();
        if panel.selected_files.is_empty() {
            // Select all (except ..)
            for file in &panel.files {
                if file.name != ".." {
                    panel.selected_files.insert(file.name.clone());
                }
            }
        } else {
            panel.selected_files.clear();
        }
    }

    pub fn select_by_extension(&mut self) {
        let panel = self.active_panel_mut();
        if let Some(current_file) = panel.files.get(panel.selected_index) {
            // Get extension of current file
            let target_ext = std::path::Path::new(&current_file.name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase());

            if let Some(ext) = target_ext {
                // Collect files with same extension
                let matching_files: Vec<String> = panel
                    .files
                    .iter()
                    .filter(|f| f.name != ".." && !f.is_directory)
                    .filter(|f| {
                        std::path::Path::new(&f.name)
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_lowercase())
                            .as_ref()
                            == Some(&ext)
                    })
                    .map(|f| f.name.clone())
                    .collect();

                // Check if all matching files are already selected
                let all_selected = matching_files
                    .iter()
                    .all(|name| panel.selected_files.contains(name));

                let count = matching_files.len();
                if all_selected {
                    // Deselect all matching files
                    for name in matching_files {
                        panel.selected_files.remove(&name);
                    }
                    self.show_message(&format!("Deselected {} .{} file(s)", count, ext));
                } else {
                    // Select all matching files
                    for name in matching_files {
                        panel.selected_files.insert(name);
                    }
                    self.show_message(&format!("Selected {} .{} file(s)", count, ext));
                }
            }
        }
    }

    pub fn toggle_sort_by_name(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Name);
    }

    pub fn toggle_sort_by_size(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Size);
    }

    pub fn toggle_sort_by_date(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Modified);
    }

    pub fn toggle_sort_by_type(&mut self) {
        self.active_panel_mut().toggle_sort(SortBy::Type);
    }

    pub fn show_message(&mut self, msg: &str) {
        self.message = Some(msg.to_string());
        self.message_timer = 10; // ~1 second at 10 FPS
    }

    /// Toggle bookmark for the current panel's path
    pub fn toggle_bookmark(&mut self) {
        let current_path = if self.active_panel().is_remote() {
            let path = self.active_panel().path.display().to_string();
            if let Some(ref ctx) = self.active_panel().remote_ctx {
                crate::services::remote::format_remote_display(&ctx.profile, &path)
            } else if let Some((ref user, ref host, port)) = self.active_panel().remote_display {
                if port != 22 {
                    format!("{}@{}:{}:{}", user, host, port, path)
                } else {
                    format!("{}@{}:{}", user, host, path)
                }
            } else {
                return;
            }
        } else {
            self.active_panel().path.display().to_string()
        };

        if let Some(pos) = self
            .settings
            .bookmarked_path
            .iter()
            .position(|p| p == &current_path)
        {
            self.settings.bookmarked_path.remove(pos);
            self.show_message(&format!("Bookmark removed: {}", current_path));
        } else {
            self.settings.bookmarked_path.push(current_path.clone());
            self.show_message(&format!("Bookmark added: {}", current_path));
        }

        let _ = self.settings.save();
    }

    pub fn refresh_panels(&mut self) {
        // Check if any panel is remote and needs async refresh
        let mut remote_panel_idx = None;
        for (i, panel) in self.panels.iter_mut().enumerate() {
            panel.selected_files.clear();
            if panel.is_remote() {
                if panel.remote_ctx.is_some() {
                    // Don't call load_files on remote panels — use spinner instead
                    remote_panel_idx = Some(i);
                }
                // If remote_ctx is temporarily taken by background thread, skip
            } else {
                panel.load_files();
            }
        }
        // Spawn async refresh for the first remote panel found
        if let Some(idx) = remote_panel_idx {
            if self.remote_spinner.is_none() {
                self.spawn_remote_refresh(idx);
            }
        }
    }

    pub fn get_operation_files(&self) -> Vec<String> {
        let panel = self.active_panel();
        if !panel.selected_files.is_empty() {
            panel.selected_files.iter().cloned().collect()
        } else if let Some(file) = panel.current_file() {
            if file.name != ".." {
                vec![file.name.clone()]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    pub fn is_ai_mode(&self) -> bool {
        self.ai_panel_index.is_some() && self.ai_state.is_some()
    }

    pub fn goto_directory_with_focus(&mut self, dir: &Path, filename: Option<String>) {
        let panel = self.active_panel_mut();
        panel.path = dir.to_path_buf();
        panel.selected_index = 0;
        panel.selected_files.clear();
        panel.pending_focus = filename;
        panel.load_files();
    }

    /// 검색 결과에서 선택한 항목의 경로로 이동
    pub fn goto_search_result(&mut self) {
        if let Some(item) = self.search_result_state.current_item().cloned() {
            if item.is_directory {
                // 디렉토리인 경우 해당 디렉토리로 이동
                self.goto_directory_with_focus(&item.full_path, None);
            } else {
                // 파일인 경우 부모 디렉토리로 이동하고 해당 파일에 커서
                if let Some(parent) = item.full_path.parent() {
                    self.goto_directory_with_focus(parent, Some(item.name.clone()));
                }
            }
            // 검색 결과 화면 닫기
            self.search_result_state.active = false;
            self.current_screen = Screen::FilePanel;
            self.show_message(&format!("Moved to: {}", item.relative_path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counter for unique temp directory names
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Helper to create a temporary directory for testing
    fn create_temp_dir() -> PathBuf {
        let unique_id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp_dir = std::env::temp_dir().join(format!(
            "remotecc_app_test_{}_{}",
            std::process::id(),
            unique_id
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");
        temp_dir
    }

    /// Helper to cleanup temp directory
    fn cleanup_temp_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    // ========== get_valid_path tests ==========

    #[test]
    fn test_get_valid_path_existing() {
        let temp_dir = create_temp_dir();
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&temp_dir, &fallback);
        assert_eq!(result, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_get_valid_path_nonexistent_uses_parent() {
        let temp_dir = create_temp_dir();
        let nonexistent = temp_dir.join("does_not_exist");
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&nonexistent, &fallback);
        assert_eq!(result, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_get_valid_path_fallback() {
        let nonexistent = PathBuf::from("/nonexistent/path/that/does/not/exist");
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&nonexistent, &fallback);
        // Should fall back to /tmp or /
        assert!(result.exists());
    }

    #[test]
    fn test_get_valid_path_root() {
        let root = PathBuf::from("/");
        let fallback = PathBuf::from("/tmp");

        let result = get_valid_path(&root, &fallback);
        assert_eq!(result, root);
    }

    // ========== PanelState tests ==========

    #[test]
    fn test_panel_state_initialization() {
        let temp_dir = create_temp_dir();

        // Create some test files
        fs::write(temp_dir.join("file1.txt"), "content").unwrap();
        fs::write(temp_dir.join("file2.txt"), "content").unwrap();
        fs::create_dir(temp_dir.join("subdir")).unwrap();

        let panel = PanelState::new(temp_dir.clone());

        assert_eq!(panel.path, temp_dir);
        assert!(!panel.files.is_empty());
        assert_eq!(panel.selected_index, 0);
        assert!(panel.selected_files.is_empty());
        assert_eq!(panel.sort_by, SortBy::Name);
        assert_eq!(panel.sort_order, SortOrder::Asc);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_panel_state_has_parent_entry() {
        let temp_dir = create_temp_dir();
        let subdir = temp_dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();

        let panel = PanelState::new(subdir);

        // Should have ".." entry
        assert!(panel.files.iter().any(|f| f.name == ".."));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_panel_state_current_file() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("test.txt"), "content").unwrap();

        let panel = PanelState::new(temp_dir.clone());

        let current = panel.current_file();
        assert!(current.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_panel_state_toggle_sort() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("a.txt"), "content").unwrap();
        fs::write(temp_dir.join("b.txt"), "content").unwrap();

        let mut panel = PanelState::new(temp_dir.clone());

        // Default is Name Asc
        assert_eq!(panel.sort_by, SortBy::Name);
        assert_eq!(panel.sort_order, SortOrder::Asc);

        // Toggle same sort field -> change order
        panel.toggle_sort(SortBy::Name);
        assert_eq!(panel.sort_by, SortBy::Name);
        assert_eq!(panel.sort_order, SortOrder::Desc);

        // Toggle different sort field -> change field, reset to Asc
        panel.toggle_sort(SortBy::Size);
        assert_eq!(panel.sort_by, SortBy::Size);
        assert_eq!(panel.sort_order, SortOrder::Asc);

        cleanup_temp_dir(&temp_dir);
    }

    // ========== App tests ==========

    #[test]
    fn test_app_initialization() {
        let temp_dir = create_temp_dir();
        let first_path = temp_dir.join("first");
        let second_path = temp_dir.join("second");

        fs::create_dir_all(&first_path).unwrap();
        fs::create_dir_all(&second_path).unwrap();

        let app = App::new(first_path.clone(), second_path.clone());

        assert_eq!(app.panels[0].path, first_path);
        assert_eq!(app.panels[1].path, second_path);
        assert_eq!(app.active_panel_index, 0);
        assert_eq!(app.current_screen, Screen::FilePanel);
        assert!(app.dialog.is_none());
        assert!(app.message.is_none());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_switch_panel() {
        let temp_dir = create_temp_dir();
        fs::create_dir_all(temp_dir.join("panel1")).unwrap();
        fs::create_dir_all(temp_dir.join("panel2")).unwrap();

        let mut app = App::new(temp_dir.join("panel1"), temp_dir.join("panel2"));

        assert_eq!(app.active_panel_index, 0);

        app.switch_panel();
        assert_eq!(app.active_panel_index, 1);

        app.switch_panel();
        assert_eq!(app.active_panel_index, 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_cursor_movement() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "").unwrap();
        fs::write(temp_dir.join("file2.txt"), "").unwrap();
        fs::write(temp_dir.join("file3.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        let initial_index = app.active_panel().selected_index;

        app.move_cursor(1);
        assert_eq!(app.active_panel().selected_index, initial_index + 1);

        app.move_cursor(-1);
        assert_eq!(app.active_panel().selected_index, initial_index);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_cursor_bounds() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move cursor way past the end
        app.move_cursor(1000);
        let len = app.active_panel().files.len();
        assert!(app.active_panel().selected_index < len);

        // Move cursor way before the start
        app.move_cursor(-1000);
        assert_eq!(app.active_panel().selected_index, 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_cursor_to_start_end() {
        let temp_dir = create_temp_dir();
        for i in 0..10 {
            fs::write(temp_dir.join(format!("file{}.txt", i)), "").unwrap();
        }

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        app.cursor_to_end();
        let len = app.active_panel().files.len();
        assert_eq!(app.active_panel().selected_index, len - 1);

        app.cursor_to_start();
        assert_eq!(app.active_panel().selected_index, 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_show_message() {
        let temp_dir = create_temp_dir();
        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        assert!(app.message.is_none());

        app.show_message("Test message");
        assert_eq!(app.message, Some("Test message".to_string()));
        assert!(app.message_timer > 0);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_toggle_selection() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "").unwrap();
        fs::write(temp_dir.join("file2.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".." if present
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        let file_name = app.active_panel().current_file().unwrap().name.clone();

        app.toggle_selection();
        assert!(app.active_panel().selected_files.contains(&file_name));

        // Move back to same file
        app.move_cursor(-1);
        app.toggle_selection();
        assert!(!app.active_panel().selected_files.contains(&file_name));

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_app_get_operation_files() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "").unwrap();
        fs::write(temp_dir.join("file2.txt"), "").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // No selection - returns current file
        let files = app.get_operation_files();
        assert_eq!(files.len(), 1);

        // With selection - returns selected files
        app.toggle_selection();
        let files = app.get_operation_files();
        assert_eq!(files.len(), 1);

        cleanup_temp_dir(&temp_dir);
    }

    // ========== Enum tests ==========

    #[test]
    fn test_panel_index_equality() {
        let idx_a: usize = 0;
        let idx_b: usize = 1;
        assert_eq!(idx_a, 0);
        assert_eq!(idx_b, 1);
        assert_ne!(idx_a, idx_b);
    }

    #[test]
    fn test_sort_by_equality() {
        assert_eq!(SortBy::Name, SortBy::Name);
        assert_eq!(SortBy::Size, SortBy::Size);
        assert_eq!(SortBy::Modified, SortBy::Modified);
    }

    #[test]
    fn test_screen_equality() {
        assert_eq!(Screen::FilePanel, Screen::FilePanel);
        assert_eq!(Screen::FileViewer, Screen::FileViewer);
        assert_ne!(Screen::FilePanel, Screen::Help);
    }

    #[test]
    fn test_dialog_type_equality() {
        assert_eq!(DialogType::Delete, DialogType::Delete);
        assert_ne!(DialogType::Delete, DialogType::Mkdir);
    }

    // ========== Clipboard tests ==========

    #[test]
    fn test_clipboard_copy() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".." if present
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Copy to clipboard
        app.clipboard_copy();

        assert!(app.clipboard.is_some());
        let clipboard = app.clipboard.as_ref().unwrap();
        assert_eq!(clipboard.operation, ClipboardOperation::Copy);
        assert_eq!(clipboard.files.len(), 1);
        assert_eq!(clipboard.source_path, temp_dir);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_cut() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file1.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Cut to clipboard
        app.clipboard_cut();

        assert!(app.clipboard.is_some());
        let clipboard = app.clipboard.as_ref().unwrap();
        assert_eq!(clipboard.operation, ClipboardOperation::Cut);
        assert_eq!(clipboard.files.len(), 1);

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_paste_copy() {
        let temp_dir = create_temp_dir();
        let src_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(src_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(src_dir.clone(), dest_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Copy to clipboard
        app.clipboard_copy();

        // Switch to right panel (dest)
        app.switch_panel();

        // Paste
        app.clipboard_paste();

        // Wait for async operation to complete
        while app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false)
        {
            if let Some(ref mut progress) = app.file_operation_progress {
                progress.poll();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // File should exist in both locations
        assert!(src_dir.join("file.txt").exists());
        assert!(dest_dir.join("file.txt").exists());

        // Clipboard should still exist (copy can be pasted multiple times)
        assert!(app.clipboard.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_paste_cut() {
        let temp_dir = create_temp_dir();
        let src_dir = temp_dir.join("src");
        let dest_dir = temp_dir.join("dest");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();
        fs::write(src_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(src_dir.clone(), dest_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Cut to clipboard
        app.clipboard_cut();

        // Switch to right panel (dest)
        app.switch_panel();

        // Paste
        app.clipboard_paste();

        // Wait for async operation to complete
        while app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false)
        {
            if let Some(ref mut progress) = app.file_operation_progress {
                progress.poll();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // File should only exist in destination
        assert!(!src_dir.join("file.txt").exists());
        assert!(dest_dir.join("file.txt").exists());

        // Clipboard should be cleared (cut can only be pasted once)
        assert!(app.clipboard.is_none());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_paste_same_folder_rejected() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        // Copy to clipboard
        app.clipboard_copy();

        // Try to paste to the same folder
        app.clipboard_paste();

        // Clipboard should still exist (paste was rejected)
        assert!(app.clipboard.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_empty_rejected() {
        let temp_dir = create_temp_dir();
        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        // Clipboard is empty
        assert!(app.clipboard.is_none());

        // Try to paste
        app.clipboard_paste();

        // Should show message but not crash
        assert!(app.message.is_some());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_has_clipboard() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        assert!(!app.has_clipboard());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        app.clipboard_copy();
        assert!(app.has_clipboard());

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_info() {
        let temp_dir = create_temp_dir();
        fs::write(temp_dir.join("file.txt"), "content").unwrap();

        let mut app = App::new(temp_dir.clone(), temp_dir.clone());

        assert!(app.clipboard_info().is_none());

        // Move past ".."
        if app.active_panel().files.first().map(|f| f.name.as_str()) == Some("..") {
            app.move_cursor(1);
        }

        app.clipboard_copy();
        let info = app.clipboard_info();
        assert!(info.is_some());
        let (count, op) = info.unwrap();
        assert_eq!(count, 1);
        assert_eq!(op, "copy");

        cleanup_temp_dir(&temp_dir);
    }

    #[test]
    fn test_clipboard_operation_equality() {
        assert_eq!(ClipboardOperation::Copy, ClipboardOperation::Copy);
        assert_eq!(ClipboardOperation::Cut, ClipboardOperation::Cut);
        assert_ne!(ClipboardOperation::Copy, ClipboardOperation::Cut);
    }
}
