//! Per-user persistent PTY session daemon.
//!
//! The daemon owns PTY masters and shell children. Interactive clients attach
//! over a private Unix-domain socket; disconnecting a client releases the
//! controller lease but does not terminate the shell.

#[cfg(unix)]
mod unix_impl {
    use std::{
        collections::{HashMap, VecDeque},
        env,
        fs,
        io::{self, Read, Write},
        os::fd::AsRawFd,
        os::unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
        path::PathBuf,
        process,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc::{self, Receiver, Sender},
            Arc, Mutex,
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
    use serde::{Deserialize, Serialize};

    const SOCKET_NAME: &str = "sessiond.sock";
    const MAX_FRAME_SIZE: usize = 1024 * 1024;
    const OUTPUT_HISTORY_BYTES: usize = 1024 * 1024;
    const FRAME_OUTPUT: u8 = 1;
    const FRAME_EXIT: u8 = 2;
    const FRAME_INPUT: u8 = 1;
    const FRAME_RESIZE: u8 = 2;
    const FRAME_DETACH: u8 = 3;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SessionInfo {
        pub id: String,
        pub role: String,
        pub shell: String,
        pub pid: Option<u32>,
        pub created_unix_ms: u128,
        pub attached: bool,
        pub exited: bool,
        pub audit_file_path: Option<String>,
        pub origin: HashMap<String, String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Request {
        op: String,
        session_id: Option<String>,
        cols: Option<u16>,
        rows: Option<u16>,
        echo: Option<bool>,
        role: Option<String>,
        replay: Option<bool>,
        origin: Option<HashMap<String, String>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Response {
        ok: bool,
        session: Option<SessionInfo>,
        sessions: Option<Vec<SessionInfo>>,
        error: Option<String>,
    }

    impl Response {
        fn ok() -> Self {
            Self { ok: true, session: None, sessions: None, error: None }
        }

        fn error(error: impl Into<String>) -> Self {
            Self { ok: false, session: None, sessions: None, error: Some(error.into()) }
        }
    }

    enum OutputPacket {
        Data(Vec<u8>),
        Exit,
    }

    #[derive(Default)]
    struct OutputState {
        history: VecDeque<u8>,
        clients: HashMap<u64, Sender<OutputPacket>>,
        controller: Option<u64>,
        exited: bool,
    }

    struct ManagedSession {
        id: String,
        role: String,
        shell: PathBuf,
        pid: Option<u32>,
        audit_file_path: Option<PathBuf>,
        origin: HashMap<String, String>,
        created_unix_ms: u128,
        writer: Mutex<Box<dyn Write + Send>>,
        master: Mutex<Box<dyn MasterPty + Send>>,
        child: Mutex<Box<dyn Child + Send + Sync>>,
        output: Mutex<OutputState>,
        next_client_id: AtomicU64,
    }

    impl ManagedSession {
        fn info(&self) -> SessionInfo {
            let exited = self.is_exited();
            let output = self.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            SessionInfo {
                id: self.id.clone(),
                role: self.role.clone(),
                shell: self.shell.display().to_string(),
                pid: self.pid,
                created_unix_ms: self.created_unix_ms,
                attached: output.controller.is_some(),
                exited,
                audit_file_path: self.audit_file_path.as_ref().map(|path| path.display().to_string()),
                origin: self.origin.clone(),
            }
        }

        fn is_exited(&self) -> bool {
            {
                let output = self.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if output.exited {
                    return true;
                }
            }
            let mut child = self.child.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match child.try_wait() {
                Ok(Some(_)) => {
                    let mut output = self.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    output.exited = true;
                    for sender in output.clients.values() {
                        let _ = sender.send(OutputPacket::Exit);
                    }
                    true
                }
                _ => false,
            }
        }

        fn publish(&self, bytes: &[u8]) {
            let mut output = self.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            for byte in bytes {
                output.history.push_back(*byte);
            }
            while output.history.len() > OUTPUT_HISTORY_BYTES {
                output.history.pop_front();
            }
            output.clients.retain(|_, sender| {
                sender.send(OutputPacket::Data(bytes.to_vec())).is_ok()
            });
        }

        fn subscribe(&self, replay_enabled: bool) -> io::Result<(u64, Vec<u8>, Receiver<OutputPacket>)> {
            let client_id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = mpsc::channel();
            let mut output = self.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if output.exited {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "session has exited"));
            }
            if output.controller.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "session already has an attached controller",
                ));
            }
            let replay = if replay_enabled { output.history.iter().copied().collect() } else { Vec::new() };
            output.controller = Some(client_id);
            output.clients.insert(client_id, sender);
            Ok((client_id, replay, receiver))
        }

        fn unsubscribe(&self, client_id: u64) {
            let mut output = self.output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            output.clients.remove(&client_id);
            if output.controller == Some(client_id) {
                output.controller = None;
            }
        }

        fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
            let mut writer = self.writer.lock()
                .map_err(|_| io::Error::other("PTY writer lock poisoned"))?;
            writer.write_all(bytes)?;
            writer.flush()
        }

        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            self.master.lock()
                .map_err(|_| io::Error::other("PTY master lock poisoned"))?
                .resize(PtySize {
                    rows: rows.max(8),
                    cols: cols.max(40),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| io::Error::other(error.to_string()))
        }

        fn terminate(&self) -> io::Result<()> {
            self.child.lock()
                .map_err(|_| io::Error::other("PTY child lock poisoned"))?
                .kill()
                .map_err(|error| io::Error::other(error.to_string()))
        }
    }

    impl Drop for ManagedSession {
        fn drop(&mut self) {
            if let Ok(child) = self.child.get_mut() {
                let _ = child.kill();
            }
        }
    }

    #[derive(Clone, Default)]
    struct SessionManager {
        sessions: Arc<Mutex<HashMap<String, Arc<ManagedSession>>>>,
    }

    impl SessionManager {
        fn create(&self, role: String, cols: u16, rows: u16, echo: bool, origin: Option<HashMap<String, String>>) -> io::Result<Arc<ManagedSession>> {
            let pty_system = native_pty_system();
            let pair = pty_system.openpty(PtySize {
                rows: rows.max(8),
                cols: cols.max(40),
                pixel_width: 0,
                pixel_height: 0,
            }).map_err(|error| io::Error::other(error.to_string()))?;

            let shell = resolve_shell_program();
            let id = new_session_id();
            let supports_bash_audit = shell.file_name()
                .is_some_and(|name| name.to_string_lossy().to_ascii_lowercase().contains("bash"));
            let audit_file_path = if role.starts_with("workspace:") && supports_bash_audit {
                Some(create_workspace_audit_files(&id)?)
            } else {
                None
            };
            let audit_rcfile_path = audit_file_path.as_ref().map(|path| path.with_extension("bashrc"));
            if !echo {
                set_input_echo(&*pair.master, false)?;
            }

            let mut command = CommandBuilder::new(&shell);
            command.arg("-i");
            command.env("SHELL", &shell);
            command.env("TERM", env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()));
            command.env("COLORTERM", env::var("COLORTERM").unwrap_or_else(|_| "truecolor".to_string()));
            command.env_remove("OPERATOR_DATABASE_URL");
            for variable in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "SSH_USER_AUTH"] {
                command.env_remove(variable);
            }
            command.env(
                crate::hermes_bridge::HERMES_INBOX_ENV,
                crate::hermes_bridge::default_inbox_path(),
            );
            command.env("STREENOTE_HERMES_INBOX", crate::hermes_bridge::default_inbox_path());

            let origin = origin.unwrap_or_default();
            for (key, value) in &origin {
                if is_allowed_origin_variable(key) {
                    command.env(key, value);
                }
            }
            let operator_session_id = if role == "primary" {
                id.as_str()
            } else {
                origin.get("OPERATOR_SESSION_ID").map(String::as_str).unwrap_or(id.as_str())
            };
            command.env("OPERATOR_SESSION_ID", operator_session_id);
            command.env("STREENOTE_OPERATOR_SESSION_ID", operator_session_id);

            if let Some(path) = audit_file_path.as_ref() {
                command.env("OPERATOR_AUDIT_COMMAND_FILE", path);
                if let Some(rcfile) = audit_rcfile_path.as_ref() {
                    command.arg("--rcfile");
                    command.arg(rcfile);
                }
            }

            let child = match pair.slave.spawn_command(command) {
                Ok(child) => child,
                Err(error) => {
                    if let Some(path) = audit_file_path.as_ref() { let _ = fs::remove_file(path); }
                    if let Some(path) = audit_rcfile_path.as_ref() { let _ = fs::remove_file(path); }
                    return Err(io::Error::other(error.to_string()));
                }
            };
            drop(pair.slave);

            let pid = child.process_id();
            let mut reader = pair.master.try_clone_reader()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let writer = pair.master.take_writer()
                .map_err(|error| io::Error::other(error.to_string()))?;
            let master = pair.master;

            let session = Arc::new(ManagedSession {
                id: id.clone(),
                role,
                shell: shell.clone(),
                pid,
                audit_file_path,
                origin,
                created_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)
                    .unwrap_or_default().as_millis(),
                writer: Mutex::new(writer),
                master: Mutex::new(master),
                child: Mutex::new(child),
                output: Mutex::new(OutputState::default()),
                next_client_id: AtomicU64::new(1),
            });

            let reader_session = Arc::clone(&session);
            thread::Builder::new()
                .name(format!("operator-session-pty-{id}"))
                .spawn(move || {
                    let mut buffer = [0u8; 8192];
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(count) => reader_session.publish(&buffer[..count]),
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    let exited = reader_session.child.lock()
                        .ok()
                        .and_then(|mut child| child.try_wait().ok())
                        .flatten()
                        .is_some();
                    let mut output = reader_session.output.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    output.exited = exited;
                    for sender in output.clients.values() {
                        let _ = sender.send(OutputPacket::Exit);
                    }
                })?;

            self.sessions.lock()
                .map_err(|_| io::Error::other("session registry lock poisoned"))?
                .insert(id, Arc::clone(&session));
            Ok(session)
        }

        fn get_or_create(&self, role: &str, cols: u16, rows: u16, echo: bool, origin: Option<HashMap<String, String>>) -> io::Result<Arc<ManagedSession>> {
            let existing = {
                let sessions = self.sessions.lock()
                    .map_err(|_| io::Error::other("session registry lock poisoned"))?;
                sessions.values()
                    .filter(|session| session.role == role)
                    .filter(|session| !session.is_exited())
                    .filter(|session| {
                        let output = session.output.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        output.controller.is_none()
                    })
                    .max_by_key(|session| session.created_unix_ms)
                    .cloned()
            };
            if let Some(session) = existing {
                return Ok(session);
            }
            self.create(role.to_string(), cols, rows, echo, origin)
        }

        fn get(&self, id: &str) -> io::Result<Arc<ManagedSession>> {
            self.sessions.lock()
                .map_err(|_| io::Error::other("session registry lock poisoned"))?
                .get(id)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown OPERATOR session"))
        }

        fn list(&self) -> Vec<SessionInfo> {
            let sessions = self.sessions.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.values().map(|session| session.info()).collect()
        }

        fn terminate(&self, id: &str) -> io::Result<()> {
            let session = self.get(id)?;
            session.terminate()
        }
    }

    pub fn socket_path() -> PathBuf {
        if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime).join("operator").join(SOCKET_NAME)
        } else {
            let home = env::var_os("HOME").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            home.join(".local/share/operator/run").join(SOCKET_NAME)
        }
    }

    /// Connect to the daemon, starting a detached per-user instance when no
    /// systemd-managed instance is already listening.
    pub fn ensure_running() -> io::Result<()> {
        let ping = || request_response(&Request {
            op: "ping".into(), session_id: None, cols: None, rows: None,
            echo: None, role: None, replay: None, origin: None,
        });
        if ping().is_ok() {
            return Ok(());
        }

        use std::os::unix::process::CommandExt;
        let path = socket_path();
        let parent = path.parent().ok_or_else(|| io::Error::other("daemon socket has no parent"))?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let log_path = parent.join("sessiond.log");
        let mut log_options = fs::OpenOptions::new();
        log_options.create(true).append(true).mode(0o600);
        let log = log_options.open(&log_path)?;
        let executable = env::current_exe()?;
        let mut command = process::Command::new(executable);
        command.arg("daemon")
            .stdin(process::Stdio::null())
            .stdout(process::Stdio::from(log.try_clone()?))
            .stderr(process::Stdio::from(log));
        // A daemon started on the first SSH login must not inherit the SSH
        // process group or be killed when that client disconnects.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        match command.spawn() {
            Ok(_) => {}
            Err(_error) if ping().is_ok() => return Ok(()),
            Err(error) => return Err(error),
        }

        for _ in 0..100 {
            if ping().is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("OPERATOR session daemon did not start; inspect {}", log_path.display()),
        ))
    }

    pub fn run_daemon() -> io::Result<()> {
        let path = socket_path();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "daemon socket has no parent directory")
        })?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing to remove non-owned or non-socket path {}", path.display()),
                ));
            }
            if UnixStream::connect(&path).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "OPERATOR session daemon is already running"));
            }
            fs::remove_file(&path)?;
        }

        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        eprintln!("OPERATOR session daemon listening on {}", path.display());
        let manager = SessionManager::default();

        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let manager = manager.clone();
                    let _ = thread::Builder::new()
                        .name("operator-session-client".to_string())
                        .spawn(move || {
                            if let Err(error) = handle_connection(stream, manager) {
                                eprintln!("OPERATOR session daemon client error: {error}");
                            }
                        });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn handle_connection(mut stream: UnixStream, manager: SessionManager) -> io::Result<()> {
        ensure_peer_uid(&stream)?;
        let mut reader = std::io::BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut request_line)?;
        if request_line.is_empty() {
            return Ok(());
        }
        let request: Request = serde_json::from_str(&request_line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        match request.op.as_str() {
            "ping" => write_response(&mut stream, Response::ok()),
            "list" => {
                let mut response = Response::ok();
                response.sessions = Some(manager.list());
                write_response(&mut stream, response)
            }
            "create" => {
                let cols = request.cols.unwrap_or(80);
                let rows = request.rows.unwrap_or(24);
                let role = request.role.unwrap_or_else(|| "standalone".to_string());
                match manager.create(role, cols, rows, request.echo.unwrap_or(true), request.origin) {
                    Ok(session) => {
                        let mut response = Response::ok();
                        response.session = Some(session.info());
                        write_response(&mut stream, response)
                    }
                    Err(error) => write_response(&mut stream, Response::error(error.to_string())),
                }
            }
            "get_or_create" => {
                let cols = request.cols.unwrap_or(80);
                let rows = request.rows.unwrap_or(24);
                let role = request.role.unwrap_or_else(|| "primary".to_string());
                match manager.get_or_create(&role, cols, rows, request.echo.unwrap_or(false), request.origin) {
                    Ok(session) => {
                        let mut response = Response::ok();
                        response.session = Some(session.info());
                        write_response(&mut stream, response)
                    }
                    Err(error) => write_response(&mut stream, Response::error(error.to_string())),
                }
            }
            "terminate" => {
                let Some(id) = request.session_id.as_deref() else {
                    return write_response(&mut stream, Response::error("session_id is required"));
                };
                match manager.terminate(id) {
                    Ok(()) => write_response(&mut stream, Response::ok()),
                    Err(error) => write_response(&mut stream, Response::error(error.to_string())),
                }
            }
            "attach" => {
                let Some(id) = request.session_id.as_deref() else {
                    return write_response(&mut stream, Response::error("session_id is required"));
                };
                let session = match manager.get(id) {
                    Ok(session) => session,
                    Err(error) => return write_response(&mut stream, Response::error(error.to_string())),
                };
                attach_client(&mut stream, session, request.replay.unwrap_or(true))
            }
            _ => write_response(&mut stream, Response::error("unknown daemon operation")),
        }
    }

    fn attach_client(stream: &mut UnixStream, session: Arc<ManagedSession>, replay_enabled: bool) -> io::Result<()> {
        let (client_id, replay, receiver) = match session.subscribe(replay_enabled) {
            Ok(subscription) => subscription,
            Err(error) => return write_response(stream, Response::error(error.to_string())),
        };
        let mut response = Response::ok();
        response.session = Some(session.info());
        write_response(stream, response)?;

        let mut output_stream = stream.try_clone()?;
        let writer = thread::Builder::new()
            .name(format!("operator-session-output-{}", session.id))
            .spawn(move || {
                if !replay.is_empty() && write_frame(&mut output_stream, FRAME_OUTPUT, &replay).is_err() {
                    return;
                }
                while let Ok(packet) = receiver.recv() {
                    match packet {
                        OutputPacket::Data(bytes) => {
                            if write_frame(&mut output_stream, FRAME_OUTPUT, &bytes).is_err() {
                                break;
                            }
                        }
                        OutputPacket::Exit => {
                            let _ = write_frame(&mut output_stream, FRAME_EXIT, &[]);
                            break;
                        }
                    }
                }
            })?;

        loop {
            let (kind, payload) = match read_frame(stream) {
                Ok(frame) => frame,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof
                    || error.kind() == io::ErrorKind::ConnectionReset => break,
                Err(error) => {
                    session.unsubscribe(client_id);
                    let _ = writer.join();
                    return Err(error);
                }
            };
            match kind {
                FRAME_INPUT => session.write_input(&payload)?,
                FRAME_RESIZE if payload.len() == 4 => {
                    let cols = u16::from_be_bytes([payload[0], payload[1]]);
                    let rows = u16::from_be_bytes([payload[2], payload[3]]);
                    session.resize(cols, rows)?;
                }
                FRAME_DETACH => break,
                _ => {}
            }
        }

        session.unsubscribe(client_id);
        drop(stream);
        let _ = writer.join();
        Ok(())
    }

    fn write_response(stream: &mut UnixStream, response: Response) -> io::Result<()> {
        serde_json::to_writer(&mut *stream, &response)
            .map_err(|error| io::Error::other(error.to_string()))?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    fn request_response(request: &Request) -> io::Result<Response> {
        let path = socket_path();
        let mut stream = UnixStream::connect(&path).map_err(|error| {
            io::Error::new(error.kind(), format!(
                "cannot connect to OPERATOR session daemon at {}: {error}; start it with the daemon command",
                path.display()
            ))
        })?;
        serde_json::to_writer(&mut stream, request)
            .map_err(|error| io::Error::other(error.to_string()))?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        std::io::BufRead::read_line(&mut reader, &mut line)?;
        if line.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed before replying"));
        }
        let response: Response = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !response.ok {
            return Err(io::Error::other(response.error.unwrap_or_else(|| "daemon request failed".to_string())));
        }
        Ok(response)
    }

    pub fn list_sessions() -> io::Result<Vec<SessionInfo>> {
        Ok(request_response(&Request {
            op: "list".into(), session_id: None, cols: None, rows: None, echo: None, role: None, replay: None, origin: None,
        })?.sessions.unwrap_or_default())
    }

    pub fn create_session(cols: u16, rows: u16, echo: bool, origin: HashMap<String, String>) -> io::Result<SessionInfo> {
        request_response(&Request {
            op: "create".into(), session_id: None, cols: Some(cols), rows: Some(rows),
            echo: Some(echo), role: Some("standalone".into()), replay: None, origin: Some(origin),
        })?.session.ok_or_else(|| io::Error::other("daemon did not return the created session"))
    }

    pub fn get_or_create_session(role: &str, cols: u16, rows: u16, echo: bool, origin: HashMap<String, String>) -> io::Result<SessionInfo> {
        request_response(&Request {
            op: "get_or_create".into(), session_id: None, cols: Some(cols), rows: Some(rows),
            echo: Some(echo), role: Some(role.to_string()), replay: None, origin: Some(origin),
        })?.session.ok_or_else(|| io::Error::other("daemon did not return a session"))
    }

    pub fn terminate_session(id: &str) -> io::Result<()> {
        request_response(&Request {
            op: "terminate".into(), session_id: Some(id.to_string()),
            cols: None, rows: None, echo: None, role: None, replay: None, origin: None,
        }).map(|_| ())
    }

    pub fn attach_session(id: &str) -> io::Result<()> {
        attach_session_with_replay(id, true)
    }

    pub fn attach_session_with_replay(id: &str, replay: bool) -> io::Result<()> {
        let path = socket_path();
        let mut stream = UnixStream::connect(&path)?;
        let request = Request {
            op: "attach".into(), session_id: Some(id.to_string()),
            cols: None, rows: None, echo: None, role: None, replay: Some(replay), origin: None,
        };
        serde_json::to_writer(&mut stream, &request)
            .map_err(|error| io::Error::other(error.to_string()))?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut response_line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte)?;
            if byte[0] == b'\n' { break; }
            if response_line.len() >= 64 * 1024 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "daemon response too large"));
            }
            response_line.push(byte[0]);
        }
        let response: Response = serde_json::from_slice(&response_line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !response.ok {
            return Err(io::Error::other(response.error.unwrap_or_else(|| "attach rejected".to_string())));
        }

        crossterm::terminal::enable_raw_mode()?;
        let _raw_mode = RawModeGuard;
        let mut tty = fs::OpenOptions::new().read(true).open("/dev/tty")?;
        let mut last_size = terminal_size(tty.as_raw_fd()).unwrap_or((80, 24));
        let resize = [last_size.0.to_be_bytes(), last_size.1.to_be_bytes()].concat();
        write_frame(&mut stream, FRAME_RESIZE, &resize)?;

        let (sender, receiver) = mpsc::channel::<io::Result<Option<Vec<u8>>>>();
        let mut read_stream = stream.try_clone()?;
        thread::Builder::new()
            .name("operator-session-attach-reader".into())
            .spawn(move || loop {
                match read_frame(&mut read_stream) {
                    Ok((FRAME_OUTPUT, bytes)) => {
                        if sender.send(Ok(Some(bytes))).is_err() { break; }
                    }
                    Ok((FRAME_EXIT, _)) => {
                        let _ = sender.send(Ok(None));
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            })?;

        let mut output = io::stdout().lock();
        loop {
            while let Ok(message) = receiver.try_recv() {
                match message {
                    Ok(Some(bytes)) => {
                        output.write_all(&bytes)?;
                        output.flush()?;
                    }
                    Ok(None) => return Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof
                        || error.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
                    Err(error) => return Err(error),
                }
            }

            let mut descriptor = libc::pollfd {
                fd: tty.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, 25) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(error);
            }
            if ready > 0 {
                let mut bytes = [0u8; 8192];
                let count = tty.read(&mut bytes)?;
                if count == 0 { break; }
                if bytes[..count].contains(&0x1d) {
                    break;
                }
                write_frame(&mut stream, FRAME_INPUT, &bytes[..count])?;
            }
            if let Some(size) = terminal_size(tty.as_raw_fd()) {
                if size != last_size {
                    last_size = size;
                    let new_size = [size.0.to_be_bytes(), size.1.to_be_bytes()].concat();
                    write_frame(&mut stream, FRAME_RESIZE, &new_size)?;
                }
            }
        }

        let _ = write_frame(&mut stream, FRAME_DETACH, &[]);
        Ok(())
    }

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) { let _ = crossterm::terminal::disable_raw_mode(); }
    }

    fn terminal_size(fd: i32) -> Option<(u16, u16)> {
        let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 && size.ws_row > 0 {
            Some((size.ws_col, size.ws_row))
        } else {
            None
        }
    }

    fn write_frame(stream: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_FRAME_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "daemon frame exceeds 1 MiB"));
        }
        stream.write_all(&[kind])?;
        stream.write_all(&(payload.len() as u32).to_be_bytes())?;
        stream.write_all(payload)?;
        stream.flush()
    }

    fn read_frame(stream: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
        let mut header = [0u8; 5];
        stream.read_exact(&mut header)?;
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if length > MAX_FRAME_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "daemon frame exceeds 1 MiB"));
        }
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload)?;
        Ok((header[0], payload))
    }

    fn ensure_peer_uid(stream: &UnixStream) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let mut credentials = unsafe { std::mem::zeroed::<libc::ucred>() };
            let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let result = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    &mut credentials as *mut _ as *mut libc::c_void,
                    &mut length,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            if credentials.uid != unsafe { libc::geteuid() } {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "daemon socket peer UID mismatch"));
            }
        }
        Ok(())
    }

    fn is_allowed_origin_variable(name: &str) -> bool {
        matches!(
            name,
            "OPERATOR_SSH_USERNAME"
                | "OPERATOR_UNIX_UID"
                | "OPERATOR_SSH_CLIENT_IP"
                | "OPERATOR_SSH_CLIENT_PORT"
                | "OPERATOR_SSH_SERVER_IP"
                | "OPERATOR_SSH_SERVER_PORT"
                | "OPERATOR_SSH_TTY"
                | "OPERATOR_SSH_AUTH_METHOD"
                | "OPERATOR_SSH_KEY_FINGERPRINT"
                | "OPERATOR_SESSION_ID"
                | "STREENOTE_OPERATOR_SESSION_ID"
                | "OPERATOR_HERMES_REVIEW_SESSION_DIR"
                | "OPERATOR_HERMES_INBOX"
                | "STREENOTE_HERMES_INBOX"
        )
    }

    fn create_workspace_audit_files(session_id: &str) -> io::Result<PathBuf> {
        let home = env::var_os("HOME").map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set for audit spool"))?;
        let directory = home.join(".local/share/operator/sessions");
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let path = directory.join(format!("{session_id}.nul"));
        let rcfile_path = path.with_extension("bashrc");

        let mut audit_options = fs::OpenOptions::new();
        audit_options.create_new(true).read(true).write(true).mode(0o600);
        audit_options.open(&path)?;

        let rc_source = r#"
if [[ -r "$HOME/.bashrc" ]]; then
  source "$HOME/.bashrc"
elif [[ -r /etc/bash.bashrc ]]; then
  source /etc/bash.bashrc
fi
__operator_audit_capture() {
  local current
  current="$HISTCMD"
  if [[ -z "$current" ]]; then current=0; fi
  if [[ ! -v __OPERATOR_AUDIT_LAST_HISTCMD ]]; then
    __OPERATOR_AUDIT_LAST_HISTCMD="$current"
    return
  fi
  if [[ "$current" != "$__OPERATOR_AUDIT_LAST_HISTCMD" ]]; then
    fc -ln -1 -1 >> "$OPERATOR_AUDIT_COMMAND_FILE" 2>/dev/null
    printf "\\0" >> "$OPERATOR_AUDIT_COMMAND_FILE"
    __OPERATOR_AUDIT_LAST_HISTCMD="$current"
  fi
}
__operator_previous_prompt_command="$PROMPT_COMMAND"
PROMPT_COMMAND="__operator_audit_capture"
if [[ -n "$__operator_previous_prompt_command" ]]; then
  PROMPT_COMMAND+=";$__operator_previous_prompt_command"
fi
trap "__operator_audit_capture" EXIT
"#;
        let mut rc_options = fs::OpenOptions::new();
        rc_options.create_new(true).write(true).mode(0o600);
        let mut rcfile = rc_options.open(&rcfile_path)?;
        rcfile.write_all(rc_source.as_bytes())?;
        rcfile.sync_all()?;
        Ok(path)
    }

    fn resolve_shell_program() -> PathBuf {
        let current_exe = env::current_exe().ok().and_then(|path| fs::canonicalize(path).ok());
        if let Some(candidate) = env::var_os("OPERATOR_SHELL").or_else(|| env::var_os("SHELL")) {
            let candidate = PathBuf::from(candidate);
            let resolves_to_self = current_exe.as_ref().is_some_and(|current| {
                fs::canonicalize(&candidate).map(|resolved| resolved == *current).unwrap_or(false)
            });
            if !resolves_to_self && !candidate.as_os_str().is_empty() {
                return candidate;
            }
        }
        PathBuf::from("/bin/bash")
    }

    #[cfg(target_os = "linux")]
    fn set_input_echo(master: &dyn MasterPty, enabled: bool) -> io::Result<()> {
        let fd = master.as_raw_fd().ok_or_else(|| io::Error::other("PTY master has no file descriptor"))?;
        let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = unsafe { attributes.assume_init() };
        if enabled {
            attributes.c_lflag |= libc::ECHO | libc::ECHONL;
        } else {
            attributes.c_lflag &= !(libc::ECHO | libc::ECHONL);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attributes) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn set_input_echo(_master: &dyn MasterPty, _enabled: bool) -> io::Result<()> {
        Ok(())
    }

    fn new_session_id() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        format!("opd-{nanos:032x}-{:x}-{:x}", process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    #[cfg(test)]
    mod tests {
        use super::{new_session_id, OUTPUT_HISTORY_BYTES};

        #[test]
        fn session_ids_are_unique_and_safe_for_cli_and_socket_protocols() {
            let first = new_session_id();
            let second = new_session_id();
            assert_ne!(first, second);
            assert!(first.starts_with("opd-"));
            assert!(first.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
        }

        #[test]
        fn replay_buffer_has_a_bounded_cap() {
            assert_eq!(OUTPUT_HISTORY_BYTES, 1024 * 1024);
        }
    }
}

#[cfg(unix)]
pub use unix_impl::{attach_session, attach_session_with_replay, create_session, ensure_running, get_or_create_session, list_sessions, run_daemon, socket_path, terminate_session, SessionInfo};

#[cfg(not(unix))]
compile_error!("OPERATOR's persistent session daemon currently requires a Unix platform.");
