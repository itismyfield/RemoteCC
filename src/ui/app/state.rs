use chrono::{DateTime, Local};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::config::Settings;
use crate::services::file_ops::{FileOperationResult, FileOperationType, ProgressMessage};
use crate::services::remote::{self, RemoteContext, RemoteProfile, SftpFileEntry};
use crate::ui::theme::DEFAULT_THEME_NAME;

/// Encode a command as base64 for safe shell execution
/// This avoids all shell escaping issues by encoding the entire command
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

pub fn encode_command_base64(command: &str) -> String {
    BASE64.encode(command.as_bytes())
}

/// Theme file watcher state for hot-reload
pub struct ThemeWatchState {
    /// Path to the current theme file (if external)
    pub theme_path: Option<PathBuf>,
    /// Last modification time of the theme file
    pub last_modified: Option<SystemTime>,
    /// Counter for polling interval (check every 10 ticks = ~1 second)
    pub check_counter: u8,
}

impl ThemeWatchState {
    /// Create a new watch state for the given theme name
    pub fn watch_theme(theme_name: &str) -> Self {
        let theme_path = crate::ui::theme_loader::theme_path(theme_name);
        let last_modified = theme_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()));

        Self {
            theme_path,
            last_modified,
            check_counter: 0,
        }
    }

    /// Check if the theme file has been modified.
    /// Returns true if the file was modified and should be reloaded.
    /// Only checks every 10 calls (~1 second with 100ms tick).
    pub fn check_for_changes(&mut self) -> bool {
        self.check_counter = self.check_counter.wrapping_add(1);
        if self.check_counter % 10 != 0 {
            return false;
        }

        let Some(ref path) = self.theme_path else {
            return false;
        };

        let current_modified = match std::fs::metadata(path) {
            Ok(m) => m.modified().ok(),
            Err(_) => return false,
        };

        if current_modified != self.last_modified {
            self.last_modified = current_modified;
            return true;
        }

        false
    }

    /// Update the watch state for a new theme
    pub fn update_theme(&mut self, theme_name: &str) {
        self.theme_path = crate::ui::theme_loader::theme_path(theme_name);
        self.last_modified = self
            .theme_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()));
        self.check_counter = 0;
    }
}

/// Help screen state for scrolling
pub struct HelpState {
    pub scroll_offset: usize,
    pub max_scroll: usize,
    pub visible_height: usize,
}

impl Default for HelpState {
    fn default() -> Self {
        Self {
            scroll_offset: 0,
            max_scroll: 0,
            visible_height: 0,
        }
    }
}

