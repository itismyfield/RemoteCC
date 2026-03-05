use chrono::{DateTime, Local};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use crate::services::remote::{self, ConnectionStatus, RemoteContext, SftpFileEntry};

use super::state::*;

#[derive(Debug)]
pub struct PanelState {
    pub path: PathBuf,
    pub files: Vec<FileItem>,
    pub selected_index: usize,
    pub selected_files: HashSet<String>,
    pub sort_by: SortBy,
    pub sort_order: SortOrder,
    pub scroll_offset: usize,
    pub pending_focus: Option<String>,
    pub disk_total: u64,
    pub disk_available: u64,
    /// Remote context — None means local panel
    pub remote_ctx: Option<Box<RemoteContext>>,
    /// Cached remote display info (user, host, port) — survives while remote_ctx is temporarily taken
    pub remote_display: Option<(String, String, u16)>,
}

impl PanelState {
    pub fn new(path: PathBuf) -> Self {
        // Validate path and get a valid one
        let fallback = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let valid_path = get_valid_path(&path, &fallback);

        let mut state = Self {
            path: valid_path,
            files: Vec::new(),
            selected_index: 0,
            selected_files: HashSet::new(),
            sort_by: SortBy::Name,
            sort_order: SortOrder::Asc,
            scroll_offset: 0,
            pending_focus: None,
            disk_total: 0,
            disk_available: 0,
            remote_ctx: None,
            remote_display: None,
        };
        state.load_files();
        state
    }

    /// Create a PanelState with settings from config
    pub fn with_settings(path: PathBuf, panel_settings: &crate::config::PanelSettings) -> Self {
        // Validate path and get a valid one
        let fallback = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let valid_path = get_valid_path(&path, &fallback);

        let sort_by = parse_sort_by(&panel_settings.sort_by);
        let sort_order = parse_sort_order(&panel_settings.sort_order);

        let mut state = Self {
            path: valid_path,
            files: Vec::new(),
            selected_index: 0,
            selected_files: HashSet::new(),
            sort_by,
            sort_order,
            scroll_offset: 0,
            pending_focus: None,
            disk_total: 0,
            disk_available: 0,
            remote_ctx: None,
            remote_display: None,
        };
        state.load_files();
        state
    }

    /// Check if this panel is connected to a remote server
    pub fn is_remote(&self) -> bool {
        self.remote_ctx.is_some() || self.remote_display.is_some()
    }

    /// Get the remote display path (user@host:/path) or local path string
    pub fn display_path(&self) -> String {
        if let Some(ref ctx) = self.remote_ctx {
            remote::format_remote_display(&ctx.profile, &self.path.display().to_string())
        } else if let Some((ref user, ref host, port)) = self.remote_display {
            let path = self.path.display().to_string();
            if port != 22 {
                format!("{}@{}:{}:{}", user, host, port, path)
            } else {
                format!("{}@{}:{}", user, host, path)
            }
        } else {
            self.path.display().to_string()
        }
    }

    pub fn load_files(&mut self) {
        if self.is_remote() {
            self.load_files_remote();
        } else {
            self.load_files_local();
        }
    }

    fn load_files_local(&mut self) {
        self.files.clear();

        // Add parent directory entry if not at root
        if self.path.parent().is_some() {
            self.files.push(FileItem {
                name: "..".to_string(),
                display_name: None,
                is_directory: true,
                is_symlink: false,
                size: 0,
                modified: Local::now(),
                permissions: String::new(),
            });
        }

        if let Ok(entries) = fs::read_dir(&self.path) {
            // Estimate capacity based on typical directory size
            let entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            let mut items: Vec<FileItem> = Vec::with_capacity(entries.len());

            items.extend(entries.into_iter().filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                let path = entry.path();

                // Check if it's a symlink first
                let symlink_meta = fs::symlink_metadata(&path).ok()?;
                let is_symlink = symlink_meta.is_symlink();

                // For symlinks, follow to get target type; for others, use direct metadata
                let metadata = if is_symlink {
                    fs::metadata(&path).ok().unwrap_or(symlink_meta.clone())
                } else {
                    symlink_meta.clone()
                };

                let is_directory = metadata.is_dir();
                let size = if is_directory { 0 } else { metadata.len() };
                let modified = metadata
                    .modified()
                    .ok()
                    .map(DateTime::<Local>::from)
                    .unwrap_or_else(Local::now);

                #[cfg(unix)]
                let permissions = {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = symlink_meta.permissions().mode();
                    crate::utils::format::format_permissions_short(mode)
                };
                #[cfg(not(unix))]
                let permissions = String::new();

                let display_name = if !is_directory && name.ends_with(crate::enc::naming::EXT) {
                    std::fs::File::open(&path)
                        .ok()
                        .and_then(|f| {
                            let mut reader = std::io::BufReader::new(f);
                            crate::enc::crypto::read_header(&mut reader).ok()
                        })
                        .map(|(_, _, hdr_name)| hdr_name)
                } else {
                    None
                };

                Some(FileItem {
                    name,
                    display_name,
                    is_directory,
                    is_symlink,
                    size,
                    modified,
                    permissions,
                })
            }));

