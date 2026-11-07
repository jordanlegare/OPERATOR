use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    conversations::{build_threads, is_hermes_event, thread_for_entry, ConversationThread},
    shell::ShellSession,
    settings::Settings,
    storage::{
        entry_transcript_text, sanitize_single_line, should_suppress_terminal_output, Entry,
        EntryKind, Journal,
    },
    theme::THEMES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Terminal,
    Notes,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "TERMINAL",
            Self::Notes => "NOTES",
        }
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TranscriptPoint {
    row: usize,
    column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptSelection {
    anchor: TranscriptPoint,
    active: TranscriptPoint,
    whole_row: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationView {
    List,
    Thread { id: String },
}

#[derive(Debug)]
pub struct App {
    pub journal: Journal,
    pub input: String,
    pub cursor: usize,
    pub input_hscroll: usize,
    pub feed_hscroll: u16,
    /// Whether transcript records wrap to the available viewport width.
    pub wrap_transcript: bool,
    pub scroll_top: usize,
    /// Journal row currently highlighted by keyboard navigation.
    pub selected_row: Option<usize>,
    pub feed_viewport_height: usize,
    /// Inner transcript viewport in terminal screen coordinates.
    pub feed_area: Rect,
    /// Input panel bounds; right-clicking here pastes clipboard contents.
    pub input_area: Rect,
    selection: Option<TranscriptSelection>,
    selection_dragging: bool,
    pub follow_tail: bool,
    pub search_query: Option<String>,
    pub matches: Vec<usize>,
    pub active_match: Option<usize>,
    pub history: Vec<String>,
    history_position: Option<usize>,
    pub input_mode: InputMode,
    /// Current user@host:path prompt for the embedded shell input line.
    pub terminal_prompt: String,
    /// Currently selected built-in color palette (1 through 9).
    pub theme_index: usize,
    /// True while a recognized password/passphrase prompt is active.
    /// PTY input echo is disabled globally to avoid duplicate command output.
    pub secret_input: bool,
    /// A recent explicit password/passphrase prompt, used together with
    /// ECHO=false to distinguish secrets from unrelated shell echo settings.
    secret_prompt_deadline: Option<Instant>,
    pub status: String,
    pub show_help: bool,
    /// Journal row currently displayed in the full-screen Hermes message viewer.
    pub message_view_index: Option<usize>,
    /// Vertical scroll offset within the Hermes message viewer.
    pub message_view_scroll: u16,
    /// Expandable Hermes conversation list/thread viewer. It is rebuilt from
    /// the live journal every frame so incoming stream deltas appear immediately.
    pub conversation_view: Option<ConversationView>,
    pub conversation_list_selected: usize,
    pub conversation_scroll: usize,
    pub conversation_content_height: usize,
    pub conversation_viewport_height: usize,
    pub conversation_follow_tail: bool,
    /// Whether the ASCII-art banner is currently visible.
    pub show_banner: bool,
    /// Whether the Canadian maple-leaf watermark is displayed behind the transcript.
    pub show_canada_watermark: bool,
    /// Whether SSH SCP/SFTP requests are delegated directly to their handlers.
    pub scp_sftp_passthrough: bool,
    pub should_quit: bool,
}

impl App {
    pub fn input_prompt(&self) -> &str {
        match self.input_mode {
            InputMode::Terminal => &self.terminal_prompt,
            InputMode::Notes => "note› ",
        }
    }

    pub fn set_terminal_prompt(&mut self, prompt: String) {
        self.terminal_prompt = prompt;
    }

    pub fn new(journal: Journal, status: String) -> Self {
        let settings = Settings::load();
        // Rehydrate editable command/note history from the persistent journal.
        // Terminal output and system messages are not input-history candidates.
        let history = journal
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Note | EntryKind::TerminalCommand))
            .map(|entry| entry.text.clone())
            .collect();
        Self {
            journal,
            input: String::new(),
            cursor: 0,
            input_hscroll: 0,
            feed_hscroll: 0,
            wrap_transcript: settings.wrap_transcript,
            scroll_top: 0,
            selected_row: None,
            feed_viewport_height: 1,
            feed_area: Rect::default(),
            input_area: Rect::default(),
            selection: None,
            selection_dragging: false,
            follow_tail: true,
            search_query: None,
            matches: Vec::new(),
            active_match: None,
            history,
            history_position: None,
            input_mode: InputMode::Terminal,
            terminal_prompt: "user@host:~$ ".to_string(),
            theme_index: settings.theme_index.clamp(1, THEMES.len()),
            secret_input: false,
            secret_prompt_deadline: None,
            status,
            show_help: false,
            message_view_index: None,
            message_view_scroll: 0,
            conversation_view: None,
            conversation_list_selected: 0,
            conversation_scroll: 0,
            conversation_content_height: 0,
            conversation_viewport_height: 1,
            conversation_follow_tail: true,
            show_banner: settings.show_banner,
            show_canada_watermark: settings.show_canada_watermark,
            scp_sftp_passthrough: settings.scp_sftp_passthrough,
            should_quit: false,
        }
    }

    pub fn current_top(&self) -> usize {
        if self.follow_tail {
            self.journal.entries.len().saturating_sub(self.feed_viewport_height)
        } else {
            self.scroll_top.min(self.max_top())
        }
    }

    /// True when the highlighted row is the current tail and should advance
    /// with incoming records while LIVE mode is active.
    pub fn is_tracking_selected_tail(&self) -> bool {
        self.follow_tail
            && self.selected_row.is_some()
            && self.selected_row == self.journal.entries.len().checked_sub(1)
    }

    /// Keep a selected live-tail row attached to the newest record after import.
    pub fn update_selected_tail_after_append(&mut self, was_tracking_tail: bool) {
        if was_tracking_tail && self.follow_tail {
            self.selected_row = self.journal.entries.len().checked_sub(1);
            self.scroll_top = self.max_top();
        }
    }

    fn max_top(&self) -> usize {
        self.journal.entries.len().saturating_sub(self.feed_viewport_height.max(1))
    }

    /// Move the highlighted transcript row. On the first keypress, start at
    /// the edge of the currently visible page in the requested direction.
    fn recall_highlighted_command(&mut self) {
        if self.secret_input {
            return;
        }
        let command = self
            .selected_row
            .and_then(|row| self.journal.entries.get(row))
            .filter(|entry| entry.kind == EntryKind::TerminalCommand)
            .map(|entry| entry.text.clone());
        if let Some(command) = command {
            self.input = command;
            self.cursor = self.input.len();
            self.input_hscroll = 0;
            self.history_position = None;
        }
    }

    /// Extend a full-row transcript selection with Shift+Up/Down.
    /// The first keypress anchors at the current row (or visible edge).
    pub fn extend_row_selection(&mut self, direction: i8) {
        let entry_count = self.journal.entries.len();
        if entry_count == 0 {
            return;
        }

        let top = self.current_top().min(entry_count - 1);
        let bottom = top
            .saturating_add(self.feed_viewport_height.max(1).saturating_sub(1))
            .min(entry_count - 1);
        let anchor_row = self.selection
            .filter(|selection| selection.whole_row)
            .map(|selection| selection.anchor.row)
            .unwrap_or_else(|| self.selected_row.unwrap_or(if direction < 0 { bottom } else { top }));
        let current_row = self.selection
            .filter(|selection| selection.whole_row)
            .map(|selection| selection.active.row)
            .unwrap_or(anchor_row);
        let next_row = if direction < 0 {
            current_row.saturating_sub(1)
        } else {
            current_row.saturating_add(1).min(entry_count - 1)
        };

        self.selection = Some(TranscriptSelection {
            anchor: TranscriptPoint { row: anchor_row, column: 0 },
            active: TranscriptPoint { row: next_row, column: usize::MAX },
            whole_row: true,
        });
        self.selection_dragging = false;
        self.selected_row = Some(next_row);
        self.follow_tail = false;
        let viewport = self.feed_viewport_height.max(1);
        let mut new_top = self.scroll_top.min(self.max_top());
        if next_row < new_top {
            new_top = next_row;
        } else if next_row >= new_top.saturating_add(viewport) {
            new_top = next_row.saturating_add(1).saturating_sub(viewport);
        }
        self.scroll_top = new_top.min(self.max_top());
        self.status = format!(
            "Selected transcript rows {}–{}; Ctrl+C copies the range.",
            anchor_row.min(next_row) + 1,
            anchor_row.max(next_row) + 1
        );
    }

    pub fn move_row_selection(&mut self, direction: i8) {
        let entry_count = self.journal.entries.len();
        if entry_count == 0 {
            self.selected_row = None;
            return;
        }

        let viewport_height = self.feed_viewport_height.max(1);
        let top = self.current_top().min(entry_count - 1);
        let bottom = top
            .saturating_add(viewport_height.saturating_sub(1))
            .min(entry_count - 1);
        let current = self.selected_row.unwrap_or(if direction < 0 { bottom } else { top });
        let next = if direction < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(entry_count - 1)
        };

        self.selected_row = Some(next);
        self.selection = None;
        self.selection_dragging = false;
        self.follow_tail = next == entry_count - 1;

        if self.follow_tail {
            self.scroll_top = self.max_top();
            self.status = format!("LIVE · latest row {}/{} selected.", next + 1, entry_count);
        } else {
            let mut next_top = top;
            if next < next_top {
                next_top = next;
            } else if next >= next_top.saturating_add(viewport_height) {
                next_top = next.saturating_add(1).saturating_sub(viewport_height);
            }
            self.scroll_top = next_top.min(self.max_top());
            self.status = format!("Row {}/{} selected.", next + 1, entry_count);
        }
        self.recall_highlighted_command();
    }

    fn keep_selected_row_visible(&mut self, top: usize) {
        let Some(row) = self.selected_row else {
            return;
        };
        let entry_count = self.journal.entries.len();
        if entry_count == 0 {
            self.selected_row = None;
            return;
        }
        let viewport_height = self.feed_viewport_height.max(1);
        let last_visible = top
            .saturating_add(viewport_height.saturating_sub(1))
            .min(entry_count - 1);
        let visible_row = row.max(top).min(last_visible);
        self.selected_row = Some(visible_row);
        if visible_row != row {
            self.status = format!("Row {}/{} selected.", visible_row + 1, entry_count);
            self.recall_highlighted_command();
        }
    }

    pub fn scroll_up(&mut self, amount: usize) {
        let current = self.current_top();
        self.scroll_top = current.saturating_sub(amount);
        self.follow_tail = false;
        self.keep_selected_row_visible(self.scroll_top);
    }

    pub fn scroll_down(&mut self, amount: usize) {
        let next = self.current_top().saturating_add(amount).min(self.max_top());
        self.scroll_top = next;
        self.follow_tail = false;
        self.keep_selected_row_visible(next);

        let highlighted_latest = self.selected_row
            == self.journal.entries.len().checked_sub(1);
        if next >= self.max_top()
            && (self.selected_row.is_none() || highlighted_latest)
        {
            self.follow_tail = true;
            self.scroll_top = self.max_top();
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.follow_tail = false;
        self.selected_row = (!self.journal.entries.is_empty()).then_some(0);
        self.recall_highlighted_command();
    }

    pub fn jump_to_latest(&mut self) {
        self.follow_tail = true;
        self.scroll_top = self.max_top();
        self.selected_row = None;
    }

    /// Explicit latest-row navigation selects the newest row and recalls it
    /// when it is a command. Automatic tail-following preserves the input draft.
    pub fn select_latest_row(&mut self) {
        self.jump_to_latest();
        self.selected_row = self.journal.entries.len().checked_sub(1);
        self.recall_highlighted_command();
    }

    pub fn scroll_feed_left(&mut self, amount: u16) {
        self.feed_hscroll = self.feed_hscroll.saturating_sub(amount);
    }

    pub fn scroll_feed_right(&mut self, amount: u16) {
        self.feed_hscroll = self.feed_hscroll.saturating_add(amount);
    }

    /// Begin a left-button selection inside the transcript viewport.
    /// A click without a drag selects the entire journal row.
    pub fn begin_transcript_selection(&mut self, x: u16, y: u16) {
        let Some(point) = self.mouse_transcript_point(x, y, false) else {
            self.selection = None;
            self.selection_dragging = false;
            return;
        };
        self.selection = Some(TranscriptSelection {
            anchor: point,
            active: point,
            whole_row: false,
        });
        self.selection_dragging = true;
    }

    /// Extend the active selection while the left mouse button is held.
    pub fn drag_transcript_selection(&mut self, x: u16, y: u16) {
        if !self.selection_dragging {
            return;
        }
        if let Some(point) = self.mouse_transcript_point(x, y, true) {
            let is_drag = self
                .selection
                .is_some_and(|selection| selection.anchor != point);
            if is_drag {
                self.selected_row = None;
            }
            if let Some(selection) = self.selection.as_mut() {
                selection.active = point;
                selection.whole_row = false;
            }
        }
    }

    /// A stationary click acts only on CMD rows. Dragging still selects
    /// arbitrary transcript text across notes, commands, and output.
    pub fn end_transcript_selection(&mut self, x: u16, y: u16) {
        if !self.selection_dragging {
            return;
        }
        self.drag_transcript_selection(x, y);
        self.selection_dragging = false;
        let clicked_row = self.selection.and_then(|selection| {
            (selection.anchor == selection.active).then_some(selection.anchor.row)
        });

        if let Some(row) = clicked_row {
            let is_hermes_event = self.journal.entries.get(row).is_some_and(|entry| {
                is_hermes_event(entry.kind)
            });

            if is_hermes_event {
                // A stationary click on a Hermes transcript row is the mouse
                // equivalent of selecting that row and executing /view.
                self.selection = None;
                self.selected_row = Some(row);
                self.follow_tail = row == self.journal.entries.len().saturating_sub(1);
                if self.follow_tail {
                    self.scroll_top = self.max_top();
                }
                self.open_conversation_for_entry(row);
                return;
            }

            let command = self
                .journal
                .entries
                .get(row)
                .filter(|entry| entry.kind == EntryKind::TerminalCommand)
                .map(|entry| entry.text.clone());
            if let Some(command) = command {
                if let Some(selection) = self.selection.as_mut() {
                    selection.whole_row = true;
                }
                self.selected_row = None;
                // Never replace a hidden password/passphrase draft.
                if !self.secret_input {
                    self.input = command;
                    self.cursor = self.input.len();
                    self.input_hscroll = 0;
                    self.history_position = None;
                    self.status = format!(
                        "Loaded command from transcript row {}. Edit it or press Enter to submit.",
                        row + 1
                    );
                }
            } else {
                // Clicking a note/output/system row is intentionally a no-op.
                self.selection = None;
            }
        } else {
            // A real drag is a text selection, not a keyboard row highlight.
            self.selected_row = None;
        }
    }

    fn mouse_transcript_point(&self, x: u16, y: u16, clamp: bool) -> Option<TranscriptPoint> {
        let area = self.feed_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }

        let right = area.x.saturating_add(area.width);
        let bottom = area.y.saturating_add(area.height);
        if !clamp && (x < area.x || x >= right || y < area.y || y >= bottom) {
            return None;
        }

        let x = x.clamp(area.x, right.saturating_sub(1));
        let y = y.clamp(area.y, bottom.saturating_sub(1));
        let row = self.current_top().saturating_add(y.saturating_sub(area.y) as usize);
        if row >= self.journal.entries.len() {
            return None;
        }

        Some(TranscriptPoint {
            row,
            column: x.saturating_sub(area.x) as usize + self.feed_hscroll as usize,
        })
    }

    /// Return the selected display-cell range for one journal row.
    pub fn selection_columns_for_row(
        &self,
        row: usize,
        line_width: usize,
    ) -> Option<(usize, usize)> {
        let selection = self.selection?;
        if selection.whole_row {
            let first = selection.anchor.row.min(selection.active.row);
            let last = selection.anchor.row.max(selection.active.row);
            return (row >= first && row <= last && line_width > 0)
                .then_some((0, line_width));
        }

        let (start, end) = if selection.anchor <= selection.active {
            (selection.anchor, selection.active)
        } else {
            (selection.active, selection.anchor)
        };
        if row < start.row || row > end.row || line_width == 0 {
            return None;
        }

        let from = (if row == start.row { start.column } else { 0 }).min(line_width);
        let to = (if row == end.row {
            end.column.saturating_add(1)
        } else {
            line_width
        })
        .min(line_width);

        (from < to).then_some((from, to))
    }

    /// Plain-text selection includes row number, full timestamp, kind and entry text.
    pub fn selected_transcript_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = if selection.anchor <= selection.active {
            (selection.anchor, selection.active)
        } else {
            (selection.active, selection.anchor)
        };

        let mut selected_rows = Vec::new();
        for row in start.row..=end.row {
            let entry = self.journal.entries.get(row)?;
            let line = entry_transcript_text(entry, row);
            let width = UnicodeWidthStr::width(line.as_str());
            let (from, to) = if selection.whole_row {
                (0, width)
            } else {
                let from = (if row == start.row { start.column } else { 0 }).min(width);
                let to = (if row == end.row {
                    end.column.saturating_add(1)
                } else {
                    width
                })
                .min(width);
                (from, to)
            };
            selected_rows.push(slice_display_columns(&line, from, to));
        }

        let selected = selected_rows.join("\n");
        (!selected.is_empty()).then_some(selected)
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        self.input.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_position = None;
    }

    pub fn paste(&mut self, value: &str) {
        // Treat CRLF as one line break before flattening clipboard text into
        // the single-line command/note input.
        let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
        let clean = sanitize_single_line(&normalized);
        self.input.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.history_position = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.input[..self.cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.input.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = self.input[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.input.len());
        self.input.replace_range(self.cursor..next, "");
    }

    pub fn cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.input[..self.cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    pub fn cursor_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor += self.input[self.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
    }

    pub fn cursor_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_to_end(&mut self) {
        self.cursor = self.input.len();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.input_hscroll = 0;
        self.history_position = None;
    }

    pub fn update_secret_input(&mut self, terminal_echo: Option<bool>, output: &[String]) {
        if self.input_mode != InputMode::Terminal {
            self.secret_input = false;
            self.secret_prompt_deadline = None;
            return;
        }

        // Do not treat every ECHO=false state as a password prompt. Shell
        // startup files and terminal programs can disable echo for unrelated
        // reasons. Require recognizable secret-prompt text as well.
        if output.iter().any(|line| looks_like_secret_prompt(line)) {
            self.secret_prompt_deadline = Some(Instant::now() + Duration::from_secs(5));
        }

        match terminal_echo {
            Some(false) => {
                let prompt_is_recent = self
                    .secret_prompt_deadline
                    .is_some_and(|deadline| Instant::now() <= deadline);
                if prompt_is_recent {
                    self.secret_input = true;
                }
            }
            Some(true) => {
                self.secret_prompt_deadline = None;
                if self.input.is_empty() {
                    self.secret_input = false;
                }
            }
            None => {}
        }
    }

    pub fn toggle_mode(&mut self) {
        if self.secret_input {
            self.status = "Terminal echo is disabled. Submit the hidden input or interrupt the process before switching modes.".to_string();
            return;
        }
        self.input_mode = match self.input_mode {
            InputMode::Terminal => InputMode::Notes,
            InputMode::Notes => InputMode::Terminal,
        };
        self.status = format!(
            "{} mode. {}",
            self.input_mode.label(),
            if self.input_mode == InputMode::Terminal {
                "Submitted lines are sent to the Debian shell."
            } else {
                "Submitted lines are captured as notes."
            }
        );
    }

    pub fn set_mode(&mut self, mode: InputMode) {
        self.input_mode = mode;
        if mode == InputMode::Notes {
            self.secret_input = false;
            self.secret_prompt_deadline = None;
        }
        self.status = format!("{} mode enabled.", self.input_mode.label());
    }

    pub fn submit(&mut self, shell: Option<&mut ShellSession>) {
        let secret_terminal_input = self.secret_input && self.input_mode == InputMode::Terminal;
        let submitted = if secret_terminal_input {
            self.input.clone()
        } else {
            self.input.trim().to_string()
        };
        if submitted.is_empty() && !secret_terminal_input {
            self.clear_input();
            return;
        }

        self.clear_input();
        if secret_terminal_input {
            // PTY echo is disabled globally. Clear the prompt-scoped mask after
            // submitting this secret; a repeated password prompt will arm it again.
            self.secret_input = false;
            self.secret_prompt_deadline = None;
            // Do not add secret input to the journal or interactive history.
            match shell {
                Some(session) => match session.send_line(&submitted) {
                    Ok(()) => self.status = "Sent hidden terminal input; it was not journaled or added to history.".to_string(),
                    Err(error) => self.status = format!("Could not send hidden terminal input: {error}"),
                },
                None => self.status = "No shell session is available.".to_string(),
            }
            return;
        }

        self.history.push(submitted.clone());
        self.history_position = None;

        if submitted.starts_with("//") {
            self.commit_note(submitted[1..].to_string());
        } else if is_streamnote_command(&submitted)
            || (self.input_mode == InputMode::Notes && submitted.starts_with('/'))
        {
            self.execute_command(&submitted);
        } else if let Some(command) = submitted.strip_prefix('!') {
            self.dispatch_terminal_command(command.trim(), shell);
        } else if self.input_mode == InputMode::Terminal {
            // Absolute paths such as /usr/bin/ls are valid shell commands;
            // only recognized StreamNote slash-commands are intercepted.
            self.dispatch_terminal_command(&submitted, shell);
        } else {
            self.commit_note(submitted);
        }
    }

    fn dispatch_terminal_command(
        &mut self,
        command: &str,
        shell: Option<&mut ShellSession>,
    ) {
        if command.is_empty() {
            return;
        }

        let text = sanitize_single_line(command);
        let entry = Entry::with_kind(EntryKind::TerminalCommand, text.clone());
        if let Err(error) = self.journal.append(entry) {
            self.status = format!(
                "Command captured in memory; journal write failed ({} pending): {}",
                self.journal.pending_count(),
                error
            );
        }

        match shell {
            Some(session) => match session.send_line(&text) {
                Ok(()) => {
                    self.status = format!("Sent to Debian shell: {text}");
                    self.clear_search();
                    self.jump_to_latest();
                }
                Err(error) => {
                    self.record_system(format!("Shell input failed: {error}"));
                    self.status = format!("Shell input failed: {error}");
                }
            },
            None => {
                self.status = "No shell session is available. Check that /bin/bash or $SHELL exists.".to_string();
            }
        }
    }

    pub fn capture_terminal_output(&mut self, lines: Vec<String>) {
        let entries: Vec<Entry> = lines
            .into_iter()
            .map(|line| sanitize_single_line(&line))
            .filter(|line| !should_suppress_terminal_output(line))
            .map(|line| Entry::with_kind(EntryKind::TerminalOutput, line))
            .collect();
        if entries.is_empty() {
            return;
        }
        let selected_live_tail = self.is_tracking_selected_tail();
        match self.journal.append_batch(entries) {
            Ok(()) => self.status = format!("Captured terminal output · {} records in journal.", self.journal.entries.len()),
            Err(error) => self.status = format!(
                "Terminal output retained in memory; {} pending disk write(s): {error}",
                self.journal.pending_count()
            ),
        }
        self.update_selected_tail_after_append(selected_live_tail);
        // Keep the current scrollback/search position when background shell
        // output arrives. The feed follows automatically only when already live.
    }

    /// Open the full Hermes body only when the highlighted row is a Hermes event.
    /// Returns true when a modal was opened. Used by Enter so ordinary input
    /// submission remains unchanged when no Hermes row is selected.
    pub fn open_selected_hermes_message(&mut self) -> bool {
        let Some(index) = self.selected_row else {
            return false;
        };
        let is_hermes_event = self.journal.entries.get(index).is_some_and(|entry| {
            is_hermes_event(entry.kind)
        });
        if !is_hermes_event {
            return false;
        }

        self.open_conversation_for_entry(index)
    }

    /// Open the conversation containing a selected Hermes event. New events
    /// are associated by session ID; legacy journal rows use a conservative
    /// prompt-to-following-events fallback grouping.
    pub fn open_conversation_for_entry(&mut self, index: usize) -> bool {
        let Some(id) = thread_for_entry(&self.journal.entries, index) else {
            return false;
        };
        self.conversation_view = Some(ConversationView::Thread { id });
        self.conversation_scroll = 0;
        self.conversation_follow_tail = true;
        true
    }

    pub fn open_conversations(&mut self) {
        let threads = build_threads(&self.journal.entries);
        if threads.is_empty() {
            self.status = "No Hermes conversations are present in the journal.".to_string();
            return;
        }
        self.conversation_list_selected = threads.len().saturating_sub(1);
        self.conversation_view = Some(ConversationView::List);
        self.status = format!("{} Hermes conversation(s). Enter expands a thread.", threads.len());
    }

    pub fn conversation_threads(&self) -> Vec<ConversationThread> {
        build_threads(&self.journal.entries)
    }

    pub fn open_selected_conversation(&mut self) {
        let threads = self.conversation_threads();
        let Some(thread) = threads.get(self.conversation_list_selected) else {
            self.conversation_view = None;
            return;
        };
        self.conversation_view = Some(ConversationView::Thread { id: thread.id.clone() });
        self.conversation_scroll = 0;
        self.conversation_follow_tail = true;
    }

    pub fn move_conversation_selection(&mut self, direction: i32) {
        let count = self.conversation_threads().len();
        if count == 0 {
            self.conversation_list_selected = 0;
            return;
        }
        self.conversation_list_selected = if direction < 0 {
            self.conversation_list_selected.saturating_sub(direction.unsigned_abs() as usize)
        } else {
            self.conversation_list_selected.saturating_add(direction as usize).min(count - 1)
        };
    }

    pub fn close_conversation_view(&mut self) {
        self.conversation_view = None;
        self.conversation_scroll = 0;
        self.conversation_follow_tail = true;
    }

    pub fn scroll_conversation_view(&mut self, amount: i32) {
        if amount < 0 {
            if self.conversation_follow_tail {
                self.conversation_scroll = self.conversation_content_height
                    .saturating_sub(self.conversation_viewport_height);
                self.conversation_follow_tail = false;
            }
            self.conversation_scroll = self.conversation_scroll
                .saturating_sub(amount.unsigned_abs() as usize);
        } else if amount > 0 && !self.conversation_follow_tail {
            let max_scroll = self.conversation_content_height
                .saturating_sub(self.conversation_viewport_height);
            self.conversation_scroll = self.conversation_scroll
                .saturating_add(amount as usize)
                .min(max_scroll);
            if self.conversation_scroll >= max_scroll {
                self.conversation_follow_tail = true;
            }
        }
    }

    pub fn conversation_view_home(&mut self) {
        self.conversation_follow_tail = false;
        self.conversation_scroll = 0;
    }

    pub fn conversation_view_end(&mut self) {
        self.conversation_follow_tail = true;
    }

    pub fn selected_conversation_text(&self) -> Option<String> {
        let Some(ConversationView::Thread { id }) = self.conversation_view.as_ref() else {
            return None;
        };
        let thread = self.conversation_threads().into_iter().find(|thread| &thread.id == id)?;
        let mut output = String::new();
        for index in thread.entries {
            let entry = self.journal.entries.get(index)?;
            let label = match entry.kind {
                EntryKind::HermesUser => "USER",
                EntryKind::HermesAssistant => "ASSISTANT",
                EntryKind::HermesToolCall => "TOOL CALL",
                EntryKind::HermesToolResult => "TOOL RESULT",
                EntryKind::HermesStreamDelta => "ASSISTANT STREAM",
                EntryKind::HermesStreamStart => "STREAM START",
                EntryKind::HermesStreamEnd => "STREAM END",
                _ => "HERMES",
            };
            output.push_str(&format!("[{label}] {}\n{}\n\n", entry.timestamp, entry.text));
        }
        Some(output)
    }

    /// Open the full body of the selected Hermes message or tool event.
    pub fn open_hermes_message(&mut self) {
        let is_hermes_event_at = |index: usize| {
            self.journal.entries.get(index).is_some_and(|entry| is_hermes_event(entry.kind))
        };
        let candidate = self
            .selected_row
            .filter(|index| is_hermes_event_at(*index))
            .or_else(|| {
                self.active_match
                    .and_then(|match_index| self.matches.get(match_index).copied())
                    .filter(|index| is_hermes_event_at(*index))
            })
            .or_else(|| {
                self.journal
                    .entries
                    .iter()
                    .rposition(|entry| is_hermes_event(entry.kind))
            });

        let Some(index) = candidate else {
            self.status = "No Hermes messages or tool events are present in the journal.".to_string();
            return;
        };

        self.message_view_index = Some(index);
        self.message_view_scroll = 0;
    }
    pub fn close_hermes_message(&mut self) {
        self.message_view_index = None;
        self.message_view_scroll = 0;
    }

    pub fn scroll_hermes_message(&mut self, amount: i32) {
        self.message_view_scroll = if amount < 0 {
            self.message_view_scroll.saturating_sub(amount.unsigned_abs() as u16)
        } else {
            self.message_view_scroll.saturating_add(amount as u16)
        };
    }

    pub fn record_system(&mut self, message: String) {
        let selected_live_tail = self.is_tracking_selected_tail();
        let entry = Entry::with_kind(EntryKind::System, sanitize_single_line(&message));
        if let Err(error) = self.journal.append(entry) {
            self.status = format!("System event retained in memory; journal write failed: {error}");
        } else {
            self.status = message;
        }
        self.update_selected_tail_after_append(selected_live_tail);
    }

    fn commit_note(&mut self, text: String) {
        let entry = Entry::with_kind(EntryKind::Note, sanitize_single_line(&text));
        match self.journal.append(entry) {
            Ok(()) => self.status = format!(
                "Captured note. {} entries · {} text bytes in cache.",
                self.journal.entries.len(),
                self.journal.total_text_bytes
            ),
            Err(error) => self.status = format!(
                "Captured in memory; disk write failed ({} pending): {}",
                self.journal.pending_count(),
                error
            ),
        }
        self.clear_search();
        self.jump_to_latest();
    }

    fn persist_settings(&self) -> io::Result<()> {
        Settings {
            scp_sftp_passthrough: self.scp_sftp_passthrough,
            theme_index: self.theme_index,
            wrap_transcript: self.wrap_transcript,
            show_banner: self.show_banner,
            show_canada_watermark: self.show_canada_watermark,
        }
        .save()
    }

    fn set_scp_sftp_passthrough(&mut self, enabled: bool) {
        if self.scp_sftp_passthrough == enabled {
            self.status = format!(
                "SCP/SFTP passthrough is already {}.",
                if enabled { "enabled" } else { "disabled" }
            );
            return;
        }

        self.scp_sftp_passthrough = enabled;
        self.status = match self.persist_settings() {
            Ok(()) => format!(
                "SCP/SFTP passthrough {}. Applies to new SSH connections; settings saved.",
                if enabled { "enabled" } else { "disabled" }
            ),
            Err(error) => format!(
                "SCP/SFTP passthrough changed for this session but could not be saved: {error}"
            ),
        };
    }

    fn set_canada_watermark(&mut self, enabled: bool) {
        if self.show_canada_watermark == enabled {
            self.status = format!(
                "Canadian maple-leaf watermark is already {}.",
                if enabled { "on" } else { "off" }
            );
            return;
        }

        self.show_canada_watermark = enabled;
        self.status = match self.persist_settings() {
            Ok(()) => format!(
                "Canadian maple-leaf watermark {} (saved).",
                if enabled { "enabled" } else { "disabled" }
            ),
            Err(error) => format!(
                "Canadian maple-leaf watermark changed for this session but could not be saved: {error}"
            ),
        };
    }

    pub fn toggle_wrap_transcript(&mut self) {
        self.wrap_transcript = !self.wrap_transcript;
        if self.wrap_transcript {
            self.feed_hscroll = 0;
        }
        self.status = match self.persist_settings() {
            Ok(()) => format!(
                "Transcript word wrap {} (saved).",
                if self.wrap_transcript { "enabled" } else { "disabled" }
            ),
            Err(error) => format!(
                "Transcript word wrap changed for this session but could not be saved: {error}"
            ),
        };
    }

    pub fn toggle_banner(&mut self) {
        self.show_banner = !self.show_banner;
        self.status = match self.persist_settings() {
            Ok(()) => format!(
                "Banner {} (saved).",
                if self.show_banner { "shown" } else { "hidden" }
            ),
            Err(error) => format!(
                "Banner visibility changed for this session but could not be saved: {error}"
            ),
        };
    }

    fn execute_command(&mut self, command_line: &str) {
        let command_line = command_line.trim();
        let (command, argument) = command_line
            .split_once(char::is_whitespace)
            .map(|(head, tail)| (head.to_ascii_lowercase(), tail.trim()))
            .unwrap_or((command_line.to_ascii_lowercase(), ""));

        match command.as_str() {
            "/help" => self.show_help = true,
            "/view" => self.open_hermes_message(),
            "/threads" | "/conversations" => self.open_conversations(),
            "/search" => {
                if argument.is_empty() {
                    self.clear_search();
                    self.status = "Search cleared. Usage: /search words to find".to_string();
                } else {
                    self.search(argument);
                }
            }
            "/clear" => {
                self.clear_search();
                self.jump_to_latest();
                self.status = "View cleared; journal and cache were not deleted.".to_string();
            }
            "/save" => self.save(),
            "/stats" => {
                self.status = format!(
                    "{} entries · {} text bytes · {} pending disk write(s) · {} search hit(s)",
                    self.journal.entries.len(),
                    self.journal.total_text_bytes,
                    self.journal.pending_count(),
                    self.matches.len()
                );
            }
            "/path" => self.status = format!("Journal: {}", self.journal.path.display()),
            "/scp" => match argument.to_ascii_lowercase().as_str() {
                "" | "status" => {
                    self.status = format!(
                        "SCP/SFTP passthrough is {}. Usage: /scp on | /scp off | /scp status",
                        if self.scp_sftp_passthrough { "enabled" } else { "disabled" }
                    );
                }
                "on" | "enable" | "enabled" => self.set_scp_sftp_passthrough(true),
                "off" | "disable" | "disabled" => self.set_scp_sftp_passthrough(false),
                _ => self.status = "Usage: /scp on | /scp off | /scp status".to_string(),
            },
            "/canada" => match argument.to_ascii_lowercase().as_str() {
                "" | "toggle" => self.set_canada_watermark(!self.show_canada_watermark),
                "status" => {
                    self.status = format!(
                        "Canadian maple-leaf watermark is {}. Usage: /canada [on|off|status]",
                        if self.show_canada_watermark { "on" } else { "off" }
                    );
                }
                "on" | "enable" | "enabled" => self.set_canada_watermark(true),
                "off" | "disable" | "disabled" => self.set_canada_watermark(false),
                _ => self.status = "Usage: /canada [on|off|status]".to_string(),
            },
            "/mode" => {
                if argument.eq_ignore_ascii_case("terminal") || argument.eq_ignore_ascii_case("shell") {
                    self.set_mode(InputMode::Terminal);
                } else if argument.eq_ignore_ascii_case("notes") || argument.eq_ignore_ascii_case("note") {
                    self.set_mode(InputMode::Notes);
                } else {
                    self.toggle_mode();
                }
            }
            "/shell" | "/terminal" => self.set_mode(InputMode::Terminal),
            "/notes" => self.set_mode(InputMode::Notes),
            "/note" => {
                if argument.is_empty() {
                    self.set_mode(InputMode::Notes);
                } else {
                    self.commit_note(argument.to_string());
                }
            }
            "/export" => {
                let destination = if argument.is_empty() {
                    default_export_path(&self.journal.path)
                } else {
                    PathBuf::from(argument)
                };
                match self.journal.export_plain_text(&destination) {
                    Ok(()) => self.status = format!(
                        "Exported {} entries to {}",
                        self.journal.entries.len(),
                        destination.display()
                    ),
                    Err(error) => self.status = format!("Export failed: {error}"),
                }
            }
            "/quit" | "/q" => self.should_quit = true,
            _ => self.status = format!("Unknown command: {command}. Type /help for command help."),
        }
    }

    pub fn save(&mut self) {
        match self.journal.save_pending() {
            Ok(()) => self.status = format!("Saved. Journal: {}", self.journal.path.display()),
            Err(error) => self.status = format!("Save failed: {error}"),
        }
    }

    pub fn clear_search(&mut self) {
        self.search_query = None;
        self.matches.clear();
        self.active_match = None;
    }

    fn search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            self.clear_search();
            self.status = "Search cleared.".to_string();
            return;
        }

        let needle = query.to_ascii_lowercase();
        let mut found = Vec::new();
        for (index, entry) in self.journal.entries.iter().enumerate() {
            if entry.text.to_ascii_lowercase().contains(needle.as_str()) {
                found.push(index);
            }
        }

        self.search_query = Some(query.to_string());
        self.matches = found;
        if let Some(&latest_match) = self.matches.last() {
            self.active_match = Some(self.matches.len() - 1);
            self.follow_tail = false;
            self.scroll_top = latest_match
                .saturating_sub(self.feed_viewport_height.min(8).max(1) / 2);
            self.scroll_top = self.scroll_top.min(self.max_top());
            self.status = format!(
                "Found {} hit(s) for {:?}. Showing latest hit with nearby context; n/N navigate.",
                self.matches.len(),
                query
            );
        } else {
            self.active_match = None;
            self.status = format!(
                "No matches for {:?} in {} cached entries.",
                query,
                self.journal.entries.len()
            );
        }
    }

    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let next = self.active_match.unwrap_or(0).saturating_add(1) % self.matches.len();
        self.activate_match(next);
    }

    pub fn previous_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let current = self.active_match.unwrap_or(0);
        let previous = if current == 0 {
            self.matches.len() - 1
        } else {
            current - 1
        };
        self.activate_match(previous);
    }

    fn activate_match(&mut self, position: usize) {
        self.active_match = Some(position);
        let entry_index = self.matches[position];
        self.follow_tail = false;
        let context = self.feed_viewport_height.min(8).max(1) / 2;
        self.scroll_top = entry_index.saturating_sub(context).min(self.max_top());
        self.status = format!(
            "Hit {}/{} · entry {} · n next / N previous · query: {}",
            position + 1,
            self.matches.len(),
            entry_index + 1,
            self.search_query.as_deref().unwrap_or("")
        );
    }

    pub fn is_match(&self, entry_index: usize) -> bool {
        self.matches.binary_search(&entry_index).is_ok()
    }

    pub fn is_active_match(&self, entry_index: usize) -> bool {
        self.active_match
            .and_then(|position| self.matches.get(position))
            .is_some_and(|index| *index == entry_index)
    }

    pub fn history_previous(&mut self) {
        if self.secret_input || self.history.is_empty() {
            return;
        }
        let position = match self.history_position {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_position = Some(position);
        self.replace_input(self.history[position].clone());
    }

    pub fn history_next(&mut self) {
        if self.secret_input {
            return;
        }
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 >= self.history.len() {
            self.history_position = None;
            self.clear_input();
        } else {
            self.history_position = Some(position + 1);
            self.replace_input(self.history[position + 1].clone());
        }
    }

    fn replace_input(&mut self, value: String) {
        self.input = value;
        self.cursor = self.input.len();
        self.input_hscroll = 0;
    }

    /// Select a built-in high-contrast palette using its 1-based shortcut number.
    pub fn set_theme(&mut self, number: usize) {
        if let Some(theme) = number.checked_sub(1).and_then(|index| THEMES.get(index)) {
            self.theme_index = number;
            self.status = match self.persist_settings() {
                Ok(()) => format!("Color theme {number}/9 selected: {} (saved).", theme.name),
                Err(error) => format!(
                    "Color theme {number}/9 selected: {}, but settings could not be saved: {error}",
                    theme.name
                ),
            };
        }
    }

    pub fn focus_search_hint(&mut self) {
        if !self.secret_input && self.input.is_empty() {
            self.replace_input("/search ".to_string());
        }
    }

    /// Complete the token at the input cursor using the embedded Bash shell.
    pub fn tab_complete(&mut self, shell: Option<&ShellSession>) {
        if self.secret_input {
            return;
        }

        if self.input_mode == InputMode::Notes {
            self.input.insert_str(self.cursor, "    ");
            self.cursor += 4;
            self.history_position = None;
            return;
        }

        let Some(shell) = shell else {
            self.status = "Shell completion is unavailable because no shell session is running.".to_string();
            return;
        };

        let candidates = match shell.complete_candidates(&self.input, self.cursor) {
            Ok(candidates) => candidates,
            Err(error) => {
                self.status = format!("Tab completion failed: {error}");
                return;
            }
        };

        if candidates.is_empty() {
            self.status = "No completion matches.".to_string();
            return;
        }

        let before_cursor = &self.input[..self.cursor];
        let token_start = before_cursor
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(index, ch)| index + ch.len_utf8())
            .unwrap_or(0);
        let prefix = self.input[token_start..self.cursor].to_string();
        let common = completion_common_prefix(&candidates);

        if common.len() > prefix.len() && common.starts_with(&prefix) {
            let replacement = escape_shell_completion(&common);
            self.input.replace_range(token_start..self.cursor, &replacement);
            self.cursor = token_start + replacement.len();
        }

        if candidates.len() == 1 {
            let candidate = &candidates[0];
            if !candidate.ends_with('/') && self.input[self.cursor..].chars().next().is_none_or(char::is_whitespace) {
                self.input.insert(self.cursor, ' ');
                self.cursor += 1;
            }
            self.status = format!("Completed: {candidate}");
        } else {
            let preview = candidates.iter().take(8).cloned().collect::<Vec<_>>().join("  ");
            let remaining = candidates.len().saturating_sub(8);
            self.status = if remaining > 0 {
                format!("{} matches: {preview}  … and {remaining} more", candidates.len())
            } else {
                format!("{} matches: {preview}", candidates.len())
            };
        }

        self.input_hscroll = 0;
        self.history_position = None;
    }
}

