use std::{
    collections::VecDeque,
    fmt,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use sqlx::{
    postgres::{PgConnection, PgListener, PgPool, PgPoolOptions},
    Row,
};
use tokio::sync::mpsc as tokio_mpsc;

use crate::storage::{sanitize_entry_text, Entry, EntryKind};

const SCHEMA: &str = include_str!("../migrations/0001_postgres_journal.sql");
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_BATCH: i64 = 5_000;
const HOT_WINDOW_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub record_id: String,
    pub kind: String,
    pub occurred_at: String,
    pub snippet: String,
    pub rank: f32,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub request_id: u64,
    pub hits: Vec<SearchHit>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub query: String,
    pub mode: String,
    pub error: Option<String>,
}

pub enum Update {
    ExternalEntries(Vec<Entry>),
    SearchResult(SearchResult),
    ArchiveRecord { request_id: u64, result: Result<Entry, String> },
    Error(String),
}

enum Command {
    Append(Entry),
    AppendBatch(Vec<Entry>),
    Search { request_id: u64, query: String },
    FetchRecord { request_id: u64, record_id: String },
    Flush(Sender<Result<(), String>>),
    ExportJsonl { path: PathBuf, reply: Sender<Result<(), String>> },
    ExportSearchResults {
        record_ids: Vec<String>,
        path: PathBuf,
        query: String,
        mode: String,
        offset: usize,
        total: usize,
        reply: Sender<Result<(), String>>,
    },
    Shutdown,
}

pub struct PostgresJournal {
    commands: tokio_mpsc::UnboundedSender<Command>,
    updates: Receiver<Update>,
    pending: Arc<AtomicUsize>,
    next_search_id: u64,
}

impl fmt::Debug for PostgresJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresJournal")
            .field("pending_writes", &self.pending_count())
            .finish_non_exhaustive()
    }
}

impl PostgresJournal {
    /// Connects to PostgreSQL, applies the idempotent schema, imports the
    /// legacy JSONL file only when the database is empty, and loads the cache.
    /// Database work after startup is handled by a dedicated Tokio worker thread.
    pub fn connect(
        database_url: String,
        legacy_path: Option<PathBuf>,
    ) -> io::Result<(Self, Vec<Entry>, usize)> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (commands_tx, commands_rx) = tokio_mpsc::unbounded_channel();
        let (updates_tx, updates_rx) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let worker_pending = Arc::clone(&pending);

        thread::Builder::new()
            .name("operator-postgres".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("create PostgreSQL runtime: {error}")));
                        return;
                    }
                };

                runtime.block_on(async move {
                    let initialized = initialize_database(&database_url, legacy_path).await;
                    let (pool, entries, imported_count, mut last_seen_id) = match initialized {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                            return;
                        }
                    };

                    let mut listener = match PgListener::connect_with(&pool).await {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!("open PostgreSQL listener: {error}")));
                            return;
                        }
                    };
                    if let Err(error) = listener.listen("operator_journal").await {
                        let _ = ready_tx.send(Err(format!("LISTEN operator_journal: {error}")));
                        return;
                    }

                    if ready_tx.send(Ok((entries, imported_count))).is_err() {
                        return;
                    }

                    let (notification_tx, notification_rx) = tokio_mpsc::unbounded_channel();
                    let notification_updates = updates_tx.clone();
                    tokio::spawn(async move {
                        loop {
                            match listener.recv().await {
                                Ok(_) => {
                                    if notification_tx.send(()).is_err() {
                                        break;
                                    }
                                }
                                Err(error) => {
                                    let _ = notification_updates.send(Update::Error(format!(
                                        "PostgreSQL notification listener disconnected; periodic synchronization remains active: {error}"
                                    )));
                                    break;
                                }
                            }
                        }
                    });

                    if let Err(error) = run_worker(
                        pool,
                        commands_rx,
                        updates_tx.clone(),
                        notification_rx,
                        &mut last_seen_id,
                        worker_pending,
                    )
                    .await
                    {
                        let _ = updates_tx.send(Update::Error(format!(
                            "PostgreSQL worker stopped: {error}"
                        )));
                    }
                });
            })?;

        let (entries, imported_count) = ready_rx
            .recv_timeout(STARTUP_TIMEOUT)
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, format!("PostgreSQL startup did not complete: {error}")))?
            .map_err(io::Error::other)?;

        Ok((
            Self {
                commands: commands_tx,
                updates: updates_rx,
                pending,
                next_search_id: 1,
            },
            entries,
            imported_count,
        ))
    }

    pub fn enqueue(&self, entry: Entry) -> io::Result<()> {
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self.commands.send(Command::Append(entry)).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PostgreSQL worker is not available",
            ));
        }
        Ok(())
    }

    pub fn enqueue_batch(&self, entries: Vec<Entry>) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.pending.fetch_add(entries.len(), Ordering::Relaxed);
        if let Err(error) = self.commands.send(Command::AppendBatch(entries)) {
            let count = match error.0 {
                Command::AppendBatch(entries) => entries.len(),
                _ => 0,
            };
            self.pending.fetch_sub(count, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PostgreSQL worker is not available",
            ));
        }
        Ok(())
    }

    pub fn search(&mut self, query: String) -> io::Result<u64> {
        let request_id = self.next_search_id;
        self.next_search_id = self.next_search_id.wrapping_add(1).max(1);
        self.commands
            .send(Command::Search { request_id, query })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PostgreSQL worker is not available"))?;
        Ok(request_id)
    }

    /// Fetch one complete historical entry on demand; only the selected
    /// payload is materialized, never the complete archive.
    pub fn fetch_record(&mut self, record_id: String) -> io::Result<u64> {
        let request_id = self.next_search_id;
        self.next_search_id = self.next_search_id.wrapping_add(1).max(1);
        self.commands
            .send(Command::FetchRecord { request_id, record_id })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PostgreSQL worker is not available"))?;
        Ok(request_id)
    }

    pub fn drain_updates(&self) -> Vec<Update> {
        self.updates.try_iter().collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    pub fn flush(&self) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::Flush(reply_tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PostgreSQL worker is not available"))?;
        reply_rx
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, format!("PostgreSQL flush timed out: {error}")))?
            .map_err(io::Error::other)
    }

    pub fn export_jsonl(&self, destination: &Path) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands
            .send(Command::ExportJsonl {
                path: destination.to_path_buf(),
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PostgreSQL worker is not available"))?;
        reply_rx
            .recv_timeout(REQUEST_TIMEOUT)
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, format!("JSONL export timed out: {error}")))?
            .map_err(io::Error::other)
    }

    /// Export only the current PostgreSQL search-results page, preserving its
    /// result order and reconstructing logical Hermes stream messages.
    pub fn export_search_results(
        &self,
        destination: &Path,
        record_ids: Vec<String>,
        query: String,
        mode: String,
        offset: usize,
        total: usize,
    ) -> io::Result<()> {
        if record_ids.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "there are no search results on the current page to export"));
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.commands.send(Command::ExportSearchResults {
            record_ids,
            path: destination.to_path_buf(),
            query,
            mode,
            offset,
            total,
            reply: reply_tx,
        }).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "PostgreSQL worker is not available"))?;
        reply_rx.recv_timeout(REQUEST_TIMEOUT)
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, format!("search-results export timed out: {error}")))?
            .map_err(io::Error::other)
    }
}

