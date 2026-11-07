use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Local, SecondsFormat};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Note,
    TerminalCommand,
    TerminalOutput,
    System,
    HermesUser,
    HermesAssistant,
    HermesToolCall,
    HermesToolResult,
}

impl Default for EntryKind {
    fn default() -> Self {
        Self::Note
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub timestamp: String,
    #[serde(default)]
    pub kind: EntryKind,
    pub text: String,
    /// Stable ID for externally imported events; used only for deduplication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

impl Entry {
    pub fn new(text: String) -> Self {
        Self::with_kind(EntryKind::Note, text)
    }

    pub fn with_kind(kind: EntryKind, text: String) -> Self {
        Self {
            timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
            kind,
            text: sanitize_entry_text(kind, &text),
            event_id: None,
        }
    }

    /// Build a timestamped external event while retaining a stable ID so
    /// retrying an unacknowledged inbox file cannot duplicate a journal entry.
    pub fn with_external_event(
        kind: EntryKind,
        text: String,
        timestamp: String,
        event_id: String,
    ) -> Self {
        Self {
            timestamp,
            kind,
            text: sanitize_entry_text(kind, &text),
            event_id: Some(event_id),
        }
    }
}

#[derive(Debug)]
pub struct Journal {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub total_text_bytes: usize,
    pub skipped_corrupt_lines: usize,
    pending: Vec<Entry>,
    /// Event IDs already durably present in the journal.
    persisted_event_ids: HashSet<String>,
    /// Event IDs accepted into memory but not yet confirmed durable.
    pending_event_ids: HashSet<String>,
}

impl Journal {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        if !path.exists() {
            File::create(&path)?;
        }

        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut total_text_bytes = 0usize;
        let mut skipped_corrupt_lines = 0usize;
        let mut persisted_event_ids = HashSet::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(mut entry) => {
                    entry.text = sanitize_entry_text(entry.kind, &entry.text);
                    if let Some(event_id) = entry.event_id.as_ref() {
                        // A retry after a crash may leave both the original and
                        // a later import attempt in JSONL. Keep the first copy.
                        if !persisted_event_ids.insert(event_id.clone()) {
                            continue;
                        }
                    }
                    if entry.kind == EntryKind::TerminalOutput
                        && should_suppress_terminal_output(&entry.text)
                    {
                        // Old empty rows and shell prompts are omitted from the
                        // in-memory transcript without rewriting the user's journal.
                        continue;
                    }
                    total_text_bytes = total_text_bytes.saturating_add(entry.text.len());
                    entries.push(entry);
                }
                Err(_) => skipped_corrupt_lines += 1,
            }
        }

