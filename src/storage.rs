use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{Local, SecondsFormat};
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

pub fn entry_kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Note => "NOTE",
        EntryKind::TerminalCommand => "CMD",
        EntryKind::TerminalOutput => "OUT",
        EntryKind::System => "SYS",
    }
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
