mod app;
mod shell;
mod storage;
mod theme;
mod ui;

use std::{
    io::{self, stdout, Write},
    path::PathBuf,
    time::Duration,
};

use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, InputMode};
use shell::ShellSession;
use storage::Journal;

#[derive(Debug, Parser)]
#[command(name = "streamnote", version, about = "A live Debian shell transcript and unwrapped note stream")]
struct Args {
    /// Journal file. Defaults to ~/.local/share/streenote/journal.jsonl
    #[arg(short, long)]
    file: Option<PathBuf>,
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        if let Err(error) = execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            ),
            crossterm::cursor::Hide
        ) {
            // Best-effort rollback in case the keyboard-protocol push was sent
            // before a later terminal command failed.
            let _ = execute!(out, PopKeyboardEnhancementFlags);
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut out = stdout();
        let _ = execute!(
            out,
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let journal_path = args.file.unwrap_or_else(default_journal_path);
    let journal = Journal::open(journal_path)?;
    let startup_message = if journal.skipped_corrupt_lines > 0 {
        format!(
            "Loaded {} entries. Skipped {} malformed journal line(s); original file was left untouched.",
            journal.entries.len(), journal.skipped_corrupt_lines
        )
    } else {
        format!("Loaded {} entries from journal.", journal.entries.len())
    };

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let size = terminal.size()?;
    let shell_rows = size.height.saturating_sub(12).max(8);
    let mut shell = match ShellSession::spawn(size.width, shell_rows) {
        Ok(session) => Some(session),
        Err(_error) => {
            // Note-taking remains available even if the host has no usable shell.
            None
        }
    };

    let mut app = App::new(journal, startup_message);
    match shell.as_ref() {
        Some(session) => app.record_system(format!(
            "Connected to interactive shell ({}) · commands are recorded once; PTY input echo is disabled.",
            session.shell_program().display()
        )),
        None => {
            app.set_mode(InputMode::Notes);
            app.record_system(
                "Shell unavailable. Notes mode is active; install /bin/bash or set STREAMNOTE_SHELL.".to_string(),
            );
        }
    }

    let result = run(&mut terminal, &mut app, &mut shell);
    // Drain any output already buffered before the shell is stopped.
    drain_shell(&mut app, &mut shell)?;
    let save_result = app.journal.save_pending();
    drop(shell);
    drop(terminal);
    drop(_guard);

    result?;
    save_result?;
    Ok(())
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    shell: &mut Option<ShellSession>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        drain_shell(app, shell)?;
        if let Some(session) = shell.as_ref() {
            app.set_terminal_prompt(session.prompt_label());
        }
        terminal.draw(|frame| ui::draw(frame, app))?;

        if !event::poll(Duration::from_millis(40))? {
            continue;
        }

        match event::read()? {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    || (key.kind == KeyEventKind::Repeat
                        && matches!(key.code, KeyCode::Up | KeyCode::Down)) =>
            {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::F(1) {
                    let size = terminal.size()?;
                    match ShellSession::spawn(size.width, size.height) {
                        Ok(mut workspace_session) => {
                            app.record_system(format!(
                                "Opened independent terminal workspace ({}) with its own PTY.",
                                workspace_session.shell_program().display()
                            ));
                            run_terminal_workspace(terminal, app, &mut workspace_session)?;
                            // Dropping this session closes only the workspace shell.
                            // STREENOTE's primary transcript shell remains untouched.
                            app.record_system("Closed independent terminal workspace; returned to STREENOTE.".to_string());
                            terminal.clear()?;
                        }
                        Err(error) => {
                            app.status = format!("Could not start independent terminal workspace: {error}");
                        }
                    }
                } else if handle_key(key.code, key.modifiers, app, shell)? {
                    break;
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    app.begin_transcript_selection(mouse.column, mouse.row);
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.drag_transcript_selection(mouse.column, mouse.row);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.end_transcript_selection(mouse.column, mouse.row);
                }
                MouseEventKind::ScrollUp => {
                    app.scroll_up(if mouse.modifiers.contains(KeyModifiers::SHIFT) { 8 } else { 3 })
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_down(if mouse.modifiers.contains(KeyModifiers::SHIFT) { 8 } else { 3 })
                }
                _ => {}
            },
            Event::Paste(text) => app.paste(&text),
            Event::Resize(cols, rows) => {
                if let Some(session) = shell.as_ref() {
                    let shell_rows = rows.saturating_sub(12).max(8);
                    let _ = session.resize(cols, shell_rows);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Temporarily hand the real terminal to the shell PTY for full-screen programs.
/// F1 returns to the StreeNote transcript workspace.
fn run_terminal_workspace(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App, session: &mut ShellSession) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
    let size = terminal.size()?;
    session.resize(size.width, size.height)?;
    session.set_echo(true)?;
    execute!(out, LeaveAlternateScreen, DisableMouseCapture, crossterm::cursor::Show)?;
    out.flush()?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let raw = session.drain_raw_output();
            if !raw.is_empty() { out.write_all(&raw)?; out.flush()?; }
            if let Some(code) = session.check_exit()? { app.record_system(format!("Workspace shell exited with status {code}.")); break; }
            if !event::poll(Duration::from_millis(12))? { continue; }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
                    if key.kind == KeyEventKind::Press && key.code == KeyCode::F(1) { break; }
                    let bytes = encode_terminal_key(key.code, key.modifiers);
                    if !bytes.is_empty() { session.send_bytes(&bytes)?; }
                }
                Event::Paste(text) => session.send_bytes(text.as_bytes())?,
                Event::Resize(cols, rows) => session.resize(cols, rows)?,
                _ => {}
            }
        }
        Ok(())
    })();
    let echo_result = session.set_echo(false);
    execute!(out, EnterAlternateScreen, EnableMouseCapture, crossterm::cursor::Hide)?;
    terminal.clear()?;
    if let Err(error) = echo_result { return Err(Box::new(error)); }
    result
}

