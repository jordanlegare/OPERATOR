//! Private Unix-socket handoff for Hermes final-answer review.
//!
//! Hermes' transform_llm_output plugin connects to this listener and blocks
//! until the STREENOTE operator approves, edits, or withholds the response.
//! The listener is deliberately separate from the append-only capture inbox.

use std::{
    env,
    fs,
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub const MAX_REVIEW_BYTES: usize = 8 * 1024 * 1024;
const DECISION_TIMEOUT: Duration = Duration::from_secs(540);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

fn diagnostic_log_path() -> PathBuf {
    if let Some(path) = env::var_os("STREENOTE_HERMES_REVIEW_LOG").map(PathBuf::from).filter(|p| p.is_absolute()) { return path; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::geteuid() };
        let runtime_dir = PathBuf::from(format!("/run/user/{uid}"));
        if fs::metadata(&runtime_dir).ok().is_some_and(|m| m.is_dir() && m.uid() == uid) {
            return runtime_dir.join("streenote").join("hermes-review.log");
        }
    }
    let state_root = env::var_os("XDG_STATE_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(PathBuf::from).map(|p| p.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    state_root.join("streenote").join("hermes-review.log")
}

fn append_diagnostic_event(event: &str, details: serde_json::Value) {
    use std::io::Write;
    let path = diagnostic_log_path();
    let Some(parent) = path.parent() else { return; };
    if fs::create_dir_all(parent).is_err() { return; }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true).write(true).mode(0o600);
        if let Ok(mut file) = options.open(&path) {
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
            let record = serde_json::json!({"event": event, "timestamp_unix": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), "details": details});
            if let Ok(mut bytes) = serde_json::to_vec(&record) { bytes.push(b'\n'); let _ = file.write_all(&bytes); }
        }
    }
    #[cfg(not(unix))]
    {
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let record = serde_json::json!({"event": event, "timestamp_unix": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), "details": details});
            if let Ok(mut bytes) = serde_json::to_vec(&record) { bytes.push(b'\n'); let _ = file.write_all(&bytes); }
        }
    }
}

/// Summarize real plugin/listener activity for `/review status`.
pub fn diagnostics_summary() -> String {
    let path = diagnostic_log_path();
    let Ok(contents) = fs::read_to_string(&path) else { return format!("No Hermes plugin diagnostics yet; expected log: {}", path.display()); };
    let records: Vec<serde_json::Value> = contents.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    if records.is_empty() { return format!("Hermes diagnostics log is empty: {}", path.display()); }
    let latest = |name: &str| records.iter().rev().find(|r| r.get("event").and_then(|v| v.as_str()) == Some(name));
    let val = |name: &str| latest(name).map(|r| r.get("timestamp").and_then(|v| v.as_str()).unwrap_or("recorded")).unwrap_or("never");
    let request = latest("REVIEW_REQUEST_RECEIVED").map(|r| r.get("timestamp_unix").map(|v| v.to_string()).unwrap_or_else(|| "recorded".into())).unwrap_or_else(|| "never".into());
    let decision = latest("REVIEW_DECISION").and_then(|r| r.get("details")).and_then(|d| d.get("decision")).and_then(|v| v.as_str()).unwrap_or("none");
    format!("plugin registered: {}; callback: {}; socket connected: {}; STREENOTE request: {}; last decision: {}; log: {}", val("PLUGIN_REGISTERED"), val("CALLBACK_INVOKED"), val("SOCKET_CONNECT_OK"), request, decision, path.display())
}


#[derive(Debug)]
pub struct ReviewRequest {
    pub request_id: String,
    pub session_id: String,
    pub model: String,
    pub platform: String,
    pub response_text: String,
    pub expires_at: Instant,
    pub reply: SyncSender<ReviewDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    Replace(String),
    Reject,
}

#[derive(Debug, Deserialize)]
struct WireRequest {
    version: u8,
    request_id: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    platform: String,
    response_text: String,
}

