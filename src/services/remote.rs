use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::runtime::Runtime;

use russh::keys::*;
use russh::*;
use russh_sftp::client::SftpSession as RusshSftpSession;

use crate::error::{AppError, AppResult};

// Legacy XOR obfuscation key (kept for backward-compatible reading of "enc:" values)
const OBFUSCATION_KEY: &[u8] = b"remotecc_remote_v1_key";

// Fixed app-specific salt for password storage key derivation (AES-256)
const PASSWORD_STORAGE_SALT: &[u8; 16] = b"rmtcc_pwd_store!";
// Fixed app-specific passphrase for password storage key derivation
const PASSWORD_STORAGE_PASSPHRASE: &[u8] = b"remotecc_aes256_password_storage_v1";

/// Encrypt a string for storage using AES-256-CBC (prefixed with "aes:")
pub fn obfuscate(plaintext: &str) -> String {
    use aes::Aes256;
    use base64::Engine;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    use rand::RngCore;

    let key = crate::enc::crypto::derive_key(PASSWORD_STORAGE_PASSPHRASE, PASSWORD_STORAGE_SALT);
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;
    let mut encryptor = Aes256CbcEnc::new(&key.into(), &iv.into());

    let plaintext_bytes = plaintext.as_bytes();
    // Manual PKCS7 padding
    let pad_len = 16 - (plaintext_bytes.len() % 16);
    let mut buf = Vec::with_capacity(plaintext_bytes.len() + pad_len);
    buf.extend_from_slice(plaintext_bytes);
    buf.resize(plaintext_bytes.len() + pad_len, pad_len as u8);

    // Encrypt in-place using block API
    fn to_blocks_mut(data: &mut [u8]) -> &mut [aes::Block] {
        assert!(data.len() % 16 == 0);
        unsafe {
            std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut aes::Block, data.len() / 16)
        }
    }
    encryptor.encrypt_blocks_mut(to_blocks_mut(&mut buf));

    // Format: aes:<base64(iv + ciphertext)>
    let mut combined = Vec::with_capacity(16 + buf.len());
    combined.extend_from_slice(&iv);
    combined.extend_from_slice(&buf);
    format!(
        "aes:{}",
        base64::engine::general_purpose::STANDARD.encode(&combined)
    )
}

/// Decrypt a stored string. Supports "aes:" (AES-256-CBC), "enc:" (legacy XOR), and plaintext fallback.
pub fn deobfuscate(stored: &str) -> String {
    if let Some(encoded) = stored.strip_prefix("aes:") {
        use aes::Aes256;
        use base64::Engine;
        use cbc::cipher::{BlockDecryptMut, KeyIvInit};

        if let Ok(combined) = base64::engine::general_purpose::STANDARD.decode(encoded) {
            if combined.len() >= 32 && (combined.len() - 16) % 16 == 0 {
                let iv: [u8; 16] = combined[..16].try_into().expect("iv slice");
                let ciphertext = &combined[16..];
                let key = crate::enc::crypto::derive_key(
                    PASSWORD_STORAGE_PASSPHRASE,
                    PASSWORD_STORAGE_SALT,
                );

                type Aes256CbcDec = cbc::Decryptor<Aes256>;
                let mut decryptor = Aes256CbcDec::new(&key.into(), &iv.into());

                let mut buf = ciphertext.to_vec();
                fn to_blocks_mut(data: &mut [u8]) -> &mut [aes::Block] {
                    assert!(data.len() % 16 == 0);
                    unsafe {
                        std::slice::from_raw_parts_mut(
                            data.as_mut_ptr() as *mut aes::Block,
                            data.len() / 16,
                        )
                    }
                }
                decryptor.decrypt_blocks_mut(to_blocks_mut(&mut buf));

                // Remove PKCS7 padding
                if let Some(&pad_byte) = buf.last() {
                    let pad_len = pad_byte as usize;
                    if pad_len >= 1
                        && pad_len <= 16
                        && buf.len() >= pad_len
                        && buf[buf.len() - pad_len..].iter().all(|&b| b == pad_byte)
                    {
                        buf.truncate(buf.len() - pad_len);
                        if let Ok(s) = String::from_utf8(buf) {
                            return s;
                        }
                    }
                }
            }
        }
        return stored.to_string(); // AES decode failed, return as-is
    }

    if let Some(encoded) = stored.strip_prefix("enc:") {
        // Legacy XOR deobfuscation (read-only backward compatibility)
        use base64::Engine;
        if let Ok(xored) = base64::engine::general_purpose::STANDARD.decode(encoded) {
            let plain: Vec<u8> = xored
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()])
                .collect();
            return String::from_utf8(plain).unwrap_or_else(|_| stored.to_string());
        }
    }
    // Fallback: treat as plaintext (backward compatibility)
    stored.to_string()
}

