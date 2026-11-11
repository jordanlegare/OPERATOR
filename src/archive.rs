//! Bounded-memory search over the complete append-only JSONL archive.
//!
//! The live transcript keeps only its hot window in memory. This worker scans
//! the durable file in order and retains only one page of lightweight byte-range
//! hits, so archive size does not determine the search result memory footprint.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use crate::storage::{Entry, EntryKind};

const DEFAULT_PAGE_LIMIT: usize = 200;
const MAX_PAGE_LIMIT: usize = 500;
const MAX_PAGE_OFFSET: usize = 10_000;
const MAX_ARCHIVE_RECORD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ArchiveSearchHit {
    pub record_id: String,
    pub kind: String,
    pub occurred_at: String,
    pub snippet: String,
    pub line_number: usize,
    pub start_offset: u64,
    pub end_offset: u64,
}

#[derive(Debug, Clone)]
pub struct ArchiveSearchResult {
    pub request_id: u64,
    pub query: String,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub hits: Vec<ArchiveSearchHit>,
    pub error: Option<String>,
}

struct SearchRequest {
    request_id: u64,
    query: String,
}

pub struct ArchiveSearcher {
    requests: Sender<SearchRequest>,
    results: Receiver<ArchiveSearchResult>,
    path: PathBuf,
}

impl ArchiveSearcher {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        let (request_tx, request_rx) = mpsc::channel::<SearchRequest>();
        let (result_tx, result_rx) = mpsc::channel::<ArchiveSearchResult>();
        let worker_path = path.clone();

        thread::Builder::new()
            .name("operator-archive-search".to_string())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let result = search_file(&worker_path, request.request_id, &request.query);
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })?;

        Ok(Self {
            requests: request_tx,
            results: result_rx,
            path,
        })
    }

    pub fn search(&self, request_id: u64, query: String) -> io::Result<()> {
        self.requests
            .send(SearchRequest { request_id, query })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "archive search worker stopped"))
    }

    pub fn drain_results(&self) -> Vec<ArchiveSearchResult> {
        self.results.try_iter().collect()
    }

    /// Load a selected result only when the operator opens it. The search
    /// workspace itself stores byte ranges and short snippets, not full bodies.
    pub fn read_hit(&self, hit: &ArchiveSearchHit) -> io::Result<Entry> {
        let length = hit.end_offset.checked_sub(hit.start_offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "archive search returned an invalid byte range")
        })?;
        if length > MAX_ARCHIVE_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "selected archive record exceeds the 64 MiB viewer limit",
            ));
        }

        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(hit.start_offset))?;
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(length).read_to_end(&mut bytes)?;

        let mut combined: Option<Entry> = None;
        for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let Ok(mut entry) = serde_json::from_slice::<Entry>(line) else {
                continue;
            };
            if let Some(previous) = combined.as_mut() {
                if can_merge_stream_delta(previous, &entry) {
                    previous.text.push_str(&entry.text);
                    continue;
                }
            }
            if combined.is_none() {
                combined = Some(entry);
            } else {
                break;
            }
        }

        combined.ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidData,
            "could not decode the selected archive record",
        ))
    }
}

