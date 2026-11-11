use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Duration as ChronoDuration, Local, SecondsFormat, Utc};

use crate::{
    archive::{ArchiveSearchHit, ArchiveSearchResult, ArchiveSearcher},
    postgres::{PostgresJournal, SearchResult as PostgresSearchResult, Update as PostgresUpdate},
};
use serde::{Deserialize, Serialize};

/// The live transcript retains the trailing 30 days; the JSONL/PostgreSQL archive remains complete.
pub const HOT_WINDOW_DAYS: i64 = 30;

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
    HermesStreamStart,
    HermesStreamDelta,
    HermesStreamEnd,
    HermesHistory,
    HermesInterim,
    HermesSession,
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
    /// Stable identity for this journal row, used for cross-terminal replication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Stable ID for externally imported events; used only for deduplication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Stable Hermes conversation identifier; it survives session rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// Current Hermes session identifier; may rotate after context compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Previous session identifier when Hermes rotates a session during compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_session_id: Option<String>,
    /// Hermes turn identifier shared by prompt, stream, and tool events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Tool-call identifier used to pair a tool invocation with its result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Provider iteration within a turn; stream hooks may fire more than once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u32>,
}

fn new_record_id() -> String {
    static NEXT_RECORD_ID: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{nanos:032x}-{:x}-{:x}",
        std::process::id(),
        NEXT_RECORD_ID.fetch_add(1, Ordering::Relaxed)
    )
}

