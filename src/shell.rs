use std::{
    env,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const IDLE_FLUSH: Duration = Duration::from_millis(180);
const READ_BUFFER_SIZE: usize = 4096;

/// A real pseudo-terminal running the user's Debian shell.
/// stdout and stderr are merged by the PTY and returned as transcript lines.
pub struct ShellSession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    receiver: Receiver<io::Result<Vec<u8>>>,
    parser: TranscriptParser,
    last_output: Option<Instant>,
    exited: bool,
    reader_closed: bool,
    shell_program: PathBuf,
}

impl std::fmt::Debug for ShellSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellSession")
            .field("exited", &self.exited)
            .finish_non_exhaustive()
    }
}

impl ShellSession {
    pub fn spawn(cols: u16, rows: u16) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(8),
                cols: cols.max(40),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| io::Error::other(error.to_string()))?;

        // Disable line echo so every submitted command appears only once in
        // the journal (as CMD); PTY output remains fully visible and captured.
        set_input_echo(&*pair.master, false)?;

        // When OPERATOR is the account's SSH login shell, SSH may set
        // $SHELL to this executable. Never launch ourselves as the nested
        // interactive shell: doing so recursively enters raw mode and makes
        // the parent mistake the child PTY's disabled ECHO for a password prompt.
        // OPERATOR_SHELL is the explicit override; otherwise use $SHELL only
        // when it does not resolve to this executable.
        let shell = resolve_shell_program();
        let mut command = CommandBuilder::new(&shell);
        // Keep PostgreSQL credentials in the OPERATOR process only; do not
        // leak the connection URL into the interactive shell's environment.
        command.env_remove("OPERATOR_DATABASE_URL");
        // Standard Hermes integration contract: every OPERATOR-managed PTY
        // receives the canonical inbox path from the Rust bridge. Child tools
        // inherit it; independent gateway services need their own hook adapter.
        command.env(
            crate::hermes_bridge::HERMES_INBOX_ENV,
            crate::hermes_bridge::default_inbox_path(),
        );
        command.env("STREENOTE_HERMES_INBOX", crate::hermes_bridge::default_inbox_path());
        // Bind Hermes' pre_llm_call capture events to this exact SSH-launched
        // OPERATOR instance. The shared gateway uses that session association
        // to return final answers to the originating terminal.
        for variable in ["OPERATOR_HERMES_REVIEW_SESSION_DIR", "STREENOTE_HERMES_REVIEW_SESSION_DIR"] {
            if let Some(value) = env::var_os(variable) {
                command.env(variable, value);
            }
        }
        // Keep subprocesses and shell startup files pointed at the real shell,
        // not at OPERATOR when it is the account's configured login shell.
        command.env("SHELL", &shell);

        // Start the configured interactive shell normally so Debian's shell
        // startup files, aliases, prompt, PATH, and environment are honored.
        command.arg("-i");

        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| io::Error::other(error.to_string()))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let master = pair.master;

        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("operator-pty-reader".to_string())
            .spawn(move || {
                let mut buffer = [0_u8; READ_BUFFER_SIZE];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            break;
                        }
                    }
                }
            })?;

        Ok(Self {
            writer,
            child,
            master,
            receiver,
            parser: TranscriptParser::default(),
            last_output: None,
            exited: false,
            reader_closed: false,
            shell_program: shell,
        })
    }

    /// The executable actually used for the nested interactive shell.
    pub fn shell_program(&self) -> &Path {
        &self.shell_program
    }

    /// Build the footer prompt from the real shell process identity and cwd.
    /// The home directory is displayed as "~" for a compact, familiar prompt.
    pub fn prompt_label(&self) -> String {
        let user = env::var("USER")
            .or_else(|_| env::var("LOGNAME"))
            .unwrap_or_else(|_| "user".to_string());
        let hostname = fs::read_to_string("/etc/hostname")
            .map(|value| value.trim().to_string())
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "localhost".to_string());
        let cwd = self
            .child
            .process_id()
            .and_then(|pid| fs::read_link(format!("/proc/{pid}/cwd")).ok())
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("?"));
        let home = env::var_os("HOME").map(PathBuf::from);
        format_shell_prompt(&user, &hostname, &cwd, home.as_deref())
    }

    /// Ask Bash for command and filesystem completions in the live shell's cwd.
    /// The cwd is read from /proc so it stays correct after interactive `cd`.
    pub fn complete_candidates(&self, line: &str, cursor: usize) -> io::Result<Vec<String>> {
        let cursor = cursor.min(line.len());
        if !line.is_char_boundary(cursor) {
            return Ok(Vec::new());
        }

        let before_cursor = &line[..cursor];
        let token_start = before_cursor
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(index, ch)| index + ch.len_utf8())
            .unwrap_or(0);
        let prefix = &line[token_start..cursor];
        if prefix.is_empty() {
            return Ok(Vec::new());
        }

        let command_position = before_cursor[..token_start].trim().is_empty();
        let cwd = self.child.process_id()
            .and_then(|pid| fs::read_link(format!("/proc/{pid}/cwd")).ok())
            .or_else(|| env::current_dir().ok());

        let script = r#"
            prefix=$1
            if [[ "$prefix" == "~"* ]]; then
                expanded="$HOME${prefix:1}"
            else
                expanded="$prefix"
            fi
            if [[ "$2" == "1" ]]; then
                compgen -A command -- "$prefix"
            fi
            compgen -f -- "$expanded"
        "#;

        let mut command = std::process::Command::new("/bin/bash");
        command.arg("-c")
            .arg(script)
            .arg("operator-completion")
            .arg(prefix)
            .arg(if command_position { "1" } else { "0" });

        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let output = command.output()?;
        if !output.status.success() && output.stdout.is_empty() {
            return Ok(Vec::new());
        }

        let mut candidates: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|candidate| !candidate.is_empty())
            .map(|candidate| {
                let mut value = candidate.to_string();
                let path = Path::new(&value);
                let resolved = if path.is_absolute() {
                    path.to_path_buf()
                } else if let Some(cwd) = self.child.process_id()
                    .and_then(|pid| fs::read_link(format!("/proc/{pid}/cwd")).ok())
                {
                    cwd.join(path)
                } else {
                    env::current_dir().unwrap_or_default().join(path)
                };
                if resolved.is_dir() && !value.ends_with('/') {
                    value.push('/');
                }
                value
            })
            .collect();

        candidates.sort();
        candidates.dedup();
        Ok(candidates)
    }

    /// Send one line to the interactive shell's PTY.
    /// Disable terminal input echo before writing so the command is logged
    /// exactly once as a CMD record rather than again as PTY OUT text.
    pub fn send_line(&mut self, command: &str) -> io::Result<()> {
        if self.exited {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the shell process has exited",
            ));
        }
        set_input_echo(&*self.master, false)?;
        self.writer.write_all(command.as_bytes())?;
        self.writer.write_all(b"\r")?;
        self.writer.flush()
    }

    /// Send raw terminal bytes while the full-screen workspace owns keyboard focus.
    pub fn send_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.exited {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the shell process has exited"));
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Toggle PTY echo when switching between transcript and direct terminal use.
    pub fn set_echo(&self, enabled: bool) -> io::Result<()> {
        set_input_echo(&*self.master, enabled)
    }

    /// Send a terminal control byte, e.g. ETX (Ctrl+C) or EOT (Ctrl+D).
    pub fn send_control(&mut self, byte: u8) -> io::Result<()> {
        if self.exited {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the shell process has exited",
            ));
        }
        self.writer.write_all(&[byte])?;
        self.writer.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows: rows.max(8),
                cols: cols.max(40),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| io::Error::other(error.to_string()))
    }

    /// Query the PTY's ECHO flag for compatibility with programs that change it.
    /// OPERATOR normally disables echo globally, so a false value alone is not
    /// treated as proof of a password prompt; the app also checks prompt text.
    #[cfg(unix)]
    pub fn input_is_echoed(&self) -> Option<bool> {
        use std::mem::MaybeUninit;

        let fd = self.master.as_raw_fd()?;
        let mut attributes = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the termios value when it returns 0.
        let result = unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) };
        if result != 0 {
            return None;
        }
        // SAFETY: the successful tcgetattr call initialized attributes.
        let attributes = unsafe { attributes.assume_init() };
        Some(attributes.c_lflag & libc::ECHO != 0)
    }

    #[cfg(not(unix))]
    pub fn input_is_echoed(&self) -> Option<bool> {
        None
    }

    /// Drain available PTY bytes without parsing them into transcript lines.
    /// Used by the independent full-screen workspace, whose output must remain
    /// isolated from OPERATOR's persistent journal.
    pub fn drain_raw_output(&mut self) -> Vec<u8> {
        let mut raw = Vec::new();
        while !self.reader_closed {
            match self.receiver.try_recv() {
                Ok(Ok(bytes)) => raw.extend_from_slice(&bytes),
                Ok(Err(_)) => {
                    self.reader_closed = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.reader_closed = true;
                    break;
                }
            }
        }
        raw
    }

    /// Drain available PTY output, preserving raw bytes for direct terminal display
    /// while also extracting readable lines for the persistent transcript.
    pub fn drain_passthrough(&mut self) -> (Vec<u8>, Vec<String>) {
        let mut raw = Vec::new();
        let mut lines = Vec::new();
        while !self.reader_closed {
            match self.receiver.try_recv() {
                Ok(Ok(bytes)) => {
                    self.last_output = Some(Instant::now());
                    lines.extend(self.parser.feed(&bytes));
                    raw.extend_from_slice(&bytes);
                }
                Ok(Err(_)) => { self.reader_closed = true; break; }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => { self.reader_closed = true; break; }
            }
        }
        if self.last_output.is_some_and(|last| last.elapsed() >= IDLE_FLUSH) {
            if let Some(line) = self.parser.flush_partial() { lines.push(line); }
            self.last_output = None;
        }
        (raw, lines)
    }

    /// Drain PTY output for the transcript-only interface.
    pub fn drain_output(&mut self) -> Vec<String> {
        self.drain_passthrough().1
    }

    pub fn finish_output(&mut self) -> Option<String> {
        self.parser.flush_partial()
    }

    pub fn check_exit(&mut self) -> io::Result<Option<u32>> {
        if self.exited {
            return Ok(None);
        }
        match self.child.try_wait()? {
            Some(status) => {
                self.exited = true;
                Ok(Some(status.exit_code()))
            }
            None => Ok(None),
        }
    }

}