fn encode_terminal_key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(ch) = code {
            let upper = ch.to_ascii_uppercase();
            if upper.is_ascii_uppercase() { return vec![upper as u8 - b'@']; }
            if ch == ' ' { return vec![0]; }
        }
    }
    let seq: &[u8] = match code {
        KeyCode::Enter => b"\r", KeyCode::Tab => b"\t", KeyCode::Backspace => b"\x7f", KeyCode::Esc => b"\x1b",
        KeyCode::Up => b"\x1b[A", KeyCode::Down => b"\x1b[B", KeyCode::Right => b"\x1b[C", KeyCode::Left => b"\x1b[D",
        KeyCode::Home => b"\x1b[H", KeyCode::End => b"\x1b[F", KeyCode::PageUp => b"\x1b[5~", KeyCode::PageDown => b"\x1b[6~",
        KeyCode::Delete => b"\x1b[3~", KeyCode::Insert => b"\x1b[2~", KeyCode::F(2) => b"\x1bOQ", KeyCode::F(3) => b"\x1bOR",
        KeyCode::F(4) => b"\x1bOS", KeyCode::F(5) => b"\x1b[15~", KeyCode::F(6) => b"\x1b[17~", KeyCode::F(7) => b"\x1b[18~",
        KeyCode::F(8) => b"\x1b[19~", KeyCode::F(9) => b"\x1b[20~", KeyCode::F(10) => b"\x1b[21~", KeyCode::F(11) => b"\x1b[23~", KeyCode::F(12) => b"\x1b[24~",
        KeyCode::Char(ch) => { let mut bytes = Vec::new(); if modifiers.contains(KeyModifiers::ALT) { bytes.push(0x1b); } let mut buf = [0;4]; bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()); return bytes; },
        _ => b"",
    };
    seq.to_vec()
}
/// Returns true when the application should exit.
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    shell: &mut Option<ShellSession>,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Unmodified F2-F10 select the nine built-in color themes.
    if let Some(number) = theme_number_from_key(code, modifiers) {
        app.set_theme(number);
        return Ok(false);
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') => {
                if let Some(selected) = app.selected_transcript_text() {
                    if selected.len() > 60_000 {
                        app.status = "Selection exceeds the 60 KB terminal clipboard limit.".to_string();
                    } else {
                        match copy_to_terminal_clipboard(&selected) {
                            Ok(()) => {
                                app.status = format!(
                                    "Sent {} characters to the terminal clipboard via OSC 52.",
                                    selected.chars().count()
                                );
                            }
                            Err(error) => {
                                app.status = format!("Could not copy transcript selection: {error}");
                            }
                        }
                    }
                } else if app.input_mode == InputMode::Terminal {
                    if let Some(session) = shell.as_mut() {
                        match session.send_control(0x03) {
                            Ok(()) => app.status = "Sent Ctrl+C to the Debian shell.".to_string(),
                            Err(error) => app.status = format!("Could not interrupt shell: {error}"),
                        }
                    }
                } else {
                    app.clear_input();
                    app.status = "Draft cleared. Use Ctrl+Q to quit.".to_string();
                }
            }
            KeyCode::Char('d') if app.input_mode == InputMode::Terminal && app.input.is_empty() => {
                if let Some(session) = shell.as_mut() {
                    let _ = session.send_control(0x04);
                    app.status = "Sent Ctrl+D (end-of-input) to the Debian shell.".to_string();
                }
            }
            KeyCode::Char('s') => app.save(),
            KeyCode::Char('p') => app.history_previous(),
            KeyCode::Char('n') => app.history_next(),
            KeyCode::Char('l') => app.jump_to_latest(),
            KeyCode::Char('a') => app.cursor_to_start(),
            KeyCode::Char('e') => app.cursor_to_end(),
            KeyCode::Char('f') => app.focus_search_hint(),
            KeyCode::Char('t') => app.toggle_mode(),
            KeyCode::Char('w') => {
                app.wrap_transcript = !app.wrap_transcript;
                if app.wrap_transcript {
                    app.feed_hscroll = 0;
                }
                app.status = format!(
                    "Transcript word wrap {}.",
                    if app.wrap_transcript { "enabled" } else { "disabled" }
                );
            }
            _ => {}
        }
        return Ok(false);
    }

    match code {
        KeyCode::Esc => {
            if !app.input.is_empty() {
                app.clear_input();
                app.status = "Input line cleared.".to_string();
            } else if app.search_query.is_some() {
                app.clear_search();
            } else {
                app.show_help = false;
            }
        }
        KeyCode::F(12) => {
            app.show_banner = !app.show_banner;
            app.status = if app.show_banner {
                "Banner shown.".to_string()
            } else {
                "Banner hidden.".to_string()
            };
        }
        KeyCode::Enter => app.submit(shell.as_mut()),
        KeyCode::Tab => app.tab_complete(shell.as_ref()),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => app.scroll_feed_left(8),
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => app.scroll_feed_right(8),
        KeyCode::Left => app.cursor_left(),
        KeyCode::Right => app.cursor_right(),
        KeyCode::Home => app.scroll_to_top(),
        KeyCode::End => app.select_latest_row(),
        KeyCode::Up if modifiers.contains(KeyModifiers::SHIFT) => app.extend_row_selection(-1),
        KeyCode::Down if modifiers.contains(KeyModifiers::SHIFT) => app.extend_row_selection(1),
        KeyCode::Up => app.move_row_selection(-1),
        KeyCode::Down => app.move_row_selection(1),
        KeyCode::PageUp => app.scroll_up(app.feed_viewport_height.max(1)),
        KeyCode::PageDown => app.scroll_down(app.feed_viewport_height.max(1)),
        KeyCode::Char('n') if app.input.is_empty() && !app.matches.is_empty() => app.next_match(),
        KeyCode::Char('N') if app.input.is_empty() && !app.matches.is_empty() => app.previous_match(),
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::ALT) => app.insert_char(character),
        _ => {}
    }
    Ok(app.should_quit)
}