impl Drop for PostgresJournal {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

async fn initialize_database(
    database_url: &str,
    legacy_path: Option<PathBuf>,
) -> Result<(PgPool, Vec<Entry>, usize, i64), String> {
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .map_err(|error| format!("connect to PostgreSQL: {error}"))?;

    // Serialize idempotent DDL too: multiple SSH clients may launch against
    // a brand-new database at nearly the same time.
    let mut migration = pool
        .begin()
        .await
        .map_err(|error| format!("begin PostgreSQL schema migration: {error}"))?;
    acquire_writer_lock(&mut migration)
        .await
        .map_err(|error| format!("lock PostgreSQL schema migration: {error}"))?;
    for statement in SCHEMA.split(';').map(str::trim).filter(|statement| !statement.is_empty()) {
        sqlx::query(statement)
            .execute(&mut *migration)
            .await
            .map_err(|error| format!("apply PostgreSQL schema: {error}"))?;
    }
    migration
        .commit()
        .await
        .map_err(|error| format!("commit PostgreSQL schema migration: {error}"))?;

    let mut imported_count = 0usize;
    if let Some(path) = legacy_path.filter(|path| path.is_file()) {
        let mut transaction = pool
            .begin()
            .await
            .map_err(|error| format!("begin JSONL import: {error}"))?;
        acquire_writer_lock(&mut transaction)
            .await
            .map_err(|error| format!("lock JSONL import: {error}"))?;

        // Re-check after taking the cross-process writer lock. Two OPERATOR
        // instances may start simultaneously; only the first may import.
        let existing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM operator_journal_entries",
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(|error| format!("count PostgreSQL journal entries: {error}"))?;

        if existing == 0 {
            // Stream legacy JSONL one record at a time. Import memory is bounded
            // by one line rather than by the size of the complete archive.
            let file = fs::File::open(&path)
                .map_err(|error| format!("open legacy JSONL archive: {error}"))?;
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = line.map_err(|error| format!("read legacy JSONL archive: {error}"))?;
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(mut entry) = serde_json::from_str::<Entry>(&line) else {
                    continue;
                };
                entry.text = sanitize_entry_text(entry.kind, &entry.text);
                entry.ensure_record_id();
                if insert_entry(&mut transaction, &entry)
                    .await
                    .map_err(|error| format!("import JSONL record: {error}"))?
                    .is_some()
                {
                    imported_count += 1;
                }
            }

            if imported_count > 0 {
                sqlx::query("SELECT pg_notify('operator_journal', 'import')")
                    .execute(&mut *transaction)
                    .await
                    .map_err(|error| format!("notify JSONL import: {error}"))?;
            }
        }
        transaction
            .commit()
            .await
            .map_err(|error| format!("commit JSONL import: {error}"))?;
    }

    // Capture the complete database cursor and the hot rows in one statement
    // snapshot. Historical rows are not materialized in the transcript cache.
    let (entries, last_seen_id) = load_entries(&pool).await?;
    Ok((pool, entries, imported_count, last_seen_id))
}

async fn acquire_writer_lock(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), sqlx::Error> {
    // Serializing ID allocation with transaction commit makes the ID cursor
    // safe under concurrent writers: no lower ID can commit after a higher one.
    sqlx::query("SELECT pg_advisory_xact_lock(1330660690, 1246713170)")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn insert_entry(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry: &Entry,
) -> Result<Option<i64>, sqlx::Error> {
    let record_id = entry.record_id.as_deref().unwrap_or_default();
    let payload = serde_json::to_string(entry).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let occurred_at = DateTime::parse_from_rfc3339(&entry.timestamp)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let iteration = entry.iteration.map(|value| i32::try_from(value).unwrap_or(i32::MAX));

    let row = sqlx::query(
        r#"INSERT INTO operator_journal_entries
           (record_id, event_id, occurred_at, kind, body, conversation_id,
            session_id, related_session_id, turn_id, tool_call_id, iteration, payload)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb)
           ON CONFLICT DO NOTHING
           RETURNING id"#,
    )
    .bind(record_id)
    .bind(entry.event_id.as_deref())
    .bind(occurred_at.clone())
    .bind(entry_kind_name(entry.kind))
    .bind(&entry.text)
    .bind(entry.conversation_id.as_deref())
    .bind(entry.session_id.as_deref())
    .bind(entry.related_session_id.as_deref())
    .bind(entry.turn_id.as_deref())
    .bind(entry.tool_call_id.as_deref())
    .bind(iteration)
    .bind(payload)
    .fetch_optional(&mut **transaction)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    let entry_id: i64 = row.try_get("id")?;
    upsert_search_document(&mut **transaction, entry, record_id, entry_id, occurred_at).await?;
    Ok(Some(entry_id))
}

async fn upsert_search_document(
    connection: &mut PgConnection,
    entry: &Entry,
    record_id: &str,
    entry_id: i64,
    occurred_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    if matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd) {
        return Ok(());
    }

    let stream_key = entry
        .turn_id
        .as_deref()
        .filter(|_| matches!(entry.kind, EntryKind::HermesStreamDelta | EntryKind::HermesAssistant))
        .map(|turn_id| {
            let conversation = entry
                .conversation_id
                .as_deref()
                .or(entry.session_id.as_deref())
                .unwrap_or(record_id);
            format!(
                "hermes:{conversation}:{turn_id}:{}",
                entry.iteration.unwrap_or(0)
            )
        });

    let document_key = stream_key.unwrap_or_else(|| format!("entry:{record_id}"));
    if entry.kind == EntryKind::HermesStreamDelta {
        sqlx::query(
            r#"INSERT INTO operator_journal_search_documents
               (document_key, record_id, entry_id, kind, occurred_at, body)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (document_key) DO UPDATE
               SET body = operator_journal_search_documents.body || EXCLUDED.body"#,
        )
        .bind(document_key)
        .bind(record_id)
        .bind(entry_id)
        .bind(entry_kind_name(entry.kind))
        .bind(occurred_at)
        .bind(&entry.text)
        .execute(&mut *connection)
        .await?;
    } else if entry.kind == EntryKind::HermesAssistant && entry.turn_id.is_some() {
        sqlx::query(
            r#"INSERT INTO operator_journal_search_documents
               (document_key, record_id, entry_id, kind, occurred_at, body)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (document_key) DO UPDATE
               SET body = EXCLUDED.body, kind = EXCLUDED.kind,
                   occurred_at = EXCLUDED.occurred_at"#,
        )
        .bind(document_key)
        .bind(record_id)
        .bind(entry_id)
        .bind(entry_kind_name(entry.kind))
        .bind(occurred_at)
        .bind(&entry.text)
        .execute(&mut *connection)
        .await?;
    } else {
        sqlx::query(
            r#"INSERT INTO operator_journal_search_documents
               (document_key, record_id, entry_id, kind, occurred_at, body)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (document_key) DO NOTHING"#,
        )
        .bind(document_key)
        .bind(record_id)
        .bind(entry_id)
        .bind(entry_kind_name(entry.kind))
        .bind(occurred_at)
        .bind(&entry.text)
        .execute(&mut *connection)
        .await?;
    }

    Ok(())
}