#[derive(Debug, Serialize)]
struct WireResponse {
    version: u8,
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub struct HermesReviewServer {
    receiver: Receiver<ReviewRequest>,
    enabled: Arc<AtomicBool>,
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for HermesReviewServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HermesReviewServer")
            .field("socket_path", &self.socket_path)
            .field("enabled", &self.enabled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl HermesReviewServer {
    pub fn start() -> io::Result<Self> {
        Self::start_at(default_review_socket_path())
    }

    #[cfg(unix)]
    fn start_at(socket_path: PathBuf) -> io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        use std::os::unix::net::{UnixListener, UnixStream};

        let parent = socket_path
            .parent()
            .ok_or_else(|| io::Error::other("review socket path has no parent directory"))?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

        // Recover a socket file left behind by an unclean STREENOTE exit, but
        // never unlink a socket that belongs to another live STREENOTE process.
        match fs::symlink_metadata(&socket_path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                if UnixStream::connect(&socket_path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "another STREENOTE review listener is already active at {}",
                            socket_path.display()
                        ),
                    ));
                }
                fs::remove_file(&socket_path)?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "review socket path exists and is not a Unix socket: {}",
                        socket_path.display()
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let (sender, receiver) = mpsc::channel();
        let enabled = Arc::new(AtomicBool::new(false));
        let worker_enabled = Arc::clone(&enabled);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("streenote-hermes-review".to_string())
            .spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Each accepted client waits on its own thread. The
                            // accept loop can keep polling, and listener shutdown
                            // never waits for an operator to decide a pending answer.
                            let connection_sender = sender.clone();
                            let connection_enabled = Arc::clone(&worker_enabled);
                            let connection_stop = Arc::clone(&worker_stop);
                            let _ = thread::Builder::new()
                                .name("streenote-review-client".to_string())
                                .spawn(move || {
                                    if let Err(error) = handle_connection(
                                        stream,
                                        &connection_sender,
                                        &connection_enabled,
                                        &connection_stop,
                                    ) {
                                        if !connection_stop.load(Ordering::Relaxed)
                                            && error.kind() != io::ErrorKind::WouldBlock
                                            && error.kind() != io::ErrorKind::TimedOut
                                        {
                                            eprintln!("STREENOTE Hermes review: {error}");
                                        }
                                    }
                                });
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25));
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            eprintln!("STREENOTE Hermes review listener stopped: {error}");
                            break;
                        }
                    }
                }
            })?;

        Ok(Self {
            receiver,
            enabled,
            socket_path,
            stop,
            worker: Some(worker),
        })
    }

    #[cfg(not(unix))]
    fn start_at(_socket_path: PathBuf) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Hermes answer review currently requires a Unix-domain socket",
        ))
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn try_recv(&self) -> Option<ReviewRequest> {
        self.receiver.try_recv().ok()
    }

    #[cfg(test)]
    fn start_at_for_test(socket_path: PathBuf) -> io::Result<Self> {
        Self::start_at(socket_path)
    }
}

impl Drop for HermesReviewServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        #[cfg(unix)]
        {
            // Wake the nonblocking accept loop so it can observe the stop flag.
            let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
        }

        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// The gateway process resolves the same path independently of the PTY
/// environment. An explicit override is available for non-standard layouts.
pub fn default_review_socket_path() -> PathBuf {
    // A user service and an interactive PTY can have different HOME and
    // XDG_DATA_HOME values. /run/user/<uid> is stable across those process
    // environments and is private to the same Unix account.
    #[cfg(unix)]
    let runtime_dir = {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::geteuid() };
        let root = PathBuf::from(format!("/run/user/{uid}"));
        fs::metadata(&root)
            .ok()
            .filter(|metadata| metadata.is_dir() && metadata.uid() == uid)
            .map(|_| root)
    };
    #[cfg(not(unix))]
    let runtime_dir: Option<PathBuf> = None;

    resolve_review_socket_path(
        env::var_os("STREENOTE_HERMES_REVIEW_SOCKET").map(PathBuf::from),
        runtime_dir,
        env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
    )
}

fn resolve_review_socket_path(
    explicit: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = explicit.filter(|path| path.is_absolute()) {
        return path;
    }

    if let Some(runtime_dir) = runtime_dir.filter(|path| path.is_absolute()) {
        return runtime_dir.join("streenote").join("review.sock");
    }

    let data_root = xdg_data_home
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".local/share"))
        })
        .unwrap_or_else(|| {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .join(".local/share")
        });

    data_root.join("streenote").join("review.sock")
}