        Ok(Self {
            path,
            entries,
            total_text_bytes,
            skipped_corrupt_lines,
            pending: Vec::new(),
            persisted_event_ids,
            pending_event_ids: HashSet::new(),
        })
    }

    /// Adds to the in-memory cache first, then appends the record to disk.
    /// No cache limit or truncation is applied. A failed disk write stays queued for retry.
    pub fn append(&mut self, entry: Entry) -> io::Result<()> {
        if entry.kind == EntryKind::TerminalOutput
            && should_suppress_terminal_output(&entry.text)
        {
            return Ok(());
        }

        if let Some(event_id) = entry.event_id.as_ref() {
            if self.pending_event_ids.contains(event_id) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "event is in the journal's pending-write queue",
                ));
            }
            if self.persisted_event_ids.contains(event_id) {
                return Ok(());
            }
        }

        self.total_text_bytes = self.total_text_bytes.saturating_add(entry.text.len());
        self.entries.push(entry.clone());

        // Never let a newer entry overtake an older entry whose disk append failed.
        if !self.pending.is_empty() {
            if let Some(event_id) = entry.event_id.as_ref() {
                self.pending_event_ids.insert(event_id.clone());
            }
            self.pending.push(entry);
            return self.save_pending();
        }

        match append_jsonl(&self.path, &entry) {
            Ok(()) => {
                if let Some(event_id) = entry.event_id.as_ref() {
                    self.persisted_event_ids.insert(event_id.clone());
                }
                Ok(())
            }
            Err(error) => {
                if let Some(event_id) = entry.event_id.as_ref() {
                    self.pending_event_ids.insert(event_id.clone());
                }
                self.pending.push(entry);
                Err(error)
            }
        }
    }

    /// Batch append is used for PTY output so high-volume commands do not force
    /// one fsync per output line. Entries enter the cache before any disk I/O.
    pub fn append_batch(&mut self, entries: Vec<Entry>) -> io::Result<()> {
        let entries: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| {
                entry.kind != EntryKind::TerminalOutput
                    || !should_suppress_terminal_output(&entry.text)
            })
            .collect();
        if entries.is_empty() {
            return Ok(());
        }

        for entry in &entries {
            self.total_text_bytes = self.total_text_bytes.saturating_add(entry.text.len());
            self.entries.push(entry.clone());
        }

        if !self.pending.is_empty() {
            for entry in &entries {
                if let Some(event_id) = entry.event_id.as_ref() {
                    self.pending_event_ids.insert(event_id.clone());
                }
            }
            self.pending.extend(entries);
            return self.save_pending();
        }

        for entry in &entries {
            if let Some(event_id) = entry.event_id.as_ref() {
                self.pending_event_ids.insert(event_id.clone());
            }
        }

        let mut file = match OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(file) => file,
            Err(error) => {
                self.pending.extend(entries);
                return Err(error);
            }
        };

        for (index, entry) in entries.iter().enumerate() {
            let mut bytes = match serde_json::to_vec(entry) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.pending.extend_from_slice(&entries[index..]);
                    return Err(io::Error::other(error));
                }
            };
            bytes.push(b'\n');
            if let Err(error) = file.write_all(&bytes) {
                self.pending.extend_from_slice(&entries[index..]);
                return Err(error);
            }
        }

        // If syncing fails, the records have already been written. Leave them
        // in place rather than queueing and duplicating them on the next save;
        // a later /save retries sync_data for the complete journal.
        file.flush()?;
        file.sync_data()?;
        self.persisted_event_ids.extend(self.pending_event_ids.drain());
        Ok(())
    }

    pub fn save_pending(&mut self) -> io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        if self.pending.is_empty() {
            file.sync_data()?;
            self.persisted_event_ids.extend(self.pending_event_ids.drain());
            return Ok(());
        }

        // If a previous write stopped halfway through a JSON line, terminate that
        // damaged fragment before retrying. Blank lines are ignored by the loader.
        file.write_all(b"\n")?;
        let pending = std::mem::take(&mut self.pending);
        for (index, entry) in pending.iter().enumerate() {
            let mut bytes = match serde_json::to_vec(entry) {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.pending.extend_from_slice(&pending[index..]);
                    return Err(io::Error::other(error));
                }
            };
            bytes.push(b'\n');
            if let Err(error) = file.write_all(&bytes) {
                self.pending.extend_from_slice(&pending[index..]);
                return Err(error);
            }
        }
        file.flush()?;
        file.sync_data()?;
        self.persisted_event_ids.extend(self.pending_event_ids.drain());
        Ok(())
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn export_plain_text(&self, destination: &Path) -> io::Result<()> {
        if let Some(parent) = destination.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut output = File::create(destination)?;
        for entry in &self.entries {
            writeln!(
                output,
                "[{}] [{}] {}",
                entry.timestamp,
                entry_kind_label(entry.kind),
                entry.text
            )?;
        }
        output.sync_all()
    }
}

/// Return true for PTY rows that are presentation noise rather than useful output:
/// blank lines and the standard user@host:path$ / user@host:path# shell prompt.
/// The prompt is already shown in StreamNote's live input footer.
/// Return true for PTY rows that are presentation noise rather than useful output:
/// blank lines and the standard user@host:path$ / user@host:path# shell prompt.
/// The prompt is already shown in StreamNote's live input footer.
pub fn should_suppress_terminal_output(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return true;
    }

    let Some((identity, path_prompt)) = line.rsplit_once(':') else {
        return false;
    };
    let Some((user, host)) = identity.split_once('@') else {
        return false;
    };
    let path = path_prompt
        .strip_suffix('$')
        .or_else(|| path_prompt.strip_suffix('#'));
    let Some(path) = path.map(str::trim) else {
        return false;
    };

    !user.is_empty()
        && !host.is_empty()
        && !user.chars().any(char::is_whitespace)
        && !host.chars().any(char::is_whitespace)
        && (path == "~" || path.starts_with("~/") || path.starts_with('/'))
}
pub fn entry_kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Note => "NOTE",
        EntryKind::TerminalCommand => "CMD",
        EntryKind::TerminalOutput => "OUT",
        EntryKind::System => "SYS",
        EntryKind::HermesUser => "HUSR",
        EntryKind::HermesAssistant => "HAI",
        EntryKind::HermesToolCall => "HCAL",
        EntryKind::HermesToolResult => "HRES",
    }
}