mod obfuscated_string {
    use super::{deobfuscate, obfuscate};
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&obfuscate(value))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(deobfuscate(&s))
    }
}

mod obfuscated_option_string {
    use super::{deobfuscate, obfuscate};
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(v) => serializer.serialize_some(&obfuscate(v)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        Ok(opt.map(|s| deobfuscate(&s)))
    }
}

/// Remote authentication method
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RemoteAuth {
    #[serde(rename = "password")]
    Password {
        #[serde(with = "obfuscated_string")]
        password: String,
    },
    #[serde(rename = "key_file")]
    KeyFile {
        path: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "obfuscated_option_string"
        )]
        passphrase: Option<String>,
    },
}

/// Remote server profile stored in settings.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProfile {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub auth: RemoteAuth,
    #[serde(default)]
    pub default_path: String,
    /// Path to claude binary on the remote host (default: "claude")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_path: Option<String>,
}

fn default_port() -> u16 {
    22
}

/// File entry from SFTP directory listing
#[derive(Debug, Clone)]
pub struct SftpFileEntry {
    pub name: String,
    pub is_directory: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: DateTime<Local>,
    pub permissions: String,
}

/// Connection status
#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Connected,
    Disconnected(String),
}

/// Remote context attached to a panel
pub struct RemoteContext {
    pub profile: RemoteProfile,
    pub session: SftpSession,
    pub status: ConnectionStatus,
}

impl std::fmt::Debug for RemoteContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteContext")
            .field("profile", &self.profile)
            .field("status", &self.status)
            .finish()
    }
}

/// SSH client handler for russh
pub(crate) struct SshHandler;

#[async_trait::async_trait]
impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Accept all server keys (like ssh -o StrictHostKeyChecking=no)
        // In a production app, you'd verify against known_hosts
        Ok(true)
    }
}

/// SFTP session wrapper around russh
pub struct SftpSession {
    runtime: Runtime,
    ssh_handle: Option<client::Handle<SshHandler>>,
    sftp: Option<RusshSftpSession>,
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpSession")
            .field("connected", &self.sftp.is_some())
            .finish()
    }
}

impl SftpSession {
    /// Connect to remote host via SSH and open SFTP channel
    pub fn connect(profile: &RemoteProfile) -> AppResult<Self> {
        let runtime = Runtime::new()?;

        let profile = profile.clone();
        let (ssh_handle, sftp) = runtime.block_on(async { Self::connect_async(&profile).await })?;

        Ok(Self {
            runtime,
            ssh_handle: Some(ssh_handle),
            sftp: Some(sftp),
        })
    }