#[cfg(unix)]
fn handle_connection(
    mut stream: std::os::unix::net::UnixStream,
    sender: &Sender<ReviewRequest>,
    enabled: &AtomicBool,
    stop: &AtomicBool,
) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Read};

    stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))?;

    let reader = BufReader::new(stream.try_clone()?);
    let mut limited_reader = reader.take((MAX_REVIEW_BYTES + 1) as u64);
    let mut line = Vec::with_capacity(8192);
    let read = limited_reader.read_until(b'\n', &mut line)?;
    if stop.load(Ordering::Relaxed) && read == 0 {
        return Ok(());
    }
    if read == 0 || line.len() > MAX_REVIEW_BYTES || !line.ends_with(b"\n") {
        return write_wire_response(
            &mut stream,
            WireResponse {
                version: 1,
                decision: "reject",
                response_text: None,
                error: Some("invalid or oversized STREENOTE review request".to_string()),
            },
        );
    }

    let request: WireRequest = match serde_json::from_slice(&line) {
        Ok(request) => request,
        Err(error) => {
            return write_wire_response(
                &mut stream,
                WireResponse {
                    version: 1,
                    decision: "reject",
                    response_text: None,
                    error: Some(format!("invalid review request: {error}")),
                },
            );
        }
    };

    if request.version != 1
        || request.request_id.is_empty()
        || request.request_id.len() > 128
        || request.session_id.len() > 1024
        || request.model.len() > 1024
        || request.platform.len() > 128
        || request.response_text.is_empty()
        || request.response_text.len() > MAX_REVIEW_BYTES
    {
        return write_wire_response(
            &mut stream,
            WireResponse {
                version: 1,
                decision: "reject",
                response_text: None,
                error: Some("review request failed validation".to_string()),
            },
        );
    }

    append_diagnostic_event("REVIEW_REQUEST_RECEIVED", serde_json::json!({
        "request_id": request.request_id,
        "platform": request.platform,
        "model": request.model,
    }));

    // A missing STREENOTE toggle must never surprise the operator with a
    // blocking prompt. The plugin remains loaded, but the original response
    // passes through immediately until /review on is selected.
    if !enabled.load(Ordering::Relaxed) {
        append_diagnostic_event("REVIEW_BYPASS", serde_json::json!({
            "request_id": request.request_id,
            "reason": "review mode is OFF",
        }));
        return write_wire_response(
            &mut stream,
            WireResponse {
                version: 1,
                decision: "bypass",
                response_text: None,
                error: None,
            },
        );
    }

    let (reply, decision) = mpsc::sync_channel(1);
    let pending = ReviewRequest {
        request_id: request.request_id,
        session_id: request.session_id,
        model: request.model,
        platform: request.platform,
        response_text: request.response_text,
        expires_at: Instant::now() + DECISION_TIMEOUT,
        reply,
    };

    if sender.send(pending).is_err() {
        return write_wire_response(
            &mut stream,
            WireResponse {
                version: 1,
                decision: "reject",
                response_text: None,
                error: Some("STREENOTE review interface is unavailable".to_string()),
            },
        );
    }

    let decision = match decision.recv_timeout(DECISION_TIMEOUT) {
        Ok(decision) => decision,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            ReviewDecision::Reject
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => ReviewDecision::Reject,
    };

    let decision_label = match &decision {
        ReviewDecision::Approve => "approve",
        ReviewDecision::Replace(_) => "replace",
        ReviewDecision::Reject => "reject",
    };
    append_diagnostic_event("REVIEW_DECISION", serde_json::json!({"decision": decision_label}));

    let response = match decision {
        ReviewDecision::Approve => WireResponse {            version: 1,
            decision: "approve",
            response_text: None,
            error: None,
        },
        ReviewDecision::Replace(text)
            if !text.trim().is_empty() && text.len() <= MAX_REVIEW_BYTES =>
        {
            WireResponse {
                version: 1,
                decision: "replace",
                response_text: Some(text),
                error: None,
            }
        }
        ReviewDecision::Replace(_) => WireResponse {
            version: 1,
            decision: "reject",
            response_text: None,
            error: Some("STREENOTE supplied an empty or oversized replacement".to_string()),
        },
        ReviewDecision::Reject => WireResponse {
            version: 1,
            decision: "reject",
            response_text: None,
            error: Some("operator rejected the Hermes response".to_string()),
        },
    };

    write_wire_response(&mut stream, response)
}