/// Get a valid directory path, falling back to parent directories if needed
pub fn get_valid_path(target_path: &Path, fallback: &Path) -> PathBuf {
    let mut current = target_path.to_path_buf();

    loop {
        if current.is_dir() {
            // Check if we can actually read the directory
            if fs::read_dir(&current).is_ok() {
                return current;
            }
        }

        // Try parent directory
        if let Some(parent) = current.parent() {
            if parent == current {
                // Reached root, use fallback
                break;
            }
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    // Fallback path validation
    if fallback.is_dir() && fs::read_dir(fallback).is_ok() {
        return fallback.to_path_buf();
    }

    // Ultimate fallback to root
    PathBuf::from("/")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Name,
    Type,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Screen {
    FilePanel,
    FileViewer,
    FileEditor,
    FileInfo,
    ProcessManager,
    Help,
    AIScreen,
    SystemInfo,
    ImageViewer,
    SearchResult,
    DiffScreen,
    DiffFileView,
    GitScreen,
    DedupScreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogType {
    Delete,
    Mkdir,
    Mkfile,
    Rename,
    Search,
    Goto,
    Tar,
    TarExcludeConfirm,
    LargeImageConfirm,
    LargeFileConfirm,
    TrueColorWarning,
    Progress,
    DuplicateConflict,
    Settings,
    ExtensionHandlerError,
    BinaryFileHandler,
    GitLogDiff,
    /// Remote connection dialog - enter auth info for new server
    RemoteConnect,
    /// Remote profile save prompt - ask to save after successful connect
    RemoteProfileSave,
    EncryptConfirm,
    DecryptConfirm,
    DedupConfirm,
}

/// Settings dialog state
#[derive(Debug, Clone)]
pub struct SettingsState {
    /// Available theme names (from ~/.remotecc/themes/)
    pub themes: Vec<String>,
    /// Currently selected theme index
    pub theme_index: usize,
    /// Currently selected field row in settings dialog (0=theme, 1=diff method)
    pub selected_field: usize,
    /// Available diff compare methods
    pub diff_methods: Vec<String>,
    /// Currently selected diff method index
    pub diff_method_index: usize,
}

impl SettingsState {
    pub fn new(settings: &Settings) -> Self {
        // Scan available themes
        let mut themes = vec!["light".to_string(), "dark".to_string()];
        if let Some(themes_dir) = Settings::themes_dir() {
            if let Ok(entries) = std::fs::read_dir(&themes_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "json").unwrap_or(false) {
                        if let Some(stem) = path.file_stem() {
                            let name = stem.to_string_lossy().to_string();
                            if name.contains(' ') {
                                continue;
                            }
                            if !themes.contains(&name) {
                                themes.push(name);
                            }
                        }
                    }
                }
            }
        }
        themes.sort();

        // Find current theme index
        let theme_index = themes
            .iter()
            .position(|t| t == &settings.theme.name)
            .unwrap_or(0);

        let diff_methods = vec![
            "content".to_string(),
            "modified_time".to_string(),
            "content_and_time".to_string(),
        ];
        let diff_method_index = diff_methods
            .iter()
            .position(|m| m == &settings.diff_compare_method)
            .unwrap_or(0);

        Self {
            themes,
            theme_index,
            selected_field: 0,
            diff_methods,
            diff_method_index,
        }
    }

    pub fn current_theme(&self) -> &str {
        self.themes
            .get(self.theme_index)
            .map(|s| s.as_str())
            .unwrap_or(DEFAULT_THEME_NAME)
    }

    pub fn next_theme(&mut self) {
        if !self.themes.is_empty() {
            self.theme_index = (self.theme_index + 1) % self.themes.len();
        }
    }

    pub fn prev_theme(&mut self) {
        if !self.themes.is_empty() {
            self.theme_index = if self.theme_index == 0 {
                self.themes.len() - 1
            } else {
                self.theme_index - 1
            };
        }
    }

    pub fn current_diff_method(&self) -> &str {
        self.diff_methods
            .get(self.diff_method_index)
            .map(|s| s.as_str())
            .unwrap_or("content")
    }

    pub fn next_diff_method(&mut self) {
        if !self.diff_methods.is_empty() {
            self.diff_method_index = (self.diff_method_index + 1) % self.diff_methods.len();
        }
    }

    pub fn prev_diff_method(&mut self) {
        if !self.diff_methods.is_empty() {
            self.diff_method_index = if self.diff_method_index == 0 {
                self.diff_methods.len() - 1
            } else {
                self.diff_method_index - 1
            };
        }
    }
}

/// State for remote connection dialog
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteField {
    Host,
    Port,
    User,
    AuthType,
    Credential, // password or key_path depending on auth_type
    Passphrase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteAuthType {
    Password,
    KeyFile,
}

#[derive(Debug, Clone)]
pub struct RemoteConnectState {
    pub selected_field: RemoteField,
    pub host: String,
    pub port: String,
    pub user: String,
    pub auth_type: RemoteAuthType,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
    pub remote_path: String,
    pub profile_name: String,
    pub error: Option<String>,
    pub cursor_pos: usize,
    /// Some(idx) when editing an existing profile via Ctrl+E
    pub editing_profile_index: Option<usize>,
}

impl RemoteConnectState {
    pub fn new() -> Self {
        Self {
            selected_field: RemoteField::Host,
            host: String::new(),
            port: "22".to_string(),
            user: String::new(),
            auth_type: RemoteAuthType::Password,
            password: String::new(),
            key_path: "~/.ssh/id_rsa".to_string(),
            passphrase: String::new(),
            remote_path: "/".to_string(),
            profile_name: String::new(),
            error: None,
            cursor_pos: 0,
            editing_profile_index: None,
        }
    }

    pub fn from_profile(profile: &remote::RemoteProfile, profile_index: usize) -> Self {
        let (auth_type, password, key_path, passphrase) = match &profile.auth {
            remote::RemoteAuth::Password { password } => (
                RemoteAuthType::Password,
                password.clone(),
                "~/.ssh/id_rsa".to_string(),
                String::new(),
            ),
            remote::RemoteAuth::KeyFile { path, passphrase } => (
                RemoteAuthType::KeyFile,
                String::new(),
                path.clone(),
                passphrase.clone().unwrap_or_default(),
            ),
        };
        Self {
            selected_field: RemoteField::Host,
            host: profile.host.clone(),
            port: profile.port.to_string(),
            user: profile.user.clone(),
            auth_type,
            password,
            key_path,
            passphrase,
            remote_path: profile.default_path.clone(),
            profile_name: profile.name.clone(),
            error: None,
            cursor_pos: 0,
            editing_profile_index: Some(profile_index),
        }
    }

    pub fn from_parsed(user: &str, host: &str, port: u16, path: &str) -> Self {
        Self {
            selected_field: if user.is_empty() {
                RemoteField::User
            } else {
                RemoteField::AuthType
            },
            host: host.to_string(),
            port: port.to_string(),
            user: user.to_string(),
            auth_type: RemoteAuthType::Password,
            password: String::new(),
            key_path: "~/.ssh/id_rsa".to_string(),
            passphrase: String::new(),
            remote_path: path.to_string(),
            profile_name: String::new(),
            error: None,
            cursor_pos: 0,
            editing_profile_index: None,
        }
    }

    pub fn is_auth_type_field(&self) -> bool {
        self.selected_field == RemoteField::AuthType
    }

    pub fn toggle_auth_type(&mut self) {
        self.auth_type = match self.auth_type {
            RemoteAuthType::Password => RemoteAuthType::KeyFile,
            RemoteAuthType::KeyFile => RemoteAuthType::Password,
        };
    }

    pub fn next_field(&self) -> RemoteField {
        match self.selected_field {
            RemoteField::Host => RemoteField::Port,
            RemoteField::Port => RemoteField::User,
            RemoteField::User => RemoteField::AuthType,
            RemoteField::AuthType => RemoteField::Credential,
            RemoteField::Credential => match self.auth_type {
                RemoteAuthType::Password => RemoteField::Host, // wrap around
                RemoteAuthType::KeyFile => RemoteField::Passphrase,
            },
            RemoteField::Passphrase => RemoteField::Host, // wrap around
        }
    }

    pub fn prev_field(&self) -> RemoteField {
        match self.selected_field {
            RemoteField::Host => match self.auth_type {
                RemoteAuthType::Password => RemoteField::Credential, // wrap around
                RemoteAuthType::KeyFile => RemoteField::Passphrase,
            },
            RemoteField::Port => RemoteField::Host,
            RemoteField::User => RemoteField::Port,
            RemoteField::AuthType => RemoteField::User,
            RemoteField::Credential => RemoteField::AuthType,
            RemoteField::Passphrase => RemoteField::Credential,
        }
    }

    pub fn active_field_mut(&mut self) -> &mut String {
        match self.selected_field {
            RemoteField::Host => &mut self.host,
            RemoteField::Port => &mut self.port,
            RemoteField::User => &mut self.user,
            RemoteField::AuthType => &mut self.password, // placeholder - handled by toggle
            RemoteField::Credential => match self.auth_type {
                RemoteAuthType::Password => &mut self.password,
                RemoteAuthType::KeyFile => &mut self.key_path,
            },
            RemoteField::Passphrase => &mut self.passphrase,
        }
    }

    pub fn active_field_value(&self) -> &str {
        match self.selected_field {
            RemoteField::Host => &self.host,
            RemoteField::Port => &self.port,
            RemoteField::User => &self.user,
            RemoteField::AuthType => match self.auth_type {
                RemoteAuthType::Password => "Password",
                RemoteAuthType::KeyFile => "Key File",
            },
            RemoteField::Credential => match self.auth_type {
                RemoteAuthType::Password => &self.password,
                RemoteAuthType::KeyFile => &self.key_path,
            },
            RemoteField::Passphrase => &self.passphrase,
        }
    }

    pub fn to_profile(&self) -> remote::RemoteProfile {
        let port: u16 = self.port.parse().unwrap_or(22);
        let auth = match self.auth_type {
            RemoteAuthType::Password => remote::RemoteAuth::Password {
                password: self.password.clone(),
            },
            RemoteAuthType::KeyFile => remote::RemoteAuth::KeyFile {
                path: self.key_path.clone(),
                passphrase: if self.passphrase.is_empty() {
                    None
                } else {
                    Some(self.passphrase.clone())
                },
            },
        };

        let name = if self.profile_name.is_empty() {
            format!("{}@{}", self.user, self.host)
        } else {
            self.profile_name.clone()
        };

        remote::RemoteProfile {
            name,
            host: self.host.clone(),
            port,
            user: self.user.clone(),
            auth,
            default_path: self.remote_path.clone(),
            claude_path: None,
        }
    }
}

/// Fuzzy match: check if all characters in pattern appear in text in order
/// e.g., "thse" matches "/path/to/base" (t-h-s-e appear in sequence)
pub fn fuzzy_match(text: &str, pattern: &str) -> bool {
    let mut text_chars = text.chars().peekable();
    for pattern_char in pattern.chars() {
        loop {
            match text_chars.next() {
                Some(c) if c == pattern_char => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Resolution option for duplicate file conflicts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    Overwrite,
    Skip,
    OverwriteAll,
    SkipAll,
}

/// State for managing file conflict resolution during paste operations
#[derive(Debug, Clone)]
pub struct ConflictState {
    /// List of conflicts: (source path, destination path, display name)
    pub conflicts: Vec<(PathBuf, PathBuf, String)>,
    /// Current conflict index being resolved
    pub current_index: usize,
    /// Files that user chose to overwrite
    pub files_to_overwrite: Vec<PathBuf>,
    /// Files that user chose to skip
    pub files_to_skip: Vec<PathBuf>,
    /// Backup of clipboard for the operation
    pub clipboard_backup: Option<Clipboard>,
    /// Whether this is a move (cut) operation
    pub is_move_operation: bool,
    /// Target directory for the operation
    pub target_path: PathBuf,
}

/// State for tar exclude confirmation dialog
#[derive(Debug, Clone)]
pub struct TarExcludeState {
    /// Archive name to create
    pub archive_name: String,
    /// Files to archive
    pub files: Vec<String>,
    /// Paths to exclude (unsafe symlinks)
    pub excluded_paths: Vec<String>,
    /// Scroll offset for viewing excluded paths
    pub scroll_offset: usize,
}

/// State for git log diff dialog
#[derive(Debug, Clone)]
pub struct GitLogDiffState {
    pub repo_path: PathBuf,
    pub project_name: String,
    pub log_entries: Vec<crate::ui::git_screen::GitLogEntry>,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub selected_commits: Vec<String>,
    pub visible_height: usize,
}

/// Clipboard operation type for Ctrl+C/X/V operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOperation {
    Copy,
    Cut,
}

/// Clipboard state for storing files to copy/move
#[derive(Debug, Clone)]
pub struct Clipboard {
    pub files: Vec<String>,
    pub source_path: PathBuf,
    pub operation: ClipboardOperation,
    /// Remote profile of the source panel (None if local)
    pub source_remote_profile: Option<remote::RemoteProfile>,
}

/// File operation progress state for progress dialog
pub struct FileOperationProgress {
    pub operation_type: FileOperationType,
    pub is_active: bool,
    pub cancel_flag: Arc<AtomicBool>,
    pub receiver: Option<Receiver<ProgressMessage>>,

    // Preparation state
    pub is_preparing: bool,
    pub preparing_message: String,

    // Progress state
    pub current_file: String,
    pub current_file_progress: f64, // 0.0 ~ 1.0
    pub total_files: usize,
    pub completed_files: usize,
    pub total_bytes: u64,
    pub completed_bytes: u64,

    pub result: Option<FileOperationResult>,

    // Store last error before result is created
    last_error: Option<String>,

    // Timestamp when the operation started (for display delay)
    pub started_at: Instant,
}

impl FileOperationProgress {
    pub fn new(operation_type: FileOperationType) -> Self {
        Self {
            operation_type,
            is_active: false,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            receiver: None,
            is_preparing: false,
            preparing_message: String::new(),
            current_file: String::new(),
            current_file_progress: 0.0,
            total_files: 0,
            completed_files: 0,
            total_bytes: 0,
            completed_bytes: 0,
            result: None,
            last_error: None,
            started_at: Instant::now(),
        }
    }

    /// Cancel the ongoing operation
    pub fn cancel(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }

    /// Poll for progress messages. Returns true if still active.
    pub fn poll(&mut self) -> bool {
        if !self.is_active {
            return false;
        }

        if let Some(ref receiver) = self.receiver {
            // Process all available messages
            loop {
                match receiver.try_recv() {
                    Ok(msg) => {
                        match msg {
                            ProgressMessage::Preparing(message) => {
                                self.is_preparing = true;
                                self.preparing_message = message;
                            }
                            ProgressMessage::PrepareComplete => {
                                self.is_preparing = false;
                                self.preparing_message.clear();
                            }
                            ProgressMessage::FileStarted(name) => {
                                self.current_file = name;
                                self.current_file_progress = 0.0;
                            }
                            ProgressMessage::FileProgress(copied, total) => {
                                if total > 0 {
                                    self.current_file_progress = copied as f64 / total as f64;
                                }
                            }
                            ProgressMessage::FileCompleted(_) => {
                                self.current_file_progress = 1.0;
                            }
                            ProgressMessage::TotalProgress(
                                completed_files,
                                total_files,
                                completed_bytes,
                                total_bytes,
                            ) => {
                                self.completed_files = completed_files;
                                self.total_files = total_files;
                                self.completed_bytes = completed_bytes;
                                self.total_bytes = total_bytes;
                            }
                            ProgressMessage::Completed(success, failure) => {
                                self.result = Some(FileOperationResult {
                                    success_count: success,
                                    failure_count: failure,
                                    last_error: self.last_error.take(),
                                });
                                self.is_active = false;
                                return false;
                            }
                            ProgressMessage::Error(_, err) => {
                                // Store error for later (result is created on Completed)
                                self.last_error = Some(err);
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        break;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.is_active = false;
                        return false;
                    }
                }
            }
        }

        self.is_active
    }

    /// Get overall progress as percentage (0.0 ~ 1.0)
    /// Incorporates partial progress of the currently transferring file
    pub fn overall_progress(&self) -> f64 {
        if self.total_bytes > 0 {
            self.completed_bytes as f64 / self.total_bytes as f64
        } else if self.total_files > 0 {
            (self.completed_files as f64 + self.current_file_progress) / self.total_files as f64
        } else {
            0.0
        }
    }
}

/// What to do after a remote file download completes
pub enum PendingRemoteOpen {
    /// Open in editor (with remote upload on save)
    Editor {
        tmp_path: PathBuf,
        panel_index: usize,
        remote_path: String,
    },
    /// Open in image viewer
    ImageViewer { tmp_path: PathBuf },
}

#[derive(Debug, Clone, Default)]
pub struct PathCompletion {
    pub suggestions: Vec<String>, // 자동완성 후보 목록
    pub selected_index: usize,    // 선택된 후보 인덱스
    pub visible: bool,            // 목록 표시 여부
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub dialog_type: DialogType,
    pub input: String,
    pub cursor_pos: usize, // 커서 위치 (문자 인덱스)
    pub message: String,
    pub completion: Option<PathCompletion>, // 경로 자동완성용
    pub selected_button: usize,             // 버튼 선택 인덱스 (0: Yes, 1: No)
    pub selection: Option<(usize, usize)>,  // 선택 범위 (start, end) - None이면 선택 없음
    pub use_md5: bool,                      // MD5 검증 옵션 (EncryptConfirm에서 사용)
}

#[derive(Debug, Clone)]
pub struct FileItem {
    pub name: String,
    /// Original filename read from .cokacenc header (plaintext, no decryption needed)
    pub display_name: Option<String>,
    pub is_directory: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: DateTime<Local>,
    #[allow(dead_code)]
    pub permissions: String,
}

/// Parse sort_by string from settings to SortBy enum
pub fn parse_sort_by(s: &str) -> SortBy {
    match s.to_lowercase().as_str() {
        "type" => SortBy::Type,
        "size" => SortBy::Size,
        "modified" | "date" => SortBy::Modified,
        _ => SortBy::Name,
    }
}

/// Parse sort_order string from settings to SortOrder enum
pub fn parse_sort_order(s: &str) -> SortOrder {
    match s.to_lowercase().as_str() {
        "desc" => SortOrder::Desc,
        _ => SortOrder::Asc,
    }
}

/// Convert SortBy enum to string for settings
pub fn sort_by_to_string(sort_by: SortBy) -> String {
    match sort_by {
        SortBy::Name => "name".to_string(),
        SortBy::Type => "type".to_string(),
        SortBy::Size => "size".to_string(),
        SortBy::Modified => "modified".to_string(),
    }
}

/// Convert SortOrder enum to string for settings
pub fn sort_order_to_string(sort_order: SortOrder) -> String {
    match sort_order {
        SortOrder::Asc => "asc".to_string(),
        SortOrder::Desc => "desc".to_string(),
    }
}

/// Remote operation spinner — shows a spinning indicator while a remote operation runs in background
pub struct RemoteSpinner {
    pub message: String,
    pub started_at: Instant,
    pub receiver: Receiver<RemoteSpinnerResult>,
}

/// Result from a background remote operation
pub enum RemoteSpinnerResult {
    /// Operation on an existing connection (ctx returned)
    PanelOp {
        ctx: Box<RemoteContext>,
        panel_idx: usize,
        outcome: PanelOpOutcome,
    },
    /// New connection completed
    Connected {
        result: Result<ConnectSuccess, String>,
        panel_idx: usize,
    },
    /// Local background operation completed (no remote ctx)
    LocalOp {
        message: Result<String, String>,
        reload: bool,
    },
    /// Search completed
    SearchComplete {
        results: Vec<crate::ui::search_result::SearchResultItem>,
        search_term: String,
        base_path: PathBuf,
    },
    /// Git log diff preparation completed
    GitDiffComplete {
        result: Result<(PathBuf, PathBuf), String>,
    },
}

/// Outcome variants for panel operations
pub enum PanelOpOutcome {
    /// mkdir, mkfile, rename, remove, upload → reload needed
    Simple {
        message: Result<String, String>,
        pending_focus: Option<String>,
        reload: bool,
    },
    /// list_dir result
    ListDir {
        entries: Result<Vec<SftpFileEntry>, String>,
        path: PathBuf,
        /// Previous path for rollback on failure (None = refresh, no rollback needed)
        old_path: Option<PathBuf>,
    },
    /// dir_exists result
    DirExists { exists: bool, target_entry: String },
}

/// Successful connection data
pub struct ConnectSuccess {
    pub ctx: Box<RemoteContext>,
    pub entries: Vec<SftpFileEntry>,
    pub path: String,
    pub fallback_msg: Option<String>,
    pub profile: RemoteProfile,
}
