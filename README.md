# StreeNote

A Rust terminal workspace combining a live **Debian shell**, a persistent command/output transcript, and a fast append-only note stream. Built with **Ratatui**, **Crossterm**, and **portable-pty**.

### Bidirectional clipboard over SSH

STREENOTE synchronizes clipboard operations in both directions:

- **Ctrl+C in STREENOTE** stores the copied text in a private per-user server cache at `~/.local/share/streenote/clipboard.txt` (mode 0600) and sends OSC 52 to update the attached client terminal clipboard.
- **Ctrl+V / right-click in the input panel** first asks the client terminal for its clipboard using an OSC 52 read query. The text is cached on the current server and inserted at the input cursor.
- If the terminal denies or does not support OSC 52 reads, STREENOTE falls back to the current server's clipboard cache and then server-side clipboard tools.
- The terminal's own paste shortcut continues to work through bracketed-paste events and also refreshes the server cache.

OSC 52 clipboard **reads are security-sensitive**: a terminal may prompt for permission or disable them. They are supported by only some terminal emulators; OSC 52 writes are more widely supported. Over SSH or across multiple servers, OSC 52 is the mechanism that carries copied text through the client terminal; the private file cache is local to each server.

For tmux, STREENOTE's OSC 52 writes from inside a pane need clipboard writes from pane applications enabled:

```tmux
set -s set-clipboard on
```

**Security note:** this permits applications running inside tmux panes to set the client clipboard. Use it only in sessions where you trust the programs you run.

On **tmux 3.7c or newer**, enable client clipboard reads as well:

```tmux
set -s get-clipboard request
```

`get-clipboard request` makes tmux ask its most recently used client for the clipboard when an application in a pane sends an OSC 52 read query. This is the setting that enables Ctrl+V/right-click to retrieve the client clipboard through tmux. Older tmux versions do not have this option; use the terminal emulator's normal paste shortcut (bracketed paste) or the per-server cache there.