impl Entry {
    pub fn with_kind(kind: EntryKind, text: String) -> Self {
        Self {
            timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Secs, false),
            kind,
            text: sanitize_entry_text(kind, &text),
            record_id: Some(new_record_id()),
            event_id: None,
            conversation_id: None,
            session_id: None,
            related_session_id: None,
            turn_id: None,
            tool_call_id: None,
            iteration: None,
        }
    }

    /// Build a timestamped external event while retaining a stable ID so
    /// retrying an unacknowledged inbox file cannot duplicate a journal entry.
    pub fn ensure_record_id(&mut self) {
        if self.record_id.is_none() {
            self.record_id = Some(new_record_id());
        }
    }

    pub fn with_external_event(
        kind: EntryKind,
        text: String,
        timestamp: String,
        event_id: String,
        conversation_id: Option<String>,
        session_id: Option<String>,
        related_session_id: Option<String>,
        turn_id: Option<String>,
        tool_call_id: Option<String>,
        iteration: Option<u32>,
    ) -> Self {
        Self {
            timestamp,
            kind,
            text: sanitize_entry_text(kind, &text),
            record_id: Some(new_record_id()),
            event_id: Some(event_id),
            conversation_id,
            session_id,
            related_session_id,
            turn_id,
            tool_call_id,
            iteration,
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
    /// Record IDs already durably present in the shared journal.
    persisted_record_ids: HashSet<String>,
    /// Record IDs already represented in this process's in-memory cache.
    cached_record_ids: HashSet<String>,
    /// Record IDs accepted into memory but not yet confirmed durable.
    pending_record_ids: HashSet<String>,
    /// Event IDs already represented in this process's in-memory cache.
    cached_event_ids: HashSet<String>,
    /// Event IDs accepted into memory but not yet confirmed durable.
    pending_event_ids: HashSet<String>,
    /// Byte offset through the shared JSONL file already checked for external
    /// rows. Each SSH process advances this while holding JournalFileLock.
    journal_scan_offset: u64,
    /// PostgreSQL is authoritative when configured; path remains the JSONL import/export path.
    postgres: Option<PostgresJournal>,
    postgres_search_results: Vec<PostgresSearchResult>,
    archive_searcher: Option<ArchiveSearcher>,
}

#[cfg(unix)]
struct JournalFileLock(File);

#[cfg(unix)]
impl JournalFileLock {
    fn acquire(journal_path: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;

        let mut lock_name = journal_path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        if let Some(parent) = lock_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for JournalFileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(not(unix))]
struct JournalFileLock;

#[cfg(not(unix))]
impl JournalFileLock {
    fn acquire(_journal_path: &Path) -> io::Result<Self> {
        Ok(Self)
    }
}

impl Journal {
    pub fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let _journal_lock = JournalFileLock::acquire(&path)?;
        if !path.exists() {
            File::create(&path)?;
        }

        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut total_text_bytes = 0usize;
        let mut skipped_corrupt_lines = 0usize;
        let cutoff = Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS);
        let mut persisted_event_ids = HashSet::new();
        let mut persisted_record_ids = HashSet::new();

        // Scan the durable file, but retain only the hot window and its IDs.
        // Historical records stay on disk and are searched by ArchiveSearcher.
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(&line) {
                Ok(mut entry) => {
                    entry.text = sanitize_entry_text(entry.kind, &entry.text);
                    if !is_hot_entry_at(&entry, cutoff) {
                        continue;
                    }
                    if let Some(event_id) = entry.event_id.as_ref() {
                        if !persisted_event_ids.insert(event_id.clone()) {
                            continue;
                        }
                    }
                    if let Some(record_id) = entry.record_id.as_ref() {
                        if !persisted_record_ids.insert(record_id.clone()) {
                            continue;
                        }
                    }
                    if entry.kind == EntryKind::TerminalOutput
                        && should_suppress_terminal_output(&entry.text)
                    {
                        continue;
                    }
                    total_text_bytes = total_text_bytes.saturating_add(entry.text.len());
                    if let Some(previous) = entries.last_mut().filter(|previous| {
                        can_merge_stream_delta(previous, &entry)
                    }) {
                        previous.text.push_str(&entry.text);
                    } else {
                        entries.push(entry);
                    }
                }
                Err(_) => skipped_corrupt_lines += 1,
            }
        }

        let journal_scan_offset = fs::metadata(&path)?.len();
        let archive_searcher = Some(ArchiveSearcher::new(path.clone())?);
        Ok(Self {
            path,
            entries,
            total_text_bytes,
            skipped_corrupt_lines,
            pending: Vec::new(),
            cached_event_ids: persisted_event_ids.clone(),
            persisted_event_ids,
            cached_record_ids: persisted_record_ids.clone(),
            persisted_record_ids,
            pending_record_ids: HashSet::new(),
            pending_event_ids: HashSet::new(),
            journal_scan_offset,
            postgres: None,
            postgres_search_results: Vec::new(),
            archive_searcher,
        })
    }

    /// Opens PostgreSQL as the authoritative journal, importing legacy JSONL
    /// records only when the configured database is empty.
    pub fn open_postgres(path: PathBuf, database_url: String) -> io::Result<(Self, usize)> {
        // PostgreSQL imports the legacy JSONL source incrementally only when
        // the database is empty; normal startup never materializes the archive.
        let (postgres, database_entries, imported_count) =
            PostgresJournal::connect(database_url, Some(path.clone()))?;

        let mut journal = Self {
            path,
            entries: Vec::new(),
            total_text_bytes: 0,
            skipped_corrupt_lines: 0,
            pending: Vec::new(),
            persisted_event_ids: HashSet::new(),
            persisted_record_ids: HashSet::new(),
            cached_record_ids: HashSet::new(),
            pending_record_ids: HashSet::new(),
            cached_event_ids: HashSet::new(),
            pending_event_ids: HashSet::new(),
            journal_scan_offset: 0,
            postgres: Some(postgres),
            postgres_search_results: Vec::new(),
            archive_searcher: None,
        };

        for mut entry in database_entries {
            entry.text = sanitize_entry_text(entry.kind, &entry.text);
            if !is_hot_entry(&entry) {
                continue;
            }
            if let Some(event_id) = entry.event_id.as_ref() {
                if !journal.persisted_event_ids.insert(event_id.clone()) { continue; }
            }
            if let Some(record_id) = entry.record_id.as_ref() {
                if !journal.persisted_record_ids.insert(record_id.clone()) { continue; }
            }
            if entry.kind == EntryKind::TerminalOutput
                && should_suppress_terminal_output(&entry.text) {
                continue;
            }
            journal.push_entry_to_memory(entry);
        }
        journal.cached_event_ids = journal.persisted_event_ids.clone();
        journal.cached_record_ids = journal.persisted_record_ids.clone();
        Ok((journal, imported_count))
    }

    pub fn search_postgres(&mut self, query: String) -> io::Result<Option<u64>> {
        self.postgres.as_mut()
            .map(|postgres| postgres.search(query).map(Some))
            .unwrap_or(Ok(None))
    }

    pub fn drain_postgres_search_results(&mut self) -> Vec<PostgresSearchResult> {
        std::mem::take(&mut self.postgres_search_results)
    }

    pub fn search_archive(&self, request_id: u64, query: String) -> io::Result<()> {
        self.archive_searcher
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "archive scan is available with the JSONL backend"))?
            .search(request_id, query)
    }

    pub fn drain_archive_search_results(&self) -> Vec<ArchiveSearchResult> {
        self.archive_searcher
            .as_ref()
            .map(ArchiveSearcher::drain_results)
            .unwrap_or_default()
    }

    pub fn load_archive_hit(&self, hit: &ArchiveSearchHit) -> io::Result<Entry> {
        self.archive_searcher
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "archive record loading is unavailable"))?
            .read_hit(hit)
    }

    /// Prune expired in-memory rows while leaving the durable archive untouched.
    /// Call periodically so the hot window remains exactly the trailing 30 days.
    pub fn prune_hot_cache(&mut self) -> usize {
        let cutoff = Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS);
        let before = self.entries.len();
        self.entries.retain(|entry| is_hot_entry_at(entry, cutoff));
        let removed = before.saturating_sub(self.entries.len());
        if removed > 0 {
            self.total_text_bytes = self.entries.iter().map(|entry| entry.text.len()).sum();
            // Stream IDs are intentionally collapsed to one in-memory row, so
            // rebuild deduplication sets from the durable hot window instead
            // of deriving them from the coalesced transcript vector.
            if let Ok((event_ids, record_ids)) = read_hot_record_ids(&self.path, cutoff) {
                self.persisted_event_ids = event_ids;
                self.persisted_record_ids = record_ids;
                self.cached_event_ids = self.persisted_event_ids.clone();
                self.cached_record_ids = self.persisted_record_ids.clone();
            }
        }
        removed
    }

    pub fn backend_label(&self) -> &'static str {
        if self.postgres.is_some() { "PostgreSQL" } else { "JSONL" }
    }

    /// Adds to the in-memory cache first, then persists the record.
    /// No cache limit or truncation is applied. A failed disk write stays queued for retry.
    pub fn append(&mut self, mut entry: Entry) -> io::Result<()> {
        if entry.kind == EntryKind::TerminalOutput
            && should_suppress_terminal_output(&entry.text)
        {
            return Ok(());
        }

        if self.postgres.is_some() {
            let refresh_error = self.refresh_external_entries().err();
            entry.ensure_record_id();
            if entry.record_id.as_ref().is_some_and(|id| self.cached_record_ids.contains(id))
                || entry.event_id.as_ref().is_some_and(|id| self.cached_event_ids.contains(id))
            {
                return Ok(());
            }
            let hot = is_hot_entry(&entry);
            self.push_entry_to_memory(entry.clone());
            if hot {
                if let Some(record_id) = entry.record_id.as_ref() {
                    self.cached_record_ids.insert(record_id.clone());
                }
                if let Some(event_id) = entry.event_id.as_ref() {
                    self.cached_event_ids.insert(event_id.clone());
                }
            }
            if let Err(error) = self.postgres.as_ref().expect("PostgreSQL backend configured").enqueue(entry.clone()) {
                self.pending.push(entry);
                return Err(error);
            }
            if let Some(error) = refresh_error {
                return Err(error);
            }
            return Ok(());
        }

        let _journal_lock = JournalFileLock::acquire(&self.path)?;
        let event_id = entry.event_id.clone();
        let record_id = entry.record_id.clone();
        if let Some(event_id) = event_id.as_ref() {
            if self.pending_event_ids.contains(event_id) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "event is in the journal's pending-write queue",
                ));
            }
        }
        if let Some(record_id) = record_id.as_ref() {
            if self.pending_record_ids.contains(record_id) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "record is in the journal's pending-write queue",
                ));
            }
        }
        self.refresh_external_entries_locked()?;

        if let Some(event_id) = event_id.as_ref() {
            if !self.persisted_event_ids.contains(event_id)
                && archive_contains_event_id(&self.path, event_id)?
            {
                return Ok(());
            }
            if self.persisted_event_ids.contains(event_id) {
                // Another SSH instance may have persisted this event first.
                // If the shared-journal poll has not already mirrored it into
                // this process's cache, add it now without writing a duplicate.
                if self.cached_event_ids.insert(event_id.clone()) {
                    if let Some(record_id) = record_id.as_ref() {
                        self.cached_record_ids.insert(record_id.clone());
                    }
                    self.push_entry_to_memory(entry);
                }
                return Ok(());
            }
        }

        if let Some(record_id) = record_id.as_ref() {
            if self.persisted_record_ids.contains(record_id) {
                if self.cached_record_ids.insert(record_id.clone()) {
                    self.push_entry_to_memory(entry);
                }
                return Ok(());
            }
        }

        let hot = is_hot_entry(&entry);
        self.push_entry_to_memory(entry.clone());
        if hot {
            if let Some(event_id) = entry.event_id.as_ref() {
                self.cached_event_ids.insert(event_id.clone());
            }
            if let Some(record_id) = record_id.as_ref() {
                self.cached_record_ids.insert(record_id.clone());
            }
        }

        // Never let a newer entry overtake an older entry whose disk append failed.
        if !self.pending.is_empty() {
            if let Some(event_id) = entry.event_id.as_ref() {
                self.pending_event_ids.insert(event_id.clone());
            }
            if let Some(record_id) = record_id.as_ref() {
                self.pending_record_ids.insert(record_id.clone());
            }
            self.pending.push(entry);
            return self.save_pending_locked();
        }

        match append_jsonl(&self.path, &entry) {
            Ok(()) => {
                if hot {
                    if let Some(event_id) = entry.event_id.as_ref() {
                        self.persisted_event_ids.insert(event_id.clone());
                    }
                    if let Some(record_id) = record_id.as_ref() {
                        self.persisted_record_ids.insert(record_id.clone());
                    }
                }
                self.journal_scan_offset = fs::metadata(&self.path)?.len();
                Ok(())
            }
            Err(error) => {
                if hot {
                    if let Some(event_id) = entry.event_id.as_ref() {
                        self.pending_event_ids.insert(event_id.clone());
                    }
                    if let Some(record_id) = record_id.as_ref() {
                        self.pending_record_ids.insert(record_id.clone());
                    }
                }
                self.pending.push(entry);
                Err(error)
            }
        }
    }

    fn push_entry_to_memory(&mut self, entry: Entry) {
        if !is_hot_entry(&entry) {
            return;
        }
        self.total_text_bytes = self.total_text_bytes.saturating_add(entry.text.len());
        if let Some(previous) = self.entries.last_mut().filter(|previous| {
            can_merge_stream_delta(previous, &entry)
        }) {
            // Keep one live assistant bubble in memory while persisting every
            // raw delta as its own append-only event for crash recovery.
            previous.text.push_str(&entry.text);
        } else {
            self.entries.push(entry);
        }
    }

    /// Poll new notes and Hermes events from the shared JSONL journal.
    ///
    /// Every terminal keeps its own in-memory view, while this tail reader
    /// mirrors newly appended note rows from any peer into every live view.
    /// Hermes events retain their stable event-ID deduplication as well.
    pub fn refresh_external_entries(&mut self) -> io::Result<usize> {
        if self.postgres.is_some() {
            let updates = self.postgres.as_ref().expect("PostgreSQL backend configured").drain_updates();
            let mut mirrored = 0usize;
            let mut first_error: Option<String> = None;
            for update in updates {
                match update {
                    PostgresUpdate::ExternalEntries(entries) => {
                        for mut entry in entries {
                            let record_is_new = entry.record_id.as_ref()
                                .map(|id| self.cached_record_ids.insert(id.clone()))
                                .unwrap_or(true);
                            let event_is_new = entry.event_id.as_ref()
                                .map(|id| self.cached_event_ids.insert(id.clone()))
                                .unwrap_or(true);
                            if let Some(id) = entry.record_id.as_ref() {
                                self.persisted_record_ids.insert(id.clone());
                            }
                            if let Some(id) = entry.event_id.as_ref() {
                                self.persisted_event_ids.insert(id.clone());
                            }
                            // Preserve the established live-sharing contract:
                            // notes and Hermes events fan out to peers; shell commands,
                            // raw PTY output, and system notices stay in their origin TUI.
                            let should_mirror = entry.kind == EntryKind::Note
                                || is_hermes_journal_kind(entry.kind);
                            if !record_is_new || !event_is_new || !should_mirror {
                                continue;
                            }
                            entry.text = sanitize_entry_text(entry.kind, &entry.text);
                            if entry.kind == EntryKind::TerminalOutput
                                && should_suppress_terminal_output(&entry.text)
                            {
                                continue;
                            }
                            self.push_entry_to_memory(entry);
                            mirrored += 1;
                        }
                    }
                    PostgresUpdate::SearchResult(result) => self.postgres_search_results.push(result),
                    PostgresUpdate::Error(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
            }
            if let Some(error) = first_error {
                return Err(io::Error::other(error));
            }
            return Ok(mirrored);
        }

        let _journal_lock = JournalFileLock::acquire(&self.path)?;
        self.refresh_external_entries_locked()
    }

    /// Refresh rows appended by other OPERATOR processes since our last file
    /// offset. The caller holds the inter-process journal lock.
    fn refresh_external_entries_locked(&mut self) -> io::Result<usize> {
        let metadata = fs::metadata(&self.path)?;
        if metadata.len() < self.journal_scan_offset {
            self.journal_scan_offset = 0;
            self.persisted_event_ids.clear();
            self.persisted_record_ids.clear();
        }
        let file = File::open(&self.path)?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.journal_scan_offset))?;
        let mut line = Vec::new();
        let mut scanned = 0u64;
        let mut mirrored = 0usize;
        let cutoff = Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS);
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            if count == 0 || !line.ends_with(b"\n") {
                break;
            }
            if let Ok(mut entry) = serde_json::from_slice::<Entry>(&line) {
                if is_hot_entry_at(&entry, cutoff) {
                    let record_id_is_new = entry
                        .record_id
                        .as_ref()
                        .map(|record_id| self.persisted_record_ids.insert(record_id.clone()));

                    let should_mirror = if is_hermes_journal_kind(entry.kind) {
                        if let Some(event_id) = entry.event_id.as_ref() {
                            let is_new = self.persisted_event_ids.insert(event_id.clone());
                            is_new && self.cached_event_ids.insert(event_id.clone())
                        } else {
                            match (record_id_is_new, entry.record_id.as_ref()) {
                                (Some(true), Some(record_id)) => {
                                    self.cached_record_ids.insert(record_id.clone())
                                }
                                (None, None) => true,
                                _ => false,
                            }
                        }
                    } else if entry.kind == EntryKind::Note {
                        match (record_id_is_new, entry.record_id.as_ref()) {
                            (Some(true), Some(record_id)) => {
                                self.cached_record_ids.insert(record_id.clone())
                            }
                            (None, None) => true,
                            _ => false,
                        }
                    } else {
                        false
                    };

                    if should_mirror {
                        if let Some(record_id) = entry.record_id.as_ref() {
                            self.cached_record_ids.insert(record_id.clone());
                        }
                        entry.text = sanitize_entry_text(entry.kind, &entry.text);
                        self.push_entry_to_memory(entry);
                        mirrored += 1;
                    }
                }
            }
            scanned = scanned.saturating_add(count as u64);
        }
        self.journal_scan_offset = self.journal_scan_offset.saturating_add(scanned);
        Ok(mirrored)
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

        if self.postgres.is_some() {
            let refresh_error = self.refresh_external_entries().err();
            let mut accepted = Vec::with_capacity(entries.len());
            for mut entry in entries {
                entry.ensure_record_id();
                if entry.record_id.as_ref().is_some_and(|id| self.cached_record_ids.contains(id))
                    || entry.event_id.as_ref().is_some_and(|id| self.cached_event_ids.contains(id))
                {
                    continue;
                }
                let hot = is_hot_entry(&entry);
                if hot {
                    if let Some(record_id) = entry.record_id.as_ref() {
                        self.cached_record_ids.insert(record_id.clone());
                    }
                    if let Some(event_id) = entry.event_id.as_ref() {
                        self.cached_event_ids.insert(event_id.clone());
                    }
                }
                self.push_entry_to_memory(entry.clone());
                accepted.push(entry);
            }
            if accepted.is_empty() {
                return Ok(());
            }
            if let Err(error) = self.postgres.as_ref().expect("PostgreSQL backend configured").enqueue_batch(accepted.clone()) {
                self.pending.extend(accepted);
                return Err(error);
            }
            if let Some(error) = refresh_error {
                return Err(error);
            }
            return Ok(());
        }

        let _journal_lock = JournalFileLock::acquire(&self.path)?;
        self.refresh_external_entries_locked()?;

        for entry in &entries {
            let hot = is_hot_entry(entry);
            self.push_entry_to_memory(entry.clone());
            if hot {
                if let Some(event_id) = entry.event_id.as_ref() {
                    self.cached_event_ids.insert(event_id.clone());
                }
                if let Some(record_id) = entry.record_id.as_ref() {
                    self.cached_record_ids.insert(record_id.clone());
                }
            }
        }

        if !self.pending.is_empty() {
            for entry in &entries {
                if is_hot_entry(entry) {
                    if let Some(event_id) = entry.event_id.as_ref() {
                        self.pending_event_ids.insert(event_id.clone());
                    }
                    if let Some(record_id) = entry.record_id.as_ref() {
                        self.pending_record_ids.insert(record_id.clone());
                    }
                }
            }
            self.pending.extend(entries);
            return self.save_pending_locked();
        }

        for entry in &entries {
            if is_hot_entry(entry) {
                if let Some(event_id) = entry.event_id.as_ref() {
                    self.pending_event_ids.insert(event_id.clone());
                }
                if let Some(record_id) = entry.record_id.as_ref() {
                    self.pending_record_ids.insert(record_id.clone());
                }
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
        self.persisted_record_ids.extend(self.pending_record_ids.drain());
        self.journal_scan_offset = fs::metadata(&self.path)?.len();
        Ok(())
    }

    pub fn save_pending(&mut self) -> io::Result<()> {
        if self.postgres.is_some() {
            if !self.pending.is_empty() {
                let pending = std::mem::take(&mut self.pending);
                if let Err(error) = self.postgres.as_ref().expect("PostgreSQL backend configured").enqueue_batch(pending.clone()) {
                    self.pending = pending;
                    return Err(error);
                }
            }
            return self.postgres.as_ref().expect("PostgreSQL backend configured").flush();
        }

        let _journal_lock = JournalFileLock::acquire(&self.path)?;
        self.refresh_external_entries_locked()?;
        self.save_pending_locked()
    }

    fn save_pending_locked(&mut self) -> io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        if self.pending.is_empty() {
            file.sync_data()?;
            self.persisted_event_ids.extend(self.pending_event_ids.drain());
            self.persisted_record_ids.extend(self.pending_record_ids.drain());
            self.journal_scan_offset = fs::metadata(&self.path)?.len();
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
        self.persisted_record_ids.extend(self.pending_record_ids.drain());
        self.journal_scan_offset = fs::metadata(&self.path)?.len();
        Ok(())
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
            + self.postgres.as_ref().map(PostgresJournal::pending_count).unwrap_or(0)
    }

    /// Export the original, uncoalesced journal events as JSON Lines.
    pub fn export_jsonl(&self, destination: &Path) -> io::Result<()> {
        if let Some(postgres) = self.postgres.as_ref() {
            return postgres.export_jsonl(destination);
        }
        if destination == self.path.as_path() {
            return Ok(());
        }
        if let Some(parent) = destination.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&self.path, destination)?;
        Ok(())
    }

    pub fn export_plain_text(&self, destination: &Path) -> io::Result<()> {
        let temporary_jsonl = if let Some(postgres) = self.postgres.as_ref() {
            let temporary = destination.with_file_name(format!(
                ".operator-export-{}-{}.jsonl",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
            ));
            postgres.export_jsonl(&temporary)?;
            Some(temporary)
        } else {
            None
        };
        let source = temporary_jsonl.as_deref().unwrap_or(self.path.as_path());
        let result = export_plain_text_from_jsonl(source, destination);
        if let Some(temporary) = temporary_jsonl {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

/// Return true for PTY rows that are presentation noise rather than useful output:
/// blank lines and the standard user@host:path$ / user@host:path# shell prompt.
/// The prompt is already shown in OPERATOR's live input footer.
/// Return true for PTY rows that are presentation noise rather than useful output:
/// blank lines and the standard user@host:path$ / user@host:path# shell prompt.
/// The prompt is already shown in OPERATOR's live input footer.
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
        EntryKind::HermesStreamStart => "HSTR",
        EntryKind::HermesStreamDelta => "HDEL",
        EntryKind::HermesStreamEnd => "HEND",
        EntryKind::HermesHistory => "HCTX",
        EntryKind::HermesInterim => "HINT",
        EntryKind::HermesSession => "HSES",
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
            | EntryKind::HermesStreamStart
            | EntryKind::HermesStreamDelta
            | EntryKind::HermesStreamEnd
            | EntryKind::HermesHistory
            | EntryKind::HermesInterim
            | EntryKind::HermesSession
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

fn is_hermes_journal_kind(kind: EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::HermesUser
            | EntryKind::HermesAssistant
            | EntryKind::HermesToolCall
            | EntryKind::HermesToolResult
            | EntryKind::HermesStreamStart
            | EntryKind::HermesStreamDelta
            | EntryKind::HermesStreamEnd
            | EntryKind::HermesHistory
            | EntryKind::HermesInterim
            | EntryKind::HermesSession
    )
}

fn export_plain_text_from_jsonl(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(destination)?;
    let reader = BufReader::new(File::open(source)?);
    for line in reader.lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };
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

fn read_hot_record_ids(
    path: &Path,
    cutoff: DateTime<Utc>,
) -> io::Result<(HashSet<String>, HashSet<String>)> {
    let reader = BufReader::new(File::open(path)?);
    let mut event_ids = HashSet::new();
    let mut record_ids = HashSet::new();
    for line in reader.lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };
        if !is_hot_entry_at(&entry, cutoff) {
            continue;
        }
        if let Some(event_id) = entry.event_id {
            event_ids.insert(event_id);
        }
        if let Some(record_id) = entry.record_id {
            record_ids.insert(record_id);
        }
    }
    Ok((event_ids, record_ids))
}