async fn load_entries(pool: &PgPool) -> Result<(Vec<Entry>, i64), String> {
    let cutoff = Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS);
    let rows = sqlx::query(
        r#"WITH cursor AS (
               SELECT COALESCE(MAX(id), 0) AS max_id FROM operator_journal_entries
           ),
           recent AS (
               SELECT id, payload::text AS payload
               FROM operator_journal_entries
               WHERE occurred_at >= $1
           )
           SELECT cursor.max_id, recent.id, recent.payload
           FROM cursor LEFT JOIN recent ON TRUE
           ORDER BY recent.id NULLS LAST"#,
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load recent PostgreSQL journal: {error}"))?;

    let mut entries = Vec::with_capacity(rows.len());
    let mut last_seen_id = 0i64;
    for row in rows {
        last_seen_id = row
            .try_get("max_id")
            .map_err(|error| format!("read PostgreSQL journal cursor: {error}"))?;
        let id: Option<i64> = row
            .try_get("id")
            .map_err(|error| format!("read PostgreSQL journal ID: {error}"))?;
        let Some(_id) = id else { continue; };
        let payload: String = row
            .try_get("payload")
            .map_err(|error| format!("read PostgreSQL journal payload: {error}"))?;
        let entry = serde_json::from_str::<Entry>(&payload)
            .map_err(|error| format!("decode PostgreSQL journal payload: {error}"))?;
        entries.push(entry);
    }
    Ok((entries, last_seen_id))
}

async fn fetch_new_entries(
    pool: &PgPool,
    last_seen_id: &mut i64,
    updates: &Sender<Update>,
) -> Result<usize, String> {
    let mut total = 0usize;
    let cutoff = Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS);
    loop {
        let rows = sqlx::query(
            r#"SELECT id, occurred_at, payload::text AS payload
               FROM operator_journal_entries
               WHERE id > $1
               ORDER BY id
               LIMIT $2"#,
        )
        .bind(*last_seen_id)
        .bind(FETCH_BATCH)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("synchronize PostgreSQL journal: {error}"))?;

        if rows.is_empty() {
            break;
        }

        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row
                .try_get("id")
                .map_err(|error| format!("read PostgreSQL journal ID: {error}"))?;
            *last_seen_id = (*last_seen_id).max(id);
            let occurred_at: DateTime<Utc> = row
                .try_get("occurred_at")
                .map_err(|error| format!("read PostgreSQL journal timestamp: {error}"))?;
            if occurred_at < cutoff {
                continue;
            }
            let payload: String = row
                .try_get("payload")
                .map_err(|error| format!("read PostgreSQL journal payload: {error}"))?;
            let entry = serde_json::from_str::<Entry>(&payload)
                .map_err(|error| format!("decode PostgreSQL journal payload: {error}"))?;
            entries.push(entry);
        }

        total += entries.len();
        if !entries.is_empty() && updates.send(Update::ExternalEntries(entries)).is_err() {
            break;
        }
        if rows.len() < FETCH_BATCH as usize {
            break;
        }
    }
    Ok(total)
}

async fn fetch_new_entries_locked(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    last_seen_id: &mut i64,
    updates: &Sender<Update>,
) -> Result<usize, String> {
    let mut total = 0usize;
    let cutoff = Utc::now() - ChronoDuration::days(HOT_WINDOW_DAYS);
    loop {
        let rows = sqlx::query(
            r#"SELECT id, occurred_at, payload::text AS payload
               FROM operator_journal_entries
               WHERE id > $1
               ORDER BY id
               LIMIT $2"#,
        )
        .bind(*last_seen_id)
        .bind(FETCH_BATCH)
        .fetch_all(&mut **transaction)
        .await
        .map_err(|error| format!("synchronize PostgreSQL journal before write: {error}"))?;

        if rows.is_empty() {
            break;
        }

        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row
                .try_get("id")
                .map_err(|error| format!("read PostgreSQL journal ID: {error}"))?;
            *last_seen_id = (*last_seen_id).max(id);
            let occurred_at: DateTime<Utc> = row
                .try_get("occurred_at")
                .map_err(|error| format!("read PostgreSQL journal timestamp: {error}"))?;
            if occurred_at < cutoff {
                continue;
            }
            let payload: String = row
                .try_get("payload")
                .map_err(|error| format!("read PostgreSQL journal payload: {error}"))?;
            let entry = serde_json::from_str::<Entry>(&payload)
                .map_err(|error| format!("decode PostgreSQL journal payload: {error}"))?;
            entries.push(entry);
        }

        total += entries.len();
        if !entries.is_empty() && updates.send(Update::ExternalEntries(entries)).is_err() {
            break;
        }
        if rows.len() < FETCH_BATCH as usize {
            break;
        }
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode { Web, Literal, Phrase, TsQuery, Fuzzy }

impl SearchMode {
    fn label(self) -> &'static str {
        match self {
            Self::Web => "WEB FULL-TEXT",
            Self::Literal => "LITERAL SUBSTRING",
            Self::Phrase => "EXACT PHRASE",
            Self::TsQuery => "POSTGRESQL TSQUERY",
            Self::Fuzzy => "TRIGRAM FUZZY",
        }
    }
}

#[derive(Debug, Clone)]
struct SearchSpec {
    mode: SearchMode,
    query: String,
    kind: Option<String>,
    session: Option<String>,
    conversation: Option<String>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    limit: usize,
    offset: usize,
}

fn parse_search_date(value: &str, exclusive_upper_bound: bool) -> Result<DateTime<Utc>, String> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("invalid search date {value:?}; use YYYY-MM-DD or RFC3339"))?;
    let date = if exclusive_upper_bound {
        date.succ_opt().ok_or_else(|| format!("search date out of range: {value}"))?
    } else { date };
    Ok(date.and_hms_opt(0, 0, 0).expect("midnight is valid").and_utc())
}

/// Parse a small search language; all expressions are passed to PostgreSQL as bind values.
fn parse_search_spec(raw: &str) -> Result<SearchSpec, String> {
    let mut tokens = raw.split_whitespace().collect::<Vec<_>>();
    let mut mode = SearchMode::Web;
    while let Some(flag) = tokens.first().copied() {
        let selected = match flag {
            "--literal" => Some(SearchMode::Literal),
            "--phrase" => Some(SearchMode::Phrase),
            "--tsquery" => Some(SearchMode::TsQuery),
            "--fuzzy" => Some(SearchMode::Fuzzy),
            "--web" => Some(SearchMode::Web),
            _ => None,
        };
        let Some(selected) = selected else { break; };
        mode = selected;
        tokens.remove(0);
    }

    let mut query_tokens = Vec::new();
    let mut kind = None;
    let mut session = None;
    let mut conversation = None;
    let mut after = None;
    let mut before = None;
    let mut limit = 200usize;
    let mut offset = 0usize;
    for token in tokens {
        if let Some(value) = token.strip_prefix("kind:") {
            let value = value.to_ascii_lowercase();
            let allowed = [
                "note", "terminal_command", "terminal_output", "system", "hermes_user",
                "hermes_assistant", "hermes_tool_call", "hermes_tool_result",
                "hermes_stream_start", "hermes_stream_delta", "hermes_stream_end",
                "hermes_history", "hermes_interim", "hermes_session",
            ];
            if !allowed.contains(&value.as_str()) { return Err(format!("unknown kind filter {value:?}")); }
            kind = Some(value);
        } else if let Some(value) = token.strip_prefix("session:") {
            if value.is_empty() { return Err("session: requires an ID".to_string()); }
            session = Some(value.to_string());
        } else if let Some(value) = token.strip_prefix("conversation:") {
            if value.is_empty() { return Err("conversation: requires an ID".to_string()); }
            conversation = Some(value.to_string());
        } else if let Some(value) = token.strip_prefix("after:") {
            after = Some(parse_search_date(value, false)?);
        } else if let Some(value) = token.strip_prefix("before:") {
            before = Some(parse_search_date(value, true)?);
        } else if let Some(value) = token.strip_prefix("limit:") {
            let parsed = value.parse::<usize>().map_err(|_| format!("invalid limit {value:?}"))?;
            if parsed == 0 { return Err("limit must be between 1 and 500".to_string()); }
            limit = parsed.min(500);
        } else if let Some(value) = token.strip_prefix("offset:") {
            let parsed = value.parse::<usize>().map_err(|_| format!("invalid offset {value:?}"))?;
            offset = parsed.min(1_000_000_000);
        } else {
            query_tokens.push(token);
        }
    }

    let mut query = query_tokens.join(" ");
    if matches!(mode, SearchMode::Literal | SearchMode::Phrase | SearchMode::TsQuery | SearchMode::Fuzzy)
        && query.len() >= 2
        && ((query.starts_with('"') && query.ends_with('"'))
            || (query.starts_with('\'') && query.ends_with('\'')))
    {
        query = query[1..query.len() - 1].to_string();
    }
    if query.trim().is_empty() {
        return Err("search query is empty; use /search WORDS or /search --literal WORDS".to_string());
    }
    if let (Some(start), Some(end)) = (after.as_ref(), before.as_ref()) {
        if start >= end { return Err("after: must be earlier than before:".to_string()); }
    }
    Ok(SearchSpec { mode, query, kind, session, conversation, after, before, limit, offset })
}