fn default_export_path(journal_path: &PathBuf) -> PathBuf {
    journal_path.with_file_name("streamnote-export.txt")
}

fn is_streamnote_command(line: &str) -> bool {
    let command = line
        .split_whitespace()
        .next()
        .unwrap_or(line)
        .to_ascii_lowercase();
    matches!(
        command.as_str(),
        "/help"
            | "/view"
            | "/threads"
            | "/conversations"
            | "/search"
            | "/clear"
            | "/save"
            | "/stats"
            | "/path"
            | "/scp"
            | "/canada"
            | "/mode"
            | "/shell"
            | "/terminal"
            | "/notes"
            | "/note"
            | "/export"
            | "/quit"
            | "/q"
    )
}

fn slice_display_columns(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }

    let mut selected = String::new();
    let mut column = 0usize;
    let mut previous_cell_selected = false;
    for character in text.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        let cell_selected = if width == 0 {
            previous_cell_selected
        } else {
            column < end && column.saturating_add(width) > start
        };
        if cell_selected {
            selected.push(character);
        }
        if width > 0 {
            previous_cell_selected = cell_selected;
            column = column.saturating_add(width);
        }
    }
    selected
}

fn looks_like_secret_prompt(line: &str) -> bool {
    let normalized = line.to_ascii_lowercase();
    [
        "password",
        "passphrase",
        "passcode",
        "secret",
        "verification code",
        "one-time code",
        "security token",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
        || normalized
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| matches!(word, "pin" | "otp"))
}

