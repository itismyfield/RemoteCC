use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};

use russh::{client, ChannelMsg, Disconnect};
use tokio::runtime::Runtime;

use crate::services::file_ops::ProgressMessage;
use crate::services::remote::{RemoteAuth, RemoteProfile, SshHandler};

/// Transfer direction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    LocalToRemote,
    RemoteToLocal,
}

/// Transfer configuration
#[derive(Debug, Clone)]
pub struct TransferConfig {
    pub direction: TransferDirection,
    pub profile: RemoteProfile,
    pub source_files: Vec<PathBuf>,
    pub source_base: String,
    pub target_path: String,
}

/// Check if rsync is available
fn has_rsync() -> bool {
    Command::new("rsync")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if two remote profiles refer to the same server
fn is_same_server(a: &RemoteProfile, b: &RemoteProfile) -> bool {
    a.host == b.host && a.port == b.port && a.user == b.user
}

/// SSH command executor using russh library (no external ssh process needed).
/// Connects once, executes multiple commands on the same connection,
/// and disconnects automatically on drop.
struct SshExec {
    runtime: Runtime,
    handle: client::Handle<SshHandler>,
}

impl SshExec {
    /// Connect to remote server via russh and authenticate.
    fn connect(profile: &RemoteProfile) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| format!("Failed to create runtime: {}", e))?;

        let profile = profile.clone();
        let handle = runtime.block_on(async {
            crate::services::remote::ssh_connect_and_auth(&profile).await
        })?;

        Ok(Self { runtime, handle })
    }

    /// Execute a command on the remote server.
    /// Returns (success, stderr_string).
    fn exec(&self, cmd: &str) -> Result<(bool, String), String> {
        let cmd = cmd.to_string();
        self.runtime.block_on(async {
            let mut channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|e| format!("Failed to open channel: {}", e))?;

            channel
                .exec(true, cmd)
                .await
                .map_err(|e| format!("Failed to exec command: {}", e))?;

            let mut stderr_bytes = Vec::new();
            let mut exit_status: Option<u32> = None;

            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::ExtendedData { data, ext } => {
                        if ext == 1 {
                            stderr_bytes.extend_from_slice(&data);
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status: s } => {
                        exit_status = Some(s);
                    }
                    _ => {}
                }
            }

            let success = exit_status.map_or(false, |s| s == 0);
            let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

            Ok((success, stderr))
        })
    }
}

impl Drop for SshExec {
    fn drop(&mut self) {
        let _ = self.runtime.block_on(async {
            self.handle
                .disconnect(Disconnect::ByApplication, "", "en")
                .await
        });
    }
}

