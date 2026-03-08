use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::services::provider::ProviderKind;
use super::runtime_store::{atomic_write, discord_inflight_root};

const INFLIGHT_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InflightTurnState {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    pub request_owner_user_id: u64,
    pub user_msg_id: u64,
    pub current_msg_id: u64,
    pub current_msg_len: usize,
    pub user_text: String,
    pub session_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub output_path: Option<String>,
    pub input_fifo_path: Option<String>,
    pub last_offset: u64,
    pub full_response: String,
    pub response_sent_offset: usize,
    pub started_at: String,
    pub updated_at: String,
}

impl InflightTurnState {
    pub fn new(
        provider: ProviderKind,
        channel_id: u64,
        channel_name: Option<String>,
        request_owner_user_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        user_text: String,
        session_id: Option<String>,
        tmux_session_name: Option<String>,
        output_path: Option<String>,
        input_fifo_path: Option<String>,
        last_offset: u64,
    ) -> Self {
        let now = now_string();
        Self {
            version: INFLIGHT_STATE_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            channel_name,
            request_owner_user_id,
            user_msg_id,
            current_msg_id,
            current_msg_len: 0,
            user_text,
            session_id,
            tmux_session_name,
            output_path,
            input_fifo_path,
            last_offset,
            full_response: String::new(),
            response_sent_offset: 0,
            started_at: now.clone(),
            updated_at: now,
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

pub(super) fn inflight_runtime_root() -> Option<PathBuf> {
    discord_inflight_root()
}

fn inflight_provider_dir(root: &Path, provider: ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

fn inflight_state_path(root: &Path, provider: ProviderKind, channel_id: u64) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(super) fn save_inflight_state(state: &InflightTurnState) -> Result<(), String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_in_root(&root, state)
}

fn save_inflight_state_in_root(root: &Path, state: &InflightTurnState) -> Result<(), String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

pub(super) fn clear_inflight_state(provider: ProviderKind, channel_id: u64) {
    let Some(root) = inflight_runtime_root() else {
        return;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let _ = fs::remove_file(path);
}

pub(super) fn load_inflight_states(provider: ProviderKind) -> Vec<InflightTurnState> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    load_inflight_states_from_root(&root, provider)
}

fn load_inflight_states_from_root(root: &Path, provider: ProviderKind) -> Vec<InflightTurnState> {
    let dir = inflight_provider_dir(root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut states = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            continue;
        };
        if state.provider_kind() != Some(provider) {
            continue;
        }
        states.push(state);
    }
    states
}

#[cfg(test)]
mod tests {
    use super::{load_inflight_states_from_root, save_inflight_state_in_root, InflightTurnState};
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_inflight_state() {
        let temp = TempDir::new().unwrap();

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("remotecc-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("remoteCC-codex-remotecc-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 123);
        assert_eq!(loaded[0].current_msg_id, 999);
        assert_eq!(loaded[0].last_offset, 42);
    }
}