fn drain_shell(
    app: &mut App,
    shell: &mut Option<ShellSession>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(session) = shell.as_mut() else {
        app.update_secret_input(None, &[]);
        return Ok(());
    };

    let output = session.drain_output();
    app.update_secret_input(session.input_is_echoed(), &output);
    if !output.is_empty() {
        app.capture_terminal_output(output);
    }

    if let Some(exit_code) = session.check_exit()? {
        if let Some(final_line) = session.finish_output() {
            app.capture_terminal_output(vec![final_line]);
        }
        app.set_mode(InputMode::Notes);
        app.record_system(format!("Debian shell exited with status {exit_code}. Switched to Notes mode."));
    }
    Ok(())
}

fn copy_to_terminal_clipboard(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text.as_bytes());
    let mut output = stdout();
    // OSC 52 is supported by many modern terminal emulators, including over SSH.
    write!(output, "\x1b]52;c;{encoded}\x07")?;
    output.flush()
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);

        encoded.push(TABLE[(first >> 2) as usize] as char);
        encoded.push(TABLE[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(third & 0b0011_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn default_journal_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join(".local")
        .join("share")
        .join("streenote")
        .join("journal.jsonl")
}


/// Map the unmodified F2-F10 keys to the 1-based theme indexes.
fn theme_number_from_key(code: KeyCode, modifiers: KeyModifiers) -> Option<usize> {
    if modifiers != KeyModifiers::NONE {
        return None;
    }

    match code {
        KeyCode::F(key @ 2..=10) => Some((key - 1) as usize),
        _ => None,
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::base64_encode;

    #[test]
    fn base64_encodes_empty_ascii_and_utf8_text() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode("café".as_bytes()), "Y2Fmw6k=");
    }
}

#[cfg(test)]
mod theme_shortcut_tests {
    use super::theme_number_from_key;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn maps_f2_through_f10_to_the_nine_themes() {
        for function_key in 2..=10 {
            assert_eq!(
                theme_number_from_key(KeyCode::F(function_key), KeyModifiers::NONE),
                Some((function_key - 1) as usize)
            );
        }
    }

    #[test]
    fn ignores_modified_function_keys() {
        for function_key in 2..=10 {
            assert_eq!(
                theme_number_from_key(KeyCode::F(function_key), KeyModifiers::ALT),
                None
            );
            assert_eq!(
                theme_number_from_key(KeyCode::F(function_key), KeyModifiers::CONTROL),
                None
            );
            assert_eq!(
                theme_number_from_key(KeyCode::F(function_key), KeyModifiers::SHIFT),
                None
            );
        }
    }

    #[test]
    fn leaves_non_theme_function_keys_unmapped() {
        for function_key in [1, 11, 12] {
            assert_eq!(
                theme_number_from_key(KeyCode::F(function_key), KeyModifiers::NONE),
                None
            );
        }
    }

    #[test]
    fn ignores_number_row_shortcuts() {
        for number in 1..=9 {
            let digit = char::from_digit(number, 10).unwrap();
            assert_eq!(
                theme_number_from_key(KeyCode::Char(digit), KeyModifiers::ALT),
                None
            );
            assert_eq!(
                theme_number_from_key(KeyCode::Char(digit), KeyModifiers::NONE),
                None
            );
        }
    }
}