fn format_shell_prompt(user: &str, hostname: &str, cwd: &Path, home: Option<&Path>) -> String {
    let shown_path = home
        .and_then(|home| cwd.strip_prefix(home).ok())
        .map(|relative| {
            if relative.as_os_str().is_empty() {
                PathBuf::from("~")
            } else {
                PathBuf::from("~").join(relative)
            }
        })
        .unwrap_or_else(|| cwd.to_path_buf());

    format!("{user}@{hostname}:{}$ ", shown_path.display())
}

/// Select a real interactive shell, avoiding recursion when OPERATOR itself
/// is configured as the user's login shell.
fn resolve_shell_program() -> PathBuf {
    let current_exe = env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok());

    let configured = env::var_os("OPERATOR_SHELL").or_else(|| env::var_os("STREAMNOTE_SHELL")).or_else(|| env::var_os("SHELL"));
    if let Some(candidate) = configured {
        let candidate = PathBuf::from(candidate);
        let resolves_to_self = current_exe.as_ref().is_some_and(|current| {
            fs::canonicalize(&candidate)
                .map(|resolved| resolved == *current)
                .unwrap_or(false)
        });

        if !resolves_to_self && !candidate.as_os_str().is_empty() {
            return candidate;
        }
    }

    // Debian and Ubuntu provide /bin/bash. This fallback is intentionally
    // independent of $SHELL, which may identify OPERATOR as the login shell.
    PathBuf::from("/bin/bash")
}