/// Check if sshpass is available
fn has_sshpass() -> bool {
    Command::new("sshpass")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build SSH command option string for rsync
fn build_ssh_option(profile: &RemoteProfile) -> String {
    let mut ssh_cmd = String::from("ssh");

    // Port
    if profile.port != 22 {
        ssh_cmd.push_str(&format!(" -p {}", profile.port));
    }

    // Key file
    if let RemoteAuth::KeyFile { ref path, .. } = profile.auth {
        let expanded = crate::utils::format::expand_tilde_path(path);
        ssh_cmd.push_str(&format!(" -i '{}'", expanded.display()));
    }

    // Disable strict host key checking for convenience
    ssh_cmd
        .push_str(" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR");

    ssh_cmd
}

/// Build remote path string for rsync: user@host:/path
/// Wraps remote path in single quotes to prevent remote shell interpretation.
fn build_remote_spec(profile: &RemoteProfile, path: &str) -> String {
    // Single quotes prevent all shell interpretation.
    // Only single quotes inside the path need escaping: ' → '\''
    let escaped = path.replace('\'', "'\\''");
    format!("{}@{}:'{}'", profile.user, profile.host, escaped)
}

/// Create a temporary SSH_ASKPASS script for password authentication.
/// Returns the script path. Caller must clean up with cleanup_askpass_script().
fn create_askpass_script(password: &str) -> Result<PathBuf, String> {
    let tmp_dir = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".remotecc")
        .join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Failed to create tmp dir: {}", e))?;

    let script_path = tmp_dir.join(format!("askpass_{}", std::process::id()));

    // Escape single quotes in password: replace ' with '\''
    let escaped = password.replace('\'', "'\\''");
    let content = format!("#!/bin/sh\necho '{}'\n", escaped);

    std::fs::write(&script_path, content)
        .map_err(|e| format!("Failed to create askpass script: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("Failed to set script permissions: {}", e))?;
    }

    Ok(script_path)
}

/// Remove the temporary askpass script
fn cleanup_askpass_script(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Transfer files using rsync with progress reporting.
/// Uses --progress flag (compatible with GNU rsync and openrsync/macOS).
/// For password auth: tries sshpass first, falls back to SSH_ASKPASS mechanism.
fn transfer_rsync(
    config: &TransferConfig,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
) -> Result<(), String> {
    let ssh_option = build_ssh_option(&config.profile);
    let total_files = config.source_files.len();
    let mut completed_files: usize = 0;

    // Prepare password auth mechanism
    let needs_password = matches!(&config.profile.auth, RemoteAuth::Password { .. });
    let use_sshpass = needs_password && has_sshpass();
    let askpass_script = if needs_password && !use_sshpass {
        if let RemoteAuth::Password { ref password } = config.profile.auth {
            Some(create_askpass_script(password)?)
        } else {
            None
        }
    } else {
        None
    };

    for source_file in &config.source_files {
        if cancel_flag.load(Ordering::Relaxed) {
            if let Some(ref path) = askpass_script {
                cleanup_askpass_script(path);
            }
            return Ok(());
        }

        let file_name = source_file.display().to_string();
        let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

        let source_full = format!(
            "{}/{}",
            config.source_base.trim_end_matches('/'),
            source_file.display()
        );
        let target = &config.target_path;

        let (src, dst) = match config.direction {
            TransferDirection::LocalToRemote => {
                (source_full, build_remote_spec(&config.profile, target))
            }
            TransferDirection::RemoteToLocal => (
                build_remote_spec(&config.profile, &source_full),
                target.clone(),
            ),
        };

        // Build rsync command with --progress (compatible with all rsync versions)
        let mut cmd = Command::new("rsync");
        cmd.arg("-a")
            .arg("--progress")
            .arg("-e")
            .arg(&ssh_option)
            .arg(&src)
            .arg(&dst)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Apply password auth
        let mut cmd = if use_sshpass {
            if let RemoteAuth::Password { ref password } = config.profile.auth {
                let mut sshpass_cmd = Command::new("sshpass");
                sshpass_cmd.arg("-p").arg(password);
                let program = cmd.get_program().to_string_lossy().to_string();
                let args: Vec<String> = cmd
                    .get_args()
                    .map(|a| a.to_string_lossy().to_string())
                    .collect();
                sshpass_cmd.arg(program);
                for arg in args {
                    sshpass_cmd.arg(arg);
                }
                sshpass_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
                sshpass_cmd
            } else {
                cmd
            }
        } else if let Some(ref script_path) = askpass_script {
            cmd.env("SSH_ASKPASS", script_path)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env("DISPLAY", ":0")
                .stdin(Stdio::null());
            cmd
        } else {
            cmd
        };

        let mut child = cmd.spawn().map_err(|e| {
            if let Some(ref path) = askpass_script {
                cleanup_askpass_script(path);
            }
            format!("Failed to start rsync: {}", e)
        })?;

        // Parse rsync progress output.
        // rsync --progress uses \r (carriage return) to update progress in-place,
        // so we read byte-by-byte and split on both \r and \n.
        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout);
            let mut line_buf = Vec::new();
            let mut byte_buf = [0u8; 1];
            loop {
                if cancel_flag.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(ref path) = askpass_script {
                        cleanup_askpass_script(path);
                    }
                    return Ok(());
                }

                match std::io::Read::read(&mut reader, &mut byte_buf) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let b = byte_buf[0];
                        if b == b'\r' || b == b'\n' {
                            if !line_buf.is_empty() {
                                let line = String::from_utf8_lossy(&line_buf).to_string();
                                if let Some(progress) = parse_rsync_progress(&line) {
                                    let _ = tx.send(ProgressMessage::FileProgress(
                                        progress.0, progress.1,
                                    ));
                                }
                                line_buf.clear();
                            }
                        } else {
                            line_buf.push(b);
                        }
                    }
                    Err(_) => break,
                }
            }
            // Process remaining data in buffer
            if !line_buf.is_empty() {
                let line = String::from_utf8_lossy(&line_buf).to_string();
                if let Some(progress) = parse_rsync_progress(&line) {
                    let _ = tx.send(ProgressMessage::FileProgress(progress.0, progress.1));
                }
            }
        }

        let status = child.wait().map_err(|e| {
            if let Some(ref path) = askpass_script {
                cleanup_askpass_script(path);
            }
            format!("rsync wait failed: {}", e)
        })?;

        if status.success() {
            completed_files += 1;
            let _ = tx.send(ProgressMessage::FileCompleted(file_name));
            let _ = tx.send(ProgressMessage::TotalProgress(
                completed_files,
                total_files,
                0,
                0,
            ));
        } else {
            let stderr_msg = if let Some(mut stderr) = child.stderr.take() {
                let mut buf = String::new();
                let _ = std::io::Read::read_to_string(&mut stderr, &mut buf);
                buf
            } else {
                format!("rsync exited with code {}", status.code().unwrap_or(-1))
            };
            let _ = tx.send(ProgressMessage::Error(file_name, stderr_msg.clone()));
            if let Some(ref path) = askpass_script {
                cleanup_askpass_script(path);
            }
            return Err(stderr_msg);
        }
    }

    // Cleanup
    if let Some(ref path) = askpass_script {
        cleanup_askpass_script(path);
    }

    Ok(())
}