fn is_hot_entry(entry: &Entry) -> bool {
    is_hot_entry_at(entry, Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS))
}

fn is_hot_entry_at(entry: &Entry, cutoff: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(&entry.timestamp)
        .map(|timestamp| timestamp.with_timezone(&Utc) >= cutoff)
        // Preserve malformed legacy timestamps rather than silently hiding data.
        .unwrap_or(true)
}

/// Historical event IDs are not retained in RAM. Check the durable archive
/// only on a cache miss so retries of events older than the hot window remain
/// idempotent without an archive-sized HashSet.
fn archive_contains_event_id(path: &Path, event_id: &str) -> io::Result<bool> {
    let encoded = serde_json::to_string(event_id).map_err(io::Error::other)?;
    let file = File::open(path)?;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.contains(&encoded) {
            if serde_json::from_str::<Entry>(&line)
                .ok()
                .and_then(|entry| entry.event_id)
                .as_deref() == Some(event_id)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn read_jsonl_for_import(path: &Path) -> io::Result<(Vec<Entry>, usize)> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut skipped_corrupt_lines = 0usize;
    let mut record_ids = HashSet::new();
    let mut event_ids = HashSet::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(&line) {
            Ok(mut entry) => {
                entry.text = sanitize_entry_text(entry.kind, &entry.text);
                entry.ensure_record_id();
                if let Some(event_id) = entry.event_id.as_ref() {
                    if !event_ids.insert(event_id.clone()) {
                        continue;
                    }
                }
                if let Some(record_id) = entry.record_id.as_ref() {
                    if !record_ids.insert(record_id.clone()) {
                        continue;
                    }
                }
                entries.push(entry);
            }
            Err(_) => skipped_corrupt_lines += 1,
        }
    }

    Ok((entries, skipped_corrupt_lines))
}

