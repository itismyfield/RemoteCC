use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use regex::Regex;
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use unicode_width::UnicodeWidthChar;

use super::{
    app::{App, Screen},
    syntax::{Language, SyntaxHighlighter},
    theme::Theme,
};
use crate::keybindings::EditorAction;

/// Undo/Redo 액션 유형
#[derive(Debug, Clone)]
pub enum EditAction {
    Insert {
        line: usize,
        col: usize,
        text: String,
    },
    Delete {
        line: usize,
        col: usize,
        text: String,
    },
    InsertLine {
        line: usize,
        content: String,
    },
    DeleteLine {
        line: usize,
        content: String,
    },
    MergeLine {
        line: usize,
        col: usize,
    },
    SplitLine {
        line: usize,
        col: usize,
    },
    Replace {
        line: usize,
        old_content: String,
        new_content: String,
    },
    SwapLines {
        line1: usize,
        line2: usize,
    },
    Batch {
        actions: Vec<EditAction>,
    },
}

/// 선택 영역
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

#[derive(Debug)]
pub struct RemoteEditOrigin {
    pub panel_index: usize,
    pub remote_path: String,
}

impl Selection {
    /// 블록 커서 선택: 현재 커서 위치의 문자를 포함하기 위해 end_col = col + 1
    pub fn new(line: usize, col: usize) -> Self {
        Self {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col + 1,
        }
    }

    /// 정규화된 선택 영역 (시작이 항상 끝보다 앞)
    /// 블록 커서 선택: start_col = anchor, end_col = cursor + 1
    pub fn normalized(&self) -> (usize, usize, usize, usize) {
        if self.start_line == self.end_line {
            // 단일 줄: 블록 커서 선택
            // anchor와 cursor 사이의 모든 문자 선택 (양쪽 끝 포함)
            let anchor = self.start_col;
            let cursor = self.end_col.saturating_sub(1);
            let min_col = anchor.min(cursor);
            let max_col = anchor.max(cursor) + 1;
            (self.start_line, min_col, self.end_line, max_col)
        } else if self.start_line < self.end_line {
            // 정방향 (아래로 선택): anchor -> cursor
            (self.start_line, self.start_col, self.end_line, self.end_col)
        } else {
            // 역방향 (위로 선택): cursor <- anchor
            // cursor 위치 문자와 anchor 위치 문자 모두 포함
            (
                self.end_line,
                self.end_col.saturating_sub(1),
                self.start_line,
                self.start_col + 1,
            )
        }
    }

    pub fn is_empty(&self) -> bool {
        self.start_line == self.end_line && self.start_col == self.end_col
    }
}

/// 찾기/바꾸기 모드
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindReplaceMode {
    None,
    Find,
    Replace,
}

/// 찾기/바꾸기 옵션
#[derive(Debug, Clone, Default)]
pub struct FindReplaceOptions {
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub whole_word: bool,
}

/// Default maximum memory for undo/redo stacks (50MB)
const DEFAULT_MAX_UNDO_MEMORY: usize = 50 * 1024 * 1024;

/// 편집기 상태
#[derive(Debug)]
pub struct EditorState {
    pub file_path: PathBuf,
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub scroll: usize,
    pub horizontal_scroll: usize,
    pub modified: bool,
    pub original_lines: Vec<String>,

    // Undo/Redo
    pub undo_stack: VecDeque<EditAction>,
    pub redo_stack: VecDeque<EditAction>,
    pub max_undo_size: usize,

    // Memory tracking for undo/redo
    undo_memory_usage: usize,
    redo_memory_usage: usize,
    max_undo_memory: usize,

    // 선택
    pub selection: Option<Selection>,
    pub clipboard: String,

    // 찾기/바꾸기
    pub find_mode: FindReplaceMode,
    pub find_input: String,
    pub find_cursor_pos: usize,
    pub replace_input: String,
    pub replace_cursor_pos: usize,
    pub find_term: String,
    pub find_options: FindReplaceOptions,
    pub match_positions: Vec<(usize, usize, usize)>,
    pub current_match: usize,
    pub input_focus: usize,         // 0: find, 1: replace
    pub find_error: Option<String>, // 정규식 에러 메시지

    // Goto
    pub goto_mode: bool,
    pub goto_input: String,

    // 문법 강조
    pub language: Language,
    pub highlighter: Option<SyntaxHighlighter>,
    pub syntax_colors: crate::ui::theme::SyntaxColors,

    // 설정
    pub auto_indent: bool,
    pub tab_size: usize,
    pub use_tabs: bool,
    #[allow(dead_code)]
    pub show_whitespace: bool,

    // 괄호 매칭
    pub matching_bracket: Option<(usize, usize)>,

    // 다중 커서 (Ctrl+D)
    pub cursors: Vec<(usize, usize)>, // (line, col) 추가 커서들
    pub last_word_selection: Option<String>, // 마지막 선택된 단어 (Ctrl+D용)

    // Esc 두 번 누르기 상태
    pub pending_exit: bool,

    // Word wrap 모드
    pub word_wrap: bool,

    // 화면 크기 (렌더링 시 업데이트)
    pub visible_height: usize,
    pub visible_width: usize,

    // 상태 메시지 (일시적으로 표시)
    pub message: Option<String>,
    pub message_timer: u8,

    // 원격 파일 편집 원본 정보
    pub remote_origin: Option<RemoteEditOrigin>,
}

impl EditorState {
    /// Estimate memory size of an EditAction
    fn estimate_action_size(action: &EditAction) -> usize {
        match action {
            EditAction::Insert { text, .. } => text.len() + 32,
            EditAction::Delete { text, .. } => text.len() + 32,
            EditAction::InsertLine { content, .. } => content.len() + 24,
            EditAction::DeleteLine { content, .. } => content.len() + 24,
            EditAction::MergeLine { .. } => 24,
            EditAction::SplitLine { .. } => 24,
            EditAction::Replace {
                old_content,
                new_content,
                ..
            } => old_content.len() + new_content.len() + 32,
            EditAction::SwapLines { .. } => 24,
            EditAction::Batch { actions } => {
                actions
                    .iter()
                    .map(Self::estimate_action_size)
                    .sum::<usize>()
                    + 24
            }
        }
    }

    pub fn new() -> Self {
        Self {
            file_path: PathBuf::new(),
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll: 0,
            horizontal_scroll: 0,
            modified: false,
            original_lines: vec![String::new()],
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_undo_size: 1000,
            undo_memory_usage: 0,
            redo_memory_usage: 0,
            max_undo_memory: DEFAULT_MAX_UNDO_MEMORY,
            selection: None,
            clipboard: String::new(),
            find_mode: FindReplaceMode::None,
            find_input: String::new(),
            find_cursor_pos: 0,
            replace_input: String::new(),
            replace_cursor_pos: 0,
            find_term: String::new(),
            find_options: FindReplaceOptions::default(),
            match_positions: Vec::new(),
            current_match: 0,
            input_focus: 0,
            find_error: None,
            goto_mode: false,
            goto_input: String::new(),
            language: Language::Plain,
            highlighter: None,
            syntax_colors: crate::ui::theme::Theme::default().syntax,
            auto_indent: true,
            tab_size: 4,
            use_tabs: false,
            show_whitespace: false,
            matching_bracket: None,
            cursors: Vec::new(),
            last_word_selection: None,
            pending_exit: false,
            word_wrap: false,
            visible_height: 20, // 기본값, 렌더링 시 업데이트됨
            visible_width: 80,  // 기본값, 렌더링 시 업데이트됨
            message: None,
            message_timer: 0,
            remote_origin: None,
        }
    }

    /// 메시지 설정 (지정된 프레임 수 동안 표시)
    pub fn set_message(&mut self, msg: impl Into<String>, duration: u8) {
        self.message = Some(msg.into());
        self.message_timer = duration;
    }

    /// 메시지 클리어
    #[allow(dead_code)]
    pub fn clear_message(&mut self) {
        self.message = None;
        self.message_timer = 0;
    }

    /// 테마의 syntax colors 설정
    pub fn set_syntax_colors(&mut self, colors: crate::ui::theme::SyntaxColors) {
        self.syntax_colors = colors;
        // 하이라이터가 있으면 새 색상으로 재생성
        if self.highlighter.is_some() {
            self.highlighter = Some(SyntaxHighlighter::new(self.language, self.syntax_colors));
        }
    }

    /// 버퍼 위치(char index) -> Visual Column
    /// TAB 문자는 tab_size 단위로 정렬된 위치까지 확장됨
    /// Wide characters (한글 등)는 2칸으로 계산됨
    pub fn char_to_visual(&self, line: &str, char_idx: usize) -> usize {
        let mut visual_col = 0;
        for (i, c) in line.chars().enumerate() {
            if i >= char_idx {
                break;
            }
            if c == '\t' {
                // TAB은 다음 tab_size 배수 위치까지 확장
                visual_col = (visual_col / self.tab_size + 1) * self.tab_size;
            } else {
                visual_col += c.width().unwrap_or(1);
            }
        }
        visual_col
    }

    /// Visual Column -> 버퍼 위치(char index)
    /// 주어진 visual column에 해당하는 문자 인덱스를 반환
    /// Wide characters (한글 등)는 2칸으로 계산됨
    #[allow(dead_code)]
    pub fn visual_to_char(&self, line: &str, visual_col: usize) -> usize {
        let mut current_visual = 0;
        for (i, c) in line.chars().enumerate() {
            if current_visual >= visual_col {
                return i;
            }
            if c == '\t' {
                current_visual = (current_visual / self.tab_size + 1) * self.tab_size;
            } else {
                current_visual += c.width().unwrap_or(1);
            }
        }
        line.chars().count()
    }

    /// 현재 커서의 visual column 반환
    pub fn cursor_visual_col(&self) -> usize {
        if self.cursor_line < self.lines.len() {
            self.char_to_visual(&self.lines[self.cursor_line], self.cursor_col)
        } else {
            self.cursor_col
        }
    }

    /// TAB을 visual column 기반으로 확장한 문자열 생성
    /// 각 TAB은 현재 visual column 위치에서 다음 tab_size 배수까지 스페이스로 확장됨
    /// Wide characters (한글 등)는 2칸으로 계산됨
    #[allow(dead_code)]
    pub fn expand_tabs_visual(&self, line: &str) -> String {
        let mut result = String::new();
        let mut visual_col = 0;
        for c in line.chars() {
            if c == '\t' {
                let next_tab_stop = (visual_col / self.tab_size + 1) * self.tab_size;
                let spaces = next_tab_stop - visual_col;
                result.push_str(&" ".repeat(spaces));
                visual_col = next_tab_stop;
            } else {
                result.push(c);
                visual_col += c.width().unwrap_or(1);
            }
        }
        result
    }

    /// 원본 문자 인덱스 -> 확장된 visual 인덱스 매핑 생성
    /// 반환값: (확장된 문자열, 원본 인덱스 배열)
    /// 원본 인덱스 배열: expanded_line의 각 visual 위치에 해당하는 원본 char index
    /// Wide characters (한글 등)는 2칸으로 계산됨
    pub fn expand_tabs_with_mapping(&self, line: &str) -> (String, Vec<usize>) {
        let mut result = String::new();
        let mut visual_to_orig: Vec<usize> = Vec::new();
        let mut visual_col = 0;
        for (char_idx, c) in line.chars().enumerate() {
            if c == '\t' {
                let next_tab_stop = (visual_col / self.tab_size + 1) * self.tab_size;
                let spaces = next_tab_stop - visual_col;
                for _ in 0..spaces {
                    result.push(' ');
                    visual_to_orig.push(char_idx);
                }
                visual_col = next_tab_stop;
            } else {
                result.push(c);
                let char_width = c.width().unwrap_or(1);
                for _ in 0..char_width {
                    visual_to_orig.push(char_idx);
                }
                visual_col += char_width;
            }
        }
        (result, visual_to_orig)
    }

    /// Maximum file size for editing (50MB - more restrictive than viewer)
    const MAX_EDIT_FILE_SIZE: u64 = 50 * 1024 * 1024;

