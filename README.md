# StreeNote

A Rust terminal workspace combining a live **Debian shell**, a persistent command/output transcript, and a fast append-only note stream. Built with **Ratatui**, **Crossterm**, and **portable-pty**.

## What's new: embedded Debian shell

StreamNote starts the configured interactive shell (`$SHELL`, falling back to `/bin/bash`) in a real pseudo-terminal (PTY) when the application initializes. The PTY gives commands a TTY rather than plain stdin/stdout pipes.

- Type a shell command in the footer and press Enter.
- Each non-secret submitted command is recorded once as a `CMD` journal entry. PTY input echo (including newline echo) is disabled, so the submitted command is not duplicated as an `OUT` row. Secret input is still omitted from the journal.
- PTY stdout and stderr are captured as `OUT` entries.
- Visible command output is captured as transcript entries. Blank output rows and standard `user@hostname:current_path$` shell prompts are suppressed because the live prompt is already shown in the input footer.
- The input footer displays `user@hostname:current_path$` and refreshes when the shell changes directories; the home directory is abbreviated as `~`.
- ANSI escape sequences are stripped from the transcript; carriage-return progress updates are represented as their final line.
- Terminal commands, output, notes, and system events all share the same searchable feed and append-only JSONL journal.
- The shell is launched at startup using the user's normal interactive shell configuration.
- PTY dimensions follow the terminal window size.

The shell remains a command-oriented transcript interface. It supports normal command lines, shell built-ins, line-oriented prompts, password/passphrase prompts (recognized secret input is masked and never journaled), and Ctrl+C interruption. It does **not** hand the full screen to nested terminal UI programs such as `vim`, `top`, `less`, or `htop`; those programs require a separate raw-screen passthrough/terminal-emulator mode. ANSI screen-control sequences are intentionally not replayed into the log.

## Modes

The default input mode is **TERMINAL**.

- `Ctrl+T` toggles between **TERMINAL** and **NOTES**.
- In TERMINAL mode, an ordinary submitted line runs in the Debian shell.
- In NOTES mode, an ordinary submitted line becomes a note.
- `/note text` saves a note without changing mode. `/note` by itself switches to NOTES mode.
- Prefix a line with `!` to run it as a shell command even while in NOTES mode.
- Recognized StreamNote commands (such as `/search`, `/save`, and `/stats`) are handled locally in either mode.
- Absolute shell paths such as `/usr/bin/ls` are sent to the shell in TERMINAL mode; they are not mistaken for local StreamNote commands.

## Other features

- ASCII-art banner that can be shown or hidden with F12, plus a live one-line input footer.
- Unwrapped feed: each record is one row; long records are horizontally clipped, never word-wrapped.
- Every row displays its full local date and time, including seconds and the UTC offset.
- Click a transcript row to select it and load its exact entry text into the editable input line; the cursor moves to the end so you can edit or resubmit it.
- Left-click and drag to select text across rows without changing the current input draft.
- Ctrl+C copies a selected transcript range through OSC 52; without a selection it retains its normal shell-interrupt / draft-clear behavior.
- Up/Down moves a highlighted row through the transcript and keeps it visible; highlighting a `CMD` row automatically loads that command into the editable input line. Holding either key repeats row navigation.
- `Alt+Left` / `Alt+Right` horizontally pan the feed.
- Unbounded in-memory `Vec<Entry>` with no application-level cache eviction.
- JSON Lines persistence; non-secret commands, notes, useful terminal output records, and system events are appended to the journal. Empty terminal-output rows and standard shell-prompt rows are filtered both when loading older journals and when appending new output; existing journal files are not rewritten.
- `/search <terms>` searches all cached notes, commands, and terminal output case-insensitively and jumps to the latest match with surrounding context.
- `n` / `N` navigate search hits.
- Command/input history, Bash-powered Tab completion for commands and filesystem paths, nine high-contrast color palettes, help overlay, export, stats, path display, and save/retry.
- Older journal entries without a `kind` field remain compatible and load as `note` entries.

## Requirements

