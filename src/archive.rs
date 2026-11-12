//! Bounded-memory search over the complete append-only JSONL archive.
//!
//! The live transcript keeps only its hot window in memory. This worker scans
//! the durable file in order and retains only one page of lightweight byte-range
//! hits, so archive size does not determine the search result memory footprint.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::storage::{Entry, EntryKind};

const DEFAULT_PAGE_LIMIT: usize = 200;
const MAX_PAGE_LIMIT: usize = 500;
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

#[derive(Debug)]
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
            let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
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

        let entry = combined.ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidData,
            "could not decode the selected archive record",
        ))?;
        // The JSONL file is append-only by contract, but it can still be
        // replaced or truncated by an external process. Never show an
        // unrelated row when an old byte range no longer identifies its hit.
        if !hit.record_id.starts_with("archive-line-")
            && entry.record_id.as_deref() != Some(hit.record_id.as_str())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive changed after search; run the search again",
            ));
        }
        Ok(entry)
    }

    /// Export the full records represented by the currently displayed local
    /// archive-search page. The durable JSONL source is never modified.
    pub fn export_hits(
        &self,
        destination: &Path,
        query: &str,
        mode: &str,
        total: usize,
        offset: usize,
        hits: &[ArchiveSearchHit],
    ) -> io::Result<()> {
        if hits.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "there are no search results on the current page to export",
            ));
        }
        if let Some(parent) = destination.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }

        let file_name = destination.file_name().and_then(|name| name.to_str()).unwrap_or("search-results.txt");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = destination.with_file_name(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            nonce
        ));

        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&temporary)?;
            let mut output = BufWriter::new(file);
            writeln!(output, "OPERATOR search results")?;
            writeln!(output, "Query: {query}")?;
            writeln!(output, "Mode: {mode}")?;
            writeln!(output, "Matches: {total}")?;
            writeln!(output, "Exported page: {} record(s), offset {offset}", hits.len())?;

            for (position, hit) in hits.iter().enumerate() {
                let entry = self.read_hit(hit)?;
                writeln!(
                    output,
                    "\n--- Result {} · {} · {} · JSONL line {} ---",
                    offset.saturating_add(position).saturating_add(1),
                    entry.timestamp,
                    entry_kind_name(entry.kind),
                    hit.line_number
                )?;
                output.write_all(entry.text.as_bytes())?;
                if !entry.text.ends_with('\n') {
                    output.write_all(b"\n")?;
                }
            }

            output.flush()?;
            let file = output.into_inner().map_err(|error| error.into_error())?;
            file.sync_all()?;
            fs::rename(&temporary, destination)?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
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

    let archive_len = match File::open(path).and_then(|file| file.metadata()) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            result.error = Some(format!("open JSONL archive: {error}"));
            return result;
        }
    };

    // First pass counts the full result set without retaining hit records.
    // Second pass captures only the requested page, so even offset:100000000
    // consumes O(limit) result memory rather than O(offset + limit).
    let total = match scan_archive(path, archive_len, &needle, None) {
        Ok((total, _)) => total,
        Err(error) => {
            result.error = Some(error);
            return result;
        }
    };
    // Normalize the query once before scanning. Stored text is normalized
    // per record in collect_match, so both sides use the same case semantics.
    let needle = needle.to_lowercase();
    let page_start = total.saturating_sub(offset.saturating_add(limit));
    let page_end = total.saturating_sub(offset);
    let (_, mut hits) = match scan_archive(path, archive_len, &needle, Some((page_start, page_end))) {
        Ok(scanned) => scanned,
        Err(error) => {
            result.error = Some(error);
            return result;
        }
    };

    result.total = total;
    result.hits = hits.drain(..).rev().collect();
    result
}

