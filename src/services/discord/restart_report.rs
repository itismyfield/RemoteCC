use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serde::{Deserialize, Serialize};

use super::formatting::send_long_message_raw;
use super::runtime_store::{atomic_write, discord_restart_reports_root};
use super::SharedData;
use crate::services::provider::ProviderKind;

const RESTART_REPORT_VERSION: u32 = 1;
const PENDING_REPORT_GRACE: Duration = Duration::from_secs(3);
pub(crate) const RESTART_REPORT_CHANNEL_ENV: &str = "REMOTECC_REPORT_CHANNEL_ID";
pub(crate) const RESTART_REPORT_PROVIDER_ENV: &str = "REMOTECC_REPORT_PROVIDER";

#[derive(Debug, Clone)]
pub(crate) struct RestartReportContext {
    pub provider: ProviderKind,
    pub channel_id: u64,
    pub current_msg_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestartCompletionReport {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    #[serde(default)]
    pub current_msg_id: Option<u64>,
    pub status: String,
    pub summary: String,
    pub completed_at: String,
}

impl RestartCompletionReport {
    pub(crate) fn new(
        provider: ProviderKind,
        channel_id: u64,
        status: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            version: RESTART_REPORT_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            current_msg_id: None,
            status: status.into(),
            summary: summary.into(),
            completed_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        }
    }

    pub(crate) fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

pub(crate) fn restart_report_context_from_env() -> Option<RestartReportContext> {
    let provider = std::env::var(RESTART_REPORT_PROVIDER_ENV).ok()?;
    let provider = ProviderKind::from_str(&provider)?;
    let channel_id = std::env::var(RESTART_REPORT_CHANNEL_ENV).ok()?;
    let channel_id = channel_id.parse::<u64>().ok()?;
    Some(RestartReportContext {
        provider,
        channel_id,
        current_msg_id: None,
    })
}

fn restart_reports_root() -> Option<PathBuf> {
    discord_restart_reports_root()
}

fn restart_provider_dir(root: &Path, provider: ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

fn restart_report_path(root: &Path, provider: ProviderKind, channel_id: u64) -> PathBuf {
    restart_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

pub(crate) fn save_restart_report(report: &RestartCompletionReport) -> Result<(), String> {
    let Some(root) = restart_reports_root() else {
        return Err("Home directory not found".to_string());
    };
    save_restart_report_in_root(&root, report)?;
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 📝 Saved restart follow-up report for provider {} channel {}",
        report.provider, report.channel_id
    );
    Ok(())
}

fn save_restart_report_in_root(
    root: &Path,
    report: &RestartCompletionReport,
) -> Result<(), String> {
    let Some(provider) = report.provider_kind() else {
        return Err(format!("Unknown provider '{}'", report.provider));
    };
    let path = restart_report_path(root, provider, report.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

pub(crate) fn clear_restart_report(provider: ProviderKind, channel_id: u64) {
    let Some(root) = restart_reports_root() else {
        return;
    };
    let path = restart_report_path(&root, provider, channel_id);
    let _ = fs::remove_file(path);
}

pub(crate) fn load_restart_reports(provider: ProviderKind) -> Vec<RestartCompletionReport> {
    let Some(root) = restart_reports_root() else {
        return Vec::new();
    };
    load_restart_reports_in_root(&root, provider)
}

pub(crate) fn load_restart_report(
    provider: ProviderKind,
    channel_id: u64,
) -> Option<RestartCompletionReport> {
    let root = restart_reports_root()?;
    let path = restart_report_path(&root, provider, channel_id);
    let content = fs::read_to_string(path).ok()?;
    let report = serde_json::from_str::<RestartCompletionReport>(&content).ok()?;
    (report.provider_kind() == Some(provider)).then_some(report)
}

fn load_restart_reports_in_root(
    root: &Path,
    provider: ProviderKind,
) -> Vec<RestartCompletionReport> {
    let dir = restart_provider_dir(&root, provider);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ restart report dir unreadable for provider {}: {} ({})",
                provider.as_str(),
                dir.display(),
                err
            );
            return Vec::new();
        }
    };

    let mut reports = Vec::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ failed to read restart report file: {}",
                path.display()
            );
            continue;
        };
        let Ok(report) = serde_json::from_str::<RestartCompletionReport>(&content) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ failed to parse restart report file: {}",
                path.display()
            );
            continue;
        };
        if report.provider_kind() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ restart report provider mismatch in {}: expected {}, found {}",
                path.display(),
                provider.as_str(),
                report.provider
            );
            continue;
        }
        reports.push(report);
    }
    reports
}