/// Parse rsync --progress output line.
/// Format: "  1,234,567  42%  1.23MB/s  0:01:23"
/// Returns (transferred_bytes, total_bytes) if parseable.
fn parse_rsync_progress(line: &str) -> Option<(u64, u64)> {
    let trimmed = line.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() >= 2 {
        // First part: bytes transferred (with commas)
        let bytes_str = parts[0].replace(',', "");
        let transferred: u64 = bytes_str.parse().ok()?;

        // Second part: percentage
        let pct_str = parts[1].trim_end_matches('%');
        let pct: u64 = pct_str.parse().ok()?;

        if pct > 0 {
            let total = transferred * 100 / pct;
            return Some((transferred, total));
        } else if transferred > 0 {
            return Some((0, transferred));
        }
    }
    None
}

/// Delete source files after a successful cut (move) transfer.
/// If `source_profile` is Some, the source is remote (delete via SSH rm -rf).
/// If `source_profile` is None, the source is local (delete via std::fs).
fn delete_source_files_after_cut(
    source_files: &[PathBuf],
    source_base: &str,
    source_profile: Option<&RemoteProfile>,
    tx: &Sender<ProgressMessage>,
) {
    match source_profile {
        Some(profile) => delete_remote_source_files(profile, source_files, source_base, tx),
        None => delete_local_source_files(source_files, source_base, tx),
    }
}

/// Delete local source files after cut
fn delete_local_source_files(
    source_files: &[PathBuf],
    source_base: &str,
    tx: &Sender<ProgressMessage>,
) {
    for source_file in source_files {
        let full_path = PathBuf::from(source_base).join(source_file);
        let result = if full_path.is_dir() {
            std::fs::remove_dir_all(&full_path)
        } else {
            std::fs::remove_file(&full_path)
        };
        if let Err(e) = result {
            let _ = tx.send(ProgressMessage::Error(
                source_file.display().to_string(),
                format!("Failed to delete source: {}", e),
            ));
        }
    }
}

