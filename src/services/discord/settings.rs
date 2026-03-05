use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use serenity::ChannelId;

use poise::serenity_prelude as serenity;

use crate::services::claude::DEFAULT_ALLOWED_TOOLS;

use super::DiscordBotSettings;

/// Compute a short hash key from the bot token (first 16 chars of SHA-256 hex)
/// Uses "discord_" prefix to namespace Discord bot entries in settings.
pub(super) fn discord_token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    format!("discord_{}", hex::encode(&result[..8]))
}

/// Path to bot settings file: ~/.remotecc/bot_settings.json
pub(super) fn bot_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".remotecc").join("bot_settings.json"))
}

/// Path to role map file: ~/.remotecc/role_map.json
pub(super) fn role_map_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".remotecc").join("role_map.json"))
}

#[derive(Clone, Debug)]
pub(super) struct RoleBinding {
    pub role_id: String,
    pub prompt_file: String,
}

pub(super) fn parse_role_binding(v: &serde_json::Value) -> Option<RoleBinding> {
    let obj = v.as_object()?;
    let role_id = obj.get("roleId")?.as_str()?.to_string();
    let prompt_file = obj.get("promptFile")?.as_str()?.to_string();
    Some(RoleBinding {
        role_id,
        prompt_file,
    })
}

pub(super) fn resolve_role_binding(channel_id: ChannelId, channel_name: Option<&str>) -> Option<RoleBinding> {
    let path = role_map_path()?;
    let content = fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // 1) Primary: exact channel ID match
    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(binding) = by_id.get(&key).and_then(parse_role_binding) {
            return Some(binding);
        }
    }

    // 2) Optional fallback: exact channel name match
    let fallback_enabled = json
        .get("fallbackByChannelName")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !fallback_enabled {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    by_name.get(cname).and_then(parse_role_binding)
}

pub(super) fn load_role_prompt(binding: &RoleBinding) -> Option<String> {
    let raw = fs::read_to_string(Path::new(&binding.prompt_file)).ok()?;
    const MAX_CHARS: usize = 12_000;
    if raw.chars().count() <= MAX_CHARS {
        return Some(raw);
    }
    let truncated: String = raw.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

pub(super) fn discord_uploads_root() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".openclaw").join("remotecc_uploads").join("discord"))
}

pub(super) fn channel_upload_dir(channel_id: ChannelId) -> Option<std::path::PathBuf> {
    discord_uploads_root().map(|p| p.join(channel_id.get().to_string()))
}

pub(super) fn cleanup_old_uploads(max_age: Duration) {
    let Some(root) = discord_uploads_root() else {
        return;
    };
    if !root.exists() {
        return;
    }

    let now = SystemTime::now();
    let Ok(channels) = fs::read_dir(&root) else {
        return;
    };

    for ch in channels.filter_map(|e| e.ok()) {
        let ch_path = ch.path();
        if !ch_path.is_dir() {
            continue;
        }

        let Ok(files) = fs::read_dir(&ch_path) else {
            continue;
        };

        for f in files.filter_map(|e| e.ok()) {
            let f_path = f.path();
            if !f_path.is_file() {
                continue;
            }

            let should_delete = fs::metadata(&f_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);

            if should_delete {
                let _ = fs::remove_file(&f_path);
            }
        }

        // Remove empty channel dir
        if fs::read_dir(&ch_path)
            .ok()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&ch_path);
        }
    }
}

pub(super) fn cleanup_channel_uploads(channel_id: ChannelId) {
    if let Some(dir) = channel_upload_dir(channel_id) {
        let _ = fs::remove_dir_all(dir);
    }
}

/// Load Discord bot settings from bot_settings.json
pub(super) fn load_bot_settings(token: &str) -> DiscordBotSettings {
    let Some(path) = bot_settings_path() else {
        return DiscordBotSettings::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return DiscordBotSettings::default();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return DiscordBotSettings::default();
    };
    let key = discord_token_hash(token);
    let Some(entry) = json.get(&key) else {
        return DiscordBotSettings::default();
    };
    let owner_user_id = entry.get("owner_user_id").and_then(|v| v.as_u64());
    let Some(tools_arr) = entry.get("allowed_tools").and_then(|v| v.as_array()) else {
        return DiscordBotSettings {
            owner_user_id,
            ..DiscordBotSettings::default()
        };
    };
    let tools: Vec<String> = tools_arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if tools.is_empty() {
        return DiscordBotSettings {
            owner_user_id,
            ..DiscordBotSettings::default()
        };
    }
    let last_sessions = entry
        .get("last_sessions")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let last_remotes = entry
        .get("last_remotes")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let allowed_user_ids = entry
        .get("allowed_user_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default();
    DiscordBotSettings {
        allowed_tools: tools,
        last_sessions,
        last_remotes,
        owner_user_id,
        allowed_user_ids,
    }
}

/// Save Discord bot settings to bot_settings.json
pub(super) fn save_bot_settings(token: &str, settings: &DiscordBotSettings) {
    let Some(path) = bot_settings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut json: serde_json::Value = if let Ok(content) = fs::read_to_string(&path) {
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    let key = discord_token_hash(token);
    let mut entry = serde_json::json!({
        "token": token,
        "allowed_tools": settings.allowed_tools,
        "last_sessions": settings.last_sessions,
        "last_remotes": settings.last_remotes,
        "allowed_user_ids": settings.allowed_user_ids,
    });
    if let Some(owner_id) = settings.owner_user_id {
        entry["owner_user_id"] = serde_json::json!(owner_id);
    }
    json[key] = entry;
    if let Ok(s) = serde_json::to_string_pretty(&json) {
        let _ = fs::write(&path, s);
    }
}

/// Resolve a Discord bot token from its hash by searching bot_settings.json
pub fn resolve_discord_token_by_hash(hash: &str) -> Option<String> {
    let path = bot_settings_path()?;
    let content = fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = json.as_object()?;
    let entry = obj.get(hash)?;
    entry
        .get("token")
        .and_then(|v| v.as_str())
        .map(String::from)
}