fn can_merge_stream_delta(previous: &Entry, next: &Entry) -> bool {
    previous.kind == EntryKind::HermesStreamDelta
        && next.kind == EntryKind::HermesStreamDelta
        && previous.conversation_id == next.conversation_id
        && previous.turn_id == next.turn_id
        && previous.iteration == next.iteration
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
/// that could otherwise affect the OPERATOR display.
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
        | EntryKind::HermesToolResult
        | EntryKind::HermesStreamStart
        | EntryKind::HermesStreamDelta
        | EntryKind::HermesStreamEnd
        | EntryKind::HermesHistory
        | EntryKind::HermesInterim
        | EntryKind::HermesSession => sanitize_multiline(input),
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
mod hot_window_tests {
    use super::{Entry, EntryKind, Journal, HOT_WINDOW_DAYS};
    use chrono::{Duration, Utc};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
        time::{Duration as StdDuration, Instant},
    };

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_path() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "operator-hot-window-{}-{id}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn startup_keeps_only_thirty_days_but_searches_the_complete_archive() {
        let path = test_path();
        let mut old = Entry::with_kind(
            EntryKind::Note,
            "historical archive needle from outside the hot window".to_string(),
        );
        old.timestamp = (Utc::now() - Duration::days(HOT_WINDOW_DAYS + 1)).to_rfc3339();
        let recent = Entry::with_kind(EntryKind::Note, "recent live transcript row".to_string());

        let mut bytes = serde_json::to_vec(&old).expect("serialize historical record");
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(&recent).expect("serialize recent record"));
        bytes.push(b'\n');
        fs::write(&path, bytes).expect("write full archive");

        let journal = Journal::open(path.clone()).expect("open hot journal");
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].text, "recent live transcript row");
        assert_eq!(journal.total_text_bytes, "recent live transcript row".len());

        journal.search_archive(17, "historical archive needle".to_string())
            .expect("start full archive search");
        let deadline = Instant::now() + StdDuration::from_secs(3);
        let result = loop {
            if let Some(result) = journal.drain_archive_search_results().into_iter()
                .find(|result| result.request_id == 17)
            {
                break result;
            }
            assert!(Instant::now() < deadline, "archive search timed out");
            thread::sleep(StdDuration::from_millis(5));
        };
        assert!(result.error.is_none(), "archive search failed: {:?}", result.error);
        assert_eq!(result.total, 1);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].line_number, 1);
        let loaded = journal.load_archive_hit(&result.hits[0])
            .expect("load selected historical record");
        assert_eq!(loaded.text, old.text);

        drop(journal);
        assert_eq!(fs::read_to_string(&path).expect("read original archive").lines().count(), 2);
        let _ = fs::remove_file(path);
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
            "operator-{label}-{}-{id}.jsonl",
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


