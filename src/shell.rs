use std::{
    env,
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize, PtySystem};

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

        // A fresh PTY must begin in normal interactive mode. Otherwise a
        // platform/default PTY state with ECHO disabled is mistaken for a
        // password prompt, leaving StreamNote in hidden-input mode at startup.
        set_initial_echo(&*pair.master)?;

        let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/bash".into());
        let mut command = CommandBuilder::new(&shell);

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
            .name("streamnote-pty-reader".to_string())
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
        })
    }

    /// Send one line to the interactive shell's PTY.
    pub fn send_line(&mut self, command: &str) -> io::Result<()> {
        if self.exited {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the shell process has exited",
            ));
        }
        self.writer.write_all(command.as_bytes())?;
        self.writer.write_all(b"\r")?;
        self.writer.flush()
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

    /// Query the PTY's ECHO flag. A false value commonly indicates a password
    /// or other secret prompt; the app then masks the footer and skips journaling
    /// that submitted input.
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

    /// Drain all available PTY output, returning complete visible transcript lines.
    /// ANSI control sequences are removed; carriage-return progress updates are
    /// represented by their final visible line rather than raw escape bytes.
    pub fn drain_output(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while !self.reader_closed {
            match self.receiver.try_recv() {
                Ok(Ok(bytes)) => {
                    self.last_output = Some(Instant::now());
                    lines.extend(self.parser.feed(&bytes));
                }
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

        if self
            .last_output
            .is_some_and(|last| last.elapsed() >= IDLE_FLUSH)
        {
            if let Some(line) = self.parser.flush_partial() {
                lines.push(line);
            }
            self.last_output = None;
        }
        lines
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


/// Put a newly-created PTY into normal interactive echo mode before the
/// child shell starts. The shell can still disable ECHO later for password
/// prompts; input_is_echoed() will detect that transition.
#[cfg(unix)]
fn set_initial_echo(master: &dyn MasterPty) -> io::Result<()> {
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
    attributes.c_lflag |= libc::ECHO | libc::ECHONL;

    // SAFETY: attributes was initialized by tcgetattr and remains valid.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attributes) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_initial_echo(_master: &dyn MasterPty) -> io::Result<()> {
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
                    completed.push(self.line.iter().collect());
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
    fn transcript_preserves_utf8_split_across_reads() {
        let mut parser = TranscriptParser::default();
        let bytes = "café\n".as_bytes();
        let split = bytes.len() - 2;
        assert!(parser.feed(&bytes[..split]).is_empty());
        let lines = parser.feed(&bytes[split..]);
        assert_eq!(lines, vec!["café".to_string()]);
    }
}