    async fn connect_async(
        profile: &RemoteProfile,
    ) -> AppResult<(client::Handle<SshHandler>, RusshSftpSession)> {
        let config = client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(300)),
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        };

        let mut ssh = client::connect(
            Arc::new(config),
            (profile.host.as_str(), profile.port),
            SshHandler,
        )
        .await
        .map_err(|e| AppError::Ssh(format!("SSH connection failed: {}", e)))?;

        // Authenticate
        let auth_result = match &profile.auth {
            RemoteAuth::Password { password } => ssh
                .authenticate_password(&profile.user, password)
                .await
                .map_err(|e| AppError::Ssh(format!("Password auth failed: {}", e)))?,
            RemoteAuth::KeyFile { path, passphrase } => {
                let key_path = if path.starts_with('~') {
                    if let Some(home) = dirs::home_dir() {
                        home.join(path.trim_start_matches('~').trim_start_matches('/'))
                    } else {
                        std::path::PathBuf::from(path)
                    }
                } else {
                    std::path::PathBuf::from(path)
                };

                let key_pair = if let Some(pass) = passphrase {
                    russh_keys::load_secret_key(&key_path, Some(pass))
                        .map_err(|e| AppError::Ssh(format!("Failed to load key: {}", e)))?
                } else {
                    russh_keys::load_secret_key(&key_path, None)
                        .map_err(|e| AppError::Ssh(format!("Failed to load key: {}", e)))?
                };

                ssh.authenticate_publickey(&profile.user, Arc::new(key_pair))
                    .await
                    .map_err(|e| AppError::Ssh(format!("Key auth failed: {}", e)))?
            }
        };

        if !auth_result {
            return Err(AppError::Ssh("Authentication rejected by server".to_string()));
        }

        // Open SFTP channel
        let channel = ssh
            .channel_open_session()
            .await
            .map_err(|e| AppError::Ssh(format!("Failed to open channel: {}", e)))?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| AppError::Ssh(format!("Failed to request SFTP subsystem: {}", e)))?;

        let sftp = RusshSftpSession::new(channel.into_stream())
            .await
            .map_err(|e| AppError::Ssh(format!("Failed to init SFTP session: {}", e)))?;

        Ok((ssh, sftp))
    }

    /// Check if a remote directory exists
    pub fn dir_exists(&self, path: &str) -> bool {
        self.list_dir(path).is_ok()
    }

    /// List directory contents via SFTP
    pub fn list_dir(&self, path: &str) -> AppResult<Vec<SftpFileEntry>> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let path = path.to_string();

        self.runtime.block_on(async {
            let dir = sftp
                .read_dir(&path)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to read dir '{}': {}", path, e)))?;

            let mut entries = Vec::new();
            for entry in dir {
                let name = entry.file_name();
                // Skip . and ..
                if name == "." || name == ".." {
                    continue;
                }

                let attrs = entry.metadata();
                let is_directory = attrs.is_dir();
                let is_symlink = attrs.is_symlink();
                let size = attrs.size.unwrap_or(0);
                let modified = attrs
                    .mtime
                    .and_then(|t| Local.timestamp_opt(t as i64, 0).single())
                    .unwrap_or_else(Local::now);

                let permissions = if let Some(perm) = attrs.permissions {
                    format_remote_permissions(perm)
                } else {
                    String::new()
                };

                entries.push(SftpFileEntry {
                    name,
                    is_directory,
                    is_symlink,
                    size,
                    modified,
                    permissions,
                });
            }

            Ok(entries)
        })
    }

    /// Remove file or directory via SFTP
    pub fn remove(&self, path: &str, is_dir: bool) -> AppResult<()> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let path = path.to_string();

        self.runtime.block_on(async {
            if is_dir {
                Self::remove_dir_recursive(sftp, &path).await
            } else {
                sftp.remove_file(&path)
                    .await
                    .map_err(|e| AppError::Ssh(format!("Failed to remove '{}': {}", path, e)))
            }
        })
    }

    /// Recursively remove directory
    async fn remove_dir_recursive(sftp: &RusshSftpSession, path: &str) -> AppResult<()> {
        let entries = sftp
            .read_dir(path)
            .await
            .map_err(|e| AppError::Ssh(format!("Failed to read dir '{}': {}", path, e)))?;

        for entry in entries {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child_path = format!("{}/{}", path.trim_end_matches('/'), name);
            let attrs = entry.metadata();
            if attrs.is_dir() {
                Box::pin(Self::remove_dir_recursive(sftp, &child_path)).await?;
            } else {
                sftp.remove_file(&child_path)
                    .await
                    .map_err(|e| AppError::Ssh(format!("Failed to remove '{}': {}", child_path, e)))?;
            }
        }

        sftp.remove_dir(path)
            .await
            .map_err(|e| AppError::Ssh(format!("Failed to remove dir '{}': {}", path, e)))
    }

    /// Rename file or directory via SFTP
    pub fn rename(&self, old_path: &str, new_path: &str) -> AppResult<()> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let old = old_path.to_string();
        let new = new_path.to_string();

        self.runtime.block_on(async {
            sftp.rename(&old, &new)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to rename '{}' to '{}': {}", old, new, e)))
        })
    }

    /// Create directory via SFTP
    pub fn mkdir(&self, path: &str) -> AppResult<()> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let path = path.to_string();

        self.runtime.block_on(async {
            sftp.create_dir(&path)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to create dir '{}': {}", path, e)))
        })
    }

    /// Create an empty file via SFTP
    pub fn create_file(&self, path: &str) -> AppResult<()> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let path = path.to_string();

        self.runtime.block_on(async {
            // Open file with write+create+truncate flags, drop to close
            let _file = sftp
                .create(&path)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to create file '{}': {}", path, e)))?;
            Ok(())
        })
    }

    /// Download remote file to local path via SFTP (streaming, chunked)
    pub fn download_file(&self, remote_path: &str, local_path: &str) -> AppResult<u64> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncReadExt;

            let mut remote_file = sftp
                .open(&remote_path)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to open '{}': {}", remote_path, e)))?;

            let mut local_file = std::fs::File::create(&local_path)?;

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let n = remote_file
                    .read(&mut buf)
                    .await
                    .map_err(|e| AppError::Ssh(format!("Failed to read '{}': {}", remote_path, e)))?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(&mut local_file, &buf[..n])?;
                total += n as u64;
            }
            Ok(total)
        })
    }

    /// Download remote file with progress callback and cancellation support
    pub fn download_file_with_progress<F>(
        &self,
        remote_path: &str,
        local_path: &str,
        file_size: u64,
        cancel_flag: &std::sync::atomic::AtomicBool,
        on_progress: F,
    ) -> AppResult<u64>
    where
        F: Fn(u64, u64),
    {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncReadExt;

            let mut remote_file = sftp
                .open(&remote_path)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to open '{}': {}", remote_path, e)))?;

            let mut local_file = std::fs::File::create(&local_path)?;

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    drop(local_file);
                    let _ = std::fs::remove_file(&local_path);
                    return Err(AppError::Other("Cancelled".to_string()));
                }
                let n = remote_file
                    .read(&mut buf)
                    .await
                    .map_err(|e| AppError::Ssh(format!("Failed to read '{}': {}", remote_path, e)))?;
                if n == 0 {
                    break;
                }
                std::io::Write::write_all(&mut local_file, &buf[..n])?;
                total += n as u64;
                on_progress(total, file_size);
            }
            Ok(total)
        })
    }

    /// Upload local file to remote path via SFTP (streaming, chunked)
    pub fn upload_file(&self, local_path: &str, remote_path: &str) -> AppResult<u64> {
        let sftp = self.sftp.as_ref().ok_or(AppError::Ssh("Not connected".to_string()))?;
        let remote_path = remote_path.to_string();
        let local_path = local_path.to_string();

        self.runtime.block_on(async {
            use tokio::io::AsyncWriteExt;

            let mut local_file = std::fs::File::open(&local_path)?;

            let mut remote_file = sftp
                .create(&remote_path)
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to create '{}': {}", remote_path, e)))?;

            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let n = std::io::Read::read(&mut local_file, &mut buf)?;
                if n == 0 {
                    break;
                }
                remote_file
                    .write_all(&buf[..n])
                    .await
                    .map_err(|e| AppError::Ssh(format!("Failed to write '{}': {}", remote_path, e)))?;
                total += n as u64;
            }
            remote_file
                .shutdown()
                .await
                .map_err(|e| AppError::Ssh(format!("Failed to close '{}': {}", remote_path, e)))?;
            Ok(total)
        })
    }

    /// Disconnect from remote host
    pub fn disconnect(&mut self) {
        // Drop SFTP first, then SSH
        self.sftp = None;
        if let Some(ssh) = self.ssh_handle.take() {
            let _ = self
                .runtime
                .block_on(async { ssh.disconnect(Disconnect::ByApplication, "", "en").await });
        }
    }

    /// Check if session is still connected
    pub fn is_connected(&self) -> bool {
        self.sftp.is_some()
    }
}

