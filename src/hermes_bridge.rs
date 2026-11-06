//! Import bridge for Hermes CLI shell-hook events.
//!
//! Hermes hooks write one atomically-renamed JSON file per event. STREENOTE
//! imports those files through Journal::append so the normal in-memory view,
//! append-only JSONL file, and retry queue remain authoritative.

use std::{
    fs,
    io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Local, SecondsFormat};
use serde::Deserialize;

use crate::storage::{Entry, EntryKind, Journal};

const MAX_EVENT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EVENTS_PER_POLL: usize = 64;

#[derive(Debug)]
pub struct HermesBridge {
    inbox: PathBuf,
}

#[derive(Debug, Deserialize)]
struct InboxEvent {
    version: u8,
    event_id: String,
    kind: EntryKind,
    timestamp: String,
    text: String,
}

impl Default for HermesBridge {
    fn default() -> Self {
        Self {
            inbox: default_inbox_path(),
        }
    }
}

impl HermesBridge {
    pub fn new(inbox: PathBuf) -> Self {
        Self { inbox }
    }

    /// Import queued events in filename order. A file is removed only after
    /// Journal::append confirms that the event is already durable or has been
    /// safely deduplicated against a previously durable event.
    pub fn drain(&mut self, journal: &mut Journal) -> io::Result<usize> {
        let ready = self.inbox.join("ready");
        if !ready.exists() {
            return Ok(0);
        }

        let failed = self.inbox.join("failed");
        create_private_dir(&failed)?;

        let mut paths = fs::read_dir(&ready)?
            .filter_map(Result::ok)
            .filter_map(|item| {
                let is_file = item.file_type().ok()?.is_file();
                let is_json = item.path().extension().is_some_and(|ext| ext == "json");
                (is_file && is_json).then_some(item.path())
            })
            .collect::<Vec<_>>();
        paths.sort();

        let mut imported = 0usize;
        for path in paths.into_iter().take(MAX_EVENTS_PER_POLL) {
            let metadata = fs::metadata(&path)?;
            if metadata.len() > MAX_EVENT_BYTES {
                quarantine(&path, &failed)?;
                continue;
            }

            let bytes = fs::read(&path)?;
            let event = match serde_json::from_slice::<InboxEvent>(&bytes) {
                Ok(event) => event,
                Err(_) => {
                    quarantine(&path, &failed)?;
                    continue;
                }
            };

            if event.version != 1
                || event.event_id.is_empty()
                || event.event_id.len() > 128
                || event.text.is_empty()
                || event.text.len() > MAX_EVENT_BYTES as usize
                || !matches!(event.kind, EntryKind::HermesUser | EntryKind::HermesAssistant)
            {
                quarantine(&path, &failed)?;
                continue;
            }

            let timestamp = if DateTime::parse_from_rfc3339(&event.timestamp).is_ok() {
                event.timestamp
            } else {
                Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
            };
            let entry = Entry::with_external_event(
                event.kind,
                event.text,
                timestamp,
                event.event_id,
            );

            // If the journal cannot flush this entry, leave the inbox file in
            // place. A later poll retries it; event IDs prevent duplicate rows.
            journal.append(entry)?;
            fs::remove_file(&path)?;
            imported += 1;
        }

        Ok(imported)
    }
}

/// Environment variable shared by STREENOTE-managed PTYs and Hermes hook adapters.
///
/// STREENOTE injects this variable into every shell PTY it creates. Processes
/// launched from those shells inherit the same inbox location automatically.
pub const HERMES_INBOX_ENV: &str = "STREENOTE_HERMES_INBOX";

/// Resolve the inbox path used by both the journal importer and child PTYs.
///
/// An explicit absolute STREENOTE_HERMES_INBOX takes precedence, followed by
/// XDG_DATA_HOME and then HOME. The resolved path is always absolute so child
/// processes do not depend on their current working directory.
pub fn default_inbox_path() -> PathBuf {
    resolve_inbox_path(
        std::env::var_os(HERMES_INBOX_ENV).map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn resolve_inbox_path(
    explicit: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = explicit.filter(|path| path.is_absolute()) {
        return path;
    }

    let data_root = xdg_data_home
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".local/share"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local/share")
        });

    data_root.join("streenote").join("hermes-inbox")
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn quarantine(path: &Path, failed_dir: &Path) -> io::Result<()> {
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::other("Hermes inbox event has no filename"))?;
    let mut destination = failed_dir.join(filename);
    if destination.exists() {
        destination.set_extension(format!("invalid-{}", std::process::id()));
    }
    fs::rename(path, destination)
}