/// Turn off PTY input echo without suppressing output written by the shell.
/// ECHONL is cleared as well so submitted CR/LF characters are not emitted.
#[cfg(unix)]
fn set_input_echo(master: &dyn MasterPty, enabled: bool) -> io::Result<()> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("PTY master does not expose a file descriptor"))?;
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();

    // SAFETY: tcgetattr initializes the termios value when it returns 0.
    if unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful tcgetattr call initialized attributes.
    let mut attributes = unsafe { attributes.assume_init() };
    if enabled {
        attributes.c_lflag |= libc::ECHO | libc::ECHONL;
    } else {
        attributes.c_lflag &= !(libc::ECHO | libc::ECHONL);
    }

    // SAFETY: attributes was initialized by tcgetattr and remains valid.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attributes) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_input_echo(_master: &dyn MasterPty, _enabled: bool) -> io::Result<()> {
    Ok(())
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    Other,
}

#[derive(Debug, Default)]
struct TranscriptParser {
    state: Option<EscapeState>,
    line: Vec<char>,
    cursor: usize,
    csi: String,
    utf8_pending: Vec<u8>,
    alternate_screen: bool,
}

impl TranscriptParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        // Preserve valid UTF-8 even when a multibyte character is split across
        // PTY reads. Invalid sequences become U+FFFD rather than stopping capture.
        let mut input = std::mem::take(&mut self.utf8_pending);
        input.extend_from_slice(bytes);
        let mut offset = 0;
        let mut completed = Vec::new();

        while offset < input.len() {
            match std::str::from_utf8(&input[offset..]) {
                Ok(valid) => {
                    self.feed_text(valid, &mut completed);
                    offset = input.len();
                }
                Err(error) => {
                    let valid_len = error.valid_up_to();
                    if valid_len > 0 {
                        // from_utf8 validated this prefix as complete UTF-8.
                        let valid = std::str::from_utf8(&input[offset..offset + valid_len])
                            .unwrap_or("");
                        self.feed_text(valid, &mut completed);
                        offset += valid_len;
                    }
                    match error.error_len() {
                        Some(invalid_len) => {
                            self.consume('\u{fffd}', &mut completed);
                            offset += invalid_len;
                        }
                        None => {
                            self.utf8_pending.extend_from_slice(&input[offset..]);
                            break;
                        }
                    }
                }
            }
        }
        completed
    }

    fn feed_text(&mut self, text: &str, completed: &mut Vec<String>) {
        for character in text.chars() {
            self.consume(character, completed);
        }
    }

    fn consume(&mut self, character: char, completed: &mut Vec<String>) {
        use EscapeState::*;
        let state = self.state.unwrap_or(Ground);
        match state {
            Ground => match character {
                '\u{1b}' => self.state = Some(Escape),
                '\r' => self.cursor = 0,
                '\n' => {
                    if !self.alternate_screen {
                        completed.push(self.line.iter().collect());
                    }
                    self.line.clear();
                    self.cursor = 0;
                }
                '\u{8}' => self.cursor = self.cursor.saturating_sub(1),
                '\t' => {
                    let spaces = 8 - (self.cursor % 8);
                    for _ in 0..spaces {
                        self.put(' ');
                    }
                }
                control if control.is_control() => {}
                printable => self.put(printable),
            },
            Escape => {
                self.state = Some(match character {
                    '[' => { self.csi.clear(); Csi },
                    ']' => Osc,
                    '(' | ')' | '*' | '+' | '-' | '.' | '/' => Other,
                    _ => Ground,
                });
            }
            Csi => {
                if ('@'..='~').contains(&character) {
                    self.apply_csi(character);
                    self.csi.clear();
                    self.state = Some(Ground);
                } else if self.csi.len() < 32 {
                    self.csi.push(character);
                }
            }
            Osc => match character {
                '\u{7}' => self.state = Some(Ground),
                '\u{1b}' => self.state = Some(OscEscape),
                _ => {}
            },
            OscEscape => {
                self.state = Some(if character == '\\' { Ground } else { Osc });
            }
            Other => self.state = Some(Ground),
        }
    }

    fn apply_csi(&mut self, final_character: char) {
        let first_parameter = self.csi.split(';').next().unwrap_or("");
        if matches!(final_character, 'h' | 'l')
            && (self.csi.contains("?1049") || self.csi.contains("?1047") || self.csi.contains("?47"))
        {
            self.alternate_screen = final_character == 'h';
            if self.alternate_screen {
                self.line.clear();
                self.cursor = 0;
            }
        }
        let amount = first_parameter.parse::<usize>().unwrap_or(1).max(1);
        match final_character {
            // Erase in line: K, 0K, 1K, or 2K.
            'K' => match first_parameter.parse::<usize>().unwrap_or(0) {
                0 => self.line.truncate(self.cursor.min(self.line.len())),
                1 => {
                    let end = self.cursor.min(self.line.len());
                    for cell in self.line.iter_mut().take(end) {
                        *cell = ' ';
                    }
                }
                2 => {
                    self.line.clear();
                    self.cursor = 0;
                }
                _ => {}
            },
            // Cursor horizontal absolute / relative movement.
            'G' => self.cursor = amount.saturating_sub(1),
            'C' => self.cursor = self.cursor.saturating_add(amount),
            'D' => self.cursor = self.cursor.saturating_sub(amount),
            _ => {}
        }
    }

    fn put(&mut self, character: char) {
        if self.cursor < self.line.len() {
            self.line[self.cursor] = character;
        } else {
            self.line.push(character);
        }
        self.cursor += 1;
    }

    fn flush_partial(&mut self) -> Option<String> {
        if self.alternate_screen {
            self.line.clear();
            self.cursor = 0;
            self.utf8_pending.clear();
            return None;
        }
        if !self.utf8_pending.is_empty() {
            self.utf8_pending.clear();
            self.put('\u{fffd}');
        }
        if self.line.is_empty() {
            return None;
        }
        let line = self.line.iter().collect();
        self.line.clear();
        self.cursor = 0;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::TranscriptParser;

    #[test]
    fn transcript_removes_ansi_and_preserves_crlf() {
        let mut parser = TranscriptParser::default();
        let lines = parser.feed(b"\x1b[32mgreen\x1b[0m\r\nnext\r\x1b[Kdone\n");
        assert_eq!(lines, vec!["green".to_string(), "done".to_string()]);
    }

    #[test]
    fn shell_prompt_includes_identity_and_abbreviated_current_path() {
        use super::format_shell_prompt;
        use std::path::Path;

        assert_eq!(
            format_shell_prompt(
                "jordan",
                "server",
                Path::new("/home/jordan/Projects/OPERATOR"),
                Some(Path::new("/home/jordan"))
            ),
            "jordan@server:~/Projects/OPERATOR$ "
        );
        assert_eq!(
            format_shell_prompt("jordan", "server", Path::new("/tmp"), Some(Path::new("/home/jordan"))),
            "jordan@server:/tmp$ "
        );
    }

    #[test]
    fn alternate_screen_redraws_are_not_added_to_transcript() {
        let mut parser = TranscriptParser::default();
        assert!(parser.feed(b"before\\n\\x1b[?1049hscreen contents\\nmore screen").len() == 1);
        assert!(parser.flush_partial().is_none());
        let lines = parser.feed(b"\\x1b[?1049lafter\\n");
        assert_eq!(lines, vec!["after".to_string()]);
    }

    #[test]
    fn transcript_preserves_utf8_split_across_reads() {
        let mut parser = TranscriptParser::default();
        let bytes = "café\n".as_bytes();
        let split = bytes.len() - 2;
        assert!(parser.feed(&bytes[..split]).is_empty());
        let lines = parser.feed(&bytes[split..]);
        assert_eq!(lines, vec!["café".to_string()]);
    }
}