/// Delete remote source files via russh SSH exec (rm -rf)
fn delete_remote_source_files(
    profile: &RemoteProfile,
    source_files: &[PathBuf],
    source_base: &str,
    tx: &Sender<ProgressMessage>,
) {
    // Build rm -rf paths
    let mut rm_paths = Vec::new();
    for source_file in source_files {
        let full_path = format!(
            "{}/{}",
            source_base.trim_end_matches('/'),
            source_file.display()
        );
        let escaped = full_path.replace('\'', "'\\''");
        rm_paths.push(format!("'{}'", escaped));
    }

    if rm_paths.is_empty() {
        return;
    }

    let rm_cmd = format!("rm -rf {}", rm_paths.join(" "));

    let ssh = match SshExec::connect(profile) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Failed to connect for source deletion: {}", e),
            ));
            return;
        }
    };

    match ssh.exec(&rm_cmd) {
        Ok((success, stderr)) => {
            if !success {
                let _ = tx.send(ProgressMessage::Error(
                    String::new(),
                    format!("Failed to delete remote source: {}", stderr.trim()),
                ));
            }
        }
        Err(e) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Failed to run SSH for source deletion: {}", e),
            ));
        }
    }
}

/// Main transfer function — always uses rsync
/// When `is_cut` is true, source files are deleted after successful transfer.
/// `source_profile` is needed when the source is remote (for SSH rm deletion).
pub fn transfer_files_with_progress(
    config: TransferConfig,
    cancel_flag: Arc<AtomicBool>,
    tx: Sender<ProgressMessage>,
    is_cut: bool,
    source_profile: Option<RemoteProfile>,
) {
    let total_files = config.source_files.len();

    let _ = tx.send(ProgressMessage::Preparing(format!(
        "Transferring {} file(s)...",
        total_files
    )));

    if !has_rsync() {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            "rsync is not installed. Please install rsync to transfer files.".to_string(),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    let _ = tx.send(ProgressMessage::PrepareComplete);
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    let result = transfer_rsync(&config, &cancel_flag, &tx);

    match result {
        Ok(_) => {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Completed(0, 0));
            } else {
                // Delete source files if this is a cut (move) operation
                if is_cut {
                    delete_source_files_after_cut(
                        &config.source_files,
                        &config.source_base,
                        source_profile.as_ref(),
                        &tx,
                    );
                }
                let _ = tx.send(ProgressMessage::Completed(total_files, 0));
            }
        }
        Err(ref msg) => {
            let _ = tx.send(ProgressMessage::Error(String::new(), msg.clone()));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
        }
    }
}

/// Transfer files within the same remote server using cp -a (copy) or mv (move) via russh SSH exec.
fn transfer_same_server(
    profile: &RemoteProfile,
    source_files: &[PathBuf],
    source_base: &str,
    target_path: &str,
    cancel_flag: &Arc<AtomicBool>,
    tx: &Sender<ProgressMessage>,
    is_cut: bool,
) -> Result<(), String> {
    let total_files = source_files.len();
    let mut completed_files: usize = 0;

    let ssh = SshExec::connect(profile)?;

    for source_file in source_files {
        if cancel_flag.load(Ordering::Relaxed) {
            return Ok(());
        }

        let file_name = source_file.display().to_string();
        let _ = tx.send(ProgressMessage::FileStarted(file_name.clone()));

        let source_full = format!(
            "{}/{}",
            source_base.trim_end_matches('/'),
            source_file.display()
        );
        let escaped_src = source_full.replace('\'', "'\\''");
        let escaped_dst = target_path.replace('\'', "'\\''");

        let remote_cmd = if is_cut {
            format!("mv '{}' '{}'", escaped_src, escaped_dst)
        } else {
            format!("cp -a '{}' '{}'", escaped_src, escaped_dst)
        };

        let (success, stderr) = ssh.exec(&remote_cmd)?;

        if success {
            completed_files += 1;
            let _ = tx.send(ProgressMessage::FileCompleted(file_name));
            let _ = tx.send(ProgressMessage::TotalProgress(
                completed_files,
                total_files,
                0,
                0,
            ));
        } else {
            let err_msg = format!(
                "Failed to {} '{}': {}",
                if is_cut { "move" } else { "copy" },
                file_name,
                stderr.trim()
            );
            let _ = tx.send(ProgressMessage::Error(file_name, err_msg.clone()));
            return Err(err_msg);
        }
    }

    Ok(())
}