fn report_age(report: &RestartCompletionReport) -> Option<Duration> {
    let created_at =
        chrono::NaiveDateTime::parse_from_str(&report.completed_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let now = chrono::Local::now().naive_local();
    let delta = now.signed_duration_since(created_at);
    delta.to_std().ok()
}

pub(super) async fn flush_restart_reports(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: ProviderKind,
) {
    let reports = load_restart_reports(provider);
    if reports.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 📬 Found {} pending restart follow-up report(s) for provider {}",
        reports.len(),
        provider.as_str()
    );

    for report in reports {
        let channel_id = serenity::ChannelId::new(report.channel_id);
        if report.status == "pending" {
            let age = report_age(&report).unwrap_or_default();
            if age < PENDING_REPORT_GRACE {
                if let Some(message_id) = report.current_msg_id {
                    let provisional_text = format!(
                        "♻️ dcserver restart 진행 중\n- status: `restarting`\n- updated_at: `{}`\n- 원래 답변은 재시작으로 잠시 끊겼습니다.\n- 새 dcserver가 이 메시지에서 마무리를 이어받는 중입니다.\n",
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    );
                    let _ = channel_id
                        .edit_message(
                            http,
                            serenity::MessageId::new(message_id),
                            serenity::EditMessage::new().content(&provisional_text),
                        )
                        .await;
                }
                continue;
            }
        }

        let (status_badge, status_label, summary, completed_at_display) = match report.status.as_str() {
            "ok" => (
                "✅",
                "ok",
                report.summary.clone(),
                report.completed_at.clone(),
            ),
            "rolled_back" => (
                "⚠️",
                "rolled_back",
                report.summary.clone(),
                report.completed_at.clone(),
            ),
            "sigterm" => (
                "♻️",
                "recovered",
                format!(
                    "{}\n새 dcserver가 정상 시작되어 작업을 이어받았습니다.",
                    report.summary
                ),
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ),
            "pending" => (
                "⏳",
                "pending",
                report.summary.clone(),
                report.completed_at.clone(),
            ),
            _ => (
                "❌",
                "failed",
                report.summary.clone(),
                report.completed_at.clone(),
            ),
        };
        let text = format!(
            "{status_badge} dcserver restart follow-up\n- status: `{}`\n- completed_at: `{}`\n- {}\n",
            status_label, completed_at_display, summary
        );
        let mut sent = false;

        if let Some(message_id) = report.current_msg_id {
            match channel_id
                .edit_message(
                    http,
                    serenity::MessageId::new(message_id),
                    serenity::EditMessage::new().content(&text),
                )
                .await
            {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ✓ Flushed restart follow-up by editing message {} in channel {}",
                        message_id, report.channel_id
                    );
                    clear_restart_report(provider, report.channel_id);
                    continue;
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚠ restart follow-up edit failed for channel {} msg {}: {}",
                        report.channel_id, message_id, e
                    );
                }
            }
        }

        for attempt in 1..=5 {
            match send_long_message_raw(http, channel_id, &text, shared).await {
                Ok(()) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ✓ Flushed restart follow-up report for channel {} on attempt {}",
                        report.channel_id, attempt
                    );
                    clear_restart_report(provider, report.channel_id);
                    sent = true;
                    break;
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if attempt < 5 {
                        println!(
                            "  [{ts}] ⚠ failed to flush restart report for channel {} on attempt {}: {}",
                            report.channel_id, attempt, e
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    } else {
                        println!(
                            "  [{ts}] ❌ keeping restart report for channel {} after {} failed attempts: {}",
                            report.channel_id, attempt, e
                        );
                    }
                }
            }
        }

        if !sent {
            continue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        load_restart_reports_in_root, save_restart_report_in_root, RestartCompletionReport,
        RESTART_REPORT_VERSION,
    };
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_restart_report() {
        let temp = TempDir::new().unwrap();
        let report = RestartCompletionReport {
            version: RESTART_REPORT_VERSION,
            provider: "codex".to_string(),
            channel_id: 123,
            current_msg_id: Some(999),
            status: "ok".to_string(),
            summary: "ready".to_string(),
            completed_at: "2026-03-08 18:00:00".to_string(),
        };

        save_restart_report_in_root(temp.path(), &report).unwrap();
        let content = std::fs::read_to_string(temp.path().join("codex").join("123.json")).unwrap();
        let loaded: RestartCompletionReport = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.channel_id, 123);
        assert_eq!(loaded.status, "ok");
    }

    #[test]
    fn test_load_restart_reports_filters_provider() {
        let temp = TempDir::new().unwrap();

        save_restart_report_in_root(
            temp.path(),
            &RestartCompletionReport {
                version: RESTART_REPORT_VERSION,
                provider: "codex".to_string(),
                channel_id: 123,
                current_msg_id: Some(111),
                status: "ok".to_string(),
                summary: "codex-ready".to_string(),
                completed_at: "2026-03-08 19:00:00".to_string(),
            },
        )
        .unwrap();

        save_restart_report_in_root(
            temp.path(),
            &RestartCompletionReport {
                version: RESTART_REPORT_VERSION,
                provider: "claude".to_string(),
                channel_id: 456,
                current_msg_id: Some(222),
                status: "ok".to_string(),
                summary: "claude-ready".to_string(),
                completed_at: "2026-03-08 19:00:01".to_string(),
            },
        )
        .unwrap();

        let codex_reports = load_restart_reports_in_root(temp.path(), ProviderKind::Codex);
        assert_eq!(codex_reports.len(), 1);
        assert_eq!(codex_reports[0].channel_id, 123);

        let claude_reports = load_restart_reports_in_root(temp.path(), ProviderKind::Claude);
        assert_eq!(claude_reports.len(), 1);
        assert_eq!(claude_reports[0].channel_id, 456);
    }
}