/// Format a journal timestamp in the current local timezone, retaining its UTC offset.
pub fn format_timestamp(timestamp: &str) -> String {
    DateTime::parse_from_rfc3339(timestamp)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.to_string())
}

/// Decode a terminal tool-call payload into a useful one-line transcript preview.
fn terminal_tool_command_preview(text: &str) -> Option<String> {
    let tool_name = text.lines().next()?.strip_prefix("Tool:")?.trim();
    if tool_name != "terminal" {
        return None;
    }

    let (_, arguments) = text.split_once("Arguments:")?;
    let arguments: serde_json::Value = serde_json::from_str(arguments.trim()).ok()?;
    let command = arguments.get("command")?.as_str()?;
    if command.trim().is_empty() {
        return None;
    }

    // Transcript rows are one line tall. Keep the complete decoded command
    // visible on that line; the normal horizontal-scroll controls handle long commands.
    Some(format!("terminal› {}", sanitize_single_line(command)))
}

/// Compact single-row preview. Full Hermes Markdown remains available in the
/// journal and in the message viewer. Terminal tool calls show their decoded
/// command rather than the generic multi-line payload placeholder.
pub fn entry_display_text(entry: &Entry) -> String {
    if entry.kind == EntryKind::HermesToolCall {
        if let Some(command) = terminal_tool_command_preview(&entry.text) {
            return command;
        }
    }

    if matches!(
        entry.kind,
        EntryKind::HermesUser
            | EntryKind::HermesAssistant
            | EntryKind::HermesToolCall
            | EntryKind::HermesToolResult
    ) {
        let first_line = entry.text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
        if entry.text.contains('\n') {
            format!("{first_line}  ↳ [full message]")
        } else {
            first_line.to_string()
        }
    } else {
        entry.text.clone()
    }
}

/// Plain-text equivalent of a rendered transcript row, used by mouse selection and copy.
pub fn entry_transcript_text(entry: &Entry, index: usize) -> String {
    format!(
        "{:>6} {} {:<4} │ {}",
        index + 1,
        format_timestamp(&entry.timestamp),
        entry_kind_label(entry.kind),
        entry_display_text(entry)
    )
}

fn append_jsonl(path: &Path, entry: &Entry) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut bytes = serde_json::to_vec(entry).map_err(io::Error::other)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_data()
}

pub fn sanitize_single_line(input: &str) -> String {
    input
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Preserve Markdown line breaks while removing terminal control characters
/// that could otherwise affect the STREENOTE display.
pub fn sanitize_multiline(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .chars()
        .filter(|ch| *ch == '\n' || *ch == '\t' || !ch.is_control())
        .collect()
}

fn sanitize_entry_text(kind: EntryKind, input: &str) -> String {
    match kind {
        EntryKind::HermesUser
        | EntryKind::HermesAssistant
        | EntryKind::HermesToolCall
        | EntryKind::HermesToolResult => sanitize_multiline(input),
        _ => sanitize_single_line(input),
    }
}


#[cfg(test)]
mod timestamp_tests {
    use super::format_timestamp;

    #[test]
    fn timestamp_display_includes_date_time_and_offset() {
        let formatted = format_timestamp("2026-11-03T22:44:00-05:00");
        assert!(formatted.contains("2026"));
        assert!(formatted.contains(':'));
        assert!(formatted.len() >= 25);
    }

    #[test]
    fn malformed_legacy_timestamp_is_preserved_in_full() {
        let timestamp = "legacy-timestamp-value";
        assert_eq!(format_timestamp(timestamp), timestamp);
    }
}


#[cfg(test)]
mod terminal_output_filter_tests {
    use super::{should_suppress_terminal_output, Entry, EntryKind, Journal};
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEST_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_path(label: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "streamnote-{label}-{}-{id}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn suppresses_empty_and_whitespace_only_terminal_rows() {
        assert!(should_suppress_terminal_output(""));
        assert!(should_suppress_terminal_output("   "));
        assert!(should_suppress_terminal_output("\t  "));
    }

    #[test]
    fn suppresses_user_host_working_directory_prompts() {
        assert!(should_suppress_terminal_output("jordan@server:~$ "));
        assert!(should_suppress_terminal_output("jordan@server:~/Desktop$"));
        assert!(should_suppress_terminal_output("root@debian:/etc# "));
    }

    #[test]
    fn retains_normal_terminal_output() {
        assert!(!should_suppress_terminal_output("/home/jordan"));
        assert!(!should_suppress_terminal_output("BACKUPS Desktop Documents"));
        assert!(!should_suppress_terminal_output("jordan@server is connected"));
        assert!(!should_suppress_terminal_output("total 12"));
    }

    #[test]
    fn batch_append_does_not_persist_blank_rows_or_shell_prompts() {
        let path = test_path("batch-filter");
        let _ = fs::remove_file(&path);
        let mut journal = Journal::open(path.clone()).expect("open test journal");
        journal.append_batch(vec![
            Entry::with_kind(EntryKind::TerminalOutput, String::new()),
            Entry::with_kind(EntryKind::TerminalOutput, "   ".to_string()),
            Entry::with_kind(EntryKind::TerminalOutput, "jordan@server:~$ ".to_string()),
            Entry::with_kind(EntryKind::TerminalOutput, "useful output".to_string()),
        ]).expect("append filtered output");

        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].text, "useful output");
        drop(journal);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn loading_legacy_journal_hides_old_blank_rows_and_prompts() {
        let path = test_path("legacy-filter");
        let _ = fs::remove_file(&path);
        let rows = vec![
            Entry::with_kind(EntryKind::TerminalOutput, String::new()),
            Entry::with_kind(EntryKind::TerminalOutput, "jordan@server:~/Desktop$ ".to_string()),
            Entry::with_kind(EntryKind::TerminalOutput, "cyberpunk.pdf  suitcase".to_string()),
        ];
        let contents = rows.iter()
            .map(|entry| serde_json::to_string(entry).expect("serialize entry"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, &contents).expect("write legacy journal");

        let journal = Journal::open(path.clone()).expect("open legacy journal");
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].text, "cyberpunk.pdf  suitcase");
        drop(journal);

        // Filtering is a display/load behavior; the original journal is left intact.
        assert_eq!(
            fs::read_to_string(&path).expect("read original journal"),
            contents
        );
        let _ = fs::remove_file(path);
    }
}


