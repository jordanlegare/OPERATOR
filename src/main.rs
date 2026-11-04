mod app;
mod shell;
mod storage;
mod theme;
mod ui;

use std::{
    io::{self, stdout},
    path::PathBuf,
    time::Duration,
};

use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
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
    /// Journal file. Defaults to ~/.local/share/streamnote/journal.jsonl
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
            crossterm::cursor::Hide
        ) {
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
            "Connected to interactive shell ({}) · commands and combined stdout/stderr are journaled.",
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
        terminal.draw(|frame| ui::draw(frame, app))?;

        if !event::poll(Duration::from_millis(40))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(key.code, key.modifiers, app, shell)? {
                    break;
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
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

/// Returns true when the application should exit.
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    shell: &mut Option<ShellSession>,
) -> Result<bool, Box<dyn std::error::Error>> {
    // Alt+1 through Alt+9 selects the built-in color themes.
    if let Some(number) = theme_number_from_key(code, modifiers) {
        app.set_theme(number);
        return Ok(false);
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') => {
                if app.input_mode == InputMode::Terminal {
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
            _ => {}
        }
        return Ok(false);
    }

    if app.show_help {
        match code {
            KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q') => app.show_help = false,
            _ => {}
        }
        return Ok(false);
    }

    match code {
        KeyCode::Esc => {
            if !app.input.is_empty() {
                app.clear_input();
            } else if app.search_query.is_some() {
                app.clear_search();
            } else {
                app.show_help = false;
            }
        }
        KeyCode::F(1) => app.show_help = true,
        KeyCode::Enter => app.submit(shell.as_mut()),
        KeyCode::Tab => app.tab_complete(shell.as_ref()),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete(),
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => app.scroll_feed_left(8),
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => app.scroll_feed_right(8),
        KeyCode::Left => app.cursor_left(),
        KeyCode::Right => app.cursor_right(),
        KeyCode::Home => app.scroll_to_top(),
        KeyCode::End => app.jump_to_latest(),
        KeyCode::Up => app.scroll_up(1),
        KeyCode::Down => app.scroll_down(1),
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

fn default_journal_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join(".local")
        .join("share")
        .join("streamnote")
        .join("journal.jsonl")
}


/// Map Alt+1..Alt+9 to their 1-based theme indexes.
fn theme_number_from_key(code: KeyCode, modifiers: KeyModifiers) -> Option<usize> {
    if !modifiers.contains(KeyModifiers::ALT)
        || modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    {
        return None;
    }

    match code {
        KeyCode::Char(digit @ '1'..='9') => digit.to_digit(10).map(|number| number as usize),
        _ => None,
    }
}

#[cfg(test)]
mod theme_shortcut_tests {
    use super::theme_number_from_key;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn recognizes_alt_number_keys() {
        for number in 1..=9 {
            let digit = char::from_digit(number, 10).unwrap();
            assert_eq!(
                theme_number_from_key(KeyCode::Char(digit), KeyModifiers::ALT),
                Some(number as usize)
            );
        }
    }

    #[test]
    fn ignores_unmodified_or_wrongly_modified_keys() {
        assert_eq!(theme_number_from_key(KeyCode::Char('0'), KeyModifiers::ALT), None);
        assert_eq!(theme_number_from_key(KeyCode::Char('q'), KeyModifiers::ALT), None);
        assert_eq!(theme_number_from_key(KeyCode::Enter, KeyModifiers::ALT), None);
        assert_eq!(theme_number_from_key(KeyCode::Char('1'), KeyModifiers::NONE), None);
        assert_eq!(theme_number_from_key(KeyCode::Char('1'), KeyModifiers::SHIFT), None);
        assert_eq!(theme_number_from_key(KeyCode::Char('1'), KeyModifiers::CONTROL), None);
        assert_eq!(
            theme_number_from_key(
                KeyCode::Char('1'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
            None
        );
        assert_eq!(
            theme_number_from_key(
                KeyCode::Char('1'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            None
        );
    }
}
