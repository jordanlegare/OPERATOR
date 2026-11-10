use std::{
    cell::{Ref, RefCell},
    collections::VecDeque,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    conversations::{build_threads, collapse_live_streams, is_hermes_event, ConversationThread},
    hermes_review::{
        diagnostics_summary, ReviewBroadcast, ReviewDecision, ReviewRequest, MAX_REVIEW_BYTES,
    },
    shell::ShellSession,
    settings::Settings,
    storage::{
        entry_transcript_text, sanitize_multiline, sanitize_single_line,
        should_suppress_terminal_output, Entry, EntryKind, Journal,
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
struct ConversationCache {
    entry_count: usize,
    total_text_bytes: usize,
    threads: Vec<ConversationThread>,
    /// Main transcript rows: ordinary journal records plus Hermes user prompts.
    transcript_indices: Vec<usize>,
}

impl Default for ConversationCache {
    fn default() -> Self {
        Self {
            entry_count: usize::MAX,
            total_text_bytes: usize::MAX,
            threads: Vec::new(),
            transcript_indices: Vec::new(),
        }
    }
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
    /// One-shot scroll target set when a search hit opens a Hermes conversation.
    /// The renderer consumes this after positioning the matching message.
    pub conversation_focus_entry: Option<usize>,
    /// Whether final Hermes responses must be approved before client delivery.
    pub review_hermes_responses: bool,
    /// Throttle disk checks while synchronizing the per-user review preference.
    last_review_settings_check: Instant,
    /// Active final-response review; its channel is the only release path to Hermes.
    pub pending_review: Option<ReviewRequest>,
    /// Read-only copies of final responses delivered to this non-reviewing terminal.
    pub review_broadcasts: VecDeque<ReviewBroadcast>,
    pub review_broadcast_scroll: u16,
    /// Editable copy of the held answer and its UTF-8 byte cursor.
    pub review_text: String,
    pub review_cursor: usize,
    pub review_vertical_scroll: u16,
    pub review_horizontal_scroll: u16,
    /// Derived conversation groups are cached until the journal's entry count
    /// or byte count changes; this avoids a full-history scan on every frame.
    conversation_cache: RefCell<ConversationCache>,
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
            conversation_focus_entry: None,
            review_hermes_responses: settings.review_hermes_responses,
            last_review_settings_check: Instant::now(),
            pending_review: None,
            review_broadcasts: VecDeque::new(),
            review_broadcast_scroll: 0,
            review_text: String::new(),
            review_cursor: 0,
            review_vertical_scroll: 0,
            review_horizontal_scroll: 0,
            conversation_cache: RefCell::new(ConversationCache::default()),
            show_banner: settings.show_banner,
            show_canada_watermark: settings.show_canada_watermark,
            scp_sftp_passthrough: settings.scp_sftp_passthrough,
            should_quit: false,
        }
    }

    pub fn current_top(&self) -> usize {
        if self.follow_tail {
            self.visible_transcript_count()
                .saturating_sub(self.feed_viewport_height)
        } else {
            self.scroll_top.min(self.max_top())
        }
    }

    /// Return original journal indices that are actually rendered in the main
    /// transcript. Hermes tool, assistant, and stream events remain available
    /// inside /threads; only HUSR prompts are shown in the main feed.
    pub fn visible_transcript_indices(&self) -> Vec<usize> {
        self.refresh_conversation_cache();
        self.conversation_cache.borrow().transcript_indices.clone()
    }

    pub fn visible_transcript_count(&self) -> usize {
        self.refresh_conversation_cache();
        self.conversation_cache.borrow().transcript_indices.len()
    }

    fn refresh_conversation_cache(&self) {
        let entry_count = self.journal.entries.len();
        let total_text_bytes = self.journal.total_text_bytes;
        let mut cache = self.conversation_cache.borrow_mut();
        if cache.entry_count != entry_count || cache.total_text_bytes != total_text_bytes {
            cache.threads = build_threads(&self.journal.entries);
            cache.transcript_indices = self
                .journal
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    (!is_hermes_event(entry.kind) || entry.kind == EntryKind::HermesUser)
                        .then_some(index)
                })
                .collect();
            cache.entry_count = entry_count;
            cache.total_text_bytes = total_text_bytes;
        }
    }

    /// True when the highlighted row is the latest visible transcript record
    /// and should advance with new visible records while LIVE mode is active.
    pub fn is_tracking_selected_tail(&self) -> bool {
        let latest = self.visible_transcript_indices().last().copied();
        self.follow_tail && self.selected_row.is_some() && self.selected_row == latest
    }

    /// Keep a selected live-tail row attached to the newest visible record.
    pub fn update_selected_tail_after_append(&mut self, was_tracking_tail: bool) {
        if was_tracking_tail && self.follow_tail {
            self.selected_row = self.visible_transcript_indices().last().copied();
            self.scroll_top = self.max_top();
        }
    }

    fn max_top(&self) -> usize {
        self.visible_transcript_count()
            .saturating_sub(self.feed_viewport_height.max(1))
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
        let visible = self.visible_transcript_indices();
        let visible_count = visible.len();
        if visible_count == 0 {
            return;
        }

        let top = self.current_top().min(visible_count - 1);
        let bottom = top
            .saturating_add(self.feed_viewport_height.max(1).saturating_sub(1))
            .min(visible_count - 1);
        let existing = self.selection.filter(|selection| selection.whole_row);
        let anchor_row = existing
            .map(|selection| selection.anchor.row)
            .or(self.selected_row.filter(|row| visible.contains(row)))
            .unwrap_or(visible[if direction < 0 { bottom } else { top }]);
        let current_row = existing.map(|selection| selection.active.row).unwrap_or(anchor_row);
        let current_position = visible
            .iter()
            .position(|row| *row == current_row)
            .unwrap_or(top);
        let next_position = if direction < 0 {
            current_position.saturating_sub(1)
        } else {
            current_position.saturating_add(1).min(visible_count - 1)
        };
        let next_row = visible[next_position];

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
        if next_position < new_top {
            new_top = next_position;
        } else if next_position >= new_top.saturating_add(viewport) {
            new_top = next_position.saturating_add(1).saturating_sub(viewport);
        }
        self.scroll_top = new_top.min(self.max_top());
        self.status = format!(
            "Selected transcript rows {}–{}; Ctrl+C copies the range.",
            anchor_row.min(next_row) + 1,
            anchor_row.max(next_row) + 1
        );
    }

    pub fn move_row_selection(&mut self, direction: i8) {
        let visible = self.visible_transcript_indices();
        let visible_count = visible.len();
        if visible_count == 0 {
            self.selected_row = None;
            return;
        }

        let viewport_height = self.feed_viewport_height.max(1);
        let top = self.current_top().min(visible_count - 1);
        let bottom = top
            .saturating_add(viewport_height.saturating_sub(1))
            .min(visible_count - 1);
        let current_position = self
            .selected_row
            .and_then(|row| visible.iter().position(|visible_row| *visible_row == row))
            .unwrap_or(if direction < 0 { bottom } else { top });
        let next_position = if direction < 0 {
            current_position.saturating_sub(1)
        } else {
            current_position.saturating_add(1).min(visible_count - 1)
        };
        let next = visible[next_position];

        self.selected_row = Some(next);
        self.selection = None;
        self.selection_dragging = false;
        self.follow_tail = next_position == visible_count - 1;

        if self.follow_tail {
            self.scroll_top = self.max_top();
            self.status = format!("LIVE · latest row {}/{} selected.", next_position + 1, visible_count);
        } else {
            let mut next_top = top;
            if next_position < next_top {
                next_top = next_position;
            } else if next_position >= next_top.saturating_add(viewport_height) {
                next_top = next_position.saturating_add(1).saturating_sub(viewport_height);
            }
            self.scroll_top = next_top.min(self.max_top());
            self.status = format!("Row {}/{} selected.", next_position + 1, visible_count);
        }
        self.recall_highlighted_command();
    }

    fn keep_selected_row_visible(&mut self, top: usize) {
        let Some(row) = self.selected_row else {
            return;
        };
        let visible = self.visible_transcript_indices();
        if visible.is_empty() {
            self.selected_row = None;
            return;
        }
        let viewport_height = self.feed_viewport_height.max(1);
        let first = top.min(visible.len() - 1);
        let last = first
            .saturating_add(viewport_height.saturating_sub(1))
            .min(visible.len() - 1);
        let position = visible
            .iter()
            .position(|visible_row| *visible_row == row)
            .unwrap_or(first);
        let visible_position = position.max(first).min(last);
        let visible_row = visible[visible_position];
        self.selected_row = Some(visible_row);
        if visible_row != row {
            self.status = format!(
                "Row {}/{} selected.",
                visible_position + 1,
                visible.len()
            );
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
            == self.visible_transcript_indices().last().copied();
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
        self.selected_row = self.visible_transcript_indices().first().copied();
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
        self.selected_row = self.visible_transcript_indices().last().copied();
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
                // A stationary click on a Hermes transcript row opens its
                // containing conversation thread.
                self.selection = None;
                self.selected_row = Some(row);
                self.follow_tail = self.visible_transcript_indices().last().copied() == Some(row);
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
        let visible = self.visible_transcript_indices();
        let position = self.current_top().saturating_add(y.saturating_sub(area.y) as usize);
        let row = *visible.get(position)?;

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

        let visible = self.visible_transcript_indices();
        let start_position = visible.iter().position(|row| *row == start.row)?;
        let end_position = visible.iter().position(|row| *row == end.row)?;
        let mut selected_rows = Vec::new();
        for position in start_position..=end_position {
            let row = visible[position];
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
        // A non-empty input draft takes priority over transcript navigation.
        // Returning false lets the existing Enter handler submit it normally.
        if !self.input.trim().is_empty() {
            return false;
        }

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

    /// Ctrl+C on a selected HUSR row is a thread-open shortcut. A drag/row
    /// selection retains its usual clipboard-copy behavior.
    pub fn open_selected_hermes_user_thread(&mut self) -> bool {
        if self.selection.is_some() {
            return false;
        }
        let Some(index) = self.selected_row else {
            return false;
        };
        if !self.journal.entries.get(index).is_some_and(|entry| {
            entry.kind == EntryKind::HermesUser
        }) {
            return false;
        }
        self.open_conversation_for_entry(index)
    }

    /// Open the conversation containing a selected Hermes event. New events
    /// are associated by session ID; legacy journal rows use a conservative
    /// prompt-to-following-events fallback grouping.
    pub fn open_conversation_for_entry(&mut self, index: usize) -> bool {
        let id = self.conversation_threads()
            .iter()
            .find(|thread| thread.entries.contains(&index))
            .map(|thread| thread.id.clone());
        let Some(id) = id else {
            return false;
        };
        self.conversation_view = Some(ConversationView::Thread { id });
        self.conversation_scroll = 0;
        self.conversation_follow_tail = true;
        true
    }

    pub fn open_conversations(&mut self) {
        let count = self.conversation_threads().len();
        if count == 0 {
            self.status = "No Hermes conversations are present in the journal.".to_string();
            return;
        }
        self.conversation_list_selected = count.saturating_sub(1);
        self.conversation_view = Some(ConversationView::List);
        self.status = format!("{count} Hermes conversation(s). Enter expands a thread.");
    }

    pub fn conversation_threads(&self) -> Ref<'_, Vec<ConversationThread>> {
        self.refresh_conversation_cache();
        Ref::map(self.conversation_cache.borrow(), |cache| &cache.threads)
    }

    pub fn open_selected_conversation(&mut self) {
        let id = self.conversation_threads()
            .get(self.conversation_list_selected)
            .map(|thread| thread.id.clone());
        let Some(id) = id else {
            self.conversation_view = None;
            return;
        };
        self.conversation_view = Some(ConversationView::Thread { id });
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

    pub fn collapse_conversation_thread(&mut self) {
        let Some(ConversationView::Thread { id }) = self.conversation_view.as_ref() else {
            return;
        };
        let id = id.clone();
        let selected = self.conversation_threads().iter().position(|thread| thread.id == id);
        if let Some(index) = selected {
            self.conversation_list_selected = index;
            self.conversation_view = Some(ConversationView::List);
        }
        self.conversation_scroll = 0;
    }

    pub fn close_conversation_view(&mut self) {
        self.conversation_view = None;
        self.conversation_scroll = 0;
        self.conversation_follow_tail = true;
        self.conversation_focus_entry = None;
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
        let indices = self.conversation_threads()
            .iter()
            .find(|thread| &thread.id == id)
            .map(|thread| thread.entries.clone())?;
        let entries = indices
            .iter()
            .filter_map(|index| self.journal.entries.get(*index).map(|entry| (*index, entry)))
            .collect::<Vec<_>>();
        let collapsed = collapse_live_streams(&entries);
        let mut output = String::new();
        for (index, entry) in entries {
            if matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd)
                || collapsed.hidden_delta_indices.contains(&index)
                || collapsed.replaced_assistant_indices.contains(&index)
            {
                continue;
            }
            let (label, text) = if let Some(stream_text) = collapsed.text_by_first_index.get(&index) {
                ("ASSISTANT - LIVE STREAM", stream_text.as_str())
            } else {
                let label = match entry.kind {
                    EntryKind::HermesUser => "USER",
                    EntryKind::HermesAssistant => "ASSISTANT",
                    EntryKind::HermesToolCall => "TOOL CALL",
                    EntryKind::HermesToolResult => "TOOL RESULT",
                    EntryKind::HermesStreamDelta => "ASSISTANT - LIVE STREAM",
                    EntryKind::HermesStreamStart => "STREAM START",
                    EntryKind::HermesStreamEnd => "STREAM END",
                    EntryKind::HermesHistory => "PRIOR CONTEXT",
                    EntryKind::HermesInterim => "INTERIM MESSAGE",
                    EntryKind::HermesSession => "SESSION EVENT",
                    _ => "HERMES",
                };
                (label, entry.text.as_str())
            };
            output.push_str(&format!("[{label}] {}\n{text}\n\n", entry.timestamp));
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
            review_hermes_responses: self.review_hermes_responses,
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

    /// Synchronize the per-user review preference written by another SSH-launched
    /// STREENOTE process. A single /review on therefore makes every live
    /// terminal eligible for the shared reviewer pool.
    pub fn sync_hermes_review_setting(&mut self) -> bool {
        if self.last_review_settings_check.elapsed() < Duration::from_millis(250) {
            return false;
        }
        self.last_review_settings_check = Instant::now();
        let persisted = Settings::load().review_hermes_responses;
        if persisted == self.review_hermes_responses {
            return false;
        }
        self.review_hermes_responses = persisted;
        self.status = format!(
            "Shared Hermes response review is now {} in all connected STREENOTE terminals.",
            if persisted { "ON" } else { "OFF" }
        );
        true
    }

    pub fn set_hermes_review_enabled(&mut self, enabled: bool) {
        if self.review_hermes_responses == enabled {
            self.status = format!(
                "Hermes response review is already {}.",
                if enabled { "ON" } else { "OFF" }
            );
            return;
        }
        self.review_hermes_responses = enabled;
        self.status = match self.persist_settings() {
            Ok(()) => format!(
                "Hermes response review {} (saved). Final answers will {}.",
                if enabled { "ON" } else { "OFF" },
                if enabled { "wait for your approval before delivery" } else { "pass through without review" }
            ),
            Err(error) => format!(
                "Hermes response review changed for this session but settings could not be saved: {error}"
            ),
        };
    }

    pub fn receive_hermes_broadcast(&mut self, broadcast: ReviewBroadcast) {
        // Pending copies let other terminals inspect the draft while one operator
        // reviews it. A result is only a lifecycle signal: close that pending
        // copy silently instead of opening a confirmation modal on every terminal.
        // The selected reviewer's F2/F3/F4 decision remains authoritative.
        if broadcast.phase == "result" {
            self.review_broadcasts
                .retain(|item| item.request_id != broadcast.request_id);
            self.review_broadcast_scroll = 0;
            return;
        }

        if let Some(existing) = self.review_broadcasts.iter_mut().find(|item| {
            item.request_id == broadcast.request_id
        }) {
            *existing = broadcast;
        } else {
            self.review_broadcasts.push_back(broadcast);
        }
        if self.pending_review.is_none() {
            self.status =
                "A Hermes response is awaiting review in another connected STREENOTE terminal."
                    .to_string();
        }
    }
    pub fn dismiss_hermes_broadcast(&mut self) {
        self.review_broadcasts.pop_front();
        self.review_broadcast_scroll = 0;
        if !self.review_broadcasts.is_empty() {
            self.status = format!(
                "{} more Hermes response broadcast(s) waiting.",
                self.review_broadcasts.len()
            );
        }
    }

    pub fn scroll_hermes_broadcast(&mut self, amount: i32) {
        self.review_broadcast_scroll = if amount < 0 {
            self.review_broadcast_scroll.saturating_sub(amount.unsigned_abs() as u16)
        } else {
            self.review_broadcast_scroll.saturating_add(amount as u16)
        };
    }

    pub fn begin_hermes_review(&mut self, request: ReviewRequest) {
        if self.pending_review.is_some() {
            let _ = request.reply.send(ReviewDecision::Reject);
            self.status = "A second Hermes review arrived while another review is pending; the second response was withheld.".to_string();
            return;
        }

        self.review_text = sanitize_multiline(&request.response_text);
        // Open at the beginning. Starting at the end made the renderer
        // horizontally scroll a long first line, hiding its initial text.
        self.review_cursor = 0;
        self.review_vertical_scroll = 0;
        self.review_horizontal_scroll = 0;
        self.status = format!(
            "Hermes final answer held for review · model {} · platform {}.",
            if request.model.is_empty() { "unknown" } else { &request.model },
            if request.platform.is_empty() { "unknown" } else { &request.platform }
        );
        self.pending_review = Some(request);
    }

    /// Remove a review whose socket-side deadline has elapsed. The server
    /// independently fails closed at the same deadline; this keeps the TUI
    /// from leaving an already-withheld draft open for a late approval.
    pub fn expire_hermes_review_if_needed(&mut self) -> bool {
        let expired = self
            .pending_review
            .as_ref()
            .is_some_and(|request| Instant::now() >= request.expires_at);
        if !expired {
            return false;
        }

        let Some(request) = self.pending_review.take() else {
            return false;
        };
        let _ = request.reply.send(ReviewDecision::Reject);
        self.review_text.clear();
        self.review_cursor = 0;
        self.record_system(format!(
            "Hermes response review {} timed out · original answer withheld.",
            request.request_id
        ));
        self.status = "Hermes final answer timed out in review and was withheld.".to_string();
        true
    }

    pub fn approve_hermes_original(&mut self) {
        let Some(request) = self.pending_review.take() else {
            return;
        };
        let result = request.reply.send(ReviewDecision::Approve);
        self.review_text.clear();
        self.review_cursor = 0;
        self.record_system(if result.is_ok() {
            format!(
                "Hermes response review {} · original answer released.",
                request.request_id
            )
        } else {
            format!(
                "Hermes response review {} · original-answer approval arrived too late; answer was not released.",
                request.request_id
            )
        });
    }

    pub fn approve_hermes_edited(&mut self) {
        if self.review_text.trim().is_empty() {
            self.status = "An empty answer cannot be released. Restore text or reject the response.".to_string();
            return;
        }
        if self.review_text.len() > MAX_REVIEW_BYTES {
            self.status = format!(
                "Edited answer exceeds the {} MiB review limit.",
                MAX_REVIEW_BYTES / (1024 * 1024)
            );
            return;
        }
        let Some(request) = self.pending_review.take() else {
            return;
        };
        let edited = self.review_text.clone();
        let result = request.reply.send(ReviewDecision::Replace(edited));
        self.review_text.clear();
        self.review_cursor = 0;
        self.record_system(if result.is_ok() {
            format!(
                "Hermes response review {} · edited answer released.",
                request.request_id
            )
        } else {
            format!(
                "Hermes response review {} · edited-answer approval arrived too late; answer was not released.",
                request.request_id
            )
        });
    }

    pub fn reject_hermes_response(&mut self) {
        let Some(request) = self.pending_review.take() else {
            return;
        };
        let result = request.reply.send(ReviewDecision::Reject);
        self.review_text.clear();
        self.review_cursor = 0;
        self.record_system(if result.is_ok() {
            format!(
                "Hermes response review {} · original answer withheld.",
                request.request_id
            )
        } else {
            format!(
                "Hermes response review {} · rejection arrived after the request had already ended; no answer was released.",
                request.request_id
            )
        });
    }

    pub fn review_insert_text(&mut self, text: &str) {
        if self.pending_review.is_none() {
            return;
        }
        let cleaned = sanitize_multiline(text);
        if self.review_text.len().saturating_add(cleaned.len()) > MAX_REVIEW_BYTES {
            self.status = format!(
                "Review editor reached its {} MiB text limit.",
                MAX_REVIEW_BYTES / (1024 * 1024)
            );
            return;
        }
        self.review_text.insert_str(self.review_cursor, &cleaned);
        self.review_cursor += cleaned.len();
    }

    pub fn review_backspace(&mut self) {
        if self.pending_review.is_none() || self.review_cursor == 0 {
            return;
        }
        let previous = self.review_text[..self.review_cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.review_text.replace_range(previous..self.review_cursor, "");
        self.review_cursor = previous;
    }

    pub fn review_delete(&mut self) {
        if self.pending_review.is_none() || self.review_cursor >= self.review_text.len() {
            return;
        }
        let next = self.review_text[self.review_cursor..]
            .chars()
            .next()
            .map(|character| self.review_cursor + character.len_utf8())
            .unwrap_or(self.review_cursor);
        self.review_text.replace_range(self.review_cursor..next, "");
    }

    pub fn review_cursor_left(&mut self) {
        if self.pending_review.is_none() || self.review_cursor == 0 {
            return;
        }
        self.review_cursor = self.review_text[..self.review_cursor]
            .char_indices()
            .last()
            .map(|(index, _)| index)
            .unwrap_or(0);
    }

    pub fn review_cursor_right(&mut self) {
        if self.pending_review.is_none() || self.review_cursor >= self.review_text.len() {
            return;
        }
        self.review_cursor += self.review_text[self.review_cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
    }

    pub fn review_cursor_home(&mut self) {
        if self.pending_review.is_none() {
            return;
        }
        self.review_cursor = self.review_text[..self.review_cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn review_cursor_end(&mut self) {
        if self.pending_review.is_none() {
            return;
        }
        self.review_cursor = self.review_text[self.review_cursor..]
            .find('\n')
            .map(|offset| self.review_cursor + offset)
            .unwrap_or(self.review_text.len());
    }

    pub fn review_cursor_document_start(&mut self) {
        if self.pending_review.is_some() {
            self.review_cursor = 0;
        }
    }

    pub fn review_cursor_document_end(&mut self) {
        if self.pending_review.is_some() {
            self.review_cursor = self.review_text.len();
        }
    }

    pub fn review_cursor_up(&mut self) {
        self.review_move_vertical(-1);
    }

    pub fn review_cursor_down(&mut self) {
        self.review_move_vertical(1);
    }

    fn review_move_vertical(&mut self, direction: i8) {
        if self.pending_review.is_none() {
            return;
        }
        let before = &self.review_text[..self.review_cursor];
        let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
        let column = before[line_start..].chars().count();

        let (target_start, target_end) = if direction < 0 {
            if line_start == 0 {
                return;
            }
            let target_end = line_start - 1;
            let target_start = self.review_text[..target_end]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(0);
            (target_start, target_end)
        } else {
            let current_end = self.review_text[self.review_cursor..]
                .find('\n')
                .map(|offset| self.review_cursor + offset)
                .unwrap_or(self.review_text.len());
            if current_end >= self.review_text.len() {
                return;
            }
            let target_start = current_end + 1;
            let target_end = self.review_text[target_start..]
                .find('\n')
                .map(|offset| target_start + offset)
                .unwrap_or(self.review_text.len());
            (target_start, target_end)
        };

        self.review_cursor = self.review_text[target_start..target_end]
            .char_indices()
            .nth(column)
            .map(|(offset, _)| target_start + offset)
            .unwrap_or(target_end);
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
            "/review" => match argument.to_ascii_lowercase().as_str() {
                "" | "status" => {
                    self.status = format!(
                        "Hermes review {} · {}",
                        if self.review_hermes_responses { "ON" } else { "OFF" },
                        diagnostics_summary()
                    );
                }
                "on" | "enable" | "enabled" => self.set_hermes_review_enabled(true),
                "off" | "disable" | "disabled" => self.set_hermes_review_enabled(false),
                _ => self.status = "Usage: /review on | /review off | /review status".to_string(),
            },
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
        self.conversation_focus_entry = None;
    }

    /// Return the text users can actually read in the journal and expanded
    /// Hermes conversations. Stream fragments are indexed as one logical
    /// assistant message, so a query may span adjacent delta events.
    fn searchable_entry_texts(&self) -> Vec<(usize, String)> {
        let mut searchable = self
            .journal
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !is_hermes_event(entry.kind))
            .map(|(index, entry)| (index, entry.text.clone()))
            .collect::<Vec<_>>();

        for thread in self.conversation_threads().iter() {
            let entries = thread
                .entries
                .iter()
                .filter_map(|index| self.journal.entries.get(*index).map(|entry| (*index, entry)))
                .collect::<Vec<_>>();
            let collapsed = collapse_live_streams(&entries);

            for (index, entry) in entries {
                if matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd)
                    || collapsed.hidden_delta_indices.contains(&index)
                    || collapsed.replaced_assistant_indices.contains(&index)
                {
                    continue;
                }

                let text = collapsed
                    .text_by_first_index
                    .get(&index)
                    .cloned()
                    .unwrap_or_else(|| entry.text.clone());
                searchable.push((index, text));
            }
        }

        searchable.sort_unstable_by_key(|(index, _)| *index);
        searchable
    }

    fn search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            self.clear_search();
            self.status = "Search cleared.".to_string();
            return;
        }

        let needle = query.to_ascii_lowercase();
        let found = self
            .searchable_entry_texts()
            .into_iter()
            .filter_map(|(index, text)| {
                text.to_ascii_lowercase()
                    .contains(needle.as_str())
                    .then_some(index)
            })
            .collect::<Vec<_>>();

        self.search_query = Some(query.to_string());
        self.matches = found;
        self.conversation_focus_entry = None;
        if !self.matches.is_empty() {
            let latest_position = self.matches.len() - 1;
            self.activate_match(latest_position);
            self.status = format!(
                "Found {} hit(s) for {:?} across the journal and full Hermes conversation text. n/N navigate.",
                self.matches.len(),
                query
            );
        } else {
            self.active_match = None;
            self.conversation_view = None;
            self.status = format!(
                "No matches for {:?} in {} journal entries or Hermes conversation messages.",
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
        let is_hermes_hit = self
            .journal
            .entries
            .get(entry_index)
            .is_some_and(|entry| is_hermes_event(entry.kind));

        self.selection = None;
        self.selection_dragging = false;
        self.follow_tail = false;

        let visible_indices = self.visible_transcript_indices();
        let visible_position = visible_indices
            .iter()
            .position(|index| *index == entry_index)
            .or_else(|| visible_indices.iter().rposition(|index| *index < entry_index))
            .unwrap_or(0);
        self.selected_row = visible_indices.get(visible_position).copied();
        let context = self.feed_viewport_height.min(8).max(1) / 2;
        self.scroll_top = visible_position.saturating_sub(context).min(self.max_top());

        if is_hermes_hit && self.open_conversation_for_entry(entry_index) {
            // Search results inside a Hermes event are hidden from the main
            // transcript. Open the containing conversation and scroll to the
            // exact message; the thread renderer highlights the query in-body.
            self.conversation_focus_entry = Some(entry_index);
            self.conversation_follow_tail = false;
            self.conversation_scroll = 0;
        } else {
            self.conversation_view = None;
            self.conversation_focus_entry = None;
        }

        self.status = format!(
            "Hit {}/{} · journal entry {} · n/N next/previous · query: {}",
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
            | "/review"
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
    use super::{looks_like_secret_prompt, App, ConversationView, InputMode};
    use crate::storage::Journal;
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant},
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
    fn edited_hermes_response_is_returned_through_the_pending_decision_channel() {
        use crate::hermes_review::{ReviewDecision, ReviewRequest};
        use std::sync::mpsc;

        let (mut app, path) = test_app();
        let (reply, decision) = mpsc::sync_channel(1);
        app.begin_hermes_review(ReviewRequest {
            request_id: "review-edit-001".to_string(),
            session_id: "session-1".to_string(),
            model: "qwen3.5:latest".to_string(),
            platform: "tui".to_string(),
            response_text: "Original draft".to_string(),
            expires_at: Instant::now() + Duration::from_secs(30),
            reply,
        });

        assert!(app.pending_review.is_some());
        assert_eq!(app.review_text, "Original draft");
        assert_eq!(app.review_cursor, 0, "review editor must open at the beginning");
        app.review_cursor_document_end();
        for _ in 0..5 {
            app.review_backspace();
        }
        app.review_insert_text("approved");
        assert_eq!(app.review_text, "Original approved");

        app.approve_hermes_edited();
        assert_eq!(
            decision.recv().expect("receive review decision"),
            ReviewDecision::Replace("Original approved".to_string())
        );
        assert!(app.pending_review.is_none());
        assert!(app.review_text.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn final_review_result_silently_closes_the_read_only_broadcast() {
        use crate::hermes_review::ReviewBroadcast;

        let (mut app, path) = test_app();
        app.status = "Keep the normal terminal status.".to_string();
        app.receive_hermes_broadcast(ReviewBroadcast {
            request_id: "shared-review-001".to_string(),
            session_id: "session-1".to_string(),
            model: "qwen3.5:latest".to_string(),
            platform: "api_server".to_string(),
            response_text: "Original draft".to_string(),
            phase: "pending".to_string(),
            decision: None,
        });
        assert_eq!(app.review_broadcasts.len(), 1);
        assert_eq!(app.review_broadcasts[0].response_text, "Original draft");

        app.review_broadcast_scroll = 12;
        app.receive_hermes_broadcast(ReviewBroadcast {
            request_id: "shared-review-001".to_string(),
            session_id: "session-1".to_string(),
            model: "qwen3.5:latest".to_string(),
            platform: "api_server".to_string(),
            response_text: "Edited and approved final text".to_string(),
            phase: "result".to_string(),
            decision: Some("replace".to_string()),
        });
        assert!(app.review_broadcasts.is_empty());
        assert_eq!(app.review_broadcast_scroll, 0);
        assert_eq!(app.status, "Keep the normal terminal status.");

        // A result-only event must not create a new notification modal.
        app.receive_hermes_broadcast(ReviewBroadcast {
            request_id: "result-only-review".to_string(),
            session_id: "session-1".to_string(),
            model: "qwen3.5:latest".to_string(),
            platform: "api_server".to_string(),
            response_text: String::new(),
            phase: "result".to_string(),
            decision: Some("approve".to_string()),
        });
        assert!(app.review_broadcasts.is_empty());

        let _ = fs::remove_file(path);
    }
    #[test]
    fn expired_review_is_closed_and_returns_a_rejection() {
        use crate::hermes_review::{ReviewDecision, ReviewRequest};
        use std::sync::mpsc;

        let (mut app, path) = test_app();
        let (reply, decision) = mpsc::sync_channel(1);
        app.begin_hermes_review(ReviewRequest {
            request_id: "review-expired-001".to_string(),
            session_id: "session-1".to_string(),
            model: "model".to_string(),
            platform: "cli".to_string(),
            response_text: "This draft must not be released late.".to_string(),
            expires_at: Instant::now() - Duration::from_secs(1),
            reply,
        });

        assert!(app.expire_hermes_review_if_needed());
        assert_eq!(
            decision.recv().expect("receive expiration rejection"),
            ReviewDecision::Reject
        );
        assert!(app.pending_review.is_none());
        assert!(app.review_text.is_empty());
        assert!(app.status.contains("withheld"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn original_answer_and_rejection_have_distinct_explicit_decisions() {
        use crate::hermes_review::{ReviewDecision, ReviewRequest};
        use std::sync::mpsc;

        let (mut app, path) = test_app();
        let (reply, decision) = mpsc::sync_channel(1);
        app.begin_hermes_review(ReviewRequest {
            request_id: "review-original-001".to_string(),
            session_id: "session-1".to_string(),
            model: "model".to_string(),
            platform: "cli".to_string(),
            response_text: "Release this exact answer".to_string(),
            expires_at: Instant::now() + Duration::from_secs(30),
            reply,
        });
        app.approve_hermes_original();
        assert_eq!(
            decision.recv().expect("receive original decision"),
            ReviewDecision::Approve
        );

        let (reply, decision) = mpsc::sync_channel(1);
        app.begin_hermes_review(ReviewRequest {
            request_id: "review-reject-001".to_string(),
            session_id: "session-1".to_string(),
            model: "model".to_string(),
            platform: "cli".to_string(),
            response_text: "Do not release this answer".to_string(),
            expires_at: Instant::now() + Duration::from_secs(30),
            reply,
        });
        app.reject_hermes_response();
        assert_eq!(
            decision.recv().expect("receive rejection decision"),
            ReviewDecision::Reject
        );
        assert!(app.pending_review.is_none());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn review_editor_cursor_respects_utf8_boundaries_and_multiline_navigation() {
        use crate::hermes_review::ReviewRequest;
        use std::sync::mpsc;

        let (mut app, path) = test_app();
        let (reply, _decision) = mpsc::sync_channel(1);
        app.begin_hermes_review(ReviewRequest {
            request_id: "review-cursor-001".to_string(),
            session_id: String::new(),
            model: String::new(),
            platform: String::new(),
            response_text: "Café\nnext".to_string(),
            expires_at: Instant::now() + Duration::from_secs(30),
            reply,
        });

        app.review_cursor_left();
        app.review_cursor_left();
        app.review_cursor_up();
        assert!(app.review_text.is_char_boundary(app.review_cursor));
        assert!(app.review_cursor <= "Café".len());
        app.review_cursor_document_start();
        app.review_insert_text("Edited ");
        assert!(app.review_text.starts_with("Edited Café"));

        let _ = fs::remove_file(path);
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
    fn main_transcript_keeps_ordinary_rows_and_only_husr_hermes_rows() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for (kind, text) in [
            (EntryKind::Note, "note"),
            (EntryKind::HermesUser, "prompt"),
            (EntryKind::HermesAssistant, "answer"),
            (EntryKind::HermesToolCall, "tool call"),
            (EntryKind::HermesToolResult, "tool result"),
            (EntryKind::HermesStreamStart, "stream started"),
            (EntryKind::HermesStreamDelta, "stream text"),
            (EntryKind::HermesStreamEnd, "stream ended"),
            (EntryKind::TerminalOutput, "shell output"),
        ] {
            app.journal
                .append(Entry::with_kind(kind, text.to_string()))
                .expect("append transcript record");
        }

        assert_eq!(app.visible_transcript_indices(), vec![0, 1, 8]);
        app.feed_viewport_height = 2;
        app.scroll_to_top();
        assert_eq!(app.selected_row, Some(0));
        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(1));
        app.move_row_selection(1);
        assert_eq!(app.selected_row, Some(8));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ctrl_c_target_on_husr_opens_thread_and_copies_one_complete_stream() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        let mut prompt = Entry::with_kind(EntryKind::HermesUser, "What is life?".to_string());
        prompt.conversation_id = Some("session-1".to_string());
        prompt.turn_id = Some("turn-1".to_string());
        app.journal.append(prompt).expect("append user prompt");

        let mut tool_call = Entry::with_kind(EntryKind::HermesToolCall, "Tool: search".to_string());
        tool_call.conversation_id = Some("session-1".to_string());
        tool_call.turn_id = Some("turn-1".to_string());
        app.journal.append(tool_call).expect("append tool call");

        let mut tool_result = Entry::with_kind(EntryKind::HermesToolResult, "Search result".to_string());
        tool_result.conversation_id = Some("session-1".to_string());
        tool_result.turn_id = Some("turn-1".to_string());
        app.journal.append(tool_result).expect("append tool result");

        let mut first_delta = Entry::with_kind(EntryKind::HermesStreamDelta, "The basic ".to_string());
        first_delta.conversation_id = Some("session-1".to_string());
        first_delta.turn_id = Some("turn-1".to_string());
        first_delta.iteration = Some(0);
        app.journal.append(first_delta).expect("append first stream delta");

        let mut stream_end = Entry::with_kind(EntryKind::HermesStreamEnd, "finished".to_string());
        stream_end.conversation_id = Some("session-1".to_string());
        stream_end.turn_id = Some("turn-1".to_string());
        stream_end.iteration = Some(0);
        app.journal.append(stream_end).expect("append stream end");

        let mut second_delta = Entry::with_kind(EntryKind::HermesStreamDelta, "necessities of life.".to_string());
        second_delta.conversation_id = Some("session-1".to_string());
        second_delta.turn_id = Some("turn-1".to_string());
        second_delta.iteration = Some(1);
        app.journal.append(second_delta).expect("append second stream delta");

        let mut final_assistant = Entry::with_kind(
            EntryKind::HermesAssistant,
            "duplicate final assistant response".to_string(),
        );
        final_assistant.conversation_id = Some("session-1".to_string());
        final_assistant.turn_id = Some("turn-1".to_string());
        app.journal.append(final_assistant).expect("append final assistant response");

        app.selected_row = Some(0);
        assert!(app.open_selected_hermes_user_thread());
        assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));

        let copied = app.selected_conversation_text().expect("thread clipboard text");
        assert!(copied.contains("[USER]"));
        assert!(copied.contains("[TOOL CALL]"));
        assert!(copied.contains("[TOOL RESULT]"));
        assert_eq!(copied.matches("[ASSISTANT - LIVE STREAM]").count(), 1);
        assert!(copied.contains("The basic necessities of life."));
        assert!(!copied.contains("duplicate final assistant response"));
        assert!(!copied.contains("[STREAM END]"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_empty_input_on_husr_row_submits_instead_of_opening_thread() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::HermesUser, "previous prompt".to_string()))
            .expect("append user prompt");
        app.selected_row = Some(0);
        app.input = "ls -la".to_string();
        app.cursor = app.input.len();

        assert!(!app.open_selected_hermes_message());
        assert_eq!(app.conversation_view, None);
        assert_eq!(app.input, "ls -la");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_input_on_husr_row_opens_active_conversation() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::HermesUser, "previous prompt".to_string()))
            .expect("append user prompt");
        app.selected_row = Some(0);

        assert!(app.open_selected_hermes_message());
        assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));

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
        assert_eq!(app.selected_row, Some(0));
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

    #[test]
    fn search_indexes_full_hermes_message_bodies_and_opens_the_matching_thread() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        let mut prompt = Entry::with_kind(EntryKind::HermesUser, "ordinary prompt".to_string());
        prompt.conversation_id = Some("deep-search-session".to_string());
        prompt.turn_id = Some("turn-1".to_string());
        app.journal.append(prompt).expect("append prompt");

        let fixtures = [
            (EntryKind::HermesAssistant, "assistant-body-needle"),
            (EntryKind::HermesToolCall, "tool-arguments-needle"),
            (EntryKind::HermesToolResult, "tool-result-needle"),
            (EntryKind::HermesHistory, "prior-context-needle"),
            (EntryKind::HermesInterim, "interim-message-needle"),
            (EntryKind::HermesSession, "session-detail-needle"),
        ];

        for (kind, text) in fixtures {
            let mut entry = Entry::with_kind(kind, format!("buried content: {text}"));
            entry.conversation_id = Some("deep-search-session".to_string());
            entry.turn_id = Some("turn-1".to_string());
            let expected_index = app.journal.entries.len();
            app.journal.append(entry).expect("append deep Hermes event");

            app.search(text);
            assert_eq!(app.matches, vec![expected_index], "search should find {kind:?}");
            assert_eq!(app.active_match, Some(0));
            assert_eq!(app.conversation_focus_entry, Some(expected_index));
            assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));
            assert!(app.is_active_match(expected_index));
        }

        let _ = fs::remove_file(path);
    }

    #[test]
    fn search_matches_a_phrase_split_across_live_stream_chunks_once() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        let mut prompt = Entry::with_kind(EntryKind::HermesUser, "streaming prompt".to_string());
        prompt.conversation_id = Some("stream-search-session".to_string());
        prompt.turn_id = Some("stream-turn".to_string());
        app.journal.append(prompt).expect("append stream prompt");

        let mut first = Entry::with_kind(EntryKind::HermesStreamDelta, "cross-chunk-".to_string());
        first.conversation_id = Some("stream-search-session".to_string());
        first.turn_id = Some("stream-turn".to_string());
        first.iteration = Some(0);
        let first_index = app.journal.entries.len();
        app.journal.append(first).expect("append first stream chunk");

        let mut second = Entry::with_kind(EntryKind::HermesStreamDelta, "needle is present".to_string());
        second.conversation_id = Some("stream-search-session".to_string());
        second.turn_id = Some("stream-turn".to_string());
        second.iteration = Some(0);
        let second_index = app.journal.entries.len();
        app.journal.append(second).expect("append second stream chunk");

        app.search("cross-chunk-needle");
        assert_eq!(app.matches, vec![first_index]);
        assert!(!app.is_match(second_index));
        assert_eq!(app.conversation_focus_entry, Some(first_index));
        assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn search_navigation_switches_between_full_threads_and_normal_journal_rows() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        app.journal
            .append(Entry::with_kind(EntryKind::Note, "needle in a note".to_string()))
            .expect("append note");

        let mut prompt = Entry::with_kind(EntryKind::HermesUser, "needle in a prompt".to_string());
        prompt.conversation_id = Some("navigation-session".to_string());
        prompt.turn_id = Some("navigation-turn".to_string());
        app.journal.append(prompt).expect("append prompt");

        let mut answer = Entry::with_kind(EntryKind::HermesAssistant, "needle in a hidden answer".to_string());
        answer.conversation_id = Some("navigation-session".to_string());
        answer.turn_id = Some("navigation-turn".to_string());
        app.journal.append(answer).expect("append assistant response");

        app.journal
            .append(Entry::with_kind(EntryKind::TerminalOutput, "needle in shell output".to_string()))
            .expect("append terminal output");

        app.search("needle");
        assert_eq!(app.matches, vec![0, 1, 2, 3]);
        assert_eq!(app.active_match, Some(3));
        assert_eq!(app.conversation_view, None);

        app.previous_match();
        assert_eq!(app.matches[app.active_match.unwrap()], 2);
        assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));

        app.previous_match();
        assert_eq!(app.matches[app.active_match.unwrap()], 1);
        assert!(matches!(app.conversation_view, Some(ConversationView::Thread { .. })));

        app.previous_match();
        assert_eq!(app.matches[app.active_match.unwrap()], 0);
        assert_eq!(app.conversation_view, None);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn search_and_n_previous_navigate_select_the_matching_transcript_row() {
        use crate::storage::{Entry, EntryKind};

        let (mut app, path) = test_app();
        for text in [
            "needle in the first note",
            "an unrelated note",
            "another unrelated note",
            "needle in the latest note",
        ] {
            app.journal
                .append(Entry::with_kind(EntryKind::Note, text.to_string()))
                .expect("append search fixture");
        }
        app.feed_viewport_height = 2;

        app.search("needle");
        assert_eq!(app.active_match, Some(1));
        assert_eq!(app.selected_row, Some(3));
        assert!(app.is_active_match(3));

        app.next_match();
        assert_eq!(app.active_match, Some(0));
        assert_eq!(app.selected_row, Some(0));
        assert!(app.is_active_match(0));

        app.previous_match();
        assert_eq!(app.active_match, Some(1));
        assert_eq!(app.selected_row, Some(3));
        assert!(app.is_active_match(3));

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