/// Transfer files between two remote servers via local temp directory
/// Phase 1: Download from source remote to local temp
/// Phase 2: Upload from local temp to target remote
/// When `is_cut` is true, source files are deleted from source remote after successful upload.
pub fn transfer_remote_to_remote_with_progress(
    source_profile: RemoteProfile,
    target_profile: RemoteProfile,
    source_files: Vec<PathBuf>,
    source_base: String,
    target_path: String,
    cancel_flag: Arc<AtomicBool>,
    tx: Sender<ProgressMessage>,
    is_cut: bool,
) {
    let total_files = source_files.len();

    let _ = tx.send(ProgressMessage::Preparing(format!(
        "Transferring {} file(s) between remote servers...",
        total_files
    )));

    // Same server optimization: use cp -a / mv directly via SSH
    if is_same_server(&source_profile, &target_profile) {
        let _ = tx.send(ProgressMessage::PrepareComplete);
        let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

        let result = transfer_same_server(
            &source_profile,
            &source_files,
            &source_base,
            &target_path,
            &cancel_flag,
            &tx,
            is_cut,
        );

        match result {
            Ok(_) => {
                if cancel_flag.load(Ordering::Relaxed) {
                    let _ = tx.send(ProgressMessage::Completed(0, 0));
                } else {
                    let _ = tx.send(ProgressMessage::Completed(total_files, 0));
                }
            }
            Err(ref msg) => {
                let _ = tx.send(ProgressMessage::Error(String::new(), msg.clone()));
                let _ = tx.send(ProgressMessage::Completed(0, total_files));
            }
        }
        return;
    }

    if !has_rsync() {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            "rsync is not installed. Please install rsync to transfer files.".to_string(),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    // Create temp directory under ~/.remotecc/tmp/
    let temp_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let base_tmp = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".remotecc")
        .join("tmp");
    let temp_dir = base_tmp.join(format!("r2r_{}", temp_id));
    if let Err(e) = std::fs::create_dir_all(&temp_dir) {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            format!("Failed to create temp dir: {}", e),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        return;
    }

    let _ = tx.send(ProgressMessage::PrepareComplete);
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    // Phase 1: Download from source remote to local temp
    let download_config = TransferConfig {
        direction: TransferDirection::RemoteToLocal,
        profile: source_profile.clone(),
        source_files: source_files.clone(),
        source_base: source_base.clone(),
        target_path: temp_dir.display().to_string(),
    };

    let dl_result = transfer_rsync(&download_config, &cancel_flag, &tx);

    if let Err(ref msg) = dl_result {
        let _ = tx.send(ProgressMessage::Error(
            String::new(),
            format!("Download failed: {}", msg),
        ));
        let _ = tx.send(ProgressMessage::Completed(0, total_files));
        let _ = std::fs::remove_dir_all(&temp_dir);
        return;
    }

    if cancel_flag.load(Ordering::Relaxed) {
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = tx.send(ProgressMessage::Completed(0, 0));
        return;
    }

    // Phase 2: Upload from local temp to target remote
    // Reset progress counters so progress bar starts from 0% again
    let _ = tx.send(ProgressMessage::TotalProgress(0, total_files, 0, 0));

    let upload_config = TransferConfig {
        direction: TransferDirection::LocalToRemote,
        profile: target_profile,
        source_files: source_files.clone(),
        source_base: temp_dir.display().to_string(),
        target_path,
    };

    let ul_result = transfer_rsync(&upload_config, &cancel_flag, &tx);

    // Cleanup temp directory
    let _ = std::fs::remove_dir_all(&temp_dir);

    match ul_result {
        Ok(_) => {
            if cancel_flag.load(Ordering::Relaxed) {
                let _ = tx.send(ProgressMessage::Completed(0, 0));
            } else {
                // Delete source files from source remote if this is a cut
                if is_cut {
                    delete_source_files_after_cut(
                        &source_files,
                        &source_base,
                        Some(&source_profile),
                        &tx,
                    );
                }
                let _ = tx.send(ProgressMessage::Completed(total_files, 0));
            }
        }
        Err(ref msg) => {
            let _ = tx.send(ProgressMessage::Error(
                String::new(),
                format!("Upload failed: {}", msg),
            ));
            let _ = tx.send(ProgressMessage::Completed(0, total_files));
        }
    }
}