    /// 파일 로드
    pub fn load_file(&mut self, path: &PathBuf) -> Result<(), String> {
        // Check file size before loading to prevent memory exhaustion
        if path.exists() {
            let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
            if metadata.len() > Self::MAX_EDIT_FILE_SIZE {
                return Err(format!(
                    "File too large for editing ({:.1} MB). Maximum size is {} MB. Use the viewer instead.",
                    metadata.len() as f64 / 1024.0 / 1024.0,
                    Self::MAX_EDIT_FILE_SIZE / 1024 / 1024
                ));
            }
        }

        self.file_path = path.clone();
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.scroll = 0;
        self.horizontal_scroll = 0;
        self.modified = false;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_memory_usage = 0;
        self.redo_memory_usage = 0;
        self.selection = None;
        self.find_mode = FindReplaceMode::None;
        self.find_error = None;

        // 파일 읽기
        match fs::read_to_string(path) {
            Ok(content) => {
                self.lines = content.lines().map(String::from).collect();
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
            }
            Err(_) => {
                // 새 파일
                self.lines = vec![String::new()];
            }
        }

        // 원본 상태 저장
        self.original_lines = self.lines.clone();

        // 언어 감지
        self.language = Language::from_extension(path);
        self.highlighter = Some(SyntaxHighlighter::new(self.language, self.syntax_colors));

        Ok(())
    }

    /// 파일 저장
    /// Security: Preserves original file permissions and uses atomic write
    pub fn save_file(&mut self) -> Result<(), String> {
        // Resolve symlink to actual file path to avoid replacing symlink with regular file
        let is_symlink = fs::symlink_metadata(&self.file_path)
            .map(|m| m.is_symlink())
            .unwrap_or(false);

        let actual_path = if is_symlink {
            fs::canonicalize(&self.file_path)
                .map_err(|e| format!("Failed to resolve symlink: {}", e))?
        } else {
            self.file_path.clone()
        };

        // Save original permissions before writing
        #[cfg(unix)]
        let original_perms = fs::metadata(&actual_path).map(|m| m.permissions()).ok();

        let content = self.lines.join("\n");

        // Use atomic write: write to temp file, then rename
        let temp_path = actual_path.with_extension("tmp");

        // Write to temporary file
        fs::write(&temp_path, &content)
            .map_err(|e| format!("Failed to write temporary file: {}", e))?;

        // Restore original permissions on temp file before rename
        #[cfg(unix)]
        if let Some(perms) = original_perms {
            let _ = fs::set_permissions(&temp_path, perms);
        }

        // Atomic rename (on same filesystem)
        fs::rename(&temp_path, &actual_path).map_err(|e| {
            // Clean up temp file on failure
            let _ = fs::remove_file(&temp_path);
            format!("Failed to save file: {}", e)
        })?;

        self.modified = false;
        self.original_lines = self.lines.clone();
        Ok(())
    }

    /// 현재 상태와 원본을 비교하여 modified 플래그 업데이트
    pub fn update_modified(&mut self) {
        self.modified = self.lines != self.original_lines;
    }

    /// Undo 액션 추가 (with memory limit enforcement)
    pub fn push_undo(&mut self, action: EditAction) {
        // Clear redo stack and its memory tracking
        self.redo_stack.clear();
        self.redo_memory_usage = 0;

        let action_size = Self::estimate_action_size(&action);

        // Enforce memory limit by removing oldest actions
        while self.undo_memory_usage + action_size > self.max_undo_memory
            && !self.undo_stack.is_empty()
        {
            if let Some(old_action) = self.undo_stack.pop_front() {
                self.undo_memory_usage = self
                    .undo_memory_usage
                    .saturating_sub(Self::estimate_action_size(&old_action));
            }
        }

        // Also enforce count limit
        while self.undo_stack.len() >= self.max_undo_size {
            if let Some(old_action) = self.undo_stack.pop_front() {
                self.undo_memory_usage = self
                    .undo_memory_usage
                    .saturating_sub(Self::estimate_action_size(&old_action));
            }
        }

        self.undo_memory_usage += action_size;
        self.undo_stack.push_back(action);
        self.modified = true;
    }

    /// Undo 실행
    pub fn undo(&mut self) {
        if let Some(action) = self.undo_stack.pop_back() {
            let action_size = Self::estimate_action_size(&action);
            self.undo_memory_usage = self.undo_memory_usage.saturating_sub(action_size);

            let reverse = self.reverse_action(&action);
            self.apply_action(&reverse, false);

            self.redo_memory_usage += action_size;
            self.redo_stack.push_back(action);
            self.update_modified();
        }
    }

    /// Redo 실행
    pub fn redo(&mut self) {
        if let Some(action) = self.redo_stack.pop_back() {
            let action_size = Self::estimate_action_size(&action);
            self.redo_memory_usage = self.redo_memory_usage.saturating_sub(action_size);

            self.apply_action(&action, false);

            self.undo_memory_usage += action_size;
            self.undo_stack.push_back(action);
            self.update_modified();
        }
    }

    /// 액션 역순 생성
    fn reverse_action(&self, action: &EditAction) -> EditAction {
        match action {
            EditAction::Insert { line, col, text } => EditAction::Delete {
                line: *line,
                col: *col,
                text: text.clone(),
            },
            EditAction::Delete { line, col, text } => EditAction::Insert {
                line: *line,
                col: *col,
                text: text.clone(),
            },
            EditAction::InsertLine { line, content } => EditAction::DeleteLine {
                line: *line,
                content: content.clone(),
            },
            EditAction::DeleteLine { line, content } => EditAction::InsertLine {
                line: *line,
                content: content.clone(),
            },
            EditAction::MergeLine { line, col } => EditAction::SplitLine {
                line: *line,
                col: *col,
            },
            EditAction::SplitLine { line, col } => EditAction::MergeLine {
                line: *line,
                col: *col,
            },
            EditAction::Replace {
                line,
                old_content,
                new_content,
            } => EditAction::Replace {
                line: *line,
                old_content: new_content.clone(),
                new_content: old_content.clone(),
            },
            EditAction::SwapLines { line1, line2 } => EditAction::SwapLines {
                line1: *line1,
                line2: *line2,
            },
            EditAction::Batch { actions } => EditAction::Batch {
                actions: actions
                    .iter()
                    .rev()
                    .map(|a| self.reverse_action(a))
                    .collect(),
            },
        }
    }

    /// 액션 적용
    fn apply_action(&mut self, action: &EditAction, _record: bool) {
        match action {
            EditAction::Insert { line, col, text } => {
                if *line < self.lines.len() {
                    let line_content = &mut self.lines[*line];
                    let mut chars: Vec<char> = line_content.chars().collect();
                    for (i, c) in text.chars().enumerate() {
                        if *col + i <= chars.len() {
                            chars.insert(*col + i, c);
                        }
                    }
                    *line_content = chars.into_iter().collect();
                }
            }
            EditAction::Delete { line, col, text } => {
                if *line < self.lines.len() {
                    let line_content = &mut self.lines[*line];
                    let mut chars: Vec<char> = line_content.chars().collect();
                    for _ in 0..text.chars().count() {
                        if *col < chars.len() {
                            chars.remove(*col);
                        }
                    }
                    *line_content = chars.into_iter().collect();
                }
            }
            EditAction::InsertLine { line, content } => {
                if *line <= self.lines.len() {
                    self.lines.insert(*line, content.clone());
                }
            }
            EditAction::DeleteLine { line, .. } => {
                if *line < self.lines.len() && self.lines.len() > 1 {
                    self.lines.remove(*line);
                }
            }
            EditAction::MergeLine { line, .. } => {
                if *line + 1 < self.lines.len() {
                    let next_line = self.lines.remove(*line + 1);
                    self.lines[*line].push_str(&next_line);
                }
            }
            EditAction::SplitLine { line, col } => {
                if *line < self.lines.len() {
                    let content = &self.lines[*line];
                    let chars: Vec<char> = content.chars().collect();
                    let before: String = chars[..*col.min(&chars.len())].iter().collect();
                    let after: String = chars[*col.min(&chars.len())..].iter().collect();
                    self.lines[*line] = before;
                    self.lines.insert(*line + 1, after);
                }
            }
            EditAction::Replace {
                line, new_content, ..
            } => {
                if *line < self.lines.len() {
                    self.lines[*line] = new_content.clone();
                }
            }
            EditAction::SwapLines { line1, line2 } => {
                if *line1 < self.lines.len() && *line2 < self.lines.len() {
                    self.lines.swap(*line1, *line2);
                }
            }
            EditAction::Batch { actions } => {
                for a in actions {
                    self.apply_action(a, false);
                }
            }
        }
    }

    /// 문자 삽입
    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();

        let action = EditAction::Insert {
            line: self.cursor_line,
            col: self.cursor_col,
            text: c.to_string(),
        };

        let line = &mut self.lines[self.cursor_line];
        let mut chars: Vec<char> = line.chars().collect();
        chars.insert(self.cursor_col, c);
        *line = chars.into_iter().collect();
        self.cursor_col += 1;