#[cfg(test)]
mod hermes_stream_delta_tests {
    use super::{Entry, EntryKind, Journal};
    use std::{env, fs, path::PathBuf, sync::atomic::{AtomicUsize, Ordering}};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn journal_path() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("operator-stream-{}-{id}.jsonl", std::process::id()))
    }

    fn delta(event_id: &str, text: &str) -> Entry {
        let mut entry = Entry::with_external_event(
            EntryKind::HermesStreamDelta,
            text.to_string(),
            "2026-11-07T12:00:00-05:00".to_string(),
            event_id.to_string(),
            Some("session-1".to_string()),
            Some("session-1".to_string()),
            None,
            Some("turn-1".to_string()),
            None,
            Some(0),
        );
        entry.event_id = Some(event_id.to_string());
        entry
    }

    #[test]
    fn streaming_deltas_coalesce_in_memory_but_remain_separate_durable_events() {
        let path = journal_path();
        let mut journal = Journal::open(path.clone()).expect("open journal");
        journal.append(delta("delta-1", "Hello ")).expect("append first delta");
        journal.append(delta("delta-2", "from Hermes.")).expect("append second delta");

        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].text, "Hello from Hermes.");
        assert_eq!(journal.total_text_bytes, "Hello from Hermes.".len());
        drop(journal);

        let durable_lines = fs::read_to_string(&path).expect("read journal");
        assert_eq!(durable_lines.lines().count(), 2);
        let reloaded = Journal::open(path.clone()).expect("reload journal");
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].text, "Hello from Hermes.");
        drop(reloaded);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn stream_deltas_from_different_iterations_are_not_coalesced() {
        let path = journal_path();
        let mut journal = Journal::open(path.clone()).expect("open journal");
        let mut first = delta("delta-a", "First iteration");
        first.iteration = Some(0);
        let mut second = delta("delta-b", "Second iteration");
        second.iteration = Some(1);
        journal.append(first).expect("append first iteration");
        journal.append(second).expect("append second iteration");
        assert_eq!(journal.entries.len(), 2);
        drop(journal);
        let _ = fs::remove_file(path);
    }
}