#[cfg(test)]
mod hermes_tool_entry_tests {
    use super::{entry_display_text, entry_kind_label, sanitize_entry_text, Entry, EntryKind};

    #[test]
    fn tool_events_have_distinct_serialized_kinds_and_row_labels() {
        let call = Entry::with_kind(
            EntryKind::HermesToolCall,
            "Tool: terminal\nArguments:\n{}".to_string(),
        );
        let result = Entry::with_kind(
            EntryKind::HermesToolResult,
            "Tool: terminal\nStatus: ok\nResult:\n{}".to_string(),
        );

        assert_eq!(serde_json::to_value(&call).unwrap()["kind"], "hermes_tool_call");
        assert_eq!(serde_json::to_value(&result).unwrap()["kind"], "hermes_tool_result");
        assert_eq!(entry_kind_label(EntryKind::HermesToolCall), "HCAL");
        assert_eq!(entry_kind_label(EntryKind::HermesToolResult), "HRES");
    }

    #[test]
    fn tool_event_previews_and_multiline_text_are_preserved() {
        let text = "Tool: terminal\nArguments:\n{\n  \"command\": \"pwd\"\n}";
        assert_eq!(sanitize_entry_text(EntryKind::HermesToolCall, text), text);
        let entry = Entry::with_kind(EntryKind::HermesToolCall, text.to_string());
        assert_eq!(entry_display_text(&entry), "terminal› pwd");
    }

    #[test]
    fn terminal_tool_preview_decodes_the_complete_json_command() {
        let text = r#"Tool: terminal
Call ID: call_el4tbrqb
Arguments:
{
  "command": "curl -sL \"https://www.icc-cpi.int/about/otp/proprio-motu-investigations 2>/dev/null | grep -i \"proprio\" | head -20",
  "timeout": 60
}"#;
        let entry = Entry::with_kind(EntryKind::HermesToolCall, text.to_string());

        assert_eq!(
            entry_display_text(&entry),
            r#"terminal› curl -sL "https://www.icc-cpi.int/about/otp/proprio-motu-investigations 2>/dev/null | grep -i "proprio" | head -20"#
        );
    }

    #[test]
    fn non_terminal_tool_calls_keep_the_generic_preview() {
        let text = "Tool: browser\nArguments:\n{\n  \"url\": \"https://example.com\"\n}";
        let entry = Entry::with_kind(EntryKind::HermesToolCall, text.to_string());

        assert_eq!(
            entry_display_text(&entry),
            "Tool: browser  ↳ [full message]"
        );
    }
}
