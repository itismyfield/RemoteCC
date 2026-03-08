use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) fn remotecc_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".remotecc"))
}

pub(super) fn runtime_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("runtime"))
}

pub(super) fn workspace_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("workspace"))
}

pub(super) fn worktrees_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("worktrees"))
}

pub(super) fn bot_settings_path() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("bot_settings.json"))
}

pub(super) fn role_map_path() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("role_map.json"))
}

pub(super) fn discord_uploads_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_uploads"))
}

pub(super) fn discord_inflight_root() -> Option<PathBuf> {
    runtime_root().map(|root| root.join("discord_inflight"))
}

pub(super) fn shared_agent_memory_root() -> Option<PathBuf> {
    remotecc_root().map(|root| root.join("shared_agent_memory"))
}

pub(super) fn atomic_write(path: &Path, data: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(|e| e.to_string())?;
    file.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}