#[cfg(unix)]
fn write_wire_response(
    stream: &mut std::os::unix::net::UnixStream,
    response: WireResponse,
) -> io::Result<()> {
    use std::io::Write;

    let mut bytes = serde_json::to_vec(&response)
        .map_err(|error| io::Error::other(format!("serialize review response: {error}")))?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_review_socket_path, HermesReviewServer, ReviewDecision, MAX_REVIEW_BYTES,
    };
    use serde_json::json;
    use std::{
        env, fs,
        io::{BufRead, BufReader, Write},
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
        time::Duration,
    };

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_root() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "streenote-hermes-review-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn resolves_review_socket_under_shared_runtime_directory_before_home_data() {
        assert_eq!(
            resolve_review_socket_path(
                None,
                Some(PathBuf::from("/run/user/1000")),
                Some(PathBuf::from("/different/xdg")),
                Some(PathBuf::from("/different/home")),
            ),
            PathBuf::from("/run/user/1000/streenote/review.sock")
        );
        assert_eq!(
            resolve_review_socket_path(
                None,
                None,
                Some(PathBuf::from("/xdg/data")),
                Some(PathBuf::from("/home/test")),
            ),
            PathBuf::from("/xdg/data/streenote/review.sock")
        );
        assert_eq!(
            resolve_review_socket_path(
                None,
                None,
                None,
                Some(PathBuf::from("/home/test")),
            ),
            PathBuf::from("/home/test/.local/share/streenote/review.sock")
        );
        assert_eq!(
            resolve_review_socket_path(
                Some(PathBuf::from("/custom/review.sock")),
                Some(PathBuf::from("/run/user/1000")),
                Some(PathBuf::from("/xdg/data")),
                Some(PathBuf::from("/home/test")),
            ),
            PathBuf::from("/custom/review.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn enabled_review_request_reaches_the_tui_and_returns_the_edited_answer() {
        use std::os::unix::net::UnixStream;

        let root = test_root();
        let socket = root.join("review.sock");
        let server = HermesReviewServer::start_at_for_test(socket.clone()).expect("start listener");
        server.set_enabled(true);

        let client_socket = socket.clone();
        let client = thread::spawn(move || {
            let mut stream = UnixStream::connect(client_socket).expect("connect review listener");
            let request = json!({
                "version": 1,
                "request_id": "review-001",
                "session_id": "session-abc",
                "model": "qwen3.5:latest",
                "platform": "tui",
                "response_text": "Original draft"
            });
            writeln!(stream, "{request}").expect("write review request");
            let mut response = String::new();
            BufReader::new(stream).read_line(&mut response).expect("read review response");
            serde_json::from_str::<serde_json::Value>(&response).expect("parse response")
        });

        let request = server
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("TUI receives pending review");
        assert_eq!(request.request_id, "review-001");
        assert_eq!(request.session_id, "session-abc");
        assert_eq!(request.model, "qwen3.5:latest");
        assert_eq!(request.platform, "tui");
        assert_eq!(request.response_text, "Original draft");
        request
            .reply
            .send(ReviewDecision::Replace("Edited and approved".to_string()))
            .expect("send operator decision");

        let response = client.join().expect("client thread");
        assert_eq!(response["decision"], "replace");
        assert_eq!(response["response_text"], "Edited and approved");

        drop(server);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn disabled_review_mode_returns_bypass_without_queueing_a_tui_request() {
        use std::os::unix::net::UnixStream;

        let root = test_root();
        let socket = root.join("review.sock");
        let server = HermesReviewServer::start_at_for_test(socket.clone()).expect("start listener");

        let mut stream = UnixStream::connect(&socket).expect("connect review listener");
        let request = json!({
            "version": 1,
            "request_id": "review-002",
            "session_id": "session-abc",
            "model": "model",
            "platform": "cli",
            "response_text": "Pass through unchanged"
        });
        writeln!(stream, "{request}").expect("write bypass request");
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).expect("read bypass response");
        let response: serde_json::Value = serde_json::from_str(&response).expect("parse bypass response");
        assert_eq!(response["decision"], "bypass");
        assert!(server.try_recv().is_none());

        drop(server);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn disconnecting_the_tui_fails_closed_instead_of_releasing_the_draft() {
        use std::os::unix::net::UnixStream;

        let root = test_root();
        let socket = root.join("review.sock");
        let server = HermesReviewServer::start_at_for_test(socket.clone()).expect("start listener");
        server.set_enabled(true);

        let mut stream = UnixStream::connect(&socket).expect("connect review listener");
        let request = json!({
            "version": 1,
            "request_id": "review-003",
            "session_id": "",
            "model": "model",
            "platform": "cli",
            "response_text": "Do not release without approval"
        });
        writeln!(stream, "{request}").expect("write disconnect request");

        let pending = server.receiver.recv_timeout(Duration::from_secs(2)).expect("receive pending review");
        drop(pending);
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .expect("read fail-closed response");
        let response: serde_json::Value = serde_json::from_str(&response).expect("parse fail-closed response");
        assert_eq!(response["decision"], "reject");

        drop(server);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_empty_or_oversized_replacements() {
        assert!(MAX_REVIEW_BYTES >= 1024 * 1024);
        assert!(ReviewDecision::Replace(String::new()) != ReviewDecision::Approve);
    }
}