- Debian or another Unix-like OS with a PTY implementation.
- Rust stable toolchain (install with [rustup](https://rustup.rs/)).
- A terminal with mouse support for wheel scrolling and OSC 52 clipboard access for selection copying (support varies by terminal emulator and SSH client).
- `$SHELL` set to a valid interactive shell, or `/bin/bash` available.

## Build and run

```bash
cargo build --release
cargo run --release
```

Choose a journal file:

```bash
cargo run --release -- --file ~/Notes/work-log.jsonl
```

By default, the journal is stored at:

```text
~/.local/share/streenote/journal.jsonl
```

## Color themes

Switch palettes directly with the unmodified **F2** through **F10** keys. Each key selects one palette in order; no Ctrl, Alt, or AltGr modifier is required. The selected palette number and name appear in the status line.

| Shortcut | Theme number | High-contrast palette |
|---|---:|---|
| `F2` | 1 | Abyss Cyan |
| `F3` | 2 | Carbon Amber |
| `F4` | 3 | Dracula Contrast |
| `F5` | 4 | Solarized Light |
| `F6` | 5 | Paper High Contrast |
| `F7` | 6 | Forest Signal |
| `F8` | 7 | Violet Pulse |
| `F9` | 8 | Arctic Blue |
| `F10` | 9 | Monochrome Max |

The palettes are built into StreamNote and require no external theme package or configuration download. F1 opens help; F12 shows or hides the ASCII-art banner.

## Commands

| Input | Action |
|---|---|
| `shell command` + Enter | Run command in the embedded Debian shell (TERMINAL mode) |
| `ordinary text` + Enter | Append a note (NOTES mode) |
| `Ctrl+T` | Toggle TERMINAL / NOTES input mode |
| `/note text` | Save a note without switching mode |
| `!command` | Run a shell command from NOTES mode |
| `/search phrase` | Search the entire journal and jump to the latest match |
| `n` / `N` | Next / previous search result when the input is empty |
| `/clear` | Clear search and return to the live tail; does not delete data |
| `/save` | Retry pending writes and sync the journal |
| `/export [path]` | Export the complete journal as readable text |
| `/stats` | Show entry count, cached text bytes, pending writes, and hit count |
| `/path` | Show the journal file path |
| `/mode [shell|notes]` | Switch mode; no argument toggles |
| `/help` or F1 | Open help |
| `F12` | Show or hide the ASCII-art banner |
| `/quit` or `/q` | Save pending records and quit |
| `//text` | Store text beginning with `/` literally as a note |

## Navigation and terminal controls

| Key | Action |
|---|---|
| `↑` / `↓` | Move the highlighted row one record at a time (hold to repeat); highlighted `CMD` entries load into the input line |
| `PageUp` / `PageDown` | Scroll one viewport while keeping the highlighted row visible |
| `Home` | Oldest record |
| `End` | Select the latest row and load it into input when it is a `CMD` entry |
| `Ctrl+L` | Follow the latest record without replacing the input draft |
| Mouse wheel | Scroll vertically |
| Left-click a transcript row | Select the complete row and load its text into the input line |
| Left-click + drag | Select text across one or more transcript rows |
| `Ctrl+C` with a selection | Copy selected transcript text via OSC 52 |
| `Ctrl+C` without a selection | Send SIGINT to shell (TERMINAL) or clear the draft (NOTES) |
| `Shift` + mouse wheel | Faster vertical scroll |
| `Alt+Left` / `Alt+Right` | Pan feed horizontally |
| `Tab` | Complete shell commands and filesystem paths (four-space indent in NOTES mode) |
| `F2` … `F10` | Switch between nine high-contrast color themes (F2 = theme 1; F10 = theme 9) |
| `Ctrl+P` / `Ctrl+N` | Previous / next input-history item |
| `Ctrl+A` / `Ctrl+E` | Move input cursor to start / end |
| `Ctrl+S` | Save / retry pending writes |
| `Ctrl+D` | Send end-of-input to the shell when the terminal input is empty |
| `Esc` | Clear the current input line; with an empty line, clear an active search |
| `Ctrl+Q` | Quit StreamNote |
| `F12` | Show or hide the ASCII-art banner |

## Journal format

Each line in `journal.jsonl` is an independent JSON object. Example:

```json
{"timestamp":"2026-10-31T17:30:00-04:00","kind":"terminal_command","text":"uname -a"}
{"timestamp":"2026-10-31T17:30:01-04:00","kind":"terminal_output","text":"Linux debian 6.1.0-amd64 ..."}
{"timestamp":"2026-10-31T17:31:00-04:00","kind":"note","text":"Investigate kernel update"}
```

The transcript renders each stored RFC 3339 timestamp as local `YYYY-MM-DD HH:MM:SS ±HH:MM`, so the date, time and UTC offset are visible for every row.

Kinds are `note`, `terminal_command`, `terminal_output`, and `system`. Old records without `kind` are treated as notes. The journal is append-only from the application's perspective; malformed lines are skipped at load and counted in the startup status. It is never truncated by StreamNote. Input entered while terminal echo is off is intentionally not included.

## Cache and persistence semantics

The cache is **unbounded by the application**: no old entries are dropped to enforce a configured size. The complete JSONL journal is loaded into memory at startup, and search scans every cached entry. PTY output is collected in the background reader thread and batched for disk persistence to avoid forcing an fsync for every line during large command output.

There is no literal infinite memory on a finite computer. The in-memory vector grows until the process or operating system reaches its available memory limit. The JSONL file is the durable journal. Each normal note and command is appended immediately; PTY output is appended in short batches. Failed appends remain queued for retry via `/save` and on normal exit.

## Security and privacy

Commands run with the permissions of the user who launched StreamNote. Terminal output is saved to the journal, which may include paths, host information, command output, or other sensitive data. When the PTY's ECHO flag is off (as with normal `sudo` password prompts), StreamNote masks the footer and does not save the submitted input to the command journal or input history. This is a safeguard, not a complete secret-management system: avoid entering secrets as ordinary command arguments, and remember that a program can itself print sensitive data to its output. The journal file inherits the creating user's umask and directory permissions.

## Implementation notes

- PTY stdout and stderr are merged by design; StreamNote records what the shell terminal visibly emits.
- ANSI escape sequences are removed before storage. This keeps the transcript searchable and avoids injecting terminal control sequences into the UI.
- The full timestamp is retained in the journal and shown in local time with its UTC offset. Mouse selection operates on rendered transcript columns; OSC 52 copying depends on terminal clipboard support.
- PTY data can be emitted in partial chunks. An unfinished visible prompt/line is flushed after a brief idle interval and again when the shell exits.
- UTF-8 characters split across PTY reads are reassembled; invalid byte sequences are represented with U+FFFD.
- Full-screen applications are not rendered inside the transcript. A future screen-passthrough mode would need to temporarily suspend Ratatui or embed a terminal emulator.