            self.sort_items(&mut items);
            self.files.reserve(items.len());
            self.files.extend(items);
        }

        self.finalize_load();
        self.update_disk_info();
    }

    fn load_files_remote(&mut self) {
        self.files.clear();

        let remote_path = self.path.display().to_string();

        // Always add parent directory entry for remote paths
        if remote_path != "/" {
            self.files.push(FileItem {
                name: "..".to_string(),
                display_name: None,
                is_directory: true,
                is_symlink: false,
                size: 0,
                modified: Local::now(),
                permissions: String::new(),
            });
        }

        let entries = if let Some(ref ctx) = self.remote_ctx {
            ctx.session.list_dir(&remote_path)
        } else {
            return;
        };

        match entries {
            Ok(sftp_entries) => {
                let mut items: Vec<FileItem> = sftp_entries
                    .into_iter()
                    .map(|entry| FileItem {
                        name: entry.name,
                        display_name: None,
                        is_directory: entry.is_directory,
                        is_symlink: entry.is_symlink,
                        size: if entry.is_directory { 0 } else { entry.size },
                        modified: entry.modified,
                        permissions: entry.permissions,
                    })
                    .collect();

                self.sort_items(&mut items);
                self.files.reserve(items.len());
                self.files.extend(items);

                // Update connection status
                if let Some(ref mut ctx) = self.remote_ctx {
                    ctx.status = ConnectionStatus::Connected;
                }
            }
            Err(e) => {
                if let Some(ref mut ctx) = self.remote_ctx {
                    ctx.status = ConnectionStatus::Disconnected(e.to_string());
                }
            }
        }

        self.finalize_load();
        // No disk info for remote panels
        self.disk_total = 0;
        self.disk_available = 0;
    }

    /// Apply remote directory listing results (no network call)
    pub fn apply_remote_entries(&mut self, entries: Vec<SftpFileEntry>, path: &std::path::Path) {
        self.files.clear();
        self.path = path.to_path_buf();

        let remote_path = path.display().to_string();
        // Always add parent directory entry for remote paths
        if remote_path != "/" {
            self.files.push(FileItem {
                name: "..".to_string(),
                display_name: None,
                is_directory: true,
                is_symlink: false,
                size: 0,
                modified: Local::now(),
                permissions: String::new(),
            });
        }

        let mut items: Vec<FileItem> = entries
            .into_iter()
            .map(|entry| FileItem {
                name: entry.name,
                display_name: None,
                is_directory: entry.is_directory,
                is_symlink: entry.is_symlink,
                size: if entry.is_directory { 0 } else { entry.size },
                modified: entry.modified,
                permissions: entry.permissions,
            })
            .collect();

        self.sort_items(&mut items);
        self.files.reserve(items.len());
        self.files.extend(items);

        self.finalize_load();
        self.disk_total = 0;
        self.disk_available = 0;
    }

    /// Sort file items (shared between local and remote)
    fn sort_items(&self, items: &mut Vec<FileItem>) {
        items.sort_by(|a, b| {
            // Directories always first
            if a.is_directory && !b.is_directory {
                return std::cmp::Ordering::Less;
            }
            if !a.is_directory && b.is_directory {
                return std::cmp::Ordering::Greater;
            }

            let cmp = match self.sort_by {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Type => {
                    let ext_a = std::path::Path::new(&a.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let ext_b = std::path::Path::new(&b.name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    ext_a.cmp(&ext_b)
                }
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Modified => a.modified.cmp(&b.modified),
            };

            match self.sort_order {
                SortOrder::Asc => cmp,
                SortOrder::Desc => cmp.reverse(),
            }
        });
    }

    /// Finalize file loading (handle focus and bounds)
    fn finalize_load(&mut self) {
        // Handle pending focus (when going to parent directory)
        if let Some(focus_name) = self.pending_focus.take() {
            if let Some(idx) = self.files.iter().position(|f| f.name == focus_name) {
                self.selected_index = idx;
            }
        }

        // Ensure selected_index is within bounds
        if self.selected_index >= self.files.len() && !self.files.is_empty() {
            self.selected_index = self.files.len() - 1;
        }
    }

    fn update_disk_info(&mut self) {
        if self.is_remote() {
            self.disk_total = 0;
            self.disk_available = 0;
            return;
        }

        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::mem::MaybeUninit;

            if let Some(path_str) = self.path.to_str() {
                if let Ok(c_path) = CString::new(path_str) {
                    let mut stat: MaybeUninit<libc::statvfs> = MaybeUninit::uninit();
                    // SAFETY: statvfs is a standard POSIX function, c_path is valid
                    let result = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
                    if result == 0 {
                        // SAFETY: statvfs succeeded, stat is initialized
                        let stat = unsafe { stat.assume_init() };
                        self.disk_total = stat.f_blocks as u64 * stat.f_frsize as u64;
                        self.disk_available = stat.f_bavail as u64 * stat.f_frsize as u64;
                        return;
                    }
                }
            }
        }
        self.disk_total = 0;
        self.disk_available = 0;
    }

    pub fn current_file(&self) -> Option<&FileItem> {
        self.files.get(self.selected_index)
    }

    pub fn toggle_sort(&mut self, sort_by: SortBy) {
        if self.sort_by == sort_by {
            self.sort_order = match self.sort_order {
                SortOrder::Asc => SortOrder::Desc,
                SortOrder::Desc => SortOrder::Asc,
            };
        } else {
            self.sort_by = sort_by;
            self.sort_order = SortOrder::Asc;
        }
        self.selected_index = 0;
        if self.is_remote() {
            // Re-sort existing items locally (no network call)
            let mut items: Vec<FileItem> =
                self.files.drain(..).filter(|f| f.name != "..").collect();
            // Re-add ".." entry
            let remote_path = self.path.display().to_string();
            if remote_path != "/" {
                self.files.push(FileItem {
                    name: "..".to_string(),
                    display_name: None,
                    is_directory: true,
                    is_symlink: false,
                    size: 0,
                    modified: Local::now(),
                    permissions: String::new(),
                });
            }
            self.sort_items(&mut items);
            self.files.reserve(items.len());
            self.files.extend(items);
            self.finalize_load();
        } else {
            self.load_files();
        }
    }
}