        self.push_undo(action);
        self.update_scroll();
    }

    /// 문자열 삽입 (단일 Undo 액션으로 처리)
    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }

        // 선택 영역 삭제 (별도 Undo 액션으로 처리됨)
        self.delete_selection();

        // 시작 위치 저장
        let start_line = self.cursor_line;
        let start_col = self.cursor_col;

        // 줄바꿈으로 분리
        let parts: Vec<&str> = s.split('\n').collect();

        if parts.len() == 1 {
            // 단일 줄 삽입 (줄바꿈 없음)
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            for (i, c) in s.chars().enumerate() {
                chars.insert(self.cursor_col + i, c);
            }
            *line = chars.into_iter().collect();
            self.cursor_col += s.chars().count();

            self.push_undo(EditAction::Insert {
                line: start_line,
                col: start_col,
                text: s.to_string(),
            });
        } else {
            // 여러 줄 삽입
            let mut actions = Vec::new();

            // 현재 줄의 커서 이후 부분 저장
            let current_line = &self.lines[self.cursor_line];
            let chars: Vec<char> = current_line.chars().collect();
            let before: String = chars[..self.cursor_col].iter().collect();
            let after: String = chars[self.cursor_col..].iter().collect();

            // 첫 부분 + 첫 번째 삽입 텍스트
            self.lines[self.cursor_line] = format!("{}{}", before, parts[0]);

            // 중간 줄들 삽입
            for (i, part) in parts.iter().enumerate().skip(1).take(parts.len() - 2) {
                let new_line = part.to_string();
                self.lines.insert(self.cursor_line + i, new_line.clone());
                actions.push(EditAction::InsertLine {
                    line: self.cursor_line + i,
                    content: new_line,
                });
            }

            // 마지막 줄 (마지막 삽입 텍스트 + 원래 커서 이후 부분)
            let last_idx = parts.len() - 1;
            let last_line = format!("{}{}", parts[last_idx], after);
            self.lines
                .insert(self.cursor_line + last_idx, last_line.clone());
            actions.push(EditAction::InsertLine {
                line: self.cursor_line + last_idx,
                content: last_line,
            });

            // 첫 줄 수정 기록
            actions.insert(
                0,
                EditAction::Replace {
                    line: start_line,
                    old_content: format!("{}{}", before, after),
                    new_content: format!("{}{}", before, parts[0]),
                },
            );

            // 커서 위치 업데이트
            self.cursor_line = start_line + last_idx;
            self.cursor_col = parts[last_idx].chars().count();

            self.push_undo(EditAction::Batch { actions });
        }

        self.update_scroll();
    }

    /// 탭 삽입
    pub fn insert_tab(&mut self) {
        let indent = if self.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.tab_size)
        };
        self.insert_str(&indent);
    }

    /// 새 줄 삽입
    pub fn insert_newline(&mut self) {
        self.delete_selection();

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let before: String = chars[..self.cursor_col.min(chars.len())].iter().collect();
        let after: String = chars[self.cursor_col.min(chars.len())..].iter().collect();

        // 자동 들여쓰기
        let indent = if self.auto_indent {
            let leading_ws: String = before.chars().take_while(|c| c.is_whitespace()).collect();
            leading_ws
        } else {
            String::new()
        };

        let action = EditAction::SplitLine {
            line: self.cursor_line,
            col: self.cursor_col,
        };

        self.lines[self.cursor_line] = before;
        self.lines
            .insert(self.cursor_line + 1, format!("{}{}", indent, after));
        self.cursor_line += 1;
        self.cursor_col = indent.len();

        self.push_undo(action);
        self.update_scroll();
    }

    /// 뒤로 삭제 (Backspace)
    pub fn delete_backward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }

        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            let deleted = chars.remove(self.cursor_col - 1);
            *line = chars.into_iter().collect();

            let action = EditAction::Delete {
                line: self.cursor_line,
                col: self.cursor_col - 1,
                text: deleted.to_string(),
            };

            self.cursor_col -= 1;
            self.push_undo(action);
        } else if self.cursor_line > 0 {
            // 이전 줄과 병합
            let current_line = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].chars().count();
            self.lines[self.cursor_line].push_str(&current_line);

            let action = EditAction::MergeLine {
                line: self.cursor_line,
                col: self.cursor_col,
            };

            self.push_undo(action);
        }
        self.update_scroll();
    }

    /// 앞으로 삭제 (Delete)
    pub fn delete_forward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }

        let line_len = self.lines[self.cursor_line].chars().count();
        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_line];
            let mut chars: Vec<char> = line.chars().collect();
            let deleted = chars.remove(self.cursor_col);
            *line = chars.into_iter().collect();

            let action = EditAction::Delete {
                line: self.cursor_line,
                col: self.cursor_col,
                text: deleted.to_string(),
            };

            self.push_undo(action);
        } else if self.cursor_line + 1 < self.lines.len() {
            // 다음 줄과 병합
            let next_line = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next_line);

            let action = EditAction::MergeLine {
                line: self.cursor_line,
                col: self.cursor_col,
            };

            self.push_undo(action);
        }
    }

    /// 선택 영역 삭제
    pub fn delete_selection(&mut self) {
        let sel = match self.selection.take() {
            Some(s) if !s.is_empty() => s,
            _ => return,
        };

        let (start_line, start_col, end_line, end_col) = sel.normalized();

        if start_line == end_line {
            // 같은 줄 내 삭제
            let line = &mut self.lines[start_line];
            let chars: Vec<char> = line.chars().collect();
            let deleted: String = chars[start_col..end_col].iter().collect();
            let new_line: String = chars[..start_col]
                .iter()
                .chain(chars[end_col..].iter())
                .collect();
            *line = new_line;

            self.push_undo(EditAction::Delete {
                line: start_line,
                col: start_col,
                text: deleted,
            });
        } else {
            // 여러 줄 삭제
            let mut actions = Vec::new();

            // 시작 줄 원본 저장
            let first_line_original = self.lines[start_line].clone();

            // 시작 줄 처리
            let first_chars: Vec<char> = self.lines[start_line].chars().collect();
            let first_part: String = first_chars[..start_col].iter().collect();

            // 끝 줄 처리
            let last_chars: Vec<char> = self.lines[end_line].chars().collect();
            let last_part: String = last_chars[end_col..].iter().collect();

            // 삭제할 줄들 저장 (undo용, 역순으로)
            for i in (start_line + 1..=end_line).rev() {
                actions.push(EditAction::DeleteLine {
                    line: i,
                    content: self.lines[i].clone(),
                });
            }

            // 줄 병합
            let new_first_line = format!("{}{}", first_part, last_part);

            // 시작 줄 수정을 Replace로 저장 (Undo 시 원본 복원을 위해)
            actions.push(EditAction::Replace {
                line: start_line,
                old_content: first_line_original,
                new_content: new_first_line.clone(),
            });

            self.lines[start_line] = new_first_line;

            // 중간 줄들 제거
            for _ in start_line + 1..=end_line {
                if start_line + 1 < self.lines.len() {
                    self.lines.remove(start_line + 1);
                }
            }

            self.push_undo(EditAction::Batch { actions });
        }

        self.cursor_line = start_line;
        self.cursor_col = start_col;
        self.update_scroll();
    }

    /// 선택된 텍스트 가져오기
    pub fn get_selected_text(&self) -> String {
        let sel = match &self.selection {
            Some(s) if !s.is_empty() => s,
            _ => return String::new(),
        };

        let (start_line, start_col, end_line, end_col) = sel.normalized();

        if start_line == end_line {
            let chars: Vec<char> = self.lines[start_line].chars().collect();
            chars[start_col..end_col].iter().collect()
        } else {
            let mut result = String::new();

            // 첫 줄
            let first_chars: Vec<char> = self.lines[start_line].chars().collect();
            result.push_str(&first_chars[start_col..].iter().collect::<String>());

            // 중간 줄
            for i in start_line + 1..end_line {
                result.push('\n');
                result.push_str(&self.lines[i]);
            }

            // 마지막 줄
            result.push('\n');
            let last_chars: Vec<char> = self.lines[end_line].chars().collect();
            result.push_str(&last_chars[..end_col].iter().collect::<String>());

            result
        }
    }

    /// 복사 (선택 없으면 줄 전체)
    pub fn copy(&mut self) {
        if self.selection.is_some() && !self.selection.as_ref().unwrap().is_empty() {
            self.clipboard = self.get_selected_text();
        } else {
            // 줄 전체 복사
            self.clipboard = self.lines[self.cursor_line].clone() + "\n";
        }
    }

    /// 잘라내기
    #[allow(dead_code)]
    pub fn cut(&mut self) {
        self.clipboard = self.get_selected_text();
        self.delete_selection();
    }

    /// 붙여넣기
    pub fn paste(&mut self) {
        if !self.clipboard.is_empty() {
            let text = self.clipboard.clone();
            self.insert_str(&text);
        }
    }

    /// 전체 선택
    pub fn select_all(&mut self) {
        if !self.lines.is_empty() {
            let last_line = self.lines.len() - 1;
            let last_col = self.lines[last_line].chars().count();
            self.selection = Some(Selection {
                start_line: 0,
                start_col: 0,
                end_line: last_line,
                end_col: last_col,
            });
            self.cursor_line = last_line;
            self.cursor_col = last_col;
        }
    }

    /// 줄 복제
    pub fn duplicate_line(&mut self) {
        let line_content = self.lines[self.cursor_line].clone();
        self.lines
            .insert(self.cursor_line + 1, line_content.clone());
        self.cursor_line += 1;

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: line_content,
        });
        self.selection = None;
        self.modified = true;
        self.update_scroll();
    }

    /// 줄 삭제
    pub fn delete_line(&mut self) {
        if self.lines.len() > 1 {
            let content = self.lines.remove(self.cursor_line);

            self.push_undo(EditAction::DeleteLine {
                line: self.cursor_line,
                content,
            });

            if self.cursor_line >= self.lines.len() {
                self.cursor_line = self.lines.len() - 1;
            }
            self.cursor_col = self
                .cursor_col
                .min(self.lines[self.cursor_line].chars().count());
            self.selection = None;
            self.modified = true;
            self.update_scroll();
        }
    }

    /// 줄 위로 이동
    pub fn move_line_up(&mut self) {
        if self.cursor_line > 0 {
            let line1 = self.cursor_line - 1;
            let line2 = self.cursor_line;
            self.lines.swap(line1, line2);
            self.push_undo(EditAction::SwapLines { line1, line2 });
            self.cursor_line -= 1;
            self.update_scroll();
        }
    }

    /// 줄 아래로 이동
    pub fn move_line_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            let line1 = self.cursor_line;
            let line2 = self.cursor_line + 1;
            self.lines.swap(line1, line2);
            self.push_undo(EditAction::SwapLines { line1, line2 });
            self.cursor_line += 1;
            self.update_scroll();
        }
    }

    /// 커서 이동
    pub fn move_cursor(&mut self, line_delta: i32, col_delta: i32, extend_selection: bool) {
        if extend_selection {
            if self.selection.is_none() {
                self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
            }
        } else {
            self.selection = None;
        }

        // 줄 이동
        let new_line = (self.cursor_line as i32 + line_delta)
            .max(0)
            .min(self.lines.len().saturating_sub(1) as i32) as usize;

        if new_line != self.cursor_line {
            self.cursor_line = new_line;
            let line_len = self.lines[self.cursor_line].chars().count();
            self.cursor_col = self.cursor_col.min(line_len);
        }

        // 열 이동
        if col_delta != 0 {
            let line_len = self.lines[self.cursor_line].chars().count();
            let new_col = (self.cursor_col as i32 + col_delta).max(0) as usize;

            if new_col > line_len && col_delta > 0 && self.cursor_line + 1 < self.lines.len() {
                // 다음 줄로 이동
                self.cursor_line += 1;
                self.cursor_col = 0;
            } else if self.cursor_col == 0 && col_delta < 0 && self.cursor_line > 0 {
                // 줄 시작에서 왼쪽으로 이동 -> 이전 줄 끝으로
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].chars().count();
            } else {
                self.cursor_col = new_col.min(line_len);
            }
        }

        // 선택 영역 업데이트 (블록 커서 선택: 현재 커서 위치의 문자 포함)
        if let Some(ref mut sel) = self.selection {
            sel.end_line = self.cursor_line;
            let line_len = self.lines[self.cursor_line].chars().count();
            sel.end_col = (self.cursor_col + 1).min(line_len);
        }

        self.update_scroll();
        self.find_matching_bracket();
    }

    /// 줄 시작으로
    pub fn move_to_line_start(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection.is_none() {
                self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
            }
        } else {
            self.selection = None;
        }

        if extend_selection {
            // 선택 모드: 항상 줄 맨 처음으로
            self.cursor_col = 0;
        } else {
            // 이동 모드: 첫 번째 비공백 문자로 이동, 이미 거기 있으면 줄 시작으로
            let line = &self.lines[self.cursor_line];
            let first_non_ws = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);

            if self.cursor_col == first_non_ws || self.cursor_col == 0 {
                self.cursor_col = if self.cursor_col == 0 {
                    first_non_ws
                } else {
                    0
                };
            } else {
                self.cursor_col = first_non_ws;
            }
        }

        // 블록 커서 선택: 현재 커서 위치의 문자 포함
        if let Some(ref mut sel) = self.selection {
            let line_len = self.lines[self.cursor_line].chars().count();
            sel.end_col = (self.cursor_col + 1).min(line_len);
        }
    }

    /// 줄 끝으로
    pub fn move_to_line_end(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection.is_none() {
                self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
            }
        } else {
            self.selection = None;
        }

        self.cursor_col = self.lines[self.cursor_line].chars().count();

        if let Some(ref mut sel) = self.selection {
            sel.end_col = self.cursor_col;
        }
    }

    /// Word wrap 모드에서 논리적 줄이 차지하는 시각적 행 수 계산
    fn count_wrapped_rows(&self, line_idx: usize) -> usize {
        if line_idx >= self.lines.len() || self.visible_width == 0 {
            return 1;
        }
        let (expanded, _) = self.expand_tabs_with_mapping(&self.lines[line_idx]);
        if expanded.is_empty() {
            return 1;
        }
        Self::compute_wrap_segments(&expanded, self.visible_width).len()
    }

    /// 확장된 줄을 visual column 기준으로 세그먼트로 분할
    /// 반환: 각 세그먼트의 시작 visual column 위치 목록
    fn compute_wrap_segments(expanded_line: &str, max_width: usize) -> Vec<usize> {
        if expanded_line.is_empty() || max_width == 0 {
            return vec![0];
        }
        let mut segments = vec![0usize];
        let mut current_width = 0usize;
        let mut seg_start = 0usize;
        for ch in expanded_line.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if current_width + w > max_width && current_width > 0 {
                seg_start += current_width;
                segments.push(seg_start);
                current_width = w;
            } else {
                current_width += w;
            }
        }
        segments
    }

    /// 스크롤 업데이트
    pub fn update_scroll(&mut self) {
        if self.word_wrap {
            self.horizontal_scroll = 0;

            if self.cursor_line < self.scroll {
                self.scroll = self.cursor_line;
            }

            // cursor_line까지의 시각적 줄 수 계산
            let mut visual_rows = 0;
            let end = self.cursor_line.min(self.lines.len().saturating_sub(1));
            for line_idx in self.scroll..=end {
                visual_rows += self.count_wrapped_rows(line_idx);
            }
            // 초과 시 scroll 증가
            while visual_rows > self.visible_height && self.scroll < self.cursor_line {
                visual_rows -= self.count_wrapped_rows(self.scroll);
                self.scroll += 1;
            }
        } else {
            if self.cursor_line < self.scroll {
                self.scroll = self.cursor_line;
            } else if self.cursor_line >= self.scroll + self.visible_height {
                self.scroll = self.cursor_line.saturating_sub(self.visible_height - 1);
            }

            // 수평 스크롤 (visual column 기반)
            let cursor_visual = self.cursor_visual_col();
            if cursor_visual < self.horizontal_scroll {
                self.horizontal_scroll = cursor_visual;
            } else if cursor_visual >= self.horizontal_scroll + self.visible_width {
                self.horizontal_scroll = cursor_visual.saturating_sub(self.visible_width - 1);
            }
        }
    }

    /// 괄호 매칭 찾기
    fn find_matching_bracket(&mut self) {
        self.matching_bracket = None;

        if self.cursor_line >= self.lines.len() {
            return;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if self.cursor_col >= chars.len() {
            return;
        }

        let current_char = chars[self.cursor_col];
        let (opening, closing, forward) = match current_char {
            '(' => ('(', ')', true),
            ')' => ('(', ')', false),
            '[' => ('[', ']', true),
            ']' => ('[', ']', false),
            '{' => ('{', '}', true),
            '}' => ('{', '}', false),
            '<' => ('<', '>', true),
            '>' => ('<', '>', false),
            _ => return,
        };

        let mut depth = 1;

        if forward {
            // 앞으로 검색
            let mut line_idx = self.cursor_line;
            let mut col_idx = self.cursor_col + 1;

            while line_idx < self.lines.len() {
                let line_chars: Vec<char> = self.lines[line_idx].chars().collect();
                while col_idx < line_chars.len() {
                    if line_chars[col_idx] == closing {
                        depth -= 1;
                        if depth == 0 {
                            self.matching_bracket = Some((line_idx, col_idx));
                            return;
                        }
                    } else if line_chars[col_idx] == opening {
                        depth += 1;
                    }
                    col_idx += 1;
                }
                line_idx += 1;
                col_idx = 0;
            }
        } else {
            // 뒤로 검색
            let mut line_idx = self.cursor_line;
            let mut col_idx = self.cursor_col.saturating_sub(1);

            loop {
                let line_chars: Vec<char> = self.lines[line_idx].chars().collect();
                loop {
                    if col_idx < line_chars.len() {
                        if line_chars[col_idx] == opening {
                            depth -= 1;
                            if depth == 0 {
                                self.matching_bracket = Some((line_idx, col_idx));
                                return;
                            }
                        } else if line_chars[col_idx] == closing {
                            depth += 1;
                        }
                    }
                    if col_idx == 0 {
                        break;
                    }
                    col_idx -= 1;
                }
                if line_idx == 0 {
                    break;
                }
                line_idx -= 1;
                col_idx = self.lines[line_idx].chars().count().saturating_sub(1);
            }
        }
    }

    /// 검색 수행
    pub fn perform_find(&mut self) {
        self.match_positions.clear();
        self.find_error = None;

        if self.find_term.is_empty() {
            return;
        }

        let pattern = if self.find_options.use_regex {
            self.find_term.clone()
        } else {
            regex::escape(&self.find_term)
        };

        let pattern = if self.find_options.whole_word {
            format!(r"\b{}\b", pattern)
        } else {
            pattern
        };

        let regex = if self.find_options.case_sensitive {
            Regex::new(&pattern)
        } else {
            Regex::new(&format!("(?i){}", pattern))
        };

        match regex {
            Ok(re) => {
                for (line_idx, line) in self.lines.iter().enumerate() {
                    for mat in re.find_iter(line) {
                        // 바이트 인덱스를 문자 인덱스로 변환
                        let byte_start = mat.start();
                        let byte_end = mat.end();
                        let char_start = line[..byte_start].chars().count();
                        let char_end = char_start + line[byte_start..byte_end].chars().count();
                        self.match_positions.push((line_idx, char_start, char_end));
                    }
                }
            }
            Err(e) => {
                self.find_error = Some(format!("Regex error: {}", e));
            }
        }

        self.current_match = 0;
        self.goto_current_match();
    }

    /// 현재 매치로 이동
    fn goto_current_match(&mut self) {
        if !self.match_positions.is_empty() && self.current_match < self.match_positions.len() {
            let (line, start, end) = self.match_positions[self.current_match];
            self.cursor_line = line;
            self.cursor_col = start;
            self.selection = Some(Selection {
                start_line: line,
                start_col: start,
                end_line: line,
                end_col: end,
            });
            self.update_scroll();
        }
    }

    /// 다음 매치
    pub fn find_next(&mut self) {
        if !self.match_positions.is_empty() {
            self.current_match = (self.current_match + 1) % self.match_positions.len();
            self.goto_current_match();
        }
    }

    /// 이전 매치
    pub fn find_prev(&mut self) {
        if !self.match_positions.is_empty() {
            self.current_match = if self.current_match == 0 {
                self.match_positions.len() - 1
            } else {
                self.current_match - 1
            };
            self.goto_current_match();
        }
    }

    /// 바꾸기
    pub fn replace_current(&mut self) {
        if self.match_positions.is_empty() || self.current_match >= self.match_positions.len() {
            return;
        }

        let (line, start, end) = self.match_positions[self.current_match];

        // 선택 영역이 현재 매치와 일치하는지 확인
        let sel = self.selection.as_ref();
        if sel.is_some_and(|s| {
            let (sl, sc, el, ec) = s.normalized();
            sl == line && sc == start && el == line && ec == end
        }) {
            // 바꾸기 실행
            let line_content = &self.lines[line];
            let chars: Vec<char> = line_content.chars().collect();
            let new_line: String = chars[..start]
                .iter()
                .chain(self.replace_input.chars().collect::<Vec<_>>().iter())
                .chain(chars[end..].iter())
                .collect();

            let old_content = self.lines[line].clone();
            self.lines[line] = new_line;

            self.push_undo(EditAction::Replace {
                line,
                old_content,
                new_content: self.lines[line].clone(),
            });

            self.selection = None;
            self.perform_find();
            self.find_next();
        }
    }

    /// 모두 바꾸기
    pub fn replace_all(&mut self) {
        if self.find_term.is_empty() {
            return;
        }

        let pattern = if self.find_options.use_regex {
            self.find_term.clone()
        } else {
            regex::escape(&self.find_term)
        };

        let pattern = if self.find_options.whole_word {
            format!(r"\b{}\b", pattern)
        } else {
            pattern
        };

        let regex = if self.find_options.case_sensitive {
            Regex::new(&pattern)
        } else {
            Regex::new(&format!("(?i){}", pattern))
        };

        if let Ok(re) = regex {
            let mut actions = Vec::new();

            for (line_idx, line) in self.lines.iter_mut().enumerate() {
                let old_content = line.clone();
                let new_content = re
                    .replace_all(line, self.replace_input.as_str())
                    .to_string();

                if old_content != new_content {
                    actions.push(EditAction::Replace {
                        line: line_idx,
                        old_content,
                        new_content: new_content.clone(),
                    });
                    *line = new_content;
                }
            }

            if !actions.is_empty() {
                self.push_undo(EditAction::Batch { actions });
            }

            self.selection = None;
            self.perform_find();
        }
    }

    /// 줄 번호로 이동
    pub fn goto_line(&mut self, line_str: &str) {
        if let Ok(line_num) = line_str.parse::<usize>() {
            if line_num > 0 && line_num <= self.lines.len() {
                self.cursor_line = line_num - 1;
                self.cursor_col = 0;
                self.selection = None;
                self.update_scroll();
            }
        }
    }

    /// 문자가 단어 문자인지 확인
    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// 단어 왼쪽으로 이동 (Ctrl+Left)
    pub fn move_word_left(&mut self, extend_selection: bool) {
        if extend_selection && self.selection.is_none() {
            self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
        } else if !extend_selection {
            self.selection = None;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if self.cursor_col == 0 {
            // 이전 줄 끝으로 이동
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].chars().count();
            }
        } else {
            let mut col = self.cursor_col;
            // 왼쪽이 non-word_char면 건너뛰기 (단어 시작에서 이전 단어로 이동하기 위해)
            // 선택 모드에서도 커서가 단어 시작에 있을 때 이전으로 이동 가능해야 함
            if col > 0 && !Self::is_word_char(chars[col - 1]) {
                while col > 0 && !Self::is_word_char(chars[col - 1]) {
                    col -= 1;
                }
            }
            // 단어 건너뛰기
            while col > 0 && Self::is_word_char(chars[col - 1]) {
                col -= 1;
            }
            self.cursor_col = col;
        }

        // 블록 커서 선택: 현재 커서 위치의 문자 포함
        if let Some(ref mut sel) = self.selection {
            sel.end_line = self.cursor_line;
            let line_len = self.lines[self.cursor_line].chars().count();
            sel.end_col = (self.cursor_col + 1).min(line_len);
        }

        self.update_scroll();
    }

    /// 단어 오른쪽으로 이동 (Ctrl+Right)
    pub fn move_word_right(&mut self, extend_selection: bool) {
        if extend_selection && self.selection.is_none() {
            self.selection = Some(Selection::new(self.cursor_line, self.cursor_col));
        } else if !extend_selection {
            self.selection = None;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let line_len = chars.len();

        if self.cursor_col >= line_len {
            // 다음 줄 시작으로 이동
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
        } else {
            let mut col = self.cursor_col;

            // 현재 단어 끝까지 이동
            while col < line_len && Self::is_word_char(chars[col]) {
                col += 1;
            }

            // 선택 모드일 때: 마지막 word_char 위치에 멈춤 (공백 제외)
            // 단, 한 칸만 이동한 경우 (단어 끝에 있었던 경우) 다음 단어로 계속 이동
            if extend_selection && col > self.cursor_col {
                if col == self.cursor_col + 1 {
                    // 단어 끝에 있었음 - non-word_char 건너뛰고 다음 단어 끝까지
                    while col < line_len && !Self::is_word_char(chars[col]) {
                        col += 1;
                    }
                    let next_start = col;
                    while col < line_len && Self::is_word_char(chars[col]) {
                        col += 1;
                    }
                    if col > next_start {
                        col -= 1;
                    }
                } else {
                    col -= 1;
                }
            }

            // 이동하지 않았으면 (non-word_char 위에 있었으면) 건너뛰고 다음 단어로
            if col == self.cursor_col && col < line_len {
                while col < line_len && !Self::is_word_char(chars[col]) {
                    col += 1;
                }
                let next_start = col;
                while col < line_len && Self::is_word_char(chars[col]) {
                    col += 1;
                }
                if extend_selection && col > next_start {
                    col -= 1;
                }
            }

            self.cursor_col = col;
        }

        // 블록 커서 선택: 현재 커서 위치의 문자 포함
        if let Some(ref mut sel) = self.selection {
            sel.end_line = self.cursor_line;
            let line_len = self.lines[self.cursor_line].chars().count();
            sel.end_col = (self.cursor_col + 1).min(line_len);
        }

        self.update_scroll();
    }

    /// 단어 삭제 (뒤, Ctrl+Backspace)
    pub fn delete_word_backward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                // 이전 줄과 병합
                self.delete_backward();
            }
            return;
        }

        let start_col = self.cursor_col;
        let mut col = self.cursor_col;

        // 공백 건너뛰기
        while col > 0 && !Self::is_word_char(chars[col - 1]) {
            col -= 1;
        }
        // 단어 건너뛰기
        while col > 0 && Self::is_word_char(chars[col - 1]) {
            col -= 1;
        }

        let deleted_text: String = chars[col..start_col].iter().collect();
        let new_line: String = chars[..col]
            .iter()
            .chain(chars[start_col..].iter())
            .collect();

        self.push_undo(EditAction::Delete {
            line: self.cursor_line,
            col,
            text: deleted_text,
        });

        self.lines[self.cursor_line] = new_line;
        self.cursor_col = col;
        self.update_scroll();
    }

    /// 단어 삭제 (앞, Ctrl+Delete)
    pub fn delete_word_forward(&mut self) {
        if self.selection.is_some() {
            self.delete_selection();
            return;
        }

        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();
        let line_len = chars.len();

        if self.cursor_col >= line_len {
            if self.cursor_line + 1 < self.lines.len() {
                // 다음 줄과 병합
                self.delete_forward();
            }
            return;
        }

        let start_col = self.cursor_col;
        let mut col = self.cursor_col;

        // 현재 단어 끝까지 이동
        while col < line_len && Self::is_word_char(chars[col]) {
            col += 1;
        }
        // 공백 건너뛰기
        while col < line_len && !Self::is_word_char(chars[col]) {
            col += 1;
        }

        let deleted_text: String = chars[start_col..col].iter().collect();
        let new_line: String = chars[..start_col]
            .iter()
            .chain(chars[col..].iter())
            .collect();

        self.push_undo(EditAction::Delete {
            line: self.cursor_line,
            col: start_col,
            text: deleted_text,
        });

        self.lines[self.cursor_line] = new_line;
        self.update_scroll();
    }

    /// 커서 위치의 단어 범위 찾기
    fn get_word_at_cursor(&self) -> Option<(usize, usize, String)> {
        let line = &self.lines[self.cursor_line];
        let chars: Vec<char> = line.chars().collect();

        if chars.is_empty() {
            return None;
        }

        let col = self.cursor_col.min(chars.len().saturating_sub(1));

        // 현재 위치가 단어 문자가 아니면 None
        if !Self::is_word_char(chars[col]) {
            return None;
        }

        // 단어 시작 찾기
        let mut start = col;
        while start > 0 && Self::is_word_char(chars[start - 1]) {
            start -= 1;
        }

        // 단어 끝 찾기
        let mut end = col;
        while end < chars.len() && Self::is_word_char(chars[end]) {
            end += 1;
        }

        let word: String = chars[start..end].iter().collect();
        Some((start, end, word))
    }

    /// 커서 위치 단어 선택 (Ctrl+D 첫 번째)
    pub fn select_word_at_cursor(&mut self) {
        if let Some((start, end, word)) = self.get_word_at_cursor() {
            self.selection = Some(Selection {
                start_line: self.cursor_line,
                start_col: start,
                end_line: self.cursor_line,
                end_col: end,
            });
            self.cursor_col = end;
            self.last_word_selection = Some(word);
            self.cursors.clear();
        }
    }

    /// 다음 동일 단어 선택 (Ctrl+D 반복)
    pub fn select_next_occurrence(&mut self) {
        // 선택이 없으면 현재 단어 선택
        if self.selection.is_none() || self.last_word_selection.is_none() {
            self.select_word_at_cursor();
            return;
        }

        let word = match &self.last_word_selection {
            Some(w) => w.clone(),
            None => return,
        };

        // 현재 커서 위치 이후에서 다음 occurrence 찾기
        let search_start_line = self.cursor_line;
        let search_start_col = self.cursor_col; // 문자 인덱스

        for line_idx in search_start_line..self.lines.len() {
            let line = &self.lines[line_idx];
            let start_char_col = if line_idx == search_start_line {
                search_start_col
            } else {
                0
            };

            // 문자 인덱스를 바이트 인덱스로 변환
            let start_byte: usize = line
                .chars()
                .take(start_char_col)
                .map(|c| c.len_utf8())
                .sum();

            // 이 줄에서 단어 찾기
            if let Some(byte_pos) = line[start_byte..].find(&word) {
                let actual_byte_pos = start_byte + byte_pos;
                // 바이트 위치를 문자 위치로 변환
                let char_pos = line[..actual_byte_pos].chars().count();

                // 단어 경계 확인
                let chars: Vec<char> = line.chars().collect();
                let word_end = char_pos + word.chars().count();

                let is_word_start = char_pos == 0 || !Self::is_word_char(chars[char_pos - 1]);
                let is_word_end = word_end >= chars.len() || !Self::is_word_char(chars[word_end]);

                if is_word_start && is_word_end {
                    // 현재 선택 위치를 다중 커서에 추가
                    if let Some(sel) = &self.selection {
                        let (_, _, el, ec) = sel.normalized();
                        self.cursors.push((el, ec));
                    }

                    // 새 위치로 선택 이동
                    self.cursor_line = line_idx;
                    self.cursor_col = word_end;
                    self.selection = Some(Selection {
                        start_line: line_idx,
                        start_col: char_pos,
                        end_line: line_idx,
                        end_col: word_end,
                    });
                    self.update_scroll();
                    return;
                }
            }
        }

        // 파일 끝에서 시작으로 wrap around
        for line_idx in 0..=search_start_line {
            let line = &self.lines[line_idx];

            // 검색 범위 끝 계산 (바이트 인덱스)
            let end_byte = if line_idx == search_start_line {
                // 원래 검색 시작 전까지만
                let sel = self.selection.as_ref().unwrap();
                let (sl, sc, _, _) = sel.normalized();
                let end_char = if line_idx == sl { sc } else { 0 };
                line.chars().take(end_char).map(|c| c.len_utf8()).sum()
            } else {
                line.len()
            };

            if let Some(byte_pos) = line[..end_byte].find(&word) {
                let char_pos = line[..byte_pos].chars().count();
                let chars: Vec<char> = line.chars().collect();
                let word_end = char_pos + word.chars().count();

                let is_word_start = char_pos == 0 || !Self::is_word_char(chars[char_pos - 1]);
                let is_word_end = word_end >= chars.len() || !Self::is_word_char(chars[word_end]);

                if is_word_start && is_word_end {
                    if let Some(sel) = &self.selection {
                        let (_, _, el, ec) = sel.normalized();
                        self.cursors.push((el, ec));
                    }

                    self.cursor_line = line_idx;
                    self.cursor_col = word_end;
                    self.selection = Some(Selection {
                        start_line: line_idx,
                        start_col: char_pos,
                        end_line: line_idx,
                        end_col: word_end,
                    });
                    self.update_scroll();
                    return;
                }
            }
        }
    }

    /// 현재 줄 선택 (Ctrl+L)
    pub fn select_line(&mut self) {
        let line_len = self.lines[self.cursor_line].chars().count();
        self.selection = Some(Selection {
            start_line: self.cursor_line,
            start_col: 0,
            end_line: self.cursor_line,
            end_col: line_len,
        });
        self.cursor_col = line_len;
    }

    /// 아래에 빈 줄 삽입 (Ctrl+Enter)
    pub fn insert_line_below(&mut self) {
        // 현재 줄의 들여쓰기 가져오기
        let indent = if self.auto_indent {
            let line = &self.lines[self.cursor_line];
            line.chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
        } else {
            String::new()
        };

        self.lines.insert(self.cursor_line + 1, indent.clone());
        self.cursor_line += 1;
        self.cursor_col = indent.len();

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: indent,
        });
        self.update_scroll();
    }

    /// 위에 빈 줄 삽입 (Ctrl+Shift+Enter)
    pub fn insert_line_above(&mut self) {
        // 현재 줄의 들여쓰기 가져오기
        let indent = if self.auto_indent {
            let line = &self.lines[self.cursor_line];
            line.chars()
                .take_while(|c| c.is_whitespace())
                .collect::<String>()
        } else {
            String::new()
        };

        self.lines.insert(self.cursor_line, indent.clone());
        self.cursor_col = indent.len();

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: indent,
        });
        self.update_scroll();
    }

    /// 줄 복사 위로 (Shift+Alt+Up)
    #[allow(dead_code)]
    pub fn copy_line_up(&mut self) {
        let line_content = self.lines[self.cursor_line].clone();
        self.lines.insert(self.cursor_line, line_content.clone());

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: line_content,
        });
        self.update_scroll();
    }

    /// 줄 복사 아래로 (Shift+Alt+Down)
    #[allow(dead_code)]
    pub fn copy_line_down(&mut self) {
        let line_content = self.lines[self.cursor_line].clone();
        self.lines
            .insert(self.cursor_line + 1, line_content.clone());
        self.cursor_line += 1;

        self.push_undo(EditAction::InsertLine {
            line: self.cursor_line,
            content: line_content,
        });
        self.update_scroll();
    }

    /// 잘라내기 (선택 없으면 줄 전체)
    pub fn cut_line_or_selection(&mut self) {
        if self.selection.is_some() && !self.selection.as_ref().unwrap().is_empty() {
            // 선택 영역 잘라내기
            self.clipboard = self.get_selected_text();
            self.delete_selection();
        } else {
            // 줄 전체 잘라내기
            if self.lines.len() > 1 {
                self.clipboard = self.lines[self.cursor_line].clone() + "\n";
                let content = self.lines.remove(self.cursor_line);

                self.push_undo(EditAction::DeleteLine {
                    line: self.cursor_line,
                    content,
                });

                if self.cursor_line >= self.lines.len() {
                    self.cursor_line = self.lines.len() - 1;
                }
                self.cursor_col = self
                    .cursor_col
                    .min(self.lines[self.cursor_line].chars().count());
            } else {
                // 유일한 줄이면 내용만 잘라내기
                self.clipboard = self.lines[0].clone() + "\n";
                let old_content = self.lines[0].clone();
                self.lines[0] = String::new();
                self.cursor_col = 0;

                self.push_undo(EditAction::Replace {
                    line: 0,
                    old_content,
                    new_content: String::new(),
                });
            }
            self.update_scroll();
        }
    }

    /// 들여쓰기 (Ctrl+])
    pub fn indent(&mut self) {
        let indent_str = if self.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.tab_size)
        };

        if let Some(sel) = &self.selection {
            let (start_line, _, end_line, _) = sel.normalized();
            let mut actions = Vec::new();

            for line_idx in start_line..=end_line {
                let old_content = self.lines[line_idx].clone();
                self.lines[line_idx] = format!("{}{}", indent_str, old_content);
                actions.push(EditAction::Replace {
                    line: line_idx,
                    old_content,
                    new_content: self.lines[line_idx].clone(),
                });
            }

            self.push_undo(EditAction::Batch { actions });
        } else {
            let old_content = self.lines[self.cursor_line].clone();
            self.lines[self.cursor_line] = format!("{}{}", indent_str, old_content);
            self.cursor_col += indent_str.len();

            self.push_undo(EditAction::Replace {
                line: self.cursor_line,
                old_content,
                new_content: self.lines[self.cursor_line].clone(),
            });
        }
    }

    /// 내어쓰기 (Ctrl+[ 또는 Shift+Tab)
    pub fn outdent(&mut self) {
        let tab_size = self.tab_size;

        let remove_indent = |line: &str, tab_size: usize| -> (String, usize) {
            let chars: Vec<char> = line.chars().collect();

            if chars.first() == Some(&'\t') {
                (chars[1..].iter().collect(), 1)
            } else {
                let spaces_to_remove = chars
                    .iter()
                    .take(tab_size)
                    .take_while(|c| **c == ' ')
                    .count();
                (chars[spaces_to_remove..].iter().collect(), spaces_to_remove)
            }
        };

        if let Some(sel) = &self.selection {
            let (start_line, _, end_line, _) = sel.normalized();
            let mut actions = Vec::new();

            for line_idx in start_line..=end_line {
                let old_content = self.lines[line_idx].clone();
                let (new_content, _) = remove_indent(&old_content, tab_size);
                self.lines[line_idx] = new_content.clone();
                actions.push(EditAction::Replace {
                    line: line_idx,
                    old_content,
                    new_content,
                });
            }

            self.push_undo(EditAction::Batch { actions });
        } else {
            let old_content = self.lines[self.cursor_line].clone();
            let (new_content, removed) = remove_indent(&old_content, tab_size);
            self.lines[self.cursor_line] = new_content.clone();
            self.cursor_col = self.cursor_col.saturating_sub(removed);

            self.push_undo(EditAction::Replace {
                line: self.cursor_line,
                old_content,
                new_content,
            });
        }
    }

    /// 언어별 주석 문자 가져오기
    fn get_comment_string(&self) -> Option<&'static str> {
        match self.language {
            Language::Rust
            | Language::C
            | Language::Cpp
            | Language::Java
            | Language::JavaScript
            | Language::TypeScript
            | Language::Go
            | Language::Swift
            | Language::Kotlin => Some("//"),

            Language::Python
            | Language::Shell
            | Language::Ruby
            | Language::Yaml
            | Language::Toml => Some("#"),

            Language::Sql => Some("--"),

            Language::Html | Language::Xml | Language::Css => None,

            Language::Php => Some("//"),

            Language::Markdown | Language::Json | Language::Plain => Some("//"),
        }
    }

    /// 주석 토글 (Ctrl+/)
    pub fn toggle_comment(&mut self) {
        let comment = match self.get_comment_string() {
            Some(c) => c,
            None => return,
        };
        let comment_with_space = format!("{} ", comment);

        if let Some(sel) = &self.selection {
            let (start_line, _, mut end_line, end_col) = sel.normalized();
            // 블록 커서: end_col = cursor + 1이므로 end_col <= 1이면 cursor가 라인 시작(col 0)
            // 여러 줄 선택에서 마지막 라인에 실제 선택된 문자가 없으므로 제외
            if end_col <= 1 && end_line > start_line {
                end_line -= 1;
            }
            let mut actions = Vec::new();

            // 모든 줄이 주석인지 확인 (라인 맨 앞 기준)
            let all_commented = (start_line..=end_line).all(|i| {
                self.lines[i].starts_with(&comment_with_space) || self.lines[i].starts_with(comment)
            });

            for line_idx in start_line..=end_line {
                let old_content = self.lines[line_idx].clone();

                let new_content = if all_commented {
                    // 주석 제거 (라인 맨 앞에서)
                    if old_content.starts_with(&comment_with_space) {
                        old_content[comment_with_space.len()..].to_string()
                    } else if old_content.starts_with(comment) {
                        old_content[comment.len()..].to_string()
                    } else {
                        old_content.clone()
                    }
                } else {
                    // 주석 추가 (라인 맨 앞에)
                    format!("{}{}", comment_with_space, old_content)
                };

                self.lines[line_idx] = new_content.clone();
                actions.push(EditAction::Replace {
                    line: line_idx,
                    old_content,
                    new_content,
                });
            }

            self.push_undo(EditAction::Batch { actions });
        } else {
            let old_content = self.lines[self.cursor_line].clone();

            let new_content = if old_content.starts_with(&comment_with_space) {
                // 주석 제거 (라인 맨 앞에서)
                old_content[comment_with_space.len()..].to_string()
            } else if old_content.starts_with(comment) {
                old_content[comment.len()..].to_string()
            } else {
                // 주석 추가 (라인 맨 앞에)
                format!("{}{}", comment_with_space, old_content)
            };

            self.lines[self.cursor_line] = new_content.clone();

            self.push_undo(EditAction::Replace {
                line: self.cursor_line,
                old_content,
                new_content,
            });
        }
    }
}