impl Drop for SftpSession {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Parse user@host:/path format
/// Returns (user, host, port, path) if matched
pub fn parse_remote_path(input: &str) -> Option<(String, String, u16, String)> {
    // Format: user@host:/path or user@host:port:/path
    let at_pos = input.find('@')?;
    let user = input[..at_pos].to_string();
    if user.is_empty() {
        return None;
    }

    let after_at = &input[at_pos + 1..];
    let colon_pos = after_at.find(':')?;
    let host_part = &after_at[..colon_pos];
    let after_first_colon = &after_at[colon_pos + 1..];

    // Check if there's a port number: host:port:/path
    let (host, port, path) = if let Some(second_colon) = after_first_colon.find(':') {
        let port_str = &after_first_colon[..second_colon];
        if let Ok(port) = port_str.parse::<u16>() {
            let path = &after_first_colon[second_colon + 1..];
            (
                host_part.to_string(),
                port,
                if path.is_empty() {
                    "/".to_string()
                } else {
                    path.to_string()
                },
            )
        } else {
            // Not a port number, treat entire after_first_colon as path
            (
                host_part.to_string(),
                22,
                if after_first_colon.is_empty() {
                    "/".to_string()
                } else {
                    after_first_colon.to_string()
                },
            )
        }
    } else {
        (
            host_part.to_string(),
            22,
            if after_first_colon.is_empty() {
                "/".to_string()
            } else {
                after_first_colon.to_string()
            },
        )
    };

    if host.is_empty() {
        return None;
    }

    // Ensure path starts with /
    let path = if path.starts_with('/') {
        path
    } else {
        format!("/{}", path)
    };

    Some((user, host, port, path))
}

/// Format remote permissions from mode bits to rwxrwxrwx string
fn format_remote_permissions(mode: u32) -> String {
    let mut perms = String::with_capacity(9);
    let flags = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    for (bit, ch) in &flags {
        perms.push(if mode & bit != 0 { *ch } else { '-' });
    }
    perms
}

/// Build remote display path string (e.g., "user@host:/path")
pub fn format_remote_display(profile: &RemoteProfile, path: &str) -> String {
    if profile.port != 22 {
        format!(
            "{}@{}:{}:{}",
            profile.user, profile.host, profile.port, path
        )
    } else {
        format!("{}@{}:{}", profile.user, profile.host, path)
    }
}

/// Find matching profile from profiles list by user, host, port
pub fn find_matching_profile<'a>(
    profiles: &'a [RemoteProfile],
    user: &str,
    host: &str,
    port: u16,
) -> Option<&'a RemoteProfile> {
    profiles
        .iter()
        .find(|p| p.user == user && p.host == host && p.port == port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_remote_path_basic() {
        let result = parse_remote_path("user@host:/home/user");
        assert_eq!(
            result,
            Some((
                "user".to_string(),
                "host".to_string(),
                22,
                "/home/user".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_remote_path_with_port() {
        let result = parse_remote_path("admin@server:2222:/var/log");
        assert_eq!(
            result,
            Some((
                "admin".to_string(),
                "server".to_string(),
                2222,
                "/var/log".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_remote_path_no_path() {
        let result = parse_remote_path("user@host:");
        assert_eq!(
            result,
            Some(("user".to_string(), "host".to_string(), 22, "/".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_path_invalid() {
        assert!(parse_remote_path("just/a/path").is_none());
        assert!(parse_remote_path("@host:/path").is_none());
        assert!(parse_remote_path("user@:/path").is_none());
    }

    #[test]
    fn test_format_remote_permissions() {
        assert_eq!(format_remote_permissions(0o755), "rwxr-xr-x");
        assert_eq!(format_remote_permissions(0o644), "rw-r--r--");
    }
}