#[cfg(test)]
mod tests {
    use super::HermesBridge;
    use crate::storage::{EntryKind, Journal};
    use std::{env, fs, path::PathBuf, sync::atomic::{AtomicUsize, Ordering}};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_root() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "streenote-hermes-bridge-{}-{id}",
            std::process::id()
        ))
    }

    fn write_event(path: &PathBuf, event_id: &str, kind: &str, text: &str) {
        fs::create_dir_all(path).expect("create ready directory");
        let json = serde_json::json!({
            "version": 1,
            "event_id": event_id,
            "kind": kind,
            "timestamp": "2026-11-05T22:00:00-05:00",
            "text": text
        });
        fs::write(
            path.join(format!("00000000000000000001-{event_id}.json")),
            serde_json::to_vec(&json).expect("serialize event"),
        ).expect("write event");
    }

    #[test]
    fn explicit_absolute_inbox_override_takes_precedence() {
        let resolved = super::resolve_inbox_path(
            Some(PathBuf::from("/srv/streenote/custom-inbox")),
            Some(PathBuf::from("/xdg/data")),
            Some(PathBuf::from("/home/test")),
        );
        assert_eq!(resolved, PathBuf::from("/srv/streenote/custom-inbox"));
    }

    #[test]
    fn xdg_data_home_is_used_when_no_explicit_override_exists() {
        let resolved = super::resolve_inbox_path(
            None,
            Some(PathBuf::from("/xdg/data")),
            Some(PathBuf::from("/home/test")),
        );
        assert_eq!(resolved, PathBuf::from("/xdg/data/streenote/hermes-inbox"));
    }

    #[test]
    fn home_is_used_when_xdg_data_home_is_unavailable_or_relative() {
        let resolved = super::resolve_inbox_path(
            Some(PathBuf::from("relative/inbox")),
            Some(PathBuf::from("relative/xdg")),
            Some(PathBuf::from("/home/test")),
        );
        assert_eq!(
            resolved,
            PathBuf::from("/home/test/.local/share/streenote/hermes-inbox")
        );
    }

    #[test]
    fn imports_multiline_hermes_messages_into_the_existing_journal() {
        let root = test_root();
        let ready = root.join("ready");
        let journal_path = root.join("journal.jsonl");
        write_event(&ready, "turn-a-user", "hermes_user", "Explain this.\n\n- First\n- Second");

        let mut journal = Journal::open(journal_path.clone()).expect("open journal");
        let mut bridge = HermesBridge::new(root.clone());
        assert_eq!(bridge.drain(&mut journal).expect("drain inbox"), 1);
        assert_eq!(journal.entries.len(), 1);
        assert_eq!(journal.entries[0].kind, EntryKind::HermesUser);
        assert_eq!(journal.entries[0].text, "Explain this.\n\n- First\n- Second");
        assert_eq!(journal.entries[0].event_id.as_deref(), Some("turn-a-user"));
        assert_eq!(fs::read_dir(&ready).expect("read ready").count(), 0);

        drop(journal);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retrying_an_already_imported_event_does_not_duplicate_the_timeline() {
        let root = test_root();
        let ready = root.join("ready");
        let journal_path = root.join("journal.jsonl");
        write_event(&ready, "turn-b-assistant", "hermes_assistant", "A useful answer.");

        let mut journal = Journal::open(journal_path.clone()).expect("open journal");
        let mut bridge = HermesBridge::new(root.clone());
        assert_eq!(bridge.drain(&mut journal).expect("first drain"), 1);
        write_event(&ready, "turn-b-assistant", "hermes_assistant", "A useful answer.");
        assert_eq!(bridge.drain(&mut journal).expect("duplicate drain"), 1);
        assert_eq!(journal.entries.len(), 1);

        drop(journal);
        let reloaded = Journal::open(journal_path).expect("reload journal");
        assert_eq!(reloaded.entries.len(), 1);
        drop(reloaded);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_events_are_quarantined_instead_of_retried_forever() {
        let root = test_root();
        let ready = root.join("ready");
        fs::create_dir_all(&ready).expect("create ready directory");
        fs::write(ready.join("broken.json"), b"not-json").expect("write malformed event");

        let mut journal = Journal::open(root.join("journal.jsonl")).expect("open journal");
        let mut bridge = HermesBridge::new(root.clone());
        assert_eq!(bridge.drain(&mut journal).expect("drain malformed event"), 0);
        assert!(root.join("failed/broken.json").exists());

        drop(journal);
        let _ = fs::remove_dir_all(root);
    }
}
