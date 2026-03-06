use crate::services::provider::ProviderKind;
use crate::services::{claude, codex};

pub async fn execute_simple(provider: ProviderKind, prompt: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || match provider {
        ProviderKind::Claude => claude::execute_command_simple(&prompt),
        ProviderKind::Codex => codex::execute_command_simple(&prompt),
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))?
}