async fn search_records(pool: &PgPool, raw_query: &str, request_id: u64) -> SearchResult {
    let spec = match parse_search_spec(raw_query) {
        Ok(spec) => spec,
        Err(error) => return SearchResult {
            request_id, hits: Vec::new(), total: 0, offset: 0, limit: 200,
            query: raw_query.to_string(), mode: "INVALID SEARCH".to_string(), error: Some(error),
        },
    };

    let query = &spec.query;
    let pattern = like_pattern(query);
    let common_filters = r#"
        AND ($3::text IS NULL OR d.kind = $3)
        AND ($4::text IS NULL OR EXISTS (
            SELECT 1 FROM operator_journal_entries e
            WHERE e.id = d.entry_id AND (e.session_id = $4 OR e.related_session_id = $4)
        ))
        AND ($5::text IS NULL OR EXISTS (
            SELECT 1 FROM operator_journal_entries e
            WHERE e.id = d.entry_id AND e.conversation_id = $5
        ))
        AND ($6::timestamptz IS NULL OR d.occurred_at >= $6)
        AND ($7::timestamptz IS NULL OR d.occurred_at < $7)
    "#;
    let (predicate, rank_expr, snippet_expr, order) = match spec.mode {
        SearchMode::Web => (
            "lower(d.body) LIKE lower($2) ESCAPE '\\' OR d.search_vector @@ websearch_to_tsquery('simple', $1)",
            "CASE WHEN d.search_vector @@ websearch_to_tsquery('simple', $1) THEN ts_rank_cd(d.search_vector, websearch_to_tsquery('simple', $1)) ELSE 0 END",
            "CASE WHEN d.search_vector @@ websearch_to_tsquery('simple', $1) THEN ts_headline('simple', d.body, websearch_to_tsquery('simple', $1), 'StartSel=‹,StopSel=›,MaxFragments=2,MaxWords=28,MinWords=8') ELSE substring(d.body from greatest(position(lower($1) in lower(d.body)) - 100, 1) for 260) END",
            "rank DESC, d.entry_id DESC",
        ),
        SearchMode::Literal => (
            "lower(d.body) LIKE lower($2) ESCAPE '\\'",
            "1.0::real",
            "substring(d.body from greatest(position(lower($1) in lower(d.body)) - 100, 1) for 260)",
            "d.entry_id DESC",
        ),
        SearchMode::Phrase => (
            "d.search_vector @@ phraseto_tsquery('simple', $1)",
            "ts_rank_cd(d.search_vector, phraseto_tsquery('simple', $1))",
            "ts_headline('simple', d.body, phraseto_tsquery('simple', $1), 'StartSel=‹,StopSel=›,MaxFragments=2,MaxWords=28,MinWords=8')",
            "rank DESC, d.entry_id DESC",
        ),
        SearchMode::TsQuery => (
            "d.search_vector @@ to_tsquery('simple', $1)",
            "ts_rank_cd(d.search_vector, to_tsquery('simple', $1))",
            "ts_headline('simple', d.body, to_tsquery('simple', $1), 'StartSel=‹,StopSel=›,MaxFragments=2,MaxWords=28,MinWords=8')",
            "rank DESC, d.entry_id DESC",
        ),
        SearchMode::Fuzzy => (
            // Compare the query against word-sized extents of the document.
            // Whole-document similarity (%/similarity) penalizes long journal
            // entries even when one word is an excellent typo-tolerant match.
            "lower($1) <% lower(d.body)",
            "word_similarity(lower($1), lower(d.body))",
            "substring(d.body from greatest(position(lower($1) in lower(d.body)) - 100, 1) for 260)",
            "rank DESC, d.entry_id DESC",
        ),
    };
    let sql = format!(
        r#"SELECT d.record_id, d.kind, d.occurred_at,
                  ({rank_expr})::real AS rank, {snippet_expr} AS snippet,
                  COUNT(*) OVER()::bigint AS total_count
           FROM operator_journal_search_documents d
           WHERE ({predicate}) {common_filters}
           ORDER BY {order} LIMIT $8 OFFSET $9"#
    );
    let count_sql = format!(
        "SELECT COUNT(*)::bigint FROM operator_journal_search_documents d WHERE ({predicate}) {common_filters}"
    );
    let rows = match sqlx::query(&sql)
        .bind(query)
        .bind(&pattern)
        .bind(spec.kind.as_deref())
        .bind(spec.session.as_deref())
        .bind(spec.conversation.as_deref())
        .bind(spec.after.clone())
        .bind(spec.before.clone())
        .bind(spec.limit as i64)
        .bind(spec.offset as i64)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows,
        Err(error) => return SearchResult {
            request_id, hits: Vec::new(), total: 0, offset: spec.offset, limit: spec.limit,
            query: spec.query, mode: spec.mode.label().to_string(),
            error: Some(format!("PostgreSQL {} search failed: {error}", spec.mode.label())),
        },
    };

    // COUNT(*) OVER() has no row to carry the total when OFFSET is beyond
    // the final result. Run a filtered count in that case so paging metadata
    // remains correct even for an empty out-of-range page.
    let total = if let Some(row) = rows.first() {
        row.try_get::<i64, _>("total_count").unwrap_or(0).max(0) as usize
    } else if spec.offset > 0 {
        match sqlx::query_scalar::<_, i64>(&count_sql)
            .bind(query)
            .bind(&pattern)
            .bind(spec.kind.as_deref())
            .bind(spec.session.as_deref())
            .bind(spec.conversation.as_deref())
            .bind(spec.after.clone())
            .bind(spec.before.clone())
            .fetch_one(pool)
            .await
        {
            Ok(count) => count.max(0) as usize,
            Err(error) => return SearchResult {
                request_id, hits: Vec::new(), total: 0, offset: spec.offset, limit: spec.limit,
                query: spec.query, mode: spec.mode.label().to_string(),
                error: Some(format!("count PostgreSQL search results: {error}")),
            },
        }
    } else {
        0
    };
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let decoded = (|| -> Result<SearchHit, sqlx::Error> {
            let occurred_at: DateTime<Utc> = row.try_get("occurred_at")?;
            Ok(SearchHit {
                record_id: row.try_get("record_id")?,
                kind: row.try_get("kind")?,
                occurred_at: occurred_at.to_rfc3339(),
                snippet: row.try_get("snippet")?,
                rank: row.try_get("rank")?,
            })
        })();
        match decoded {
            Ok(hit) => hits.push(hit),
            Err(error) => return SearchResult {
                request_id, hits: Vec::new(), total: 0, offset: spec.offset, limit: spec.limit,
                query: spec.query, mode: spec.mode.label().to_string(),
                error: Some(format!("decode PostgreSQL search result: {error}")),
            },
        }
    }
    SearchResult {
        request_id, hits, total, offset: spec.offset, limit: spec.limit,
        query: spec.query, mode: spec.mode.label().to_string(), error: None,
    }
}
fn like_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for character in query.chars() {
        if matches!(character, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

async fn persist_one(
    pool: &PgPool,
    entry: &Entry,
    last_seen_id: &mut i64,
    updates: &Sender<Update>,
) -> Result<(), String> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| format!("begin journal write: {error}"))?;
    acquire_writer_lock(&mut transaction)
        .await
        .map_err(|error| format!("lock journal write: {error}"))?;
    // Read all earlier committed rows while holding the writer lock, before a
    // new ID can move this instance's cursor past a concurrent commit.
    fetch_new_entries_locked(&mut transaction, last_seen_id, updates).await?;
    if let Some(id) = insert_entry(&mut transaction, entry)
        .await
        .map_err(|error| format!("write journal entry: {error}"))?
    {
        sqlx::query("SELECT pg_notify('operator_journal', 'entry')")
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("notify journal write: {error}"))?;
        *last_seen_id = (*last_seen_id).max(id);
    }
    transaction
        .commit()
        .await
        .map_err(|error| format!("commit journal write: {error}"))?;
    Ok(())
}