Inside tmux, verify `tmux info | grep Ms` reports the OSC 52 capability. Depending on the outer terminal and `TERM`, you may need a `terminal-features` or `terminal-overrides` entry. Clipboard reads may prompt for approval and are subject to terminal-emulator security settings. See the [tmux clipboard guide](https://github.com/tmux/tmux/wiki/Clipboard) for terminal-specific setup.

**Nested SSH / F1 workspace:** the F1 workspace forwards raw terminal input bytes to its child PTY, rather than parsing and re-encoding them. That lets OSC 52 clipboard replies from the client pass through an outer STREENOTE session into a second STREENOTE instance on another SSH server. F1 itself remains reserved to return to the parent transcript.

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

When STREENOTE is installed as an SSH login shell, SSH non-interactive requests are dispatched before the TUI starts. SFTP subsystem requests execute the configured external SFTP server directly with the original SSH stdin/stdout/stderr; legacy SCP server requests are delegated to Bash. Ordinary remote SSH commands are also run through Bash. No transfer data enters the STREENOTE PTYs, terminal renderer, transcript, or journal.

Use `/scp on`, `/scp off`, or `/scp status` inside STREENOTE to control SCP/SFTP passthrough for new SSH connections. The bottom hint line continuously displays `/scp ON` or `/scp OFF` so the active setting is visible without opening the status command. It is enabled by default and persisted in `~/.config/streenote/config.json` (or `$XDG_CONFIG_HOME/streenote/config.json`). The same file stores `theme_index`, `wrap_transcript`, and `show_banner`; F2–F10, Ctrl+W, and F12 save their changes immediately. Disabling it rejects recognized SFTP subsystem and legacy SCP server requests; it does not disable ordinary remote SSH commands.

Press F1 to open a separate interactive terminal workspace backed by its own independent PTY and shell process. Full-screen programs such as `vim`, `nano`, `less`, `top`, and `htop` use the terminal emulator normally. Press F1 again to return to the transcript; closing the workspace does not interrupt STREENOTE's primary shell. Workspace terminal output is displayed directly and is not added to the journal. Opening, closing, and exiting the F1 workspace do not create journal entries. The primary STREENOTE shell and its transcript continue independently.

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

- ASCII-art banner that can be shown or hidden with F12, plus a live one-line input footer; banner visibility is restored at startup.
- Transcript display can toggle between unwrapped, horizontally clipped rows and word-wrapped rows with Ctrl+W; the selected mode is restored at startup.
- The selected F2–F10 color theme is restored at startup; theme, word-wrap, banner visibility, and SCP/SFTP passthrough preferences are saved together in the same config file.
- A subdued Canadian maple-leaf watermark is centered behind the transcript, scaled to an 80% viewport region, and drawn only into unused cells so transcript text and selection remain readable; toggle it with `/canada`, or use `/canada on|off|status`.
- Every row displays its full local date and time, including seconds and the UTC offset.
- Click a `CMD` transcript row to select it and load its command into the editable input line; clicking notes or output rows does not change the input. Drag selection still works across any transcript rows.
- Left-click and drag to select text across rows without changing the current input draft.
- Ctrl+C copies an explicit transcript selection through OSC 52. With a keyboard-highlighted HUSR row and no active selection, Ctrl+C opens that prompt's Hermes thread; Ctrl+C in the expanded thread copies the full thread with all live deltas joined into one uninterrupted assistant message.
- Up/Down moves a highlighted row through the transcript and keeps it visible; highlighting a `CMD` row automatically loads that command into the editable input line. Holding either key repeats row navigation.
- `Alt+Left` / `Alt+Right` horizontally pan the feed.
- Unbounded in-memory `Vec<Entry>` with no application-level cache eviction.
- JSON Lines persistence; non-secret commands, notes, useful terminal output records, and system events are appended to the journal. Empty terminal-output rows and standard shell-prompt rows are filtered both when loading older journals and when appending new output; existing journal files are not rewritten.
- `/search <terms>` searches the complete text of journal entries and every Hermes conversation event, including assistant responses, tool arguments/results, prior context, and coalesced live-stream text. A hit in a hidden Hermes event opens its conversation at the matching message.
- `n` / `N` navigate search hits.
- Command/input history, Bash-powered Tab completion for commands and filesystem paths, nine high-contrast color palettes, export, stats, path display, and save/retry.
- Older journal entries without a `kind` field remain compatible and load as `note` entries.


## Hermes CLI conversation capture

STREENOTE captures Hermes CLI prompts, assistant responses, tool calls/results, and live text-stream deltas in the same chronological JSONL journal as notes and shell activity. Session and turn identifiers correlate those events into expandable conversations; this uses Hermes hooks rather than scraping terminal rendering or maintaining a second conversation journal.

The primary shell PTY and the independent F1 terminal workspace both receive `STREENOTE_HERMES_INBOX` directly from Rust when STREENOTE creates their `CommandBuilder`. The variable name is centralized as `HERMES_INBOX_ENV` in `src/hermes_bridge.rs`, and the same Rust path resolver is used by the journal importer and every managed PTY. An absolute `STREENOTE_HERMES_INBOX` already present in STREENOTE's own environment overrides the default; otherwise the path is resolved from `XDG_DATA_HOME`, then `HOME/.local/share`. No export in `.bashrc`, a shell startup file, or the F1 launcher is required.

Processes launched from either STREENOTE-managed PTY inherit this variable normally. Launch `hermes --tui` from the F1 workspace and CLI-launched child processes can write to the same inbox consumed by the STREENOTE instance that opened that workspace. STREENOTE imports those events into the parent journal while the F1 workspace is running; it does not create a separate workspace journal. Events with the same Hermes `session_id` are grouped as one conversation across multiple `turn_id` values.

**Separate Hermes gateway services are a different process tree.** A systemd-managed Hermes gateway and its hook subprocesses cannot inherit environment variables from the F1 PTY. When Hermes executes hooks through such a gateway, configure the hook command to use a small wrapper that exports `STREENOTE_HERMES_INBOX` to the same resolved inbox path before executing `streenote-capture.py`. That wrapper is a gateway adapter only; Rust-side PTY injection remains the standard contract for programs and tools launched from STREENOTE shells.

Install the hook script and a small gateway adapter:

```bash
mkdir -p ~/.hermes/agent-hooks
install -m 700 contrib/hermes/streenote-capture.py \
  ~/.hermes/agent-hooks/streenote-capture.py

cat > ~/.hermes/agent-hooks/streenote-capture <<'SH'
#!/bin/sh
: "${XDG_DATA_HOME:=$HOME/.local/share}"
export STREENOTE_HERMES_INBOX="${STREENOTE_HERMES_INBOX:-$XDG_DATA_HOME/streenote/hermes-inbox}"
exec "$HOME/.hermes/agent-hooks/streenote-capture.py"
SH
chmod 700 ~/.hermes/agent-hooks/streenote-capture
```

The wrapper explicitly supplies the inbox path when hooks run in a separate systemd-managed gateway. Merge the following into the existing `hooks:` block in `~/.hermes/config.yaml`. Replace `/home/jordan` with the actual account home directory. Do not create a duplicate top-level `hooks:` key if one already exists.

```yaml
hooks:
  pre_tool_call:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  post_tool_call:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  pre_llm_call: 
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  post_llm_call:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_stream_start:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_stream_delta:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_stream_end:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_interim_message:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_session_start:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_session_end:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_session_finalize:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  on_session_reset:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3

  post_compaction:
    - command: "/home/jordan/.hermes/hooks/streenote-capture"
      timeout: 3
```

Hermes requests first-use approval for each hook event/command combination. Review and approve the STREENOTE capture script when prompted, then inspect the result:

```bash
hermes hooks list
hermes hooks doctor
```

Rebuild and restart STREENOTE so its Rust-created PTYs receive the inbox variable, then launch Hermes from that shell. If your Hermes configuration routes hooks through a separate gateway service, point the hook command at a gateway wrapper that exports the same inbox path; see the process-boundary note above.

- `pre_llm_call` captures the user's prompt as a `hermes_user` entry and, once per session, any prior user/assistant/tool history as a `hermes_history` entry (`HCTX`).
- `post_llm_call` captures the final assistant response as a `hermes_assistant` entry.
- `pre_tool_call` captures the tool name and arguments as a `hermes_tool_call` entry.
- `post_tool_call` captures the tool result, status, duration, and error details (when present) as a `hermes_tool_result` entry.
- On the first observed `pre_llm_call` for a session, STREENOTE imports a one-time snapshot of the existing `conversation_history`. This lets a resumed Hermes session bring its earlier user, assistant, and tool messages into the same thread instead of starting with a partial transcript. The snapshot is stored once per session; subsequent turns append only new events.
- Capture begins when Hermes fires its hook for a submitted turn. Text typed into the Hermes input editor but not submitted is not visible to shell hooks and is not captured.
- The history snapshot excludes system and developer messages. Message bodies, tool calls, and tool results pass through the same best-effort redaction before capture. The snapshot is capped at 1 MiB of rendered text; the inbox accepts events up to 8 MiB.
- `on_stream_start`, `on_stream_delta`, and `on_stream_end` capture assistant text incrementally. Reasoning deltas are intentionally excluded.
- `on_interim_message` captures user-visible interim assistant messages. `on_session_start` records creation, `on_session_end` records each turn's completion/interruption outcome, `on_session_finalize` and `on_session_reset` record session teardown/boundaries, and `post_compaction` records context compaction metadata.
- The capture adapter stores `session_id`, `turn_id`, `tool_call_id`, and stream iteration metadata so prompts, responses, and tool events remain correlated.
- Stream deltas are written durably as separate JSONL events but coalesced into one in-memory assistant bubble per turn/iteration; the thread viewer therefore updates live without creating one visible row per token chunk.
- Tool-call and tool-result records share the tool's `tool_call_id` for correlation; pre/post event IDs remain distinct for retry deduplication.
- Tool argument and result previews are capped at 48 KiB each. Common secret-bearing object keys and common inline token/password assignments are redacted on a best-effort basis; this is not a complete secret filter.
- Interrupted or unsuccessful turns can have a prompt entry without a final-answer entry. A tool call that never reaches its post-hook can likewise have a call entry without a result entry.
- Full Markdown is retained in JSONL. Prior history snapshots appear as `HCTX`, interim messages as `HINT`, and session lifecycle events as `HSES`; they are included in the expanded thread and thread copy/export. The main transcript shows HUSR prompts but hides other Hermes events, including HCTX, HINT, and HSES. With an empty input bar, Enter on a highlighted HUSR row opens its complete conversation; when the input bar contains text, Enter submits that input instead. Ctrl+C on a highlighted HUSR row also opens its conversation. `/threads` opens the conversation index (Enter or Ctrl+C expands the selected thread). The expanded view includes user prompts, tool calls/results, and one uninterrupted `ASSISTANT - LIVE STREAM` block per active streamed turn, including while tools are running. Stream deltas across iterations are concatenated. Once a final assistant event arrives after the latest stream and tool activity, it replaces the live block so the completed answer appears once without duplication. If a turn is interrupted before a final assistant event arrives, its captured live text remains visible. The viewer follows new stream deltas automatically while it is open. `/view` remains available for an individual event's full body.
- Hermes events use stable event IDs for retry deduplication. Those IDs are internal metadata, not journal partitions or visible session labels.

Events are staged in `~/.local/share/streenote/hermes-inbox/ready` (or under `$XDG_DATA_HOME/streenote/hermes-inbox/ready`) with private directory/file permissions. STREENOTE imports them through its in-memory journal and normal append/retry path. Malformed or oversized events are moved to the inbox's `failed` directory.

Conversation prompts and answers may contain sensitive information. Protect the journal and its backups accordingly.

## Hermes final-answer review

STREENOTE can hold Hermes' completed answer until an operator approves the original, edits it, or withholds it. This uses Hermes' `transform_llm_output` plugin hook registered with `hold_stream=True`; Hermes does not stream the unreviewed final answer to the client. The replacement text is returned through the same transform hook, so Hermes persists and delivers the edited response as its actual assistant answer. This does not run another LLM inference.

Install the plugin into the **user-level Hermes plugin directory** on the same host and Unix account that runs the Hermes gateway. A gateway does not load a plugin from the F1 working directory:

```bash
mkdir -p ~/.hermes/plugins/streenote-review
install -m 600 contrib/hermes/streenote-review/plugin.yaml \
  ~/.hermes/plugins/streenote-review/plugin.yaml
install -m 700 contrib/hermes/streenote-review/__init__.py \
  ~/.hermes/plugins/streenote-review/__init__.py
hermes plugins enable streenote-review
```

Restart the Hermes gateway/process after installing and enabling the plugin. By default, both processes use `/run/user/<uid>/streenote/review.sock` when that directory exists and is owned by the current Unix user. This runtime-directory path is independent of `HOME` and `XDG_DATA_HOME`, so a systemd-managed Hermes gateway and an F1-launched STREENOTE session still find the same listener. If the per-user runtime directory is unavailable, both fall back to `$XDG_DATA_HOME/streenote/review.sock`, or `$HOME/.local/share/streenote/review.sock` when `XDG_DATA_HOME` is unset. A custom path can be set with the absolute `STREENOTE_HERMES_REVIEW_SOCKET` environment variable in **both** processes; it takes precedence over the default. The socket directory is created with mode `0700` and the socket with mode `0600`. The transform hook waits synchronously for the operator; its own socket-side deadline is nine minutes. Hermes' `plugins.hook_callback_timeout` setting does not govern this transform hook.

The plugin writes content-free diagnostics (no prompt or answer text) to `/run/user/<uid>/streenote/hermes-review.log` when that per-user runtime directory is available, so the Hermes gateway and STREENOTE read the same log even with different `HOME` or `XDG_STATE_HOME` values. Otherwise it falls back to `$XDG_STATE_HOME/streenote/hermes-review.log`, or `$HOME/.local/state/streenote/hermes-review.log` when `XDG_STATE_HOME` is unset. The log is private to the user (directory mode `0700`, file mode `0600`). It records plugin registration, callback invocation, socket resolution/connection, request delivery, and the operator decision. `/review status` displays the latest recorded activity.

To fail closed when the review listener is unavailable or the socket exchange fails, set this environment variable in the **Hermes gateway service environment** before restarting the gateway:

```bash
systemctl --user set-environment STREENOTE_HERMES_REVIEW_REQUIRED=1
hermes gateway restart
```

With this setting, a missing socket or socket communication failure raises an error and withholds the final answer. An explicit `/review off` is an operator override: STREENOTE returns `bypass`, and the original Hermes answer passes through unchanged even when fail-closed mode is enabled. Fail-closed mode protects against unavailable or broken review infrastructure; it does not override the operator's deliberate choice to disable review. This is opt-in so ordinary Hermes sessions can still run when STREENOTE is not active. To restore optional/pass-through behavior:

```bash
systemctl --user unset-environment STREENOTE_HERMES_REVIEW_REQUIRED
hermes gateway restart
```

In STREENOTE, run:

```text
/review status
/review on
```

Review is disabled by default and its setting is saved by STREENOTE. While a response is held, the review editor takes control of the STREENOTE terminal:

- **F2** releases the exact original answer.
- **F3** releases the edited text.
- **F4** withholds the answer. This does not automatically ask Hermes to regenerate; send a follow-up instruction as a new turn when appropriate.

Use the arrow keys, Home/End, Backspace/Delete, Enter for a newline, and Ctrl+V or terminal paste to edit. Escape does not dismiss or release a held answer. `/review off` returns to pass-through behavior for new final responses; an already pending review still needs an explicit decision.

The STREENOTE listener waits up to nine minutes. If the review is rejected, the channel fails, or the timeout expires, the plugin raises an error and Hermes withholds the draft rather than releasing it. When STREENOTE is not running and the socket does not exist, the plugin passes the response through unchanged by default; with `STREENOTE_HERMES_REVIEW_REQUIRED=1`, it withholds the response instead. An explicit `/review off` always passes new final responses through, including when fail-closed mode is enabled.

This gate covers the **final assistant response**. Hermes commentary between tool calls is not passed through this transform and is hidden on held turns. Tool progress and reasoning are separate display surfaces and are not covered by this approval gate; adjust Hermes' `display.tool_progress` and `display.show_reasoning` settings separately if those surfaces also need to be hidden.

## Requirements

- Debian or another Unix-like OS with a PTY implementation.
- Rust stable toolchain (install with [rustup](https://rustup.rs/)).
- A terminal with mouse support for wheel scrolling and OSC 52 clipboard access for selection copying (support varies by terminal emulator and SSH client).
- `$SHELL` set to a valid interactive shell, or `/bin/bash` available.
- For SSH login-shell integration, `/bin/bash` must be available and the configured external SFTP server path must match sshd's `Subsystem sftp` setting.

## Persistent settings

STREENOTE stores visual and SSH-transfer preferences together in:

```text
~/.config/streenote/config.json
```

Example:

```json
{
  "scp_sftp_passthrough": true,
  "theme_index": 6,
  "wrap_transcript": true,
  "show_banner": false,
  "show_canada_watermark": true
}
```

The theme index ranges from 1 to 9 (F2 through F10). Theme changes, Ctrl+W word-wrap changes, F12 banner changes, `/scp on|off`, and `/canada on|off` changes are saved immediately. `show_canada_watermark` defaults to `true`, preserving the current watermark for existing installations. Older configuration files remain compatible; missing preferences receive their defaults.

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

The palettes are built into StreamNote and require no external theme package or configuration download. F1 opens a full-screen interactive terminal workspace with a separate PTY and shell process; press F1 again to return. The primary STREENOTE shell continues running independently. F12 shows or hides the ASCII-art banner.

## Commands

| Input | Action |
|---|---|
| `shell command` + Enter | Run command in the embedded Debian shell (TERMINAL mode) |
| `ordinary text` + Enter | Append a note (NOTES mode) |
| `Ctrl+T` | Toggle TERMINAL / NOTES input mode |
| `Ctrl+W` | Toggle transcript word wrap on/off |
| `/note text` | Save a note without switching mode |
| `!command` | Run a shell command from NOTES mode |
| `/search phrase` | Search the entire journal and jump to the latest match |
| `/view` | Open the full body of the selected Hermes event |
| `/threads` | Browse expandable Hermes conversations and their complete tool/assistant timeline |
| `n` / `N` | Next / previous search result when the input is empty |
| `/clear` | Clear search and return to the live tail; does not delete data |
| `/save` | Retry pending writes and sync the journal |
| `/export [path]` | Export the complete journal as readable text |
| `/stats` | Show entry count, cached text bytes, pending writes, and hit count |
| `/path` | Show the journal file path |
| `/scp on|off|status` | Enable, disable, or inspect SSH SCP/SFTP passthrough (persistent; enabled by default) |
| `/mode [shell|notes]` | Switch mode; no argument toggles |
| `F1` | Enter/exit the full-screen interactive terminal workspace |
| `F12` | Show or hide the ASCII-art banner |
| `/quit` or `/q` | Save pending records and quit |
| `//text` | Store text beginning with `/` literally as a note |

## Navigation and terminal controls

| Key | Action |
|---|---|
| `↑` / `↓` | Move the highlighted row one record at a time (hold to repeat); highlighted `CMD` entries load into the input line |
| `Shift+↑` / `Shift+↓` | Extend or shrink a full-row transcript selection |
| `PageUp` / `PageDown` | Scroll one viewport while keeping the highlighted row visible |
| `Home` | Oldest record |
| `End` | Select the latest row and load it into input when it is a `CMD` entry |
| `Ctrl+L` | Follow the latest record without replacing the input draft |
| Mouse wheel | Scroll vertically |
| Left-click a `CMD` row | Select the complete command row and load its command into the input line |
| Left-click a `NOTE`, `OUT`, or `SYS` row | No action; the input line is left unchanged |
| Left-click + drag | Select text across one or more transcript rows |
| `Ctrl+C` with a text/row selection | Copy selected transcript text via OSC 52 |
| `Ctrl+C` on a highlighted HUSR row | Open that prompt's expanded Hermes thread |
| `Ctrl+C` in the expanded thread | Copy the full thread with each streamed assistant turn joined into one continuous message |
| `Ctrl+C` without a selection or HUSR row | Send SIGINT to shell (TERMINAL) or clear the draft (NOTES) |
| `Shift` + mouse wheel | Faster vertical scroll |
| `Alt+Left` / `Alt+Right` | Pan feed horizontally |
| `Ctrl+W` | Toggle transcript word wrap on/off |
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

Kinds include `note`, `terminal_command`, `terminal_output`, `system`, `hermes_user`, `hermes_assistant`, `hermes_tool_call`, `hermes_tool_result`, `hermes_stream_start`, `hermes_stream_delta`, `hermes_stream_end`, `hermes_history`, `hermes_interim`, and `hermes_session`. Hermes records also carry optional session, turn, tool-call, and iteration identifiers. Old records without `kind` are treated as notes. The journal is append-only from the application's perspective; malformed lines are skipped at load and counted in the startup status. It is never truncated by StreamNote. Input entered while terminal echo is off is intentionally not included.

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
- Full-screen applications are displayed directly by the host terminal while the independent workspace PTY is active. Their alternate-screen redraws are intentionally excluded from the readable transcript; ordinary output is journaled.
