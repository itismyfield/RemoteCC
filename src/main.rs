mod cli;
mod config;
mod enc;
mod error;
mod keybindings;
mod services;
mod ui;
mod utils;

// Re-export for crate-level access (used by services::discord::mod.rs)
pub(crate) use cli::remotecc_runtime_root;

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::env;
use std::io;
use std::time::Duration;

use crate::keybindings::PanelAction;
use crate::ui::app::{App, Screen};

fn main() -> io::Result<()> {
    // Migrate config directory from old name
    cli::migrate_config_dir();

    // Handle command line arguments
    let args: Vec<String> = env::args().collect();
    let mut design_mode = false;
    let mut start_paths: Vec<std::path::PathBuf> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                cli::print_help();
                return Ok(());
            }
            "-v" | "--version" => {
                cli::print_version();
                return Ok(());
            }
            "--prompt" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --prompt requires a text argument");
                    eprintln!("Usage: remotecc --prompt \"your question\"");
                    return Ok(());
                }
                cli::handle_prompt(&args[i + 1]);
                return Ok(());
            }
            "--base64" => {
                if i + 1 >= args.len() {
                    std::process::exit(1);
                }
                cli::handle_base64(&args[i + 1]);
                return Ok(());
            }
            "--dcserver" => {
                let token = if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    Some(args[i + 1].clone())
                } else if let Ok(t) = std::env::var("REMOTECC_TOKEN") {
                    Some(t)
                } else {
                    None
                };
                cli::handle_dcserver(token);
                return Ok(());
            }
            "--restart-dcserver" => {
                match cli::parse_restart_dcserver_report_context(&args, i + 1) {
                    Ok(report_context) => cli::handle_restart_dcserver(report_context),
                    Err(err) => eprintln!("Error: {err}"),
                }
                return Ok(());
            }
            "--discord-sendfile" => {
                // Parse: --discord-sendfile <PATH> --channel <ID> --key <HASH>
                let mut file_path: Option<String> = None;
                let mut channel_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--channel" => {
                            if j + 1 < args.len() {
                                channel_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" => {
                            if j + 1 < args.len() {
                                key = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        _ if file_path.is_none() && !args[j].starts_with("--") => {
                            file_path = Some(args[j].clone());
                            j += 1;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (file_path, channel_id, key) {
                    (Some(fp), Some(cid), Some(k)) => {
                        cli::handle_discord_sendfile(&fp, cid, &k);
                    }
                    _ => {
                        eprintln!("Error: --discord-sendfile requires <PATH>, --channel <ID>, and --key <HASH>");
                        eprintln!(
                            "Usage: remotecc --discord-sendfile <PATH> --channel <ID> --key <HASH>"
                        );
                    }
                }
                return Ok(());
            }
            "--discord-sendmessage" => {
                let mut message: Option<String> = None;
                let mut channel_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--channel" => {
                            if j + 1 < args.len() {
                                channel_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--message" => {
                            if j + 1 < args.len() {
                                message = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" => {
                            if j + 1 < args.len() {
                                key = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (message, channel_id) {
                    (Some(msg), Some(cid)) => {
                        cli::handle_discord_sendmessage(&msg, cid, key.as_deref());
                    }
                    _ => {
                        eprintln!(
                            "Error: --discord-sendmessage requires --channel <ID> and --message <TEXT>"
                        );
                        eprintln!(
                            "Usage: remotecc --discord-sendmessage --channel <ID> --message <TEXT> [--key <HASH>]"
                        );
                    }
                }
                return Ok(());
            }
            "--discord-senddm" => {
                let mut message: Option<String> = None;
                let mut user_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--user" => {
                            if j + 1 < args.len() {
                                user_id = args[j + 1].parse().ok();
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--message" => {
                            if j + 1 < args.len() {
                                message = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        "--key" => {
                            if j + 1 < args.len() {
                                key = Some(args[j + 1].clone());
                                j += 2;
                            } else {
                                j += 1;
                            }
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (message, user_id) {
                    (Some(msg), Some(uid)) => {
                        cli::handle_discord_senddm(&msg, uid, key.as_deref());
                    }
                    _ => {
                        eprintln!(
                            "Error: --discord-senddm requires --user <ID> and --message <TEXT>"
                        );
                        eprintln!(
                            "Usage: remotecc --discord-senddm --user <ID> --message <TEXT> [--key <HASH>]"
                        );
                    }
                }
                return Ok(());
            }
            "--ismcptool" => {
                let tool_names: Vec<String> = args[i + 1..]
                    .iter()
                    .take_while(|a| !a.starts_with('-'))
                    .cloned()
                    .collect();
                if tool_names.is_empty() {
                    eprintln!("Error: --ismcptool requires at least one tool name");
                    eprintln!("Usage: remotecc --ismcptool \"TOOL1\" \"TOOL2\" ...");
                    return Ok(());
                }
                cli::handle_ismcptool(&tool_names);
                return Ok(());
            }
            "--addmcptool" => {
                let tool_names: Vec<String> = args[i + 1..]
                    .iter()
                    .take_while(|a| !a.starts_with('-'))
                    .cloned()
                    .collect();
                if tool_names.is_empty() {
                    eprintln!("Error: --addmcptool requires at least one tool name");
                    eprintln!("Usage: remotecc --addmcptool \"TOOL1\" \"TOOL2\" ...");
                    return Ok(());
                }
                cli::handle_addmcptool(&tool_names);
                return Ok(());
            }
            "--reset-tmux" => {
                cli::handle_reset_tmux();
                return Ok(());
            }
            "--tmux-wrapper" => {
                // Internal: runs inside tmux session as bidirectional Claude wrapper
                // Usage: remotecc --tmux-wrapper --output-file <PATH> --input-fifo <PATH> --prompt-file <PATH> --cwd <PATH> -- <claude-cmd...>
                let mut output_file: Option<String> = None;
                let mut input_fifo: Option<String> = None;
                let mut prompt_file: Option<String> = None;
                let mut cwd: Option<String> = None;
                let mut claude_cmd: Vec<String> = Vec::new();
                let mut j = i + 1;
                let mut after_separator = false;
                while j < args.len() {
                    if after_separator {
                        claude_cmd.push(args[j].clone());
                        j += 1;
                        continue;
                    }
                    match args[j].as_str() {
                        "--" => {
                            after_separator = true;
                            j += 1;
                        }
                        "--output-file" => {
                            output_file = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--input-fifo" => {
                            input_fifo = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--prompt-file" => {
                            prompt_file = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--cwd" => {
                            cwd = args.get(j + 1).cloned();
                            j += 2;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (output_file, input_fifo, prompt_file) {
                    (Some(of), Some(inf), Some(pf)) => {
                        let wd = cwd.unwrap_or_else(|| ".".to_string());
                        services::tmux_wrapper::run(&of, &inf, &pf, &wd, &claude_cmd);
                    }
                    _ => {
                        eprintln!("Error: --tmux-wrapper requires --output-file, --input-fifo, and --prompt-file");
                    }
                }
                return Ok(());
            }
            "--codex-tmux-wrapper" => {
                let mut output_file: Option<String> = None;
                let mut input_fifo: Option<String> = None;
                let mut prompt_file: Option<String> = None;
                let mut cwd: Option<String> = None;
                let mut codex_bin: Option<String> = None;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--output-file" => {
                            output_file = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--input-fifo" => {
                            input_fifo = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--prompt-file" => {
                            prompt_file = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--cwd" => {
                            cwd = args.get(j + 1).cloned();
                            j += 2;
                        }
                        "--codex-bin" => {
                            codex_bin = args.get(j + 1).cloned();
                            j += 2;
                        }
                        _ => {
                            j += 1;
                        }
                    }
                }
                match (output_file, input_fifo, prompt_file, codex_bin) {
                    (Some(of), Some(inf), Some(pf), Some(bin)) => {
                        let wd = cwd.unwrap_or_else(|| ".".to_string());
                        services::codex_tmux_wrapper::run(&of, &inf, &pf, &wd, &bin);
                    }
                    _ => {
                        eprintln!("Error: --codex-tmux-wrapper requires --output-file, --input-fifo, --prompt-file, and --codex-bin");
                    }
                }
                return Ok(());
            }
            "--design" => {
                design_mode = true;
            }
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {}", arg);
                eprintln!("Use --help for usage information");
                return Ok(());
            }
            path => {
                // Treat as a directory path
                let p = std::path::PathBuf::from(path);
                let resolved = if p.is_absolute() {
                    p
                } else {
                    env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
                        .join(p)
                };
                start_paths.push(resolved);
            }
        }
        i += 1;
    }

    // Setup panic hook to restore terminal on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Clear screen before entering alternate screen
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Detect terminal image protocol (must be after alternate screen, before event loop)
    let picker = {
        let mut p = ratatui_image::picker::Picker::from_termios()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::new((8, 16)));
        p.guess_protocol();
        p
    };

    // Load settings and create app state
    let (settings, settings_error) = match config::Settings::load_with_error() {
        Ok(s) => (s, None),
        Err(e) => (config::Settings::default(), Some(e)),
    };
    let mut app = App::with_settings(settings);
    app.image_picker = Some(picker);
    app.design_mode = design_mode;

    // Override panels with command-line paths if provided
    if !start_paths.is_empty() {
        app.set_panels_from_paths(start_paths);
    }

    // Show settings load error if any
    if let Some(err) = settings_error {
        app.show_message(&format!("Settings error: {} (using defaults)", err));
    }

    // Show design mode message if active
    if design_mode {
        app.show_message("Design mode: theme hot-reload enabled");
    }

    // Run app
    let result = run_app(&mut terminal, &mut app);

    // Save settings before exit
    app.save_settings();

    // Save last directory for shell cd (skip remote paths)
    if !app.active_panel().is_remote() {
        let last_dir = app.active_panel().path.display().to_string();
        if let Some(config_dir) = config::Settings::config_dir() {
            let lastdir_path = config_dir.join("lastdir");
            let _ = std::fs::write(&lastdir_path, &last_dir);
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Show
    )?;

    if let Err(err) = result {
        eprintln!("Error: {}", err);
    }

    // Print goodbye message
    cli::print_goodbye_message();

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        // Check if full redraw is needed (after terminal mode command like vim)
        if app.needs_full_redraw {
            terminal.clear()?;
            app.needs_full_redraw = false;
        }

        terminal.draw(|f| ui::draw::draw(f, app))?;

        // For AI screen, FileInfo with calculation, ImageViewer loading, diff comparing, file operation progress, or remote spinner, use fast polling
        let is_file_info_calculating = app.current_screen == Screen::FileInfo
            && app
                .file_info_state
                .as_ref()
                .map(|s| s.is_calculating)
                .unwrap_or(false);
        let is_image_loading = app.current_screen == Screen::ImageViewer
            && app
                .image_viewer_state
                .as_ref()
                .map(|s| s.is_loading)
                .unwrap_or(false);
        let is_diff_comparing = app.current_screen == Screen::DiffScreen
            && app
                .diff_state
                .as_ref()
                .map(|s| s.is_comparing)
                .unwrap_or(false);
        let is_dedup_active = app.current_screen == Screen::DedupScreen
            && app
                .dedup_screen_state
                .as_ref()
                .map(|s| !s.is_complete)
                .unwrap_or(false);
        let is_progress_active = app
            .file_operation_progress
            .as_ref()
            .map(|p| p.is_active)
            .unwrap_or(false);
        let is_remote_spinner = app.remote_spinner.is_some();

        let poll_timeout = if is_progress_active || is_dedup_active {
            Duration::from_millis(16) // ~60fps for smooth real-time updates
        } else if is_remote_spinner {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else if app.current_screen == Screen::AIScreen
            || app.is_ai_mode()
            || is_file_info_calculating
            || is_image_loading
            || is_diff_comparing
        {
            Duration::from_millis(100) // Fast polling for spinner animation
        } else {
            Duration::from_millis(250)
        };

        // Poll for AI responses if on AI screen or AI mode (panel)
        if app.current_screen == Screen::AIScreen || app.is_ai_mode() {
            if let Some(ref mut state) = app.ai_state {
                // poll_response()가 true를 반환하면 새 내용이 추가된 것
                let has_new_content = state.poll_response();
                if has_new_content {
                    app.refresh_panels();
                }
            }
        }

        // Poll for file info calculation if on FileInfo screen
        if app.current_screen == Screen::FileInfo {
            if let Some(ref mut state) = app.file_info_state {
                state.poll();
            }
        }

        // Poll for image loading if on ImageViewer screen
        if app.current_screen == Screen::ImageViewer {
            if let Some(ref mut state) = app.image_viewer_state {
                let was_loading = state.is_loading;
                state.poll();
                // Create inline protocol when loading completes
                if was_loading && !state.is_loading && state.image.is_some() {
                    if let Some(ref mut picker) = app.image_picker {
                        if picker.protocol_type != ratatui_image::picker::ProtocolType::Halfblocks {
                            let img = state.image.as_ref().expect("checked above").clone();
                            state.inline_protocol = Some(picker.new_resize_protocol(img));
                            state.use_inline = true;
                        }
                    }
                }
            }
        }

        // Poll for diff comparison progress if on DiffScreen
        if app.current_screen == Screen::DiffScreen {
            if let Some(ref mut state) = app.diff_state {
                let just_completed = state.poll();
                if just_completed && !state.has_differences() {
                    app.diff_state = None;
                    app.current_screen = Screen::FilePanel;
                    app.show_message("No differences found");
                }
            }
        }

        // Poll for remote spinner completion
        app.poll_remote_spinner();

        // Check for theme file changes (hot-reload, only in design mode)
        if app.design_mode && app.theme_watch_state.check_for_changes() {
            app.reload_theme();
        }

        // Poll for file operation progress
        let progress_message: Option<String> = if let Some(ref mut progress) =
            app.file_operation_progress
        {
            let still_active = progress.poll();
            if !still_active {
                // Operation completed - extract result info before releasing borrow
                let msg = if let Some(ref result) = progress.result {
                    // Special handling for Tar - show archive name
                    if progress.operation_type == crate::services::file_ops::FileOperationType::Tar
                    {
                        if result.failure_count == 0 {
                            if let Some(ref archive_name) = app.pending_tar_archive {
                                Some(format!("Created: {}", archive_name))
                            } else {
                                Some(format!("Archived {} file(s)", result.success_count))
                            }
                        } else {
                            Some(format!(
                                "Error: {}",
                                result.last_error.as_deref().unwrap_or("Archive failed")
                            ))
                        }
                    } else if progress.operation_type
                        == crate::services::file_ops::FileOperationType::Untar
                    {
                        if result.failure_count == 0 {
                            if let Some(ref extract_dir) = app.pending_extract_dir {
                                Some(format!("Extracted to: {}", extract_dir))
                            } else {
                                Some(format!("Extracted {} file(s)", result.success_count))
                            }
                        } else {
                            Some(format!(
                                "Error: {}",
                                result.last_error.as_deref().unwrap_or("Extract failed")
                            ))
                        }
                    } else {
                        let op_name = match progress.operation_type {
                            crate::services::file_ops::FileOperationType::Copy => "Copied",
                            crate::services::file_ops::FileOperationType::Move => "Moved",
                            crate::services::file_ops::FileOperationType::Tar => "Archived",
                            crate::services::file_ops::FileOperationType::Untar => "Extracted",
                            crate::services::file_ops::FileOperationType::Download => "Downloaded",
                            crate::services::file_ops::FileOperationType::Encrypt => "Encrypted",
                            crate::services::file_ops::FileOperationType::Decrypt => "Decrypted",
                        };
                        let total = result.success_count + result.failure_count;
                        if result.failure_count == 0 {
                            Some(format!("{} {} file(s)", op_name, result.success_count))
                        } else {
                            Some(format!(
                                "{} {}/{}. Error: {}",
                                op_name,
                                result.success_count,
                                total,
                                result.last_error.as_deref().unwrap_or("Unknown error")
                            ))
                        }
                    }
                } else {
                    None
                };
                msg
            } else {
                None
            }
        } else {
            None
        };

        // Handle progress completion (outside of borrow)
        if progress_message.is_some() {
            // 원격 다운로드 완료 → 편집기/뷰어 열기
            if let Some(pending) = app.pending_remote_open.take() {
                app.file_operation_progress = None;
                app.dialog = None;

                // tmp 파일 존재 확인으로 성공/실패 판단
                let tmp_exists = match &pending {
                    crate::ui::app::PendingRemoteOpen::Editor { tmp_path, .. } => tmp_path.exists(),
                    crate::ui::app::PendingRemoteOpen::ImageViewer { tmp_path } => {
                        tmp_path.exists()
                    }
                };

                if !tmp_exists {
                    if let Some(msg) = progress_message {
                        app.show_message(&msg);
                    } else {
                        app.show_message("Download failed");
                    }
                } else {
                    match pending {
                        crate::ui::app::PendingRemoteOpen::Editor {
                            tmp_path,
                            panel_index,
                            remote_path,
                        } => {
                            let mut editor = crate::ui::file_editor::EditorState::new();
                            editor.set_syntax_colors(app.theme.syntax);
                            match editor.load_file(&tmp_path) {
                                Ok(_) => {
                                    editor.remote_origin =
                                        Some(crate::ui::file_editor::RemoteEditOrigin {
                                            panel_index,
                                            remote_path,
                                        });
                                    app.editor_state = Some(editor);
                                    app.current_screen = Screen::FileEditor;
                                }
                                Err(e) => {
                                    app.show_message(&format!("Cannot open file: {}", e));
                                }
                            }
                        }
                        crate::ui::app::PendingRemoteOpen::ImageViewer { tmp_path } => {
                            if !crate::ui::image_viewer::supports_true_color() {
                                app.pending_large_image = Some(tmp_path);
                                app.dialog = Some(crate::ui::app::Dialog {
                                    dialog_type: crate::ui::app::DialogType::TrueColorWarning,
                                    input: String::new(),
                                    cursor_pos: 0,
                                    message: "Terminal doesn't support true color. Open anyway?"
                                        .to_string(),
                                    completion: None,
                                    selected_button: 1,
                                    selection: None,
                                    use_md5: false,
                                });
                            } else {
                                app.image_viewer_state =
                                    Some(crate::ui::image_viewer::ImageViewerState::new(&tmp_path));
                                app.current_screen = Screen::ImageViewer;
                            }
                        }
                    }
                }
            } else {
                if let Some(msg) = progress_message {
                    app.show_message(&msg);
                }
                // Focus on created tar archive if applicable
                if let Some(archive_name) = app.pending_tar_archive.take() {
                    app.refresh_panels();
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| f.name == archive_name)
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                // Focus on extracted directory if applicable
                } else if let Some(extract_dir) = app.pending_extract_dir.take() {
                    app.refresh_panels();
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| f.name == extract_dir)
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                // Focus on first pasted file (by panel's sorted order) if applicable
                } else if let Some(paste_names) = app.pending_paste_focus.take() {
                    app.refresh_panels();
                    // Find the first file in the panel's sorted list that matches any pasted name
                    if let Some(idx) = app
                        .active_panel()
                        .files
                        .iter()
                        .position(|f| paste_names.contains(&f.name))
                    {
                        app.active_panel_mut().selected_index = idx;
                    }
                } else {
                    app.refresh_panels();
                }
                app.file_operation_progress = None;
                app.dialog = None;
            }
        }

        // Check for key events with timeout
        if event::poll(poll_timeout)? {
            // Block all input while remote spinner is active
            if app.remote_spinner.is_some() {
                let ev = event::read()?;
                if let Event::Key(key) = ev {
                    if key.code == KeyCode::Esc {
                        app.remote_spinner = None;
                        app.show_message("Connection cancelled");
                    }
                }
                continue;
            }
            match event::read()? {
                Event::Key(key) => {
                    match app.current_screen {
                        Screen::FilePanel => {
                            if handle_panel_input(app, key.code, key.modifiers) {
                                return Ok(());
                            }
                        }
                        Screen::FileViewer => {
                            ui::file_viewer::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::FileEditor => {
                            ui::file_editor::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::FileInfo => {
                            ui::file_info::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::ProcessManager => {
                            ui::process_manager::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::Help => {
                            if ui::help::handle_input(app, key.code) {
                                app.current_screen = Screen::FilePanel;
                            }
                        }
                        Screen::AIScreen => {
                            if let Some(ref mut state) = app.ai_state {
                                if ui::ai_screen::handle_input(
                                    state,
                                    key.code,
                                    key.modifiers,
                                    &app.keybindings,
                                ) {
                                    // Save session to file before leaving
                                    state.save_session_to_file();
                                    app.current_screen = Screen::FilePanel;
                                    app.ai_state = None;
                                    // Refresh panels in case AI modified files
                                    app.refresh_panels();
                                }
                            }
                        }
                        Screen::SystemInfo => {
                            if ui::system_info::handle_input(
                                &mut app.system_info_state,
                                key.code,
                                key.modifiers,
                                &app.keybindings,
                            ) {
                                app.current_screen = Screen::FilePanel;
                            }
                        }
                        Screen::ImageViewer => {
                            // 다이얼로그가 열려있으면 다이얼로그 입력 처리
                            if app.dialog.is_some() {
                                ui::dialogs::handle_dialog_input(app, key.code, key.modifiers);
                            } else {
                                ui::image_viewer::handle_input(app, key.code, key.modifiers);
                            }
                        }
                        Screen::SearchResult => {
                            let result = ui::search_result::handle_input(
                                &mut app.search_result_state,
                                key.code,
                                key.modifiers,
                                &app.keybindings,
                            );
                            match result {
                                Some(crate::keybindings::SearchResultAction::Open) => {
                                    app.goto_search_result();
                                }
                                Some(crate::keybindings::SearchResultAction::Close) => {
                                    app.search_result_state.active = false;
                                    app.current_screen = Screen::FilePanel;
                                }
                                _ => {}
                            }
                        }
                        Screen::DiffScreen => {
                            ui::diff_screen::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::DiffFileView => {
                            ui::diff_file_view::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::GitScreen => {
                            ui::git_screen::handle_input(app, key.code, key.modifiers);
                        }
                        Screen::DedupScreen => {
                            if let Some(ref mut state) = app.dedup_screen_state {
                                if ui::dedup_screen::handle_input(state, key.code, key.modifiers) {
                                    app.current_screen = Screen::FilePanel;
                                    app.dedup_screen_state = None;
                                    app.refresh_panels();
                                }
                            }
                        }
                    }
                }
                Event::Paste(text) => {
                    match app.current_screen {
                        Screen::AIScreen => {
                            if let Some(ref mut state) = app.ai_state {
                                ui::ai_screen::handle_paste(state, &text);
                            }
                        }
                        Screen::FilePanel => {
                            // AI mode with focus on AI panel
                            if app.is_ai_mode()
                                && app.ai_panel_index == Some(app.active_panel_index)
                            {
                                if let Some(ref mut state) = app.ai_state {
                                    ui::ai_screen::handle_paste(state, &text);
                                }
                            } else if app.dialog.is_some() {
                                ui::dialogs::handle_paste(app, &text);
                            } else if app.advanced_search_state.active {
                                ui::advanced_search::handle_paste(
                                    &mut app.advanced_search_state,
                                    &text,
                                );
                            }
                        }
                        Screen::FileEditor => {
                            ui::file_editor::handle_paste(app, &text);
                        }
                        Screen::ImageViewer => {
                            if app.dialog.is_some() {
                                ui::dialogs::handle_paste(app, &text);
                            }
                        }
                        Screen::GitScreen => {
                            if let Some(ref mut state) = app.git_screen_state {
                                ui::git_screen::handle_paste(state, &text);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn handle_panel_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    // AI 모드일 때: active_panel이 AI 패널 쪽이면 AI로 입력 전달, 아니면 파일 패널 조작
    if app.is_ai_mode() {
        let ai_has_focus = app.ai_panel_index == Some(app.active_panel_index);
        if app.keybindings.panel_action(code, modifiers) == Some(PanelAction::SwitchPanel) {
            // AI fullscreen 모드에서는 패널 전환 차단
            let ai_fullscreen = app.ai_state.as_ref().map_or(false, |s| s.ai_fullscreen);
            if !ai_fullscreen {
                app.switch_panel();
            }
            return false;
        }
        if ai_has_focus {
            if let Some(ref mut state) = app.ai_state {
                if ui::ai_screen::handle_input(state, code, modifiers, &app.keybindings) {
                    // AI 화면 종료 요청
                    app.close_ai_screen();
                }
            }
            return false;
        }
        // ai_has_focus가 false면 아래 파일 패널 로직으로 진행
    }

    // Handle advanced search dialog first
    if app.advanced_search_state.active {
        if let Some(criteria) = ui::advanced_search::handle_input(
            &mut app.advanced_search_state,
            code,
            modifiers,
            &app.keybindings,
        ) {
            app.execute_advanced_search(&criteria);
        }
        return false;
    }

    // Handle dialog input first
    if app.dialog.is_some() {
        return ui::dialogs::handle_dialog_input(app, code, modifiers);
    }

    // Look up action from keybindings
    if let Some(action) = app.keybindings.panel_action(code, modifiers) {
        match action {
            PanelAction::Quit => return true,
            PanelAction::MoveUp => app.move_cursor(-1),
            PanelAction::MoveDown => app.move_cursor(1),
            PanelAction::PageUp => app.move_cursor(-10),
            PanelAction::PageDown => app.move_cursor(10),
            PanelAction::GoHome => app.cursor_to_start(),
            PanelAction::GoEnd => app.cursor_to_end(),
            PanelAction::Open => app.enter_selected(),
            PanelAction::ParentDir => {
                if app.diff_first_panel.is_some() {
                    app.diff_first_panel = None;
                    app.show_message("Diff cancelled");
                } else {
                    app.go_to_parent();
                }
            }
            PanelAction::SwitchPanel => app.switch_panel(),
            PanelAction::SwitchPanelLeft => app.switch_panel_left(),
            PanelAction::SwitchPanelRight => app.switch_panel_right(),
            PanelAction::ToggleSelect => app.toggle_selection(),
            PanelAction::SelectAll => app.toggle_all_selection(),
            PanelAction::SelectByExtension => app.select_by_extension(),
            PanelAction::SelectUp => app.move_cursor_with_selection(-1),
            PanelAction::SelectDown => app.move_cursor_with_selection(1),
            PanelAction::Copy => app.clipboard_copy(),
            PanelAction::Cut => app.clipboard_cut(),
            PanelAction::Paste => app.clipboard_paste(),
            PanelAction::SortByName => app.toggle_sort_by_name(),
            PanelAction::SortByType => app.toggle_sort_by_type(),
            PanelAction::SortBySize => app.toggle_sort_by_size(),
            PanelAction::SortByDate => app.toggle_sort_by_date(),
            PanelAction::Help => app.show_help(),
            PanelAction::FileInfo => app.show_file_info(),
            PanelAction::Edit => app.edit_file(),
            PanelAction::Mkdir => app.show_mkdir_dialog(),
            PanelAction::Mkfile => app.show_mkfile_dialog(),
            PanelAction::Delete => app.show_delete_dialog(),
            PanelAction::ProcessManager => app.show_process_manager(),
            PanelAction::Rename => app.show_rename_dialog(),
            PanelAction::Tar => app.show_tar_dialog(),
            PanelAction::Search => app.show_search_dialog(),
            PanelAction::GoToPath => app.show_goto_dialog(),
            PanelAction::AddPanel => app.add_panel(),
            PanelAction::GoHomeDir => app.goto_home(),
            PanelAction::Refresh => app.refresh_panels(),
            PanelAction::GitLogDiff => app.show_git_log_diff_dialog(),
            PanelAction::StartDiff => app.start_diff(),
            PanelAction::ClosePanel => app.close_panel(),
            PanelAction::AIScreen => app.show_ai_screen(),
            PanelAction::Settings => app.show_settings_dialog(),
            PanelAction::GitScreen => app.show_git_screen(),
            PanelAction::ToggleBookmark => app.toggle_bookmark(),
            PanelAction::SetHandler => app.show_handler_dialog(),
            PanelAction::EncryptAll => app.show_encrypt_dialog(),
            PanelAction::DecryptAll => app.show_decrypt_dialog(),
            PanelAction::RemoveDuplicates => app.show_dedup_screen(),
            #[cfg(target_os = "macos")]
            PanelAction::OpenInFinder => app.open_in_finder(),
            #[cfg(target_os = "macos")]
            PanelAction::OpenInVSCode => app.open_in_vscode(),
        }
    }
    false
}