async fn persist_batch(
    pool: &PgPool,
    entries: &[Entry],
    last_seen_id: &mut i64,
    updates: &Sender<Update>,
) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }

    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| format!("begin journal batch: {error}"))?;
    acquire_writer_lock(&mut transaction)
        .await
        .map_err(|error| format!("lock journal batch: {error}"))?;
    fetch_new_entries_locked(&mut transaction, last_seen_id, updates).await?;
    let mut newest_id = *last_seen_id;
    let mut inserted = 0usize;

    for entry in entries {
        if let Some(id) = insert_entry(&mut transaction, entry)
            .await
            .map_err(|error| format!("write journal batch: {error}"))?
        {
            newest_id = newest_id.max(id);
            inserted += 1;
        }
    }

    if inserted > 0 {
        sqlx::query("SELECT pg_notify('operator_journal', 'batch')")
            .execute(&mut *transaction)
            .await
            .map_err(|error| format!("notify journal batch: {error}"))?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| format!("commit journal batch: {error}"))?;
    *last_seen_id = newest_id;
    Ok(())
}

async fn retry_pending(
    pool: &PgPool,
    pending: &mut VecDeque<Entry>,
    last_seen_id: &mut i64,
    pending_count: &AtomicUsize,
    updates: &Sender<Update>,
) -> Result<(), String> {
    while !pending.is_empty() {
        let batch: Vec<Entry> = pending.iter().take(500).cloned().collect();
        persist_batch(pool, &batch, last_seen_id, updates).await?;
        for _ in 0..batch.len() {
            pending.pop_front();
        }
        pending_count.fetch_sub(batch.len(), Ordering::Relaxed);
    }
    Ok(())
}

async fn run_worker(
    pool: PgPool,
    mut commands: tokio_mpsc::UnboundedReceiver<Command>,
    updates: Sender<Update>,
    mut notifications: tokio_mpsc::UnboundedReceiver<()>,
    last_seen_id: &mut i64,
    pending_count: Arc<AtomicUsize>,
) -> Result<(), String> {
    let mut pending = VecDeque::new();
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    let mut notifications_open = true;

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Command::Append(entry)) => {
                        if pending.is_empty() {
                            if let Err(error) = persist_one(&pool, &entry, last_seen_id, &updates).await {
                                pending.push_back(entry);
                                let _ = updates.send(Update::Error(error));
                            } else {
                                pending_count.fetch_sub(1, Ordering::Relaxed);
                            }
                        } else {
                            pending.push_back(entry);
                        }
                    }
                    Some(Command::AppendBatch(entries)) => {
                        pending.extend(entries);
                        if let Err(error) = retry_pending(&pool, &mut pending, last_seen_id, &pending_count, &updates).await {
                            let _ = updates.send(Update::Error(error));
                        }
                    }
                    Some(Command::Search { request_id, query }) => {
                        if let Err(error) = fetch_new_entries(&pool, last_seen_id, &updates).await {
                            let _ = updates.send(Update::Error(error));
                        }
                        let result = search_records(&pool, &query, request_id).await;
                        let _ = updates.send(Update::SearchResult(result));
                    }
                    Some(Command::FetchRecord { request_id, record_id }) => {
                        let result = async {
                            let row = sqlx::query(
                                r#"SELECT e.payload::text AS payload,
                                          d.body AS indexed_body,
                                          d.kind AS indexed_kind,
                                          d.occurred_at AS indexed_at
                                   FROM operator_journal_entries e
                                   LEFT JOIN operator_journal_search_documents d
                                     ON d.record_id = e.record_id
                                   WHERE e.record_id = $1
                                   LIMIT 1"#
                            )
                            .bind(&record_id)
                            .fetch_optional(&pool)
                            .await
                            .map_err(|error| format!("fetch historical PostgreSQL record: {error}"))?
                            .ok_or_else(|| format!("archive record {record_id} no longer exists"))?;

                            let payload: String = row.try_get("payload")
                                .map_err(|error| format!("read historical PostgreSQL payload: {error}"))?;
                            let mut entry = serde_json::from_str::<Entry>(&payload)
                                .map_err(|error| format!("decode historical PostgreSQL record: {error}"))?;

                            // A Hermes stream's search document is its logical,
                            // reconstructed message. Use that body for the
                            // selected archive result instead of returning only
                            // the first token-delta row.
                            let indexed_body: Option<String> = row.try_get("indexed_body")
                                .map_err(|error| format!("read indexed historical body: {error}"))?;
                            if let Some(body) = indexed_body {
                                entry.text = body;
                            }
                            entry.text = sanitize_entry_text(entry.kind, &entry.text);
                            let indexed_kind: Option<String> = row.try_get("indexed_kind")
                                .map_err(|error| format!("read indexed historical kind: {error}"))?;
                            if let Some(kind) = indexed_kind.as_deref().and_then(entry_kind_from_name) {
                                entry.kind = kind;
                            }
                            let indexed_at: Option<DateTime<Utc>> = row.try_get("indexed_at")
                                .map_err(|error| format!("read indexed historical timestamp: {error}"))?;
                            if let Some(timestamp) = indexed_at {
                                entry.timestamp = timestamp.to_rfc3339();
                            }
                            Ok(entry)
                        }.await;
                        let _ = updates.send(Update::ArchiveRecord { request_id, result });
                    }
                    Some(Command::Flush(reply)) => {
                        let result = retry_pending(&pool, &mut pending, last_seen_id, &pending_count, &updates).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::ExportJsonl { path, reply }) => {
                        let result = match retry_pending(&pool, &mut pending, last_seen_id, &pending_count, &updates).await {
                            Ok(()) => export_jsonl_from_database(&pool, &path).await,
                            Err(error) => Err(format!("cannot export while PostgreSQL writes are pending: {error}")),
                        };
                        let _ = reply.send(result);
                    }
                    Some(Command::ExportSearchResults { record_ids, path, query, mode, offset, total, reply }) => {
                        let result = match retry_pending(&pool, &mut pending, last_seen_id, &pending_count, &updates).await {
                            Ok(()) => export_search_results_from_database(
                                &pool, &record_ids, &path, &query, &mode, offset, total
                            ).await,
                            Err(error) => Err(format!("cannot export while PostgreSQL writes are pending: {error}")),
                        };
                        let _ = reply.send(result);
                    }
                    Some(Command::Shutdown) | None => {
                        // Make one final best-effort commit during orderly shutdown.
                        let _ = retry_pending(&pool, &mut pending, last_seen_id, &pending_count, &updates).await;
                        break;
                    },
                }
            }
            notification = notifications.recv(), if notifications_open => {
                if notification.is_some() {
                    let _ = fetch_new_entries(&pool, last_seen_id, &updates).await;
                } else {
                    notifications_open = false;
                }
            }
            _ = interval.tick() => {
                if !pending.is_empty() {
                    let _ = retry_pending(&pool, &mut pending, last_seen_id, &pending_count, &updates).await;
                }
                let _ = fetch_new_entries(&pool, last_seen_id, &updates).await;
            }
        }
    }

    Ok(())
}