fn search_file(path: &Path, request_id: u64, raw_query: &str) -> ArchiveSearchResult {
    let (needle, offset, limit) = match parse_query(raw_query) {
        Ok(parsed) => parsed,
        Err(error) => return ArchiveSearchResult {
            request_id,
            query: raw_query.to_string(),
            total: 0,
            offset: 0,
            limit: DEFAULT_PAGE_LIMIT,
            hits: Vec::new(),
            error: Some(error),
        },
    };

    let mut result = ArchiveSearchResult {
        request_id,
        query: needle.clone(),
        total: 0,
        offset,
        limit,
        hits: Vec::new(),
        error: None,
    };

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            result.error = Some(format!("open JSONL archive: {error}"));
            return result;
        }
    };
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut byte_offset = 0u64;
    let mut line_number = 0usize;
    let mut recent_hits = VecDeque::with_capacity(offset.saturating_add(limit));
    let mut pending_delta: Option<(Entry, u64, u64, usize)> = None;

    loop {
        buffer.clear();
        let start_offset = byte_offset;
        let count = match reader.read_until(b'\n', &mut buffer) {
            Ok(count) => count,
            Err(error) => {
                result.error = Some(format!("read JSONL archive: {error}"));
                return result;
            }
        };
        if count == 0 {
            break;
        }
        byte_offset = byte_offset.saturating_add(count as u64);
        line_number = line_number.saturating_add(1);

        // A crash can leave a partial final append. Ignore that incomplete row.
        if !buffer.ends_with(b"\n") {
            break;
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(&buffer) else {
            continue;
        };

        if entry.kind == EntryKind::HermesStreamDelta {
            match pending_delta.as_mut() {
                Some((previous, _, end, _)) if can_merge_stream_delta(previous, &entry) => {
                    previous.text.push_str(&entry.text);
                    *end = byte_offset;
                    continue;
                }
                _ => {
                    if let Some((previous, start, end, first_line)) = pending_delta.take() {
                        add_match(
                            previous,
                            start,
                            end,
                            first_line,
                            &needle,
                            &mut result.total,
                            &mut recent_hits,
                            offset.saturating_add(limit),
                        );
                    }
                    pending_delta = Some((entry, start_offset, byte_offset, line_number));
                    continue;
                }
            }
        }

        if let Some((previous, start, end, first_line)) = pending_delta.take() {
            add_match(
                previous,
                start,
                end,
                first_line,
                &needle,
                &mut result.total,
                &mut recent_hits,
                offset.saturating_add(limit),
            );
        }

        if !matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd) {
            add_match(
                entry,
                start_offset,
                byte_offset,
                line_number,
                &needle,
                &mut result.total,
                &mut recent_hits,
                offset.saturating_add(limit),
            );
        }
    }

    if let Some((previous, start, end, first_line)) = pending_delta {
        add_match(
            previous,
            start,
            end,
            first_line,
            &needle,
            &mut result.total,
            &mut recent_hits,
            offset.saturating_add(limit),
        );
    }

    // Newest matches appear first. Retain only offset + limit lightweight hits,
    // regardless of how many matching rows exist in the archive.
    result.hits = recent_hits
        .into_iter()
        .rev()
        .skip(offset)
        .take(limit)
        .collect();
    result
}

fn add_match(
    entry: Entry,
    start_offset: u64,
    end_offset: u64,
    line_number: usize,
    needle: &str,
    total: &mut usize,
    recent_hits: &mut VecDeque<ArchiveSearchHit>,
    keep: usize,
) {
    if !entry.text.to_lowercase().contains(needle) {
        return;
    }

    *total = total.saturating_add(1);
    let record_id = entry.record_id.clone().unwrap_or_else(|| format!("archive-line-{line_number}"));
    let hit = ArchiveSearchHit {
        record_id,
        kind: entry_kind_name(entry.kind).to_string(),
        occurred_at: entry.timestamp,
        snippet: entry.text.chars().take(320).collect(),
        line_number,
        start_offset,
        end_offset,
    };

    if keep == 0 {
        return;
    }
    if recent_hits.len() == keep {
        recent_hits.pop_front();
    }
    recent_hits.push_back(hit);
}

fn parse_query(raw_query: &str) -> Result<(String, usize, usize), String> {
    let mut terms = Vec::new();
    let mut limit = DEFAULT_PAGE_LIMIT;
    let mut offset = 0usize;

    for token in raw_query.split_whitespace() {
        if token == "--literal" || token == "--web" {
            continue;
        }
        if let Some(value) = token.strip_prefix("limit:") {
            let parsed = value.parse::<usize>().map_err(|_| format!("invalid limit {value:?}"))?;
            if parsed == 0 {
                return Err("limit must be between 1 and 500".to_string());
            }
            limit = parsed.min(MAX_PAGE_LIMIT);
        } else if let Some(value) = token.strip_prefix("offset:") {
            let parsed = value.parse::<usize>().map_err(|_| format!("invalid offset {value:?}"))?;
            offset = parsed.min(MAX_PAGE_OFFSET);
        } else if token.contains(':') && token.split_once(':').is_some_and(|(key, _)| {
            matches!(key, "kind" | "session" | "conversation" | "after" | "before")
        }) {
            // These structured filters are currently provided by PostgreSQL.
            continue;
        } else {
            terms.push(token);
        }
    }

    let needle = terms.join(" ").trim_matches('"').trim_matches('\'').to_string();
    if needle.is_empty() {
        return Err("enter search terms; local archive search supports literal substrings".to_string());
    }
    Ok((needle, offset, limit))
}

