use std::{
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
}

impl Entry {
    pub fn new(text: String) -> Self {
        Self::with_kind(EntryKind::Note, text)
    }

    pub fn with_kind(kind: EntryKind, text: String) -> Self {
        Self {
            timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
            kind,
            text,
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

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(mut entry) => {
                    entry.text = sanitize_single_line(&entry.text);
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
        self.total_text_bytes = self.total_text_bytes.saturating_add(entry.text.len());
        self.entries.push(entry.clone());

        // Never let a newer entry overtake an older entry whose disk append failed.
        if !self.pending.is_empty() {
            self.pending.push(entry);
            return self.save_pending();
        }

        match append_jsonl(&self.path, &entry) {
            Ok(()) => Ok(()),
            Err(error) => {
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
            self.pending.extend(entries);
            return self.save_pending();
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
        Ok(())
    }

    pub fn save_pending(&mut self) -> io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        if self.pending.is_empty() {
            file.sync_data()?;
            return Ok(());
        }

        // If a previous write stopped halfway through a JSON line, terminate that
        // damaged fragment before retrying. Blank lines are ignored by the loader.
        file.write_all(b"\n")?;
        let pending = std::mem::take(&mut self.pending);
        for (index, entry) in pending.iter().enumerate() {
            let mut bytes = serde_json::to_vec(entry).map_err(io::Error::other)?;
            bytes.push(b'\n');
            if let Err(error) = file.write_all(&bytes) {
                self.pending.extend_from_slice(&pending[index..]);
                return Err(error);
            }
        }
        file.flush()?;
        file.sync_data()
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
        .strip_suffix('
    match kind {
        EntryKind::Note => "NOTE",
        EntryKind::TerminalCommand => "CMD",
        EntryKind::TerminalOutput => "OUT",
        EntryKind::System => "SYS",
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

/// Plain-text equivalent of a rendered transcript row, used by mouse selection and copy.
pub fn entry_transcript_text(entry: &Entry, index: usize) -> String {
    format!(
        "{:>6} {} {:<4} │ {}",
        index + 1,
        format_timestamp(&entry.timestamp),
        entry_kind_label(entry.kind),
        entry.text
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
)
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

/// Plain-text equivalent of a rendered transcript row, used by mouse selection and copy.
pub fn entry_transcript_text(entry: &Entry, index: usize) -> String {
    format!(
        "{:>6} {} {:<4} │ {}",
        index + 1,
        format_timestamp(&entry.timestamp),
        entry_kind_label(entry.kind),
        entry.text
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
    use super::should_suppress_terminal_output;

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
}