/// Scan a stable byte-length snapshot of the append-only archive. Match indices
/// are chronological; an optional half-open range limits retained page hits.
fn scan_archive(
    path: &Path,
    archive_len: u64,
    needle: &str,
    capture_range: Option<(usize, usize)>,
) -> Result<(usize, Vec<ArchiveSearchHit>), String> {
    let file = File::open(path).map_err(|error| format!("open JSONL archive: {error}"))?;
    let mut reader = BufReader::new(file).take(archive_len);
    let mut buffer = Vec::new();
    let mut byte_offset = 0u64;
    let mut line_number = 0usize;
    let mut matched_count = 0usize;
    let mut hits = Vec::with_capacity(
        capture_range.map(|(start, end)| end.saturating_sub(start)).unwrap_or(0),
    );
    let mut pending_delta: Option<(Entry, u64, u64, usize)> = None;

    loop {
        buffer.clear();
        let start_offset = byte_offset;
        let count = reader.read_until(b'\n', &mut buffer)
            .map_err(|error| format!("read JSONL archive: {error}"))?;
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
                        collect_match(
                            previous, start, end, first_line, needle,
                            capture_range, &mut matched_count, &mut hits,
                        );
                    }
                    pending_delta = Some((entry, start_offset, byte_offset, line_number));
                    continue;
                }
            }
        }

        if let Some((previous, start, end, first_line)) = pending_delta.take() {
            collect_match(
                previous, start, end, first_line, needle,
                capture_range, &mut matched_count, &mut hits,
            );
        }

        if !matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd) {
            collect_match(
                entry, start_offset, byte_offset, line_number, needle,
                capture_range, &mut matched_count, &mut hits,
            );
        }
    }

    if let Some((previous, start, end, first_line)) = pending_delta {
        collect_match(
            previous, start, end, first_line, needle,
            capture_range, &mut matched_count, &mut hits,
        );
    }

    Ok((matched_count, hits))
}

fn collect_match(
    entry: Entry,
    start_offset: u64,
    end_offset: u64,
    line_number: usize,
    needle: &str,
    capture_range: Option<(usize, usize)>,
    matched_count: &mut usize,
    hits: &mut Vec<ArchiveSearchHit>,
) {
    if !entry.text.to_lowercase().contains(needle) {
        return;
    }

    let match_index = *matched_count;
    *matched_count = matched_count.saturating_add(1);
    if !capture_range.is_some_and(|(start, end)| match_index >= start && match_index < end) {
        return;
    }

    let record_id = entry.record_id.clone().unwrap_or_else(|| format!("archive-line-{line_number}"));
    hits.push(ArchiveSearchHit {
        record_id,
        kind: entry_kind_name(entry.kind).to_string(),
        occurred_at: entry.timestamp,
        snippet: entry.text.chars().take(320).collect(),
        line_number,
        start_offset,
        end_offset,
    });
}

fn parse_query(raw_query: &str) -> Result<(String, usize, usize), String> {
    let mut terms = Vec::new();
    let mut limit = DEFAULT_PAGE_LIMIT;
    let mut offset = 0usize;

    for token in raw_query.split_whitespace() {
        match token {
            "--literal" => continue,
            "--web" | "--phrase" | "--tsquery" | "--fuzzy" => {
                return Err(format!(
                    "{token} search mode requires the PostgreSQL backend; local JSONL archive search supports literal substrings"
                ));
            }
            _ => {}
        }
        if let Some(value) = token.strip_prefix("limit:") {
            let parsed = value.parse::<usize>().map_err(|_| format!("invalid limit {value:?}"))?;
            if parsed == 0 {
                return Err("limit must be between 1 and 500".to_string());
            }
            limit = parsed.min(MAX_PAGE_LIMIT);
        } else if let Some(value) = token.strip_prefix("offset:") {
            offset = value.parse::<usize>().map_err(|_| format!("invalid offset {value:?}"))?;
        } else if token.split_once(':').is_some_and(|(key, _)| {
            matches!(key, "kind" | "session" | "conversation" | "after" | "before")
        }) {
            return Err(format!(
                "filter {token:?} requires the PostgreSQL backend; remove the filter for a full literal archive scan"
            ));
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
        time::Duration,
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
    fn huge_offsets_do_not_require_offset_sized_hit_buffers() {
        let path = test_path();
        let mut bytes = Vec::new();
        for index in 0..8 {
            let entry = Entry::with_kind(EntryKind::Note, format!("offset archive match {index}"));
            bytes.extend(serde_json::to_vec(&entry).expect("serialize record"));
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).expect("write archive");
        let searcher = ArchiveSearcher::new(path.clone()).expect("start search worker");

        searcher.search(4, "offset limit:2 offset:1000000000".to_string())
            .expect("queue distant archive page");
        let result = wait_for_result(&searcher, 4);
        assert_eq!(result.total, 8);
        assert!(result.hits.is_empty());

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