fn can_merge_stream_delta(previous: &Entry, next: &Entry) -> bool {
    previous.kind == EntryKind::HermesStreamDelta
        && next.kind == EntryKind::HermesStreamDelta
        && previous.conversation_id == next.conversation_id
        && previous.turn_id == next.turn_id
        && previous.iteration == next.iteration
}

fn entry_kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Note => "note",
        EntryKind::TerminalCommand => "terminal_command",
        EntryKind::TerminalOutput => "terminal_output",
        EntryKind::System => "system",
        EntryKind::HermesUser => "hermes_user",
        EntryKind::HermesAssistant => "hermes_assistant",
        EntryKind::HermesToolCall => "hermes_tool_call",
        EntryKind::HermesToolResult => "hermes_tool_result",
        EntryKind::HermesStreamStart => "hermes_stream_start",
        EntryKind::HermesStreamDelta => "hermes_stream_delta",
        EntryKind::HermesStreamEnd => "hermes_stream_end",
        EntryKind::HermesHistory => "hermes_history",
        EntryKind::HermesInterim => "hermes_interim",
        EntryKind::HermesSession => "hermes_session",
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveSearcher;
    use crate::storage::{Entry, EntryKind};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_path() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "operator-archive-search-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create archive test directory");
        root.join("journal.jsonl")
    }

    fn wait_for_result(searcher: &ArchiveSearcher, request_id: u64) -> super::ArchiveSearchResult {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Some(result) = searcher.drain_results().into_iter().find(|r| r.request_id == request_id) {
                return result;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("archive search timed out");
    }

    #[test]
    fn searches_old_records_without_loading_them_into_the_hot_cache() {
        let path = test_path();
        let old = Entry::with_kind(EntryKind::Note, "historical needle from last year".to_string());
        let mut json = serde_json::to_vec(&old).expect("serialize old record");
        json.push(b'\n');
        fs::write(&path, json).expect("write archive");
        let searcher = ArchiveSearcher::new(path.clone()).expect("start search worker");

        searcher.search(1, "needle".to_string()).expect("queue archive search");
        let result = wait_for_result(&searcher, 1);
        assert_eq!(result.total, 1);
        assert_eq!(result.hits.len(), 1);
        assert!(result.hits[0].line_number == 1);
        let loaded = searcher.read_hit(&result.hits[0]).expect("read selected archive entry");
        assert_eq!(loaded.text, old.text);

        drop(searcher);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn searches_across_adjacent_hermes_stream_deltas() {
        let path = test_path();
        let mut first = Entry::with_kind(EntryKind::HermesStreamDelta, "archive-cross-".to_string());
        first.conversation_id = Some("conversation".to_string());
        first.turn_id = Some("turn".to_string());
        first.iteration = Some(0);
        let mut second = Entry::with_kind(EntryKind::HermesStreamDelta, "delta-needle".to_string());
        second.conversation_id = first.conversation_id.clone();
        second.turn_id = first.turn_id.clone();
        second.iteration = first.iteration;
        let mut bytes = serde_json::to_vec(&first).expect("serialize first delta");
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(&second).expect("serialize second delta"));
        bytes.push(b'\n');
        fs::write(&path, bytes).expect("write archive");
        let searcher = ArchiveSearcher::new(path.clone()).expect("start search worker");

        searcher.search(2, "cross-delta".to_string()).expect("queue archive search");
        let result = wait_for_result(&searcher, 2);
        assert_eq!(result.total, 1);
        assert_eq!(result.hits.len(), 1);
        let loaded = searcher.read_hit(&result.hits[0]).expect("read merged archive entry");
        assert_eq!(loaded.text, "archive-cross-delta-needle");

        drop(searcher);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn returns_newest_results_first_and_supports_paging() {
        let path = test_path();
        let mut bytes = Vec::new();
        for index in 0..6 {
            let entry = Entry::with_kind(EntryKind::Note, format!("pageable archive hit {index}"));
            bytes.extend(serde_json::to_vec(&entry).expect("serialize record"));
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).expect("write archive");
        let searcher = ArchiveSearcher::new(path.clone()).expect("start search worker");

        searcher.search(3, "pageable limit:2 offset:2".to_string()).expect("queue paged search");
        let result = wait_for_result(&searcher, 3);
        assert_eq!(result.total, 6);
        assert_eq!(result.hits.len(), 2);
        assert!(result.hits[0].snippet.contains("3"));
        assert!(result.hits[1].snippet.contains("2"));

        drop(searcher);
        let _ = fs::remove_file(path);
    }
}