#[cfg(test)]
mod shared_journal_tests {
    use super::{Entry, EntryKind, Journal};
    use std::{fs, path::PathBuf, sync::atomic::{AtomicUsize, Ordering}};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn journal_path() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "operator-shared-journal-{}-{id}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn live_terminal_mirrors_peer_hermes_writes_from_shared_journal_without_inbox_delivery() {
        let path = journal_path();
        let mut first = Journal::open(path.clone()).expect("open first terminal journal");
        let mut second = Journal::open(path.clone()).expect("open second terminal journal");
        let event = Entry::with_external_event(
            EntryKind::HermesUser,
            "First prompt must appear in every live terminal.".to_string(),
            "2026-11-10T12:00:00-05:00".to_string(),
            "shared-journal-prompt-001".to_string(),
            Some("conversation-1".to_string()),
            Some("session-1".to_string()),
            None,
            Some("turn-1".to_string()),
            None,
            None,
        );

        first.append(event.clone()).expect("first terminal persists prompt");
        assert!(second.entries.is_empty(), "second cache starts unchanged");

        assert_eq!(
            second.refresh_external_entries().expect("poll shared journal"),
            1,
            "second terminal must import the externally appended prompt",
        );
        assert_eq!(second.entries.len(), 1);
        assert_eq!(
            second.entries[0].text,
            "First prompt must appear in every live terminal."
        );

