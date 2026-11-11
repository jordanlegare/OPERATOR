use std::{
    collections::VecDeque,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use sqlx::{
    postgres::{PgConnection, PgListener, PgPool, PgPoolOptions},
    Row,
};
use tokio::sync::mpsc as tokio_mpsc;

use crate::storage::{Entry, EntryKind};

const SCHEMA: &str = include_str!("../migrations/0001_postgres_journal.sql");
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_BATCH: i64 = 5_000;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub request_id: u64,
    pub record_ids: Vec<String>,
}

pub enum Update {
    ExternalEntries(Vec<Entry>),
    SearchResult(SearchResult),
    Error(String),
}

enum Command {
    Append(Entry),
    AppendBatch(Vec<Entry>),
    Search { request_id: u64, query: String },
    Flush(Sender<Result<(), String>>),
    ExportJsonl { path: PathBuf, reply: Sender<Result<(), String>> },
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
        legacy_entries: Vec<Entry>,
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
                    let initialized = initialize_database(&database_url, legacy_entries).await;
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
}

impl Drop for PostgresJournal {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

async fn initialize_database(
    database_url: &str,
    mut legacy_entries: Vec<Entry>,
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
    if !legacy_entries.is_empty() {
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
            for entry in &mut legacy_entries {
                entry.ensure_record_id();
                if insert_entry(&mut transaction, entry)
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

    // Load the initial rows and cursor in one statement snapshot. A separate
    // MAX(id) query could advance past a concurrent commit not present in the
    // loaded cache, causing that record to be missed by the catch-up poll.
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
    let rows = sqlx::query(
        "SELECT id, payload::text AS payload FROM operator_journal_entries ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load PostgreSQL journal: {error}"))?;

    let mut entries = Vec::with_capacity(rows.len());
    let mut last_seen_id = 0i64;
    for row in rows {
        let id: i64 = row
            .try_get("id")
            .map_err(|error| format!("read PostgreSQL journal ID: {error}"))?;
        let payload: String = row
            .try_get("payload")
            .map_err(|error| format!("read PostgreSQL journal payload: {error}"))?;
        let entry = serde_json::from_str::<Entry>(&payload)
            .map_err(|error| format!("decode PostgreSQL journal payload: {error}"))?;
        last_seen_id = last_seen_id.max(id);
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
    loop {
        let rows = sqlx::query(
            r#"SELECT id, payload::text AS payload
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
            let payload: String = row
                .try_get("payload")
                .map_err(|error| format!("read PostgreSQL journal payload: {error}"))?;
            let entry = serde_json::from_str::<Entry>(&payload)
                .map_err(|error| format!("decode PostgreSQL journal payload: {error}"))?;
            *last_seen_id = (*last_seen_id).max(id);
            entries.push(entry);
        }

        total += entries.len();
        if updates.send(Update::ExternalEntries(entries)).is_err() {
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
    loop {
        let rows = sqlx::query(
            r#"SELECT id, payload::text AS payload
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
            let payload: String = row
                .try_get("payload")
                .map_err(|error| format!("read PostgreSQL journal payload: {error}"))?;
            let entry = serde_json::from_str::<Entry>(&payload)
                .map_err(|error| format!("decode PostgreSQL journal payload: {error}"))?;
            *last_seen_id = (*last_seen_id).max(id);
            entries.push(entry);
        }

        total += entries.len();
        if updates.send(Update::ExternalEntries(entries)).is_err() {
            break;
        }
        if rows.len() < FETCH_BATCH as usize {
            break;
        }
    }
    Ok(total)
}

async fn search_records(pool: &PgPool, query: &str) -> Result<Vec<String>, String> {
    let pattern = like_pattern(query);
    let rows = sqlx::query(
        r#"SELECT record_id
           FROM operator_journal_search_documents
           WHERE lower(body) LIKE lower($1) ESCAPE '\'
              OR search_vector @@ websearch_to_tsquery('simple', $2)
           ORDER BY entry_id"#,
    )
    .bind(pattern)
    .bind(query)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("search PostgreSQL journal: {error}"))?;

    rows.into_iter()
        .map(|row| row.try_get("record_id").map_err(|error| error.to_string()))
        .collect()
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
                        match search_records(&pool, &query).await {
                            Ok(record_ids) => {
                                let _ = updates.send(Update::SearchResult(SearchResult { request_id, record_ids }));
                            }
                            Err(error) => {
                                let _ = updates.send(Update::Error(error));
                            }
                        }
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

async fn export_jsonl_from_database(pool: &PgPool, destination: &Path) -> Result<(), String> {
    let rows = sqlx::query(
        "SELECT payload::text AS payload FROM operator_journal_entries ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("read journal for JSONL export: {error}"))?;

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

    for row in rows {
        let payload: String = row
            .try_get("payload")
            .map_err(|error| format!("read JSONL payload: {error}"))?;
        output
            .write_all(payload.as_bytes())
            .and_then(|_| output.write_all(b"\n"))
            .map_err(|error| format!("write JSONL export: {error}"))?;
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

        let searchable_text = format!("operator-postgres-smoke-{}", std::process::id());
        let entry = Entry::with_kind(EntryKind::Note, searchable_text.clone());
        let record_id = entry.record_id.clone().expect("new entry record ID");

        let (writer, _, _) = PostgresJournal::connect(database_url.clone(), Vec::new())
            .expect("connect PostgreSQL writer");
        let (mut reader, _, _) = PostgresJournal::connect(database_url, Vec::new())
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

        let request_id = reader.search(searchable_text).expect("submit indexed search");
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        let mut found_record = false;
        while std::time::Instant::now() < deadline && !found_record {
            for update in reader.drain_updates() {
                if let Update::SearchResult(result) = update {
                    if result.request_id == request_id {
                        found_record = result.record_ids.iter().any(|id| id == &record_id);
                    }
                }
            }
            if !found_record {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(found_record, "PostgreSQL trigram/full-text index did not return the record");

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
                        found_split_stream = result.record_ids.iter().any(|id| id == &first_delta_record);
                    }
                }
            }
            if !found_split_stream {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(found_split_stream, "PostgreSQL search did not reconstruct Hermes stream deltas");
    }

    #[test]
    fn database_kind_names_preserve_the_json_enum_spelling() {
        assert_eq!(entry_kind_name(EntryKind::TerminalCommand), "terminal_command");
        assert_eq!(entry_kind_name(EntryKind::HermesStreamDelta), "hermes_stream_delta");
    }
}