async fn export_search_results_from_database(
    pool: &PgPool,
    record_ids: &[String],
    destination: &Path,
    query: &str,
    mode: &str,
    offset: usize,
    total: usize,
) -> Result<(), String> {
    let parent = destination.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("create search export directory: {error}"))?;
    let temporary = destination.with_file_name(format!(
        ".{}.{}.{}.tmp",
        destination.file_name().and_then(|name| name.to_str()).unwrap_or("operator-search-results.txt"),
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()
    ));

    let result = async {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&temporary)
            .map_err(|error| format!("create temporary search export: {error}"))?;
        let mut output = BufWriter::new(file);
        writeln!(output, "OPERATOR search results").map_err(|error| error.to_string())?;
        writeln!(output, "Query: {query}").map_err(|error| error.to_string())?;
        writeln!(output, "Mode: {mode}").map_err(|error| error.to_string())?;
        writeln!(output, "Matches: {total}").map_err(|error| error.to_string())?;
        writeln!(output, "Exported page: {} record(s), offset {offset}", record_ids.len())
            .map_err(|error| error.to_string())?;

        // Bound database result materialization to small batches while retaining
        // the ordering selected by the search page.
        for chunk in record_ids.chunks(32) {
            let rows = sqlx::query(
                r#"SELECT e.record_id, e.payload::text AS payload,
                          d.body AS indexed_body, d.kind AS indexed_kind,
                          d.occurred_at AS indexed_at
                   FROM operator_journal_entries e
                   LEFT JOIN LATERAL (
                       SELECT body, kind, occurred_at
                       FROM operator_journal_search_documents
                       WHERE record_id = e.record_id
                       LIMIT 1
                   ) d ON TRUE
                   WHERE e.record_id = ANY($1)"#
            )
            .bind(chunk.to_vec())
            .fetch_all(pool)
            .await
            .map_err(|error| format!("fetch PostgreSQL search-result records: {error}"))?;

            let mut decoded = Vec::with_capacity(rows.len());
            for row in rows {
                let record_id: String = row.try_get("record_id")
                    .map_err(|error| format!("read exported record ID: {error}"))?;
                let payload: String = row.try_get("payload")
                    .map_err(|error| format!("read exported record payload: {error}"))?;
                let mut entry = serde_json::from_str::<Entry>(&payload)
                    .map_err(|error| format!("decode exported record: {error}"))?;
                let indexed_body: Option<String> = row.try_get("indexed_body")
                    .map_err(|error| format!("read exported indexed body: {error}"))?;
                if let Some(body) = indexed_body {
                    entry.text = body;
                }
                let indexed_kind: Option<String> = row.try_get("indexed_kind")
                    .map_err(|error| format!("read exported indexed kind: {error}"))?;
                if let Some(kind) = indexed_kind.as_deref().and_then(entry_kind_from_name) {
                    entry.kind = kind;
                }
                let indexed_at: Option<DateTime<Utc>> = row.try_get("indexed_at")
                    .map_err(|error| format!("read exported indexed timestamp: {error}"))?;
                if let Some(timestamp) = indexed_at {
                    entry.timestamp = timestamp.to_rfc3339();
                }
                entry.text = sanitize_entry_text(entry.kind, &entry.text);
                decoded.push((record_id, entry));
            }

            for record_id in chunk {
                let Some((_, entry)) = decoded.iter().find(|(id, _)| id == record_id) else {
                    return Err(format!("search result record {record_id} no longer exists"));
                };
                writeln!(
                    output,
                    "\n--- Result {} · {} · {} ---",
                    offset.saturating_add(
                        record_ids.iter().position(|id| id == record_id).unwrap_or(0)
                    ).saturating_add(1),
                    entry.timestamp,
                    entry_kind_name(entry.kind)
                ).map_err(|error| error.to_string())?;
                output.write_all(entry.text.as_bytes()).map_err(|error| error.to_string())?;
                if !entry.text.ends_with('\n') {
                    output.write_all(b"\n").map_err(|error| error.to_string())?;
                }
            }
        }

        output.flush().map_err(|error| format!("flush search export: {error}"))?;
        let file = output.into_inner().map_err(|error| format!("finish search export: {}", error.into_error()))?;
        file.sync_all().map_err(|error| format!("sync search export: {error}"))?;
        fs::rename(&temporary, destination)
            .map_err(|error| format!("replace search export destination: {error}"))?;
        Ok(())
    }.await;

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

async fn export_jsonl_from_database(pool: &PgPool, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("create export directory: {error}"))?;

    let temporary = destination.with_file_name(format!(
        ".{}.{}.tmp",
        destination.file_name().and_then(|name| name.to_str()).unwrap_or("journal.jsonl"),
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(&temporary)
        .map_err(|error| format!("create JSONL export: {error}"))?;

    // Page through the complete archive; never fetch every payload into RAM.
    let mut last_id = 0i64;
    loop {
        let rows = sqlx::query(
            r#"SELECT id, payload::text AS payload
               FROM operator_journal_entries
               WHERE id > $1
               ORDER BY id
               LIMIT $2"#,
        )
        .bind(last_id)
        .bind(FETCH_BATCH)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("read journal for JSONL export: {error}"))?;

        if rows.is_empty() {
            break;
        }
        for row in &rows {
            last_id = row
                .try_get("id")
                .map_err(|error| format!("read JSONL export cursor: {error}"))?;
            let payload: String = row
                .try_get("payload")
                .map_err(|error| format!("read JSONL payload: {error}"))?;
            output
                .write_all(payload.as_bytes())
                .and_then(|_| output.write_all(b"\n"))
                .map_err(|error| format!("write JSONL export: {error}"))?;
        }
        if rows.len() < FETCH_BATCH as usize {
            break;
        }
    }

    output
        .sync_all()
        .map_err(|error| format!("sync JSONL export: {error}"))?;
    drop(output);
    fs::rename(&temporary, destination).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("publish JSONL export: {error}")
    })?;
    Ok(())
}