        // A later duplicate from the second terminal's private inbox must not
        // add a second in-memory row or a second durable JSONL record.
        second.append(event).expect("deduplicate its private inbox copy");
        assert_eq!(second.entries.len(), 1);

        drop(first);
        drop(second);
        let contents = fs::read_to_string(&path).expect("read shared journal");
        assert_eq!(
            contents.lines().filter(|line| line.contains("shared-journal-prompt-001")).count(),
            1,
        );
        let _ = fs::remove_file(&path);
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn live_notes_sync_between_terminals_in_both_directions_without_mirroring_commands() {
        let path = journal_path();
        let mut first = Journal::open(path.clone()).expect("open first terminal journal");
        let mut second = Journal::open(path.clone()).expect("open second terminal journal");

        first
            .append(Entry::with_kind(
                EntryKind::TerminalCommand,
                "private shell command".to_string(),
            ))
            .expect("persist first terminal command");
        first
            .append(Entry::with_kind(
                EntryKind::Note,
                "note written in terminal one".to_string(),
            ))
            .expect("persist first terminal note");

        // The second terminal discovers the first note even when its next
        // action is another local append rather than an explicit refresh.
        second
            .append(Entry::with_kind(
                EntryKind::Note,
                "note written in terminal two".to_string(),
            ))
            .expect("persist second terminal note");

        assert_eq!(second.entries.len(), 2);
        assert_eq!(second.entries[0].kind, EntryKind::Note);
        assert_eq!(second.entries[0].text, "note written in terminal one");
        assert_eq!(second.entries[1].text, "note written in terminal two");

        // The first terminal receives the second terminal's note on its next
        // poll, and repeated polls cannot create duplicate rows.
        assert_eq!(
            first.refresh_external_entries().expect("sync second note"),
            1
        );
        assert_eq!(
            first.refresh_external_entries().expect("idempotent note poll"),
            0
        );
        assert_eq!(first.entries.len(), 3);
        assert_eq!(first.entries[2].text, "note written in terminal two");

        // Neither terminal imports another terminal's shell command as a note.
        assert_eq!(
            second.refresh_external_entries().expect("no extra shared rows"),
            0
        );

        drop(first);
        drop(second);
        let contents = fs::read_to_string(&path).expect("read shared journal");
        assert_eq!(contents.lines().count(), 3);
        assert_eq!(contents.matches("note written in terminal one").count(), 1);
        assert_eq!(contents.matches("note written in terminal two").count(), 1);
        let _ = fs::remove_file(&path);
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(lock);
    }

