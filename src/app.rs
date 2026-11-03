use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    shell::ShellSession,
    storage::{sanitize_single_line, Entry, EntryKind, Journal},
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

    pub fn prompt(self) -> &'static str {
        match self {
            Self::Terminal => "debian$ ",
            Self::Notes => "note› ",
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
    pub scroll_top: usize,
    pub feed_viewport_height: usize,
    pub follow_tail: bool,
    pub search_query: Option<String>,
    pub matches: Vec<usize>,
    pub active_match: Option<usize>,
    pub history: Vec<String>,
    history_position: Option<usize>,
    pub input_mode: InputMode,
    /// True while the PTY has disabled echo (typically a password prompt).
    pub secret_input: bool,
    /// A recent explicit password/passphrase prompt, used together with
    /// ECHO=false to distinguish secrets from unrelated shell echo settings.
    secret_prompt_deadline: Option<Instant>,
    pub status: String,
    pub show_help: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new(journal: Journal, status: String) -> Self {
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
            scroll_top: 0,
            feed_viewport_height: 1,
            follow_tail: true,
            search_query: None,
            matches: Vec::new(),
            active_match: None,
            history,
            history_position: None,
            input_mode: InputMode::Terminal,
            secret_input: false,
            secret_prompt_deadline: None,
            status,
            show_help: false,
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

    fn max_top(&self) -> usize {
        self.journal.entries.len().saturating_sub(self.feed_viewport_height.max(1))
    }

    pub fn scroll_up(&mut self, amount: usize) {
        let current = self.current_top();
        self.scroll_top = current.saturating_sub(amount);
        self.follow_tail = false;
    }

    pub fn scroll_down(&mut self, amount: usize) {
        let next = self.current_top().saturating_add(amount).min(self.max_top());
        self.scroll_top = next;
        self.follow_tail = next >= self.max_top();
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.follow_tail = false;
    }

    pub fn jump_to_latest(&mut self) {
        self.follow_tail = true;
        self.scroll_top = self.max_top();
    }

    pub fn scroll_feed_left(&mut self, amount: u16) {
        self.feed_hscroll = self.feed_hscroll.saturating_sub(amount);
    }

    pub fn scroll_feed_right(&mut self, amount: u16) {
        self.feed_hscroll = self.feed_hscroll.saturating_add(amount);
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
        let clean = sanitize_single_line(value);
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
        if lines.is_empty() {
            return;
        }
        let entries = lines
            .into_iter()
            .map(|line| Entry::with_kind(EntryKind::TerminalOutput, sanitize_single_line(&line)))
            .collect();
        match self.journal.append_batch(entries) {
            Ok(()) => self.status = format!("Captured terminal output · {} records in journal.", self.journal.entries.len()),
            Err(error) => self.status = format!(
                "Terminal output retained in memory; {} pending disk write(s): {error}",
                self.journal.pending_count()
            ),
        }
        // Keep the current scrollback/search position when background shell
        // output arrives. The feed follows automatically only when already live.
    }

    pub fn record_system(&mut self, message: String) {
        let entry = Entry::with_kind(EntryKind::System, sanitize_single_line(&message));
        if let Err(error) = self.journal.append(entry) {
            self.status = format!("System event retained in memory; journal write failed: {error}");
        } else {
            self.status = message;
        }
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

    fn execute_command(&mut self, command_line: &str) {
        let command_line = command_line.trim();
        let (command, argument) = command_line
            .split_once(char::is_whitespace)
            .map(|(head, tail)| (head.to_ascii_lowercase(), tail.trim()))
            .unwrap_or((command_line.to_ascii_lowercase(), ""));

        match command.as_str() {
            "/help" => self.show_help = true,
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
            _ => self.status = format!("Unknown command: {command}. Press F1 or type /help."),
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
            | "/search"
            | "/clear"
            | "/save"
            | "/stats"
            | "/path"
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