#[cfg(test)]
mod tests {
    use super::{looks_like_secret_prompt, App, InputMode};
    use crate::storage::Journal;
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEST_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_app() -> (App, PathBuf) {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "streamnote-secret-input-test-{}-{id}.jsonl",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let journal = Journal::open(path.clone()).expect("open test journal");
        (App::new(journal, String::new()), path)
    }

    #[test]
    fn selected_hermes_rows_open_the_full_content_viewer() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for kind in [
            EntryKind::HermesUser,
            EntryKind::HermesAssistant,
            EntryKind::HermesToolCall,
            EntryKind::HermesToolResult,
        ] {
            let index = app.journal.entries.len();
            app.journal
                .append(Entry::with_kind(kind, "full Hermes content".to_string()))
                .expect("append Hermes event");
            app.selected_row = Some(index);

            assert!(app.open_selected_hermes_message());
            assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));
            app.close_conversation_view();
        }

        let note_index = app.journal.entries.len();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "ordinary note".to_string()))
            .expect("append note");
        app.selected_row = Some(note_index);
        assert!(!app.open_selected_hermes_message());
        assert_eq!(app.conversation_view, None);

        app.selected_row = None;
        assert!(!app.open_selected_hermes_message());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn submitted_secret_is_not_journaled_and_rearms_mask_for_next_prompt() {
        let (mut app, path) = test_app();
        app.secret_input = true;
        app.input = "do-not-log-this".to_string();
        app.cursor = app.input.len();

        app.submit(None);

        assert!(!app.secret_input);
        assert!(app.input.is_empty());
        assert!(app.history.is_empty());
        assert!(app.journal.entries.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn paste_inserts_at_cursor_and_flattens_multiline_clipboard_text() {
        let (mut app, path) = test_app();
        app.input = "abef".to_string();
        app.cursor = 2;

        app.paste("cd\nGH\r\n");

        assert_eq!(app.input, "abcd GH ");
        assert_eq!(app.cursor, "abcd GH ".len());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn input_prompt_tracks_shell_prompt_and_notes_mode() {
        let (mut app, path) = test_app();
        app.set_terminal_prompt("jordan@server:~/Projects/STREENOTE$ ".to_string());
        assert_eq!(app.input_prompt(), "jordan@server:~/Projects/STREENOTE$ ");

        app.input_mode = InputMode::Notes;
        assert_eq!(app.input_prompt(), "note› ");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clicking_a_command_row_selects_it_and_loads_its_command() {
        use crate::storage::{Entry, EntryKind};
        use ratatui::layout::Rect;

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalCommand, "ls -lah /home".to_string()))
            .expect("append command");
        app.input = "replace this draft".to_string();
        app.cursor = app.input.len();
        app.feed_viewport_height = 3;
        app.feed_area = Rect::new(10, 5, 60, 3);

        app.begin_transcript_selection(15, 5);
        app.end_transcript_selection(15, 5);

        let selected = app.selected_transcript_text().expect("selected command row");
        assert!(selected.contains("CMD"));
        assert!(selected.ends_with("ls -lah /home"));
        assert_eq!(app.input, "ls -lah /home");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.selected_row, None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clicking_a_non_command_row_does_not_select_it_or_change_the_input() {
        use crate::storage::{Entry, EntryKind};
        use ratatui::layout::Rect;

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "do not load this note".to_string()))
            .expect("append note");
        app.input = "keep this draft".to_string();
        app.cursor = app.input.len();
        app.selected_row = Some(0);
        app.feed_viewport_height = 3;
        app.feed_area = Rect::new(10, 5, 60, 3);

        app.begin_transcript_selection(15, 5);
        app.end_transcript_selection(15, 5);

        assert_eq!(app.selected_transcript_text(), None);
        assert_eq!(app.input, "keep this draft");
        assert_eq!(app.cursor, "keep this draft".len());
        assert_eq!(app.selected_row, Some(0));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clicking_a_command_row_does_not_replace_hidden_secret_input() {
        use crate::storage::{Entry, EntryKind};
        use ratatui::layout::Rect;

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalCommand, "sudo apt update".to_string()))
            .expect("append test command");
        app.input = "hidden password".to_string();
        app.cursor = app.input.len();
        app.secret_input = true;
        app.feed_viewport_height = 3;
        app.feed_area = Rect::new(10, 5, 60, 3);

        app.begin_transcript_selection(15, 5);
        app.end_transcript_selection(15, 5);

        assert!(app.selected_transcript_text().is_some());
        assert_eq!(app.input, "hidden password");
        assert_eq!(app.cursor, "hidden password".len());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn dragging_selects_a_partial_text_range() {
        use crate::storage::{entry_transcript_text, Entry, EntryKind};
        use ratatui::layout::Rect;
        use unicode_width::UnicodeWidthStr;

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "select this note".to_string()))
            .expect("append test row");
        app.input = "keep this draft".to_string();
        app.cursor = app.input.len();
        app.feed_viewport_height = 3;
        app.feed_area = Rect::new(10, 5, 100, 3);

        let row = entry_transcript_text(&app.journal.entries[0], 0);
        let start_column = UnicodeWidthStr::width(&row[..row.find("select").unwrap()]);
        let x = 10 + start_column as u16;
        app.begin_transcript_selection(x, 5);
        app.drag_transcript_selection(x + 5, 5);
        app.end_transcript_selection(x + 5, 5);

        assert_eq!(
            app.selected_transcript_text().as_deref(),
            Some("select")
        );
        assert_eq!(app.input, "keep this draft");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn dragging_selects_text_across_transcript_rows() {
        use crate::storage::{Entry, EntryKind};
        use ratatui::layout::Rect;

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "first row".to_string()))
            .expect("append first row");
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalOutput, "second row".to_string()))
            .expect("append second row");
        app.feed_viewport_height = 3;
        app.feed_area = Rect::new(10, 5, 100, 3);

        app.begin_transcript_selection(10, 5);
        app.drag_transcript_selection(99, 6);
        app.end_transcript_selection(99, 6);

        let selected = app.selected_transcript_text().expect("selected rows");
        assert!(selected.contains("first row"));
        assert!(selected.contains("second row"));
        assert!(selected.contains("\n"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn initial_echo_disabled_does_not_enable_secret_mode() {
        let (mut app, path) = test_app();

        app.update_secret_input(Some(false), &[]);

        assert!(!app.secret_input);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn echo_disabled_without_secret_prompt_stays_visible() {
        let (mut app, path) = test_app();

        app.update_secret_input(Some(true), &["Welcome to Debian".to_string()]);
        app.update_secret_input(Some(false), &["Starting interactive shell".to_string()]);

        assert!(!app.secret_input);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn password_prompt_and_disabled_echo_enable_secret_mode() {
        let (mut app, path) = test_app();

        app.update_secret_input(Some(true), &[]);
        app.update_secret_input(Some(false), &["[sudo] password for user:".to_string()]);

        assert!(app.secret_input);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn prompt_text_can_arrive_before_echo_is_disabled() {
        let (mut app, path) = test_app();

        app.update_secret_input(Some(true), &["Enter passphrase for key:".to_string()]);
        assert!(!app.secret_input);

        app.update_secret_input(Some(false), &[]);

        assert!(app.secret_input);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn restored_echo_clears_secret_mode_after_input_is_cleared() {
        let (mut app, path) = test_app();

        app.update_secret_input(Some(true), &[]);
        app.update_secret_input(Some(false), &["Password:".to_string()]);
        assert!(app.secret_input);

        app.insert_char('x');
        app.clear_input();
        app.update_secret_input(Some(true), &[]);

        assert!(!app.secret_input);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn notes_mode_never_displays_secret_input() {
        let (mut app, path) = test_app();

        app.input_mode = InputMode::Notes;
        app.update_secret_input(Some(false), &["Password:".to_string()]);

        assert!(!app.secret_input);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn secret_prompt_detection_uses_prompt_terms() {
        assert!(looks_like_secret_prompt("Password:"));
        assert!(looks_like_secret_prompt("Enter passphrase for key"));
        assert!(looks_like_secret_prompt("PIN:"));
        assert!(looks_like_secret_prompt("Verification code:"));
        assert!(!looks_like_secret_prompt("Welcome to Debian"));
        assert!(!looks_like_secret_prompt("Compiling streamnote"));
    }
    #[test]
    fn arrow_navigation_highlights_rows_and_scrolls_the_selected_row_into_view() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for index in 0..6 {
            app.journal
                .append(Entry::with_kind(EntryKind::Note, format!("row {index}")))
                .expect("append test row");
        }
        app.feed_viewport_height = 3;

        // The first Up starts at the bottom of the live viewport.
        app.move_row_selection(-1);
        assert_eq!(app.selected_row, Some(5));
        assert_eq!(app.current_top(), 3);

        app.move_row_selection(-1);
        assert_eq!(app.selected_row, Some(4));
        app.move_row_selection(-1);
        assert_eq!(app.selected_row, Some(3));
        app.move_row_selection(-1);
        assert_eq!(app.selected_row, Some(2));
        assert_eq!(app.current_top(), 2);

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(3));
        assert_eq!(app.current_top(), 2);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn down_navigation_starts_at_the_top_of_the_visible_page() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for index in 0..6 {
            app.journal
                .append(Entry::with_kind(EntryKind::Note, format!("row {index}")))
                .expect("append test row");
        }
        app.feed_viewport_height = 3;

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(3));
        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(4));
        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(5));
        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(5));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn page_scrolling_keeps_the_highlighted_row_visible() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for index in 0..8 {
            app.journal
                .append(Entry::with_kind(EntryKind::Note, format!("row {index}")))
                .expect("append test row");
        }
        app.feed_viewport_height = 3;
        app.move_row_selection(-1);
        assert_eq!(app.selected_row, Some(7));

        app.scroll_up(4);
        assert_eq!(app.current_top(), 1);
        assert_eq!(app.selected_row, Some(3));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn selected_live_tail_tracks_new_terminal_output() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "existing row".to_string()))
            .expect("append existing row");
        app.feed_viewport_height = 1;
        app.select_latest_row();
        assert_eq!(app.selected_row, Some(0));

        app.capture_terminal_output(vec!["new terminal output".to_string()]);

        assert_eq!(app.selected_row, Some(1));
        assert_eq!(app.current_top(), 1);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn app_without_a_keyboard_selection_does_not_select_new_output_automatically() {
        let (mut app, path) = test_app();
        app.capture_terminal_output(vec!["first terminal output".to_string()]);
        assert_eq!(app.selected_row, None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn highlighting_a_command_row_loads_it_into_the_input_line() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "keep this note out".to_string()))
            .expect("append note");
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalCommand, "ls -lah /home".to_string()))
            .expect("append command");
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalOutput, "directory listing".to_string()))
            .expect("append output");
        app.feed_viewport_height = 3;
        app.input = "draft to replace".to_string();
        app.cursor = app.input.len();

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(0));
        assert_eq!(app.input, "draft to replace");

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(1));
        assert_eq!(app.input, "ls -lah /home");
        assert_eq!(app.cursor, app.input.len());
        assert_eq!(app.input_hscroll, 0);

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(2));
        assert_eq!(app.input, "ls -lah /home");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn selecting_latest_row_enters_live_and_moving_back_exits() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for index in 0..3 {
            app.journal
                .append(Entry::with_kind(EntryKind::Note, format!("row {index}")))
                .expect("append test row");
        }
        app.feed_viewport_height = 2;
        app.scroll_to_top();

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(1));
        assert!(!app.follow_tail);

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(2));
        assert!(app.follow_tail);

        app.move_row_selection(-1);
        assert_eq!(app.selected_row, Some(1));
        assert!(!app.follow_tail);

        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(2));
        assert!(app.follow_tail);

        app.scroll_up(1);
        assert!(!app.follow_tail);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn selected_live_tail_advances_when_external_rows_are_appended() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::HermesUser, "prompt".to_string()))
            .expect("append initial Hermes row");
        app.feed_viewport_height = 2;
        app.selected_row = Some(0);
        app.follow_tail = true;

        let was_tracking_tail = app.is_tracking_selected_tail();
        app.journal
            .append(Entry::with_kind(
                EntryKind::HermesAssistant,
                "response".to_string(),
            ))
            .expect("append Hermes response");
        app.update_selected_tail_after_append(was_tracking_tail);

        assert!(app.follow_tail);
        assert_eq!(app.selected_row, Some(1));
        assert_eq!(app.current_top(), 0);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn command_recall_does_not_overwrite_hidden_secret_input() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalCommand, "sudo apt update".to_string()))
            .expect("append command");
        app.feed_viewport_height = 2;
        app.secret_input = true;
        app.input = "hidden passphrase".to_string();
        app.cursor = app.input.len();

        app.move_row_selection(1);

        assert_eq!(app.selected_row, Some(0));
        assert_eq!(app.input, "hidden passphrase");
        assert!(app.secret_input);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn home_and_explicit_end_navigation_recall_highlighted_commands() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalCommand, "first command".to_string()))
            .expect("append first command");
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "middle note".to_string()))
            .expect("append note");
        app.journal
            .append(Entry::with_kind(EntryKind::TerminalCommand, "last command".to_string()))
            .expect("append last command");
        app.feed_viewport_height = 3;

        app.scroll_to_top();
        assert_eq!(app.input, "first command");

        app.select_latest_row();
        assert_eq!(app.selected_row, Some(2));
        assert_eq!(app.input, "last command");

        let _ = fs::remove_file(path);
    }

}


fn completion_common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for candidate in candidates.iter().skip(1) {
        let common_len = prefix
            .char_indices()
            .zip(candidate.char_indices())
            .take_while(|((_, left), (_, right))| left == right)
            .map(|((index, ch), _)| index + ch.len_utf8())
            .last()
            .unwrap_or(0);
        prefix.truncate(common_len);
    }
    prefix
}

fn escape_shell_completion(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_whitespace()
            || matches!(character, '\\' | '\'' | '"' | '$' | '`' | '!' | '*' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '<' | '>' | ';' | '&' | '|')
        {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}


#[cfg(test)]
mod completion_tests {
    use super::{completion_common_prefix, escape_shell_completion};

    #[test]
    fn completion_finds_longest_common_prefix() {
        let candidates = vec![
            "streamnote/".to_string(),
            "streamnoted/".to_string(),
            "streamnotes/".to_string(),
        ];
        assert_eq!(completion_common_prefix(&candidates), "streamnote");
    }

    #[test]
    fn completion_escapes_shell_whitespace_and_metacharacters() {
        assert_eq!(escape_shell_completion("my folder/file"), "my\\ folder/file");
        assert_eq!(escape_shell_completion("a&b"), "a\\&b");
    }
}