    #[test]
    fn same_hermes_event_imported_by_two_ssh_instances_is_persisted_once_and_cached_in_both() {
        let path = journal_path();
        let mut first = Journal::open(path.clone()).expect("open first terminal journal");
        let mut second = Journal::open(path.clone()).expect("open second terminal journal");
        let event = Entry::with_external_event(
            EntryKind::HermesUser,
            "A prompt delivered to all connected terminals.".to_string(),
            "2026-11-10T12:00:00-05:00".to_string(),
            "shared-hermes-event-001".to_string(),
            Some("conversation-1".to_string()),
            Some("session-1".to_string()),
            None,
            Some("turn-1".to_string()),
            None,
            None,
        );

        first.append(event.clone()).expect("first terminal persists event");
        second.append(event).expect("second terminal imports its own copy");

        assert_eq!(first.entries.len(), 1);
        assert_eq!(second.entries.len(), 1);
        assert_eq!(
            second.entries[0].text,
            "A prompt delivered to all connected terminals."
        );
        drop(first);
        drop(second);

        let contents = fs::read_to_string(&path).expect("read unified journal");
        assert_eq!(
            contents
                .lines()
                .filter(|line| line.contains("shared-hermes-event-001"))
                .count(),
            1,
            "both terminals must display the event while only one JSONL row is stored"
        );
        let _ = fs::remove_file(&path);
        let lock = PathBuf::from(format!("{}.lock", path.display()));
        let _ = fs::remove_file(lock);
    }
}