fn entry_kind_from_name(kind: &str) -> Option<EntryKind> {
    Some(match kind {
        "note" => EntryKind::Note,
        "terminal_command" => EntryKind::TerminalCommand,
        "terminal_output" => EntryKind::TerminalOutput,
        "system" => EntryKind::System,
        "hermes_user" => EntryKind::HermesUser,
        "hermes_assistant" => EntryKind::HermesAssistant,
        "hermes_tool_call" => EntryKind::HermesToolCall,
        "hermes_tool_result" => EntryKind::HermesToolResult,
        "hermes_stream_start" => EntryKind::HermesStreamStart,
        "hermes_stream_delta" => EntryKind::HermesStreamDelta,
        "hermes_stream_end" => EntryKind::HermesStreamEnd,
        "hermes_history" => EntryKind::HermesHistory,
        "hermes_interim" => EntryKind::HermesInterim,
        "hermes_session" => EntryKind::HermesSession,
        _ => return None,
    })
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
    use super::{entry_kind_name, like_pattern, PostgresJournal, Update};
    use crate::storage::{Entry, EntryKind};
    use std::{thread, time::Duration};

    #[test]
    fn substring_search_escapes_sql_wildcards() {
        assert_eq!(like_pattern("100%_ready\\now"), "%100\\%\\_ready\\\\now%");
    }

    #[test]
    fn postgres_index_and_live_notifications() {
        let Ok(database_url) = std::env::var("OPERATOR_TEST_DATABASE_URL") else {
            return;
        };

        let searchable_text = format!("operator-postgres-smoke-{} phrase", std::process::id());
        let entry = Entry::with_kind(EntryKind::Note, searchable_text.clone());
        let record_id = entry.record_id.clone().expect("new entry record ID");

        let (writer, _, _) = PostgresJournal::connect(database_url.clone(), None)
            .expect("connect PostgreSQL writer");
        let (mut reader, _, _) = PostgresJournal::connect(database_url, None)
            .expect("connect PostgreSQL reader");

        writer.enqueue(entry).expect("queue PostgreSQL note");
        writer.flush().expect("commit PostgreSQL note");

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut received_notification = false;
        while std::time::Instant::now() < deadline && !received_notification {
            for update in reader.drain_updates() {
                if let Update::ExternalEntries(entries) = update {
                    received_notification |= entries.iter().any(|entry| {
                        entry.record_id.as_deref() == Some(record_id.as_str())
                    });
                }
            }
            if !received_notification {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(received_notification, "peer instance did not receive the committed journal record");

        let request_id = reader.search(searchable_text.clone()).expect("submit indexed search");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut found_record = false;
        while std::time::Instant::now() < deadline && !found_record {
            for update in reader.drain_updates() {
                if let Update::SearchResult(result) = update {
                    if result.request_id == request_id {
                        found_record = result.hits.iter().any(|hit| hit.record_id == record_id);
                    }
                }
            }
            if !found_record {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(found_record, "PostgreSQL trigram/full-text index did not return the record");


        // An empty page beyond the end must still report the real total.
        let request_id = reader
            .search(format!("{searchable_text} kind:note limit:1 offset:999999"))
            .expect("submit out-of-range page");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut verified_empty_page = false;
        while std::time::Instant::now() < deadline && !verified_empty_page {
            for update in reader.drain_updates() {
                if let Update::SearchResult(result) = update {
                    if result.request_id == request_id {
                        assert!(result.error.is_none(), "out-of-range page count failed: {:?}", result.error);
                        assert!(result.hits.is_empty());
                        assert!(result.total >= 1);
                        verified_empty_page = true;
                    }
                }
            }
            if !verified_empty_page { thread::sleep(Duration::from_millis(20)); }
        }
        assert!(verified_empty_page, "out-of-range page count timed out");
        for query in [
            format!("--literal {searchable_text}"),
            format!("--phrase \"{searchable_text}\""),
            "--tsquery 'operator & postgres'".to_string(),
            format!("--fuzzy {searchable_text}"),
            format!("{searchable_text} kind:note limit:25 offset:0"),
        ] {
            let request_id = reader.search(query.clone()).expect("submit PostgreSQL search mode");
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            let mut completed = false;
            while std::time::Instant::now() < deadline && !completed {
                for update in reader.drain_updates() {
                    if let Update::SearchResult(result) = update {
                        if result.request_id == request_id {
                            assert!(result.error.is_none(), "search {query:?} failed: {:?}", result.error);
                            assert!(
                                result.hits.iter().any(|hit| hit.record_id == record_id),
                                "PostgreSQL search mode {query:?} did not return its fixture"
                            );
                            assert!(result.total >= 1);
                            completed = true;
                        }
                    }
                }
                if !completed { thread::sleep(Duration::from_millis(20)); }
            }
            assert!(completed, "PostgreSQL search mode {query:?} timed out");
        }

        // Regression: fuzzy search must match a misspelled word inside a
        // long transcript document, not compare the query to the entire body.
        let fuzzy_text = format!(
            "Client 192.168.50.54 connected to interactive shell (/bin/bash); commands are recorded once; PTY input echo is disabled. {}",
            "additional transcript context ".repeat(12)
        );
        let fuzzy_entry = Entry::with_kind(EntryKind::Note, fuzzy_text);
        let fuzzy_record_id = fuzzy_entry.record_id.clone().expect("fuzzy fixture record ID");
        writer.enqueue(fuzzy_entry).expect("queue fuzzy-search fixture");
        writer.flush().expect("commit fuzzy-search fixture");

        let request_id = reader
            .search("--fuzzy conected kind:note".to_string())
            .expect("submit misspelled word-similarity search");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut found_fuzzy_record = false;
        while std::time::Instant::now() < deadline && !found_fuzzy_record {
            for update in reader.drain_updates() {
                if let Update::SearchResult(result) = update {
                    if result.request_id == request_id {
                        assert!(
                            result.error.is_none(),
                            "fuzzy search failed: {:?}",
                            result.error
                        );
                        found_fuzzy_record = result.hits.iter().any(|hit| {
                            hit.record_id == fuzzy_record_id && hit.rank > 0.0
                        });
                    }
                }
            }
            if !found_fuzzy_record {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(
            found_fuzzy_record,
            "fuzzy search did not find 'connected' from the misspelling 'conected' in a long document"
        );

        // The logical search document must reassemble deltas so a substring
        // split across adjacent Hermes stream events remains searchable.
        let mut first_delta = Entry::with_kind(
            EntryKind::HermesStreamDelta,
            "multi-part-search-".to_string(),
        );
        first_delta.event_id = Some(format!("operator-pg-stream-a-{}", std::process::id()));
        first_delta.conversation_id = Some("operator-pg-smoke-conversation".to_string());
        first_delta.session_id = Some("operator-pg-smoke-session".to_string());
        first_delta.turn_id = Some(format!("operator-pg-smoke-turn-{}", std::process::id()));
        first_delta.iteration = Some(0);
        let first_delta_record = first_delta.record_id.clone().expect("first delta record ID");

        let mut second_delta = Entry::with_kind(
            EntryKind::HermesStreamDelta,
            "token-crosses-boundary".to_string(),
        );
        second_delta.event_id = Some(format!("operator-pg-stream-b-{}", std::process::id()));
        second_delta.conversation_id = first_delta.conversation_id.clone();
        second_delta.session_id = first_delta.session_id.clone();
        second_delta.turn_id = first_delta.turn_id.clone();
        second_delta.iteration = Some(0);

        writer
            .enqueue_batch(vec![first_delta, second_delta])
            .expect("queue Hermes stream deltas");
        writer.flush().expect("commit Hermes stream deltas");

        let request_id = reader
            .search("search-token-crosses".to_string())
            .expect("submit cross-delta search");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut found_split_stream = false;
        while std::time::Instant::now() < deadline && !found_split_stream {
            for update in reader.drain_updates() {
                if let Update::SearchResult(result) = update {
                    if result.request_id == request_id {
                        found_split_stream = result.hits.iter().any(|hit| hit.record_id == first_delta_record);
                    }
                }
            }
            if !found_split_stream {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(found_split_stream, "PostgreSQL search did not reconstruct Hermes stream deltas");

        // The indexed logical body is also what the historical record viewer
        // retrieves; it must not return only the first raw delta payload.
        let request_id = reader
            .fetch_record(first_delta_record.clone())
            .expect("request reconstructed Hermes archive record");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut loaded_stream = false;
        while std::time::Instant::now() < deadline && !loaded_stream {
            for update in reader.drain_updates() {
                if let Update::ArchiveRecord { request_id: result_id, result } = update {
                    if result_id == request_id {
                        let entry = result.expect("fetch reconstructed Hermes record");
                        assert_eq!(entry.record_id.as_deref(), Some(first_delta_record.as_str()));
                        assert_eq!(entry.text, "multi-part-search-token-crosses-boundary");
                        loaded_stream = true;
                    }
                }
            }
            if !loaded_stream {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(loaded_stream, "reconstructed PostgreSQL record fetch timed out");

        let stream_export_path = std::env::temp_dir()
            .join(format!("operator-stream-search-export-{}.txt", std::process::id()));
        reader.export_search_results(
            &stream_export_path,
            vec![first_delta_record],
            "search-token-crosses".to_string(),
            "LITERAL".to_string(),
            0,
            1,
        ).expect("export reconstructed Hermes search result");
        let exported_stream = std::fs::read_to_string(&stream_export_path)
            .expect("read reconstructed Hermes search export");
        assert!(exported_stream.contains("multi-part-search-token-crosses-boundary"));
        let _ = std::fs::remove_file(stream_export_path);
    }

    #[test]
    fn postgres_fetches_full_archive_record_on_demand() {
        let Ok(database_url) = std::env::var("OPERATOR_TEST_DATABASE_URL") else {
            return;
        };

        let text = format!("historical full-record payload {}", std::process::id());
        let entry = Entry::with_kind(EntryKind::Note, text.clone());
        let record_id = entry.record_id.clone().expect("record ID");
        let (writer, _, _) = PostgresJournal::connect(database_url.clone(), None)
            .expect("connect PostgreSQL writer");
        let (mut reader, _, _) = PostgresJournal::connect(database_url, None)
            .expect("connect PostgreSQL reader");

        writer.enqueue(entry).expect("queue historical record");
        writer.flush().expect("commit historical record");
        let request_id = reader.fetch_record(record_id.clone()).expect("request full record");

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut loaded = false;
        while std::time::Instant::now() < deadline && !loaded {
            for update in reader.drain_updates() {
                if let Update::ArchiveRecord { request_id: result_id, result } = update {
                    if result_id == request_id {
                        let entry = result.expect("fetch historical record");
                        assert_eq!(entry.record_id.as_deref(), Some(record_id.as_str()));
                        assert_eq!(entry.text, text);
                        loaded = true;
                    }
                }
            }
            if !loaded {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(loaded, "full PostgreSQL archive record fetch timed out");

        let unrelated = format!("unselected export record {}", std::process::id());
        let unrelated_entry = Entry::with_kind(EntryKind::Note, unrelated.clone());
        writer.enqueue(unrelated_entry).expect("queue unrelated export fixture");
        writer.flush().expect("commit unrelated export fixture");

        let export_path = std::env::temp_dir()
            .join(format!("operator-search-export-{}.txt", std::process::id()));
        reader.export_search_results(
            &export_path,
            vec![record_id],
            "historical".to_string(),
            "LITERAL".to_string(),
            0,
            2,
        ).expect("export selected PostgreSQL search result");
        let exported = std::fs::read_to_string(&export_path).expect("read PostgreSQL search export");
        assert!(exported.contains("historical full-record payload"));
        assert!(!exported.contains(&unrelated));
        let _ = std::fs::remove_file(export_path);
    }

    #[test]
    fn search_parser_supports_modes_filters_and_pagination() {
        use super::{parse_search_spec, SearchMode};

        let web = parse_search_spec(r#""connection refused" OR timeout kind:terminal_output after:2026-11-01 limit:40 offset:80"#)
            .expect("parse web search");
        assert_eq!(web.mode, SearchMode::Web);
        assert_eq!(web.query, r#""connection refused" OR timeout"#);
        assert_eq!(web.kind.as_deref(), Some("terminal_output"));
        assert_eq!(web.limit, 40);
        assert_eq!(web.offset, 80);
        assert!(web.after.is_some());

        let literal = parse_search_spec("--literal \"100% ready\" kind:note")
            .expect("parse literal search");
        assert_eq!(literal.mode, SearchMode::Literal);
        assert_eq!(literal.query, "100% ready");

        let phrase = parse_search_spec("--phrase \"connection refused\"")
            .expect("parse phrase search");
        assert_eq!(phrase.mode, SearchMode::Phrase);
        assert_eq!(phrase.query, "connection refused");

        let tsquery = parse_search_spec("--tsquery 'connection & !refused'")
            .expect("parse PostgreSQL tsquery");
        assert_eq!(tsquery.mode, SearchMode::TsQuery);
        assert_eq!(tsquery.query, "connection & !refused");

        let fuzzy = parse_search_spec("--fuzzy opertor").expect("parse fuzzy search");
        assert_eq!(fuzzy.mode, SearchMode::Fuzzy);
        assert_eq!(fuzzy.query, "opertor");
    }

    #[test]
    fn search_parser_rejects_invalid_filters_and_date_ranges() {
        use super::parse_search_spec;
        assert!(parse_search_spec("needle kind:not_a_kind").is_err());
        assert!(parse_search_spec("needle after:not-a-date").is_err());
        assert!(parse_search_spec("needle after:2026-11-10 before:2026-11-01").is_err());
        assert!(parse_search_spec("needle limit:0").is_err());
    }

    #[test]
    fn database_kind_names_preserve_the_json_enum_spelling() {
        assert_eq!(entry_kind_name(EntryKind::TerminalCommand), "terminal_command");
        assert_eq!(entry_kind_name(EntryKind::HermesStreamDelta), "hermes_stream_delta");
    }
}