pub fn draw(
    frame: &mut Frame,
    state: &mut EditorState,
    area: Rect,
    theme: &Theme,
    kb: &crate::keybindings::Keybindings,
) {
    let border_color = if state.modified {
        theme.editor.modified_mark
    } else {
        theme.editor.border
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // 화면 크기 업데이트 (스크롤 계산에 사용)
    state.visible_height = inner.height.saturating_sub(2) as usize; // 헤더와 푸터 제외
                                                                    // visible_width는 Content 섹션에서 동적 줄 번호 폭 기준으로 설정됨

    // Header
    let file_name = state
        .file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "New File".to_string());

    let remote_span = if let Some(ref origin) = state.remote_origin {
        Span::styled(
            format!("[Remote: {}] ", origin.remote_path),
            Style::default().fg(theme.editor.remote_path_text),
        )
    } else {
        Span::raw("")
    };

    let header = Line::from(vec![
        Span::raw(" "),
        if state.modified {
            Span::styled("✻", Style::default().fg(theme.editor.modified_mark))
        } else {
            Span::raw("")
        },
        Span::styled(format!("{} ", file_name), theme.header_style()),
        remote_span,
        Span::styled(format!("[{}] ", state.language.name()), theme.dim_style()),
        Span::styled(
            format!(
                "Ln {}, Col {} ",
                state.cursor_line + 1,
                state.cursor_visual_col() + 1
            ),
            theme.dim_style(),
        ),
        if !state.undo_stack.is_empty() {
            Span::styled(
                format!("Undo:{} ", state.undo_stack.len()),
                theme.dim_style(),
            )
        } else {
            Span::raw("")
        },
    ]);
    frame.render_widget(
        Paragraph::new(header).style(theme.status_bar_style()),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    // Content
    let content_height = (inner.height - 2) as usize;

    // 줄 번호 폭 동적 계산 (총 줄 수 기준)
    let total_lines = state.lines.len();
    let line_num_width = if total_lines == 0 {
        1
    } else {
        ((total_lines as f64).log10().floor() as usize) + 1
    }
    .max(4); // 최소 4자리
    let line_num_col_width = line_num_width + 1; // 공백 포함

    // visible_width 업데이트
    state.visible_width = inner.width.saturating_sub(line_num_col_width as u16 + 1) as usize;

    // 선택 영역 정규화
    let selection = state.selection.as_ref().map(|s| s.normalized());

    // 하이라이터
    let mut highlighter = state.highlighter.clone();
    if let Some(ref mut hl) = highlighter {
        hl.reset();
        for line in state.lines.iter().take(state.scroll) {
            hl.tokenize_line(line);
        }
    }

    if state.word_wrap {
        // Word wrap 모드: 논리적 줄을 시각적 세그먼트로 분할하여 렌더링
        let content_width = state.visible_width;
        let in_find_mode = state.find_mode != FindReplaceMode::None;
        let mut visual_row: usize = 0;
        let mut line_idx = state.scroll;

        while visual_row < content_height && line_idx < state.lines.len() {
            let original_line = &state.lines[line_idx];
            let (expanded_line, visual_to_orig) = state.expand_tabs_with_mapping(original_line);
            let is_cursor_line = line_idx == state.cursor_line;

            // 하이라이터로 토큰화 (논리적 줄당 1회만)
            let orig_chars: Vec<char> = original_line.chars().collect();
            let orig_styles: Vec<ratatui::style::Style> = if let Some(ref mut hl) = highlighter {
                let tokens = hl.tokenize_line(original_line);
                if !tokens.is_empty() {
                    let mut styles = vec![theme.normal_style(); orig_chars.len()];
                    let mut char_idx = 0;
                    for token in &tokens {
                        let token_len = token.text.chars().count();
                        let style = hl.style_for(token.token_type);
                        for i in char_idx..(char_idx + token_len).min(orig_chars.len()) {
                            styles[i] = style;
                        }
                        char_idx += token_len;
                    }
                    styles
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            // 줄을 visual column 기준 세그먼트로 분할
            let seg_starts = EditorState::compute_wrap_segments(&expanded_line, content_width);

            for (seg_idx, &seg_start_visual) in seg_starts.iter().enumerate() {
                if visual_row >= content_height {
                    break;
                }

                let is_first = seg_idx == 0;

                // 줄 번호: 첫 세그먼트만 표시
                let line_num_style = if is_cursor_line {
                    Style::default()
                        .fg(theme.editor.line_number)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.editor.line_number)
                };

                let line_num_span = if is_first {
                    Span::styled(
                        format!("{:>width$} ", line_idx + 1, width = line_num_width),
                        line_num_style,
                    )
                } else {
                    Span::styled(
                        format!("{:>width$} ", "", width = line_num_width),
                        line_num_style,
                    )
                };

                let content_spans = render_editor_line(
                    &expanded_line,
                    original_line,
                    &visual_to_orig,
                    line_idx,
                    state,
                    &selection,
                    &mut None, // 하이라이터 건너뜀 (이미 토큰화됨)
                    theme,
                    is_cursor_line,
                    in_find_mode,
                    seg_start_visual,
                    content_width,
                    Some(&orig_styles),
                );

                let mut spans = vec![line_num_span];
                spans.extend(content_spans);

                frame.render_widget(
                    Paragraph::new(Line::from(spans)),
                    Rect::new(inner.x, inner.y + 1 + visual_row as u16, inner.width, 1),
                );

                visual_row += 1;
            }

            line_idx += 1;
        }
    } else {
        for (i, original_line) in state
            .lines
            .iter()
            .skip(state.scroll)
            .take(content_height)
            .enumerate()
        {
            // TAB을 visual column 기반으로 스페이스로 확장 (매핑 정보 포함)
            let (expanded_line, visual_to_orig) = state.expand_tabs_with_mapping(original_line);
            let line_num = state.scroll + i;
            let is_cursor_line = line_num == state.cursor_line;

            // 줄 번호
            let line_num_style = if is_cursor_line {
                Style::default()
                    .fg(theme.editor.line_number)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.editor.line_number)
            };

            let line_num_span = Span::styled(
                format!("{:>width$} ", line_num + 1, width = line_num_width),
                line_num_style,
            );

            // 라인 렌더링
            let in_find_mode = state.find_mode != FindReplaceMode::None;
            let content_spans = render_editor_line(
                &expanded_line,
                original_line,
                &visual_to_orig,
                line_num,
                state,
                &selection,
                &mut highlighter,
                theme,
                is_cursor_line,
                in_find_mode,
                state.horizontal_scroll,
                state.visible_width,
                None,
            );

            let mut spans = vec![line_num_span];
            spans.extend(content_spans);

            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(inner.x, inner.y + 1 + i as u16, inner.width, 1),
            );
        }
    }

    // 스크롤바
    let total_lines = state.lines.len();
    if total_lines > content_height {
        let scrollbar = Scrollbar::default()
            .orientation(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"));

        let max_scroll = total_lines.saturating_sub(content_height);
        let mut scrollbar_state = ScrollbarState::new(max_scroll + 1).position(state.scroll);

        let scrollbar_area = Rect::new(
            inner.x + inner.width - 1,
            inner.y + 1,
            1,
            content_height as u16,
        );

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // Footer
    let footer_y = inner.y + inner.height - 1;

    match state.find_mode {
        FindReplaceMode::None => {
            if state.goto_mode {
                let goto_line = Line::from(vec![
                    Span::styled("Go to line: ", theme.header_style()),
                    Span::styled(&state.goto_input, theme.normal_style()),
                    Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
                ]);
                frame.render_widget(
                    Paragraph::new(goto_line).style(theme.status_bar_style()),
                    Rect::new(inner.x, footer_y, inner.width, 1),
                );
            } else {
                let mut footer_spans = vec![];

                // Word wrap 표시자
                if state.word_wrap {
                    footer_spans.push(Span::styled(
                        "Wrap ",
                        Style::default().fg(theme.editor.wrap_indicator),
                    ));
                }

                // 단축키 안내 (keybindings에서 동적으로)
                let shortcuts: Vec<(String, &str)> = vec![
                    (kb.editor_first_key(EditorAction::Save).to_string(), "save "),
                    (
                        kb.editor_first_key(EditorAction::DeleteLine).to_string(),
                        "del ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::DuplicateLine).to_string(),
                        "dup ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::ToggleComment).to_string(),
                        "comment ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::SelectNextOccurrence)
                            .to_string(),
                        "select ",
                    ),
                    (kb.editor_first_key(EditorAction::Find).to_string(), "find "),
                    (
                        kb.editor_first_key(EditorAction::GotoLine).to_string(),
                        "goto ",
                    ),
                    (
                        kb.editor_first_key(EditorAction::ToggleWordWrap)
                            .to_string(),
                        "wrap ",
                    ),
                    (kb.editor_first_key(EditorAction::Exit).to_string(), "exit"),
                ];

                for (key, rest) in &shortcuts {
                    footer_spans.push(Span::styled(key.as_str(), theme.header_style()));
                    footer_spans.push(Span::styled(":", theme.dim_style()));
                    footer_spans.push(Span::styled(*rest, theme.dim_style()));
                }

                let footer = Line::from(footer_spans);
                frame.render_widget(
                    Paragraph::new(footer).style(theme.status_bar_style()),
                    Rect::new(inner.x, footer_y, inner.width, 1),
                );
            }
        }
        FindReplaceMode::Find | FindReplaceMode::Replace => {
            let find_opts = format!(
                "[{}{}{}]",
                if state.find_options.case_sensitive {
                    "Aa"
                } else {
                    "aa"
                },
                if state.find_options.use_regex {
                    " Re"
                } else {
                    ""
                },
                if state.find_options.whole_word {
                    " W"
                } else {
                    ""
                }
            );

            let (match_info, match_info_style) = if let Some(ref err) = state.find_error {
                // 정규식 에러 표시 (빨간색)
                let truncated = if err.len() > 30 {
                    format!(" {}... ", &err[..27])
                } else {
                    format!(" {} ", err)
                };
                (truncated, Style::default().fg(Color::Red))
            } else if !state.match_positions.is_empty() {
                let count = state.match_positions.len();
                (
                    format!(
                        " {}/{} ({} matches) ",
                        state.current_match + 1,
                        count,
                        count
                    ),
                    theme.dim_style(),
                )
            } else if !state.find_term.is_empty() {
                (" No matches ".to_string(), theme.dim_style())
            } else {
                (String::new(), theme.dim_style())
            };

            let cursor_style = Style::default()
                .fg(theme.editor.bg)
                .bg(theme.editor.selection_bg)
                .add_modifier(Modifier::SLOW_BLINK);
            let input_style = Style::default().fg(theme.editor.find_input_text);

            // Find 입력 필드
            let find_chars: Vec<char> = state.find_input.chars().collect();
            let find_cursor = state.find_cursor_pos.min(find_chars.len());
            let find_before: String = find_chars[..find_cursor].iter().collect();
            let find_cursor_char = if find_cursor < find_chars.len() {
                find_chars[find_cursor].to_string()
            } else {
                " ".to_string()
            };
            let find_after: String = if find_cursor < find_chars.len() {
                let mut s: String = find_chars[find_cursor + 1..].iter().collect();
                s.push(' '); // 끝에 공백 유지
                s
            } else {
                String::new()
            };

            let mut spans = vec![Span::styled("Find: ", theme.header_style())];
            if state.input_focus == 0 {
                spans.push(Span::styled(find_before, input_style));
                spans.push(Span::styled(find_cursor_char, cursor_style));
                spans.push(Span::styled(find_after, input_style));
            } else {
                spans.push(Span::styled(
                    format!("{} ", &state.find_input),
                    theme.dim_style(),
                ));
            }

            // Replace 입력 필드
            if state.find_mode == FindReplaceMode::Replace {
                let replace_chars: Vec<char> = state.replace_input.chars().collect();
                let replace_cursor = state.replace_cursor_pos.min(replace_chars.len());
                let replace_before: String = replace_chars[..replace_cursor].iter().collect();
                let replace_cursor_char = if replace_cursor < replace_chars.len() {
                    replace_chars[replace_cursor].to_string()
                } else {
                    " ".to_string()
                };
                let replace_after: String = if replace_cursor < replace_chars.len() {
                    let mut s: String = replace_chars[replace_cursor + 1..].iter().collect();
                    s.push(' '); // 끝에 공백 유지
                    s
                } else {
                    String::new()
                };

                spans.push(Span::styled(" Replace: ", theme.header_style()));
                if state.input_focus == 1 {
                    spans.push(Span::styled(replace_before, input_style));
                    spans.push(Span::styled(replace_cursor_char, cursor_style));
                    spans.push(Span::styled(replace_after, input_style));
                } else {
                    spans.push(Span::styled(
                        format!("{} ", &state.replace_input),
                        theme.dim_style(),
                    ));
                }
            }

            spans.push(Span::styled(match_info, match_info_style));
            spans.push(Span::styled(find_opts, theme.dim_style()));

            // 단축키 안내 (VSCode 스타일)
            spans.push(Span::styled(" ^C", theme.header_style()));
            spans.push(Span::styled("ase ", theme.dim_style()));
            spans.push(Span::styled("^R", theme.header_style()));
            spans.push(Span::styled("egex ", theme.dim_style()));
            spans.push(Span::styled("^W", theme.header_style()));
            spans.push(Span::styled("ord ", theme.dim_style()));
            if state.find_mode == FindReplaceMode::Replace {
                spans.push(Span::styled("^A", theme.header_style()));
                spans.push(Span::styled("ll ", theme.dim_style()));
                spans.push(Span::styled("Tab", theme.header_style()));
                spans.push(Span::styled(" ", theme.dim_style()));
            }
            spans.push(Span::styled("Esc", theme.header_style()));

            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(theme.status_bar_style()),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    }

    // 메시지 표시 (화면 상단에 오버레이)
    if let Some(ref msg) = state.message {
        let msg_width = (msg.len() + 4).min(inner.width as usize) as u16;
        let msg_x = inner.x + (inner.width.saturating_sub(msg_width)) / 2;
        let msg_y = inner.y + 1;
        let msg_area = Rect::new(msg_x, msg_y, msg_width, 1);
        // Clear the area first to ensure message is visible
        frame.render_widget(Clear, msg_area);
        frame.render_widget(
            Paragraph::new(format!(" {} ", msg))
                .style(Style::default().fg(theme.message.text).bg(theme.message.bg)),
            msg_area,
        );
    }

    // 메시지 타이머 업데이트
    if state.message_timer > 0 {
        state.message_timer -= 1;
        if state.message_timer == 0 {
            state.message = None;
        }
    }
}

/// 편집기 라인 렌더링
/// expanded_line: TAB이 스페이스로 확장된 문자열
/// original_line: 원본 문자열 (TAB 포함)
/// visual_to_orig: 확장된 문자열의 각 visual 위치에 해당하는 원본 char index
fn render_editor_line(
    expanded_line: &str,
    original_line: &str,
    visual_to_orig: &[usize],
    line_num: usize,
    state: &EditorState,
    selection: &Option<(usize, usize, usize, usize)>,
    highlighter: &mut Option<SyntaxHighlighter>,
    theme: &Theme,
    is_cursor_line: bool,
    in_find_mode: bool,
    horizontal_scroll: usize,
    visible_width: usize,
    pre_computed_styles: Option<&Vec<ratatui::style::Style>>,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = expanded_line.chars().collect();
    let orig_chars: Vec<char> = original_line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();

    // 보이는 범위 (visual column 기준)
    let view_start = horizontal_scroll;
    let view_end = horizontal_scroll + visible_width;

    // 커서의 visual column 계산
    let cursor_visual = state.char_to_visual(original_line, state.cursor_col);

    // 선택 영역이 이 줄에 있는지 확인 (원본 인덱스 기준)
    let line_selection = if let Some((sl, sc, el, ec)) = selection {
        if *sl <= line_num && line_num <= *el {
            let start = if line_num == *sl { *sc } else { 0 };
            let end = if line_num == *el {
                *ec
            } else {
                orig_chars.len()
            };
            Some((start, end))
        } else {
            None
        }
    } else {
        None
    };

    // 문법 강조: pre_computed_styles가 있으면 토큰화 건너뜀
    let orig_styles: Vec<ratatui::style::Style> = if let Some(pcs) = pre_computed_styles {
        pcs.clone()
    } else {
        // 문법 강조 토큰 가져오기 (원본 라인에서 토큰화)
        let tokens = if let Some(ref mut hl) = highlighter {
            hl.tokenize_line(original_line)
        } else {
            vec![]
        };

        // 원본 char index -> token style 매핑 생성
        if !tokens.is_empty() {
            let mut styles = vec![theme.normal_style(); orig_chars.len()];
            let mut char_idx = 0;
            for token in &tokens {
                let token_len = token.text.chars().count();
                let style = if let Some(ref mut hl) = highlighter {
                    hl.style_for(token.token_type)
                } else {
                    theme.normal_style()
                };
                for i in char_idx..(char_idx + token_len).min(orig_chars.len()) {
                    styles[i] = style;
                }
                char_idx += token_len;
            }
            styles
        } else {
            vec![]
        }
    };

    // 문자를 순회하면서 visual column 누적 — CJK 전각 문자 올바르게 처리
    let mut visual_col = 0; // 현재 문자의 visual column 시작 위치
    let mut vis_idx = 0; // visual_to_orig 인덱스

    for (_char_idx, c) in chars.iter().enumerate() {
        let char_width = UnicodeWidthChar::width(*c).unwrap_or(1);
        let char_visual_start = visual_col;
        let char_visual_end = visual_col + char_width;

        // 이 문자가 보이는 영역을 벗어나면 종료
        if char_visual_start >= view_end {
            break;
        }

        // 이 문자가 보이는 영역과 겹치는 경우만 렌더링
        if char_visual_end > view_start {
            // visual_to_orig에서 원본 인덱스
            let orig_idx = if vis_idx < visual_to_orig.len() {
                visual_to_orig[vis_idx]
            } else {
                orig_chars.len()
            };

            // 기본 스타일 결정
            let mut style = if !orig_styles.is_empty() && orig_idx < orig_styles.len() {
                orig_styles[orig_idx]
            } else {
                theme.normal_style()
            };

            // 선택 영역 하이라이트 (원본 인덱스 기준)
            if let Some((sel_start, sel_end)) = line_selection {
                if orig_idx >= sel_start && orig_idx < sel_end {
                    style = style
                        .bg(theme.editor.selection_bg)
                        .fg(theme.editor.selection_text);
                }
            }

            // 검색 매치 하이라이트 (원본 인덱스 기준)
            for (idx, (ml, ms, me)) in state.match_positions.iter().enumerate() {
                if *ml == line_num && orig_idx >= *ms && orig_idx < *me {
                    if idx == state.current_match {
                        style = style.bg(theme.editor.match_current_bg).fg(theme.editor.bg);
                    } else {
                        style = style.bg(theme.editor.match_bg).fg(theme.editor.bg);
                    }
                }
            }

            // 매칭 괄호 하이라이트 (원본 인덱스 기준)
            if let Some((bl, bc)) = state.matching_bracket {
                if bl == line_num && bc == orig_idx {
                    style = style.bg(theme.editor.bracket_match).fg(Color::Black);
                }
            }

            // 커서 하이라이트 (visual column 기준)
            if is_cursor_line && char_visual_start == cursor_visual && state.selection.is_none() {
                if in_find_mode {
                    style = Style::default()
                        .fg(theme.editor.text)
                        .bg(theme.editor.footer_bg);
                } else {
                    style = theme.selected_style();
                }
            }

            // 전각 문자가 왼쪽 경계에 걸리는 경우: 공백으로 대체
            if char_visual_start < view_start && char_width == 2 {
                spans.push(Span::styled(" ", style));
            }
            // 전각 문자가 오른쪽 경계에 걸리는 경우: 공백으로 대체
            else if char_visual_end > view_end && char_width == 2 {
                spans.push(Span::styled(" ", style));
            } else {
                spans.push(Span::styled(c.to_string(), style));
            }
        }

        visual_col += char_width;
        vis_idx += char_width; // visual_to_orig는 visual column 단위
    }

    // 커서가 줄 끝에 있고 보이는 범위 내인 경우
    if is_cursor_line && state.cursor_col >= orig_chars.len() && state.selection.is_none() {
        if cursor_visual >= view_start && cursor_visual < view_end {
            let cursor_style = if in_find_mode {
                Style::default()
                    .fg(theme.editor.text)
                    .bg(theme.editor.footer_bg)
            } else {
                theme.selected_style()
            };
            spans.push(Span::styled(" ", cursor_style));
        }
    }

    if spans.is_empty() {
        // 빈 줄에 커서 표시 (수평 스크롤이 0일 때만)
        if is_cursor_line && state.selection.is_none() && horizontal_scroll == 0 {
            let cursor_style = if in_find_mode {
                Style::default()
                    .fg(theme.editor.text)
                    .bg(theme.status_bar.bg)
            } else {
                theme.selected_style()
            };
            spans.push(Span::styled(" ", cursor_style));
        } else if horizontal_scroll == 0 {
            spans.push(Span::styled(" ", theme.normal_style()));
        }
    }

    spans
}

/// Handle paste event for file editor
pub fn handle_paste(app: &mut App, text: &str) {
    let state = match &mut app.editor_state {
        Some(s) => s,
        None => return,
    };
    if !text.is_empty() {
        // Normalize line endings
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        state.insert_str(&normalized);
    }
}

pub fn handle_input(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let state = match &mut app.editor_state {
        Some(s) => s,
        None => return,
    };

    // Goto 모드 (텍스트 입력 모드이므로 pending_exit 리셋)
    if state.goto_mode {
        state.pending_exit = false;
        match code {
            KeyCode::Esc => {
                state.goto_mode = false;
                state.goto_input.clear();
            }
            KeyCode::Enter => {
                state.goto_line(&state.goto_input.clone());
                state.goto_mode = false;
                state.goto_input.clear();
            }
            KeyCode::Backspace => {
                state.goto_input.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                state.goto_input.push(c);
            }
            _ => {}
        }
        return;
    }

    // Find/Replace 모드 (텍스트 입력 모드이므로 pending_exit 리셋)
    if state.find_mode != FindReplaceMode::None {
        state.pending_exit = false;
        match code {
            KeyCode::Esc => {
                state.find_mode = FindReplaceMode::None;
                state.selection = None;
                state.match_positions.clear();
            }
            KeyCode::Tab if state.find_mode == FindReplaceMode::Replace => {
                state.input_focus = 1 - state.input_focus;
                if state.input_focus == 0 {
                    state.find_cursor_pos = state.find_input.chars().count();
                } else {
                    state.replace_cursor_pos = state.replace_input.chars().count();
                }
            }
            KeyCode::Enter => {
                if state.input_focus == 0 {
                    // 검색 필드에서 Enter - 항상 새로 검색
                    state.find_term = state.find_input.clone();
                    state.perform_find();
                } else if state.find_mode == FindReplaceMode::Replace {
                    state.replace_current();
                }
            }
            KeyCode::Backspace => {
                if state.input_focus == 0 {
                    if state.find_cursor_pos > 0 {
                        let mut chars: Vec<char> = state.find_input.chars().collect();
                        chars.remove(state.find_cursor_pos - 1);
                        state.find_input = chars.into_iter().collect();
                        state.find_cursor_pos -= 1;
                    }
                } else if state.replace_cursor_pos > 0 {
                    let mut chars: Vec<char> = state.replace_input.chars().collect();
                    chars.remove(state.replace_cursor_pos - 1);
                    state.replace_input = chars.into_iter().collect();
                    state.replace_cursor_pos -= 1;
                }
            }
            KeyCode::Delete => {
                if state.input_focus == 0 {
                    let char_count = state.find_input.chars().count();
                    if state.find_cursor_pos < char_count {
                        let mut chars: Vec<char> = state.find_input.chars().collect();
                        chars.remove(state.find_cursor_pos);
                        state.find_input = chars.into_iter().collect();
                    }
                } else {
                    let char_count = state.replace_input.chars().count();
                    if state.replace_cursor_pos < char_count {
                        let mut chars: Vec<char> = state.replace_input.chars().collect();
                        chars.remove(state.replace_cursor_pos);
                        state.replace_input = chars.into_iter().collect();
                    }
                }
            }
            KeyCode::Left => {
                if state.input_focus == 0 {
                    if state.find_cursor_pos > 0 {
                        state.find_cursor_pos -= 1;
                    }
                } else if state.replace_cursor_pos > 0 {
                    state.replace_cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if state.input_focus == 0 {
                    if state.find_cursor_pos < state.find_input.chars().count() {
                        state.find_cursor_pos += 1;
                    }
                } else if state.replace_cursor_pos < state.replace_input.chars().count() {
                    state.replace_cursor_pos += 1;
                }
            }
            KeyCode::Home => {
                if state.input_focus == 0 {
                    state.find_cursor_pos = 0;
                } else {
                    state.replace_cursor_pos = 0;
                }
            }
            KeyCode::End => {
                if state.input_focus == 0 {
                    state.find_cursor_pos = state.find_input.chars().count();
                } else {
                    state.replace_cursor_pos = state.replace_input.chars().count();
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.find_options.case_sensitive = !state.find_options.case_sensitive;
                state.find_term = state.find_input.clone();
                state.perform_find();
            }
            KeyCode::Char('r') | KeyCode::Char('R')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.find_options.use_regex = !state.find_options.use_regex;
                state.find_term = state.find_input.clone();
                state.perform_find();
            }
            KeyCode::Char('w') | KeyCode::Char('W')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                state.find_options.whole_word = !state.find_options.whole_word;
                state.find_term = state.find_input.clone();
                state.perform_find();
            }
            KeyCode::Char('a') | KeyCode::Char('A')
                if modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // 모두 바꾸기
                if state.find_mode == FindReplaceMode::Replace {
                    state.find_term = state.find_input.clone();
                    state.replace_all();
                }
            }
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                // Shift 처리: 일부 터미널에서 Shift+문자가 소문자로 올 수 있음
                let ch = if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                if state.input_focus == 0 {
                    let mut chars: Vec<char> = state.find_input.chars().collect();
                    chars.insert(state.find_cursor_pos, ch);
                    state.find_input = chars.into_iter().collect();
                    state.find_cursor_pos += 1;
                } else {
                    let mut chars: Vec<char> = state.replace_input.chars().collect();
                    chars.insert(state.replace_cursor_pos, ch);
                    state.replace_input = chars.into_iter().collect();
                    state.replace_cursor_pos += 1;
                }
            }
            KeyCode::Down => {
                state.find_next();
            }
            KeyCode::Up => {
                state.find_prev();
            }
            _ => {}
        }
        return;
    }

    // EditorAction 조회 (Ctrl/Alt 조합 및 Esc)
    if let Some(action) = app.keybindings.editor_action(code, modifiers) {
        // Exit 이외의 action은 pending_exit 리셋
        if action != EditorAction::Exit {
            state.pending_exit = false;
        }

        match action {
            EditorAction::Save => {
                let save_result = state.save_file();
                let is_settings = App::is_settings_file(&state.file_path);
                let remote_info = state
                    .remote_origin
                    .as_ref()
                    .map(|o| (o.panel_index, o.remote_path.clone()));
                let local_path = state.file_path.display().to_string();
                match save_result {
                    Ok(_) => {
                        state.pending_exit = false;
                        if is_settings {
                            state.set_message("Settings saved and applied!", 30);
                        } else {
                            state.set_message("File saved!", 30);
                        }
                    }
                    Err(e) => {
                        state.set_message(format!("Save error: {}", e), 50);
                        return;
                    }
                }
                // state borrow ends here due to NLL — now access app freely
                if let Some((panel_idx, remote_path)) = remote_info {
                    if app.remote_spinner.is_some() {
                        // Spinner already active — skip upload
                        if let Some(ref mut editor) = app.editor_state {
                            editor.set_message("Saved locally, remote upload busy".to_string(), 50);
                        }
                    } else {
                        let is_connected = app
                            .panels
                            .get(panel_idx)
                            .and_then(|p| p.remote_ctx.as_ref())
                            .map(|ctx| {
                                matches!(
                                    ctx.status,
                                    crate::services::remote::ConnectionStatus::Connected
                                )
                            })
                            .unwrap_or(false);

                        if is_connected {
                            let ctx = match app.panels[panel_idx].remote_ctx.take() {
                                Some(ctx) => ctx,
                                None => {
                                    if let Some(ref mut editor) = app.editor_state {
                                        editor.set_message(
                                            "Saved locally, remote connection was disconnected"
                                                .to_string(),
                                            50,
                                        );
                                    }
                                    if is_settings {
                                        app.reload_settings();
                                    }
                                    app.refresh_panels();
                                    return;
                                }
                            };
                            let (tx, rx) = std::sync::mpsc::channel();

                            std::thread::spawn(move || {
                                let msg = match ctx.session.upload_file(&local_path, &remote_path) {
                                    Ok(_) => Ok("Saved & uploaded to remote!".to_string()),
                                    Err(e) => Err(format!("Saved locally, upload failed: {}", e)),
                                };
                                let _ = tx.send(crate::ui::app::RemoteSpinnerResult::PanelOp {
                                    ctx,
                                    panel_idx,
                                    outcome: crate::ui::app::PanelOpOutcome::Simple {
                                        message: msg,
                                        pending_focus: None,
                                        reload: true,
                                    },
                                });
                            });

                            app.remote_spinner = Some(crate::ui::app::RemoteSpinner {
                                message: "Uploading...".to_string(),
                                started_at: std::time::Instant::now(),
                                receiver: rx,
                            });
                        } else {
                            let msg = if app
                                .panels
                                .get(panel_idx)
                                .and_then(|p| p.remote_ctx.as_ref())
                                .is_some()
                            {
                                "Saved locally, remote connection lost".to_string()
                            } else {
                                "Saved locally, remote connection was disconnected".to_string()
                            };
                            if let Some(ref mut editor) = app.editor_state {
                                editor.set_message(msg, 50);
                            }
                        }
                    }
                }
                if is_settings {
                    app.reload_settings();
                }
                app.refresh_panels();
            }
            EditorAction::Cut => {
                state.cut_line_or_selection();
            }
            EditorAction::Undo => {
                state.undo();
            }
            EditorAction::Redo => {
                state.redo();
            }
            EditorAction::SelectAll => {
                state.select_all();
            }
            EditorAction::Copy => {
                state.copy();
            }
            EditorAction::Paste => {
                state.paste();
            }
            EditorAction::ToggleWordWrap => {
                state.word_wrap = !state.word_wrap;
                if state.word_wrap {
                    state.horizontal_scroll = 0;
                }
            }
            EditorAction::DeleteLine => {
                state.delete_line();
            }
            EditorAction::DuplicateLine => {
                state.duplicate_line();
            }
            EditorAction::SelectNextOccurrence => {
                state.select_next_occurrence();
            }
            EditorAction::SelectLine => {
                state.select_line();
            }
            EditorAction::ToggleComment => {
                state.toggle_comment();
            }
            EditorAction::Indent => {
                state.indent();
            }
            EditorAction::InsertLineBelow => {
                state.insert_line_below();
            }
            EditorAction::InsertLineAbove => {
                state.insert_line_above();
            }
            EditorAction::MoveWordLeft => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                state.move_word_left(extend_sel);
            }
            EditorAction::MoveWordRight => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                state.move_word_right(extend_sel);
            }
            EditorAction::DeleteWordBackward => {
                state.delete_word_backward();
            }
            EditorAction::DeleteWordForward => {
                state.delete_word_forward();
            }
            EditorAction::Find => {
                state.find_mode = FindReplaceMode::Find;
                state.input_focus = 0;
                state.find_cursor_pos = state.find_input.chars().count();
            }
            EditorAction::Replace => {
                state.find_mode = FindReplaceMode::Replace;
                state.input_focus = 0;
                state.find_cursor_pos = state.find_input.chars().count();
                state.replace_cursor_pos = state.replace_input.chars().count();
            }
            EditorAction::GotoLine => {
                state.goto_mode = true;
                state.goto_input.clear();
            }
            EditorAction::GoToFileStart => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                if extend_sel {
                    if state.selection.is_none() {
                        state.selection = Some(Selection::new(state.cursor_line, state.cursor_col));
                    }
                } else {
                    state.selection = None;
                }
                state.cursor_line = 0;
                state.cursor_col = 0;
                if let Some(ref mut sel) = state.selection {
                    sel.end_line = state.cursor_line;
                    sel.end_col = state.cursor_col;
                }
                state.update_scroll();
            }
            EditorAction::GoToFileEnd => {
                let extend_sel = modifiers.contains(KeyModifiers::SHIFT);
                if extend_sel {
                    if state.selection.is_none() {
                        state.selection = Some(Selection::new(state.cursor_line, state.cursor_col));
                    }
                } else {
                    state.selection = None;
                }
                state.cursor_line = state.lines.len().saturating_sub(1);
                state.cursor_col = state.lines[state.cursor_line].chars().count();
                if let Some(ref mut sel) = state.selection {
                    sel.end_line = state.cursor_line;
                    sel.end_col = state.cursor_col;
                }
                state.update_scroll();
            }
            EditorAction::MoveLineUp => {
                state.move_line_up();
            }
            EditorAction::MoveLineDown => {
                state.move_line_down();
            }
            EditorAction::Exit => {
                if state.selection.is_some() {
                    // 선택 해제 및 다중 커서 초기화
                    state.selection = None;
                    state.cursors.clear();
                    state.last_word_selection = None;
                } else if state.modified {
                    // 변경사항이 있을 때
                    if state.pending_exit {
                        // 두 번째 Esc: 변경 무시하고 종료
                        if let Some(Screen::FileViewer) = app.previous_screen {
                            if let Some(ref mut viewer) = app.viewer_state {
                                viewer.scroll = state.scroll;
                            }
                            app.previous_screen = None;
                            app.current_screen = Screen::FileViewer;
                        } else {
                            app.current_screen = Screen::FilePanel;
                        }
                    } else {
                        // 첫 번째 Esc: 경고 메시지
                        state.pending_exit = true;
                        let exit_key = app.keybindings.editor_first_key(EditorAction::Exit);
                        let save_key = app.keybindings.editor_first_key(EditorAction::Save);
                        state.set_message(
                            format!(
                                "Unsaved changes! Press {} again to discard, {} to save",
                                exit_key, save_key
                            ),
                            60,
                        );
                    }
                } else {
                    // 변경사항 없으면 바로 종료
                    if let Some(Screen::FileViewer) = app.previous_screen {
                        if let Some(ref mut viewer) = app.viewer_state {
                            let scroll = state.scroll;
                            let path = viewer.file_path.clone();
                            let _ = viewer.load_file(&path);
                            viewer.scroll = scroll;
                        }
                        app.previous_screen = None;
                        app.current_screen = Screen::FileViewer;
                    } else {
                        app.current_screen = Screen::FilePanel;
                    }
                }
            }
        }
        return;
    }

    // 일반 모드 (화살표, Home/End, Enter, Tab, Backspace, Delete, 문자 입력)
    state.pending_exit = false;
    let extend_selection = modifiers.contains(KeyModifiers::SHIFT);

    match code {
        KeyCode::Up => {
            state.move_cursor(-1, 0, extend_selection);
        }
        KeyCode::Down => {
            state.move_cursor(1, 0, extend_selection);
        }
        KeyCode::Left => {
            state.move_cursor(0, -1, extend_selection);
        }
        KeyCode::Right => {
            state.move_cursor(0, 1, extend_selection);
        }
        KeyCode::Home => {
            state.move_to_line_start(extend_selection);
        }
        KeyCode::End => {
            state.move_to_line_end(extend_selection);
        }
        KeyCode::PageUp => {
            let page_size = state.visible_height.max(1) as i32;
            state.move_cursor(-page_size, 0, extend_selection);
        }
        KeyCode::PageDown => {
            let page_size = state.visible_height.max(1) as i32;
            state.move_cursor(page_size, 0, extend_selection);
        }
        KeyCode::Backspace => {
            state.delete_backward();
        }
        KeyCode::Delete => {
            state.delete_forward();
        }
        KeyCode::Enter => {
            state.insert_newline();
        }
        KeyCode::Tab => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                // Shift+Tab: 내어쓰기
                state.outdent();
            } else if state.selection.is_some() {
                // 선택 영역이 있으면 들여쓰기
                state.indent();
            } else {
                state.insert_tab();
            }
        }
        KeyCode::BackTab => {
            // BackTab (일부 터미널에서 Shift+Tab): 내어쓰기
            state.outdent();
        }
        KeyCode::Char(c) => {
            if !modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::ALT)
            {
                // 방어적 처리: 일부 터미널에서 Shift+문자가 소문자로 올 수 있음
                let ch = if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                state.insert_char(ch);
            }
        }
        _ => {}
    }
}
