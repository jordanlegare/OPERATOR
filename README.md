# OPERATOR

A Rust terminal workspace combining a live **Debian shell**, a persistent command/output transcript, and a fast append-only note stream. Built with **Ratatui**, **Crossterm**, and **portable-pty**.

### Bidirectional clipboard over SSH

OPERATOR synchronizes clipboard operations in both directions:

- **Ctrl+C in OPERATOR** stores the copied text in a private per-user server cache at `~/.local/share/operator/clipboard.txt` (mode 0600) and sends OSC 52 to update the attached client terminal clipboard.
- **Ctrl+V / right-click in the input panel** first asks the client terminal for its clipboard using an OSC 52 read query. The text is cached on the current server and inserted at the input cursor.
- If the terminal denies or does not support OSC 52 reads, OPERATOR falls back to the current server's clipboard cache and then server-side clipboard tools.
- The terminal's own paste shortcut continues to work through bracketed-paste events and also refreshes the server cache.

OSC 52 clipboard **reads are security-sensitive**: a terminal may prompt for permission or disable them. They are supported by only some terminal emulators; OSC 52 writes are more widely supported. Over SSH or across multiple servers, OSC 52 is the mechanism that carries copied text through the client terminal; the private file cache is local to each server.

For tmux, OPERATOR's OSC 52 writes from inside a pane need clipboard writes from pane applications enabled:

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

**Nested SSH / F1 workspace:** the F1 workspace forwards raw terminal input bytes to its child PTY, rather than parsing and re-encoding them. That lets OSC 52 clipboard replies from the client pass through an outer OPERATOR session into a second OPERATOR instance on another SSH server. F1 itself remains reserved to return to the parent transcript.

## What's new: embedded Debian shell

OPERATOR starts the configured interactive shell (`$SHELL`, falling back to `/bin/bash`) in a real pseudo-terminal (PTY) when the application initializes. The PTY gives commands a TTY rather than plain stdin/stdout pipes.

- Type a shell command in the footer and press Enter.
- Each non-secret submitted command is recorded once as a `CMD` journal entry. PTY input echo (including newline echo) is disabled, so the submitted command is not duplicated as an `OUT` row. Secret input is still omitted from the journal.
- PTY stdout and stderr are captured as `OUT` entries.
- Visible command output is captured as transcript entries. Blank output rows and standard `user@hostname:current_path$` shell prompts are suppressed because the live prompt is already shown in the input footer.
- The input footer displays `user@hostname:current_path$` and refreshes when the shell changes directories; the home directory is abbreviated as `~`.
- ANSI escape sequences are stripped from the transcript; carriage-return progress updates are represented as their final line.
- Terminal commands, output, notes, and system events all share the same searchable feed and append-only JSONL journal.
- The shell is launched at startup using the user's normal interactive shell configuration.
- PTY dimensions follow the terminal window size.

When OPERATOR is installed as an SSH login shell, SSH non-interactive requests are dispatched before the TUI starts. SFTP subsystem requests execute the configured external SFTP server directly with the original SSH stdin/stdout/stderr; legacy SCP server requests are delegated to Bash. Ordinary remote SSH commands are also run through Bash. No transfer data enters the OPERATOR PTYs, terminal renderer, transcript, or journal.

Use `/scp on`, `/scp off`, or `/scp status` inside OPERATOR to control SCP/SFTP passthrough for new SSH connections. The bottom hint line continuously displays `/scp ON` or `/scp OFF` so the active setting is visible without opening the status command. It is enabled by default and persisted in `~/.config/operator/config.json` (or `$XDG_CONFIG_HOME/operator/config.json`). The same file stores `theme_index`, `wrap_transcript`, and `show_banner`; F2–F10, Ctrl+W, and F12 save their changes immediately. Disabling it rejects recognized SFTP subsystem and legacy SCP server requests; it does not disable ordinary remote SSH commands.

Press F1 to open a separate interactive terminal workspace backed by its own independent PTY and shell process. Full-screen programs such as `vim`, `nano`, `less`, `top`, and `htop` use the terminal emulator normally. Press F1 again to return to the transcript; closing the workspace does not interrupt OPERATOR's primary shell. Workspace terminal output is displayed directly and is not added to the journal. Opening, closing, and exiting the F1 workspace do not create journal entries. The primary OPERATOR shell and its transcript continue independently.

## Modes

The default input mode is **TERMINAL**.

- `Ctrl+T` toggles between **TERMINAL** and **NOTES**.
- In TERMINAL mode, an ordinary submitted line runs in the Debian shell.
- In NOTES mode, an ordinary submitted line becomes a note.
- `/note text` saves a note without changing mode. `/note` by itself switches to NOTES mode.
- Notes are shared live across every running OPERATOR terminal using the same journal path. A note submitted in one SSH terminal appears in the others on their next journal poll (normally within one 40 ms UI cycle). Stable row IDs and the journal's inter-process file lock prevent duplicate replication when terminals write concurrently. This live fan-out applies to notes; shell commands and raw terminal output are not echoed into other active terminals.
- Prefix a line with `!` to run it as a shell command even while in NOTES mode.
- Recognized OPERATOR commands (such as `/search`, `/save`, and `/stats`) are handled locally in either mode.
- Absolute shell paths such as `/usr/bin/ls` are sent to the shell in TERMINAL mode; they are not mistaken for local OPERATOR commands.

## Other features

- Animated, full-screen ASCII credits splash centered to the terminal, with a rolling view of the complete `/help` guide; F12 shows or hides it globally, and visibility is restored at startup.
- Transcript display can toggle between unwrapped, horizontally clipped rows and word-wrapped rows with Ctrl+W; the selected mode is restored at startup.
- The selected F2–F10 color theme is restored at startup; theme, word-wrap, banner visibility, and SCP/SFTP passthrough preferences are saved together in the same config file.
- A subdued Canadian maple-leaf watermark is centered behind the transcript, scaled to an 80% viewport region, and drawn only into unused cells so transcript text and selection remain readable; toggle it with `/canada`, or use `/canada on|off|status`.
- Every row displays its full local date and time, including seconds and the UTC offset.
- Click a `CMD` transcript row to select it and load its command into the editable input line; clicking notes or output rows does not change the input. Drag selection still works across any transcript rows.
- Left-click and drag to select text across rows without changing the current input draft.
- Ctrl+C copies an explicit transcript selection through OSC 52. With a keyboard-highlighted HUSR row and no active selection, Ctrl+C opens that prompt's Hermes thread; Ctrl+C in the expanded thread copies the full thread with all live deltas joined into one uninterrupted assistant message.
- Up/Down moves a highlighted row through the transcript and keeps it visible; highlighting a `CMD` row automatically loads that command into the editable input line. Holding either key repeats row navigation.
- `Alt+Left` / `Alt+Right` horizontally pan the feed.
- Rolling 30-day hot transcript: only the trailing 30 × 24 hours of journal entries are kept in the live in-memory transcript. A periodic sweep removes expired rows from the live view without deleting or rewriting durable history.
- Complete archive retention: JSONL remains append-only; PostgreSQL remains authoritative when configured. Historical records outside the hot window are not loaded into the transcript cache.
- Append-only persistence: local JSONL by default, or PostgreSQL as the authoritative journal when `OPERATOR_DATABASE_URL` is configured. Non-secret commands, notes, useful terminal output records, and system events are retained. Empty terminal-output rows and standard shell-prompt rows are filtered both when loading older journals and when appending new output; existing journal files are not rewritten.
- `/search <terms>` searches the complete history, not just the 30-day transcript. JSONL mode runs a background streaming scan of the full archive and retains only a paged set of lightweight hit offsets/snippets; PostgreSQL mode searches its full-history indexes. Live capture and persistence continue while the separate results workspace is open; press `Esc` to return to the newest live transcript row.
- Local JSONL archive results are newest-first. Use `limit:` and `offset:` to page results; press Enter to load the selected historical record on demand. Adjacent Hermes stream deltas are reassembled for phrase matching.
- `n` / `N` navigate ranked search results; Enter opens the matching Hermes conversation when applicable.
- PostgreSQL search modes include web full-text, literal substring, exact phrase, advanced `tsquery`, and trigram fuzzy matching, with kind/session/conversation/date filters and paged results.
- PostgreSQL search results are rendered from indexed archive records, so records originating in other terminals and records older than the hot window remain discoverable. Search results show indexed snippets and metadata; Enter loads the selected full record on demand (by byte range in JSONL mode or by `record_id` in PostgreSQL mode), rather than mirroring historical bodies into the live cache.
- Command/input history, Bash-powered Tab completion for commands and filesystem paths, nine high-contrast color palettes, export, stats, path display, and save/retry.
- Older journal entries without a `kind` field remain compatible and load as `note` entries.


## Hermes CLI conversation capture

OPERATOR captures Hermes CLI prompts, assistant responses, tool calls/results, and live text-stream deltas in the same chronological JSONL journal as notes and shell activity. The active review plugin captures the original prompt through Hermes' native `pre_llm_call` hook, with independent `pre_api_request` and `post_llm_call` fallback observers; it fans each event out using the live socket registry. Session and turn identifiers correlate those events into expandable conversations; this uses Hermes hooks rather than scraping terminal rendering or maintaining a second conversation journal.

Every SSH-launched OPERATOR instance receives its own `OPERATOR_HERMES_INBOX` under `/run/user/<uid>/operator/review-sessions/inboxes/<instance-id>/`. The variable name is centralized as `HERMES_INBOX_ENV` in `src/hermes_bridge.rs`; Rust injects the per-instance path into the primary shell PTY and the F1 workspace. The shared registry is also passed to Hermes as `OPERATOR_HERMES_REVIEW_SESSION_DIR`. No export in `.bashrc`, a shell startup file, or the F1 launcher is required.

The native review-plugin `pre_llm_call` observer fans the original prompt directly to every registered terminal inbox, and `pre_api_request` plus `post_llm_call` provide independent fallbacks; the shell capture hook remains a redundant prompt path and captures the initial history snapshot. Other shell-hook events fan every assistant event, tool call/result, and stream delta out into a separate private inbox for every live OPERATOR registration. Each TUI therefore imports its own copy instead of competing with other SSH windows to consume one shared queue. Every process appends into the same JSONL journal; an inter-process file lock and incremental event-ID scan ensure the shared file stores each captured event once. Each TUI also tails newly appended Hermes records from that shared journal into its own in-memory view, providing a second live-replication path if a private inbox delivery is delayed or missed. The F1 workspace uses the same per-instance inbox as its parent OPERATOR session. Events with the same Hermes `session_id` are grouped as one conversation across multiple `turn_id` values.

A systemd-managed Hermes gateway and its hook subprocesses cannot inherit environment variables from an SSH PTY. When Hermes executes hooks through such a gateway, configure the hook command to export both `OPERATOR_HERMES_INBOX` (any absolute fallback inbox path) and `OPERATOR_HERMES_REVIEW_SESSION_DIR` (the shared review registry path) before executing `operator-capture.py`. The shell capture hook uses the registry to fan events out to all active terminal inboxes. The review plugin independently captures `pre_llm_call` prompts in-process and retries capture from `pre_api_request` and `post_llm_call` when necessary, so the original prompt does not depend on the gateway's shell-hook subprocess or its inherited environment. Content-free `PROMPT_HOOK_INVOKED`, `PROMPT_FANOUT`, and `PROMPT_CAPTURE_SKIPPED` records identify which path ran and how many inboxes received the event. Both prompt paths use the same event ID when Hermes supplies `turn_id`, and the shared journal deduplicates the result. That wrapper is a gateway adapter only; Rust-side PTY injection remains the standard contract for programs and tools launched from OPERATOR shells.

Install the hook script and a small gateway adapter:

```bash
mkdir -p ~/.hermes/hooks
install -m 700 contrib/hermes/hooks/operator-capture.py \
  ~/.hermes/hooks/operator-capture.py
install -m 700 contrib/hermes/hooks/operator-capture \
  ~/.hermes/hooks/operator-capture
```

The wrapper explicitly supplies the fallback inbox and shared registry when hooks run in a separate systemd-managed gateway. The capture hook discovers the standard `/run/user/<uid>/operator/review-sessions` registry itself, checks legacy socket/XDG/HOME paths as well, and unions inboxes from every valid candidate instead of stopping at the first partial registry. This avoids silently limiting prompts to the fallback inbox when the gateway was started before SSH terminals. Set `OPERATOR_HERMES_CAPTURE_DEBUG=1` in the hook environment temporarily to log the number of terminal inboxes reached (never prompt text). The hook fans events out to the inbox for every currently registered OPERATOR instance. Merge the following into the existing `hooks:` block in `~/.hermes/config.yaml`. Replace `/home/jordan` with the actual account home directory. Do not create a duplicate top-level `hooks:` key if one already exists.

```yaml
hooks:
  pre_tool_call:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  post_tool_call:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  pre_llm_call: 
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  post_llm_call:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_stream_start:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_stream_delta:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_stream_end:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_interim_message:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_session_start:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_session_end:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_session_finalize:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  on_session_reset:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3

  post_compaction:
    - command: "/home/jordan/.hermes/hooks/operator-capture"
      timeout: 3
```

Hermes requests first-use approval for each hook event/command combination. Review and approve the OPERATOR capture script when prompted, then inspect the result:

```bash
hermes hooks list
hermes hooks doctor
```

Rebuild and restart OPERATOR so its Rust-created PTYs receive the inbox variable, then launch Hermes from that shell. If your Hermes configuration routes hooks through a separate gateway service, point the hook command at a gateway wrapper that exports the same inbox path; see the process-boundary note above.

- The review plugin's native `pre_llm_call` hook captures Hermes' original `user_message`—before hook-injected skill/plugin context—as a `hermes_user` entry and writes it to every registered terminal inbox. If that hook omits the prompt, the plugin attempts the same capture from `pre_api_request` and `post_llm_call`, falling back to the last user message in conversation history when needed. The shell hook provides a redundant prompt capture plus initial history snapshot; stable event IDs prevent duplicate journal rows. Text-only prompts are preserved; for multimodal prompts, readable text parts are retained and image/audio/file payloads are represented by safe placeholders rather than storing binary data or data URLs.
- Once per session, the first non-empty `conversation_history` is captured as a `hermes_history` entry (`HCTX`), allowing a resumed session to bring its earlier user/assistant/tool messages into the same thread. The shared journal preserves this history for terminals that connect later; subsequent turns append only their new prompt and conversation events.
- `post_llm_call` captures the final assistant response as a `hermes_assistant` entry.
- `pre_tool_call` captures the tool name and arguments as a `hermes_tool_call` entry.
- `post_tool_call` captures the tool result, status, duration, and error details (when present) as a `hermes_tool_result` entry.
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

Events are staged in `~/.local/share/operator/hermes-inbox/ready` (or under `$XDG_DATA_HOME/operator/hermes-inbox/ready`) with private directory/file permissions. OPERATOR imports them through its in-memory journal and normal append/retry path. Malformed or oversized events are moved to the inbox's `failed` directory.

Conversation prompts and answers may contain sensitive information. Protect the journal and its backups accordingly.

## Hermes final-answer review

OPERATOR can hold Hermes' completed answer until an operator approves the original, edits it, or withholds it. This uses Hermes' `transform_llm_output` plugin hook registered with `hold_stream=True`; Hermes does not stream the unreviewed final answer to the client. The replacement text is returned through the same transform hook, so Hermes persists and delivers the edited response as its actual assistant answer. This does not run another LLM inference.

Install the plugin into the **user-level Hermes plugin directory** on the same host and Unix account that runs the Hermes gateway. A gateway does not load a plugin from the F1 working directory:

```bash
mkdir -p ~/.hermes/plugins/operator-review
install -m 600 contrib/hermes/operator-review/plugin.yaml \
  ~/.hermes/plugins/operator-review/plugin.yaml
install -m 700 contrib/hermes/operator-review/__init__.py \
  ~/.hermes/plugins/operator-review/__init__.py
hermes plugins enable operator-review
```

Before enabling the renamed plugin, disable any previously installed OPERATOR review plugin so two final-response transform hooks are not registered at the same time. Restart the Hermes gateway/process after installing and enabling the plugin. Every OPERATOR process publishes a private Unix socket and registration manifest in `/run/user/<uid>/operator/review-sessions/`. The registry is mode `0700`; sockets and manifests are mode `0600`. This supports simultaneous SSH sessions under the same Unix account. Only the selected `/review on` terminal receives the editable review request; opted-out terminals are not contacted. The plugin acquires one available reviewer using a round-robin scheduler and a per-terminal OS file lock. A terminal can hold only one active review at a time; concurrent answers are assigned to other available reviewers or wait for a free slot. If a terminal disappears before accepting a request, the plugin tries another live reviewer. The OS releases a review lease automatically if the gateway callback exits or crashes. The selected reviewer alone can approve, edit, or reject. The final delivered text is returned directly to Hermes; a rejection withholds the answer. Other connected terminals remain in their normal interactive state. The native review-plugin hook captures each incoming prompt directly; the shell capture hook also fans tool events, assistant events, and text-stream deltas into private inboxes for every active terminal; event IDs and a shared journal lock prevent duplicate durable rows. Set `OPERATOR_HERMES_REVIEW_SESSION_DIR` to the same absolute directory in **both** the Hermes gateway and OPERATOR processes if you override the registry location. If you use the older `OPERATOR_HERMES_REVIEW_SOCKET` override, its parent directory is used as the base for `review-sessions/`. The transform hook waits synchronously for the selected operator; its socket-side deadline is nine minutes. Hermes' `plugins.hook_callback_timeout` setting does not govern this transform hook.

The plugin writes content-free diagnostics (no prompt or answer text) to `/run/user/<uid>/operator/hermes-review.log` when that per-user runtime directory is available, so the Hermes gateway and all OPERATOR instances read the same log even with different `HOME` or `XDG_STATE_HOME` values. Otherwise it falls back to `$XDG_STATE_HOME/operator/hermes-review.log`, or `$HOME/.local/state/operator/hermes-review.log` when `XDG_STATE_HOME` is unset. The log is private to the user (directory mode `0700`, file mode `0600`). It records plugin registration, callback invocation, route selection, socket connection, request delivery, and the operator decision. `/review status` displays the latest recorded activity.

To fail closed when the review listener is unavailable or the socket exchange fails, set this environment variable in the **Hermes gateway service environment** before restarting the gateway:

```bash
systemctl --user set-environment OPERATOR_HERMES_REVIEW_REQUIRED=1
hermes gateway restart
```

With this setting, a missing review pool raises an error and withholds the final answer. An individual `/review off` terminal is simply removed from reviewer eligibility; other connected terminals with `/review on` continue to receive and review responses. If every connected terminal has review disabled, the original answer is broadcast and passed through unchanged. Fail-closed mode protects against unavailable review infrastructure; it does not override the deliberate choice to disable review in every terminal. To restore optional/pass-through behavior:

```bash
systemctl --user unset-environment OPERATOR_HERMES_REVIEW_REQUIRED
hermes gateway restart
```

In OPERATOR, run:

```text
/review status
/review on
```
Reviewer availability is **per terminal session**. New OPERATOR terminals join the active reviewer pool by default. An operator who is busy or unavailable can run `/review off`; this immediately removes only that terminal from new assignments. Running `/review on` rejoins the pool at any time. These commands do not change other terminals and are not written as a shared preference. `/review status` reports this terminal's setting and recent dispatcher diagnostics. The status bar under the OPERATOR banner shows the live number of enabled, connected reviewers across all SSH terminals. It refreshes from the shared session registry every 300 ms, counts only enabled registrations with a live process and socket, and updates on every terminal after an operator changes `/review on` or `/review off`. The adjacent reviews-handled counter is local to that OPERATOR terminal session: it increments when its operator explicitly releases the original answer, releases an edited answer, or withholds a response. It does not include reviews completed by other terminals or reviews that expire without an operator decision; it resets when that OPERATOR process restarts.

Hermes assigns each held final answer to one available `/review on` terminal using a round-robin scheduler. A terminal can handle one review at a time. Concurrent answers are assigned to other eligible terminals when possible; if all eligible terminals are busy, they wait for an available review slot. The dispatcher refreshes each terminal's live registration while waiting, so an operator who runs `/review off` is skipped even when a request is queued. If every connected operator has opted out, Hermes passes the original response through unchanged. The selected reviewer alone sees the red review editor; every other terminal continues operating normally.

- **F2** releases the exact original answer.
- **F3** releases the edited text.
- **F4** withholds the answer. This does not automatically ask Hermes to regenerate; send a follow-up instruction as a new turn when appropriate.
Use Ctrl+A to select the complete held answer; type or paste to replace it. Backspace/Delete remove the selection. The arrow keys collapse the selection and move the cursor. Home/End move within a line, Ctrl+Home/End move to the document boundaries, and Enter inserts a newline. Ctrl+V requests the SSH client's clipboard through OSC 52; Shift+Insert is also supported. The terminal emulator's own Paste command (commonly Ctrl+Shift+V) inserts the pasted text directly through bracketed paste. The review footer displays the paste result or the reason clipboard access was unavailable. Escape does not dismiss or release a held answer. Turning `/review off` while this terminal already owns a pending review prevents future assignments but does not cancel that review; finish it with F2, F3, or F4.

The OPERATOR listener waits up to nine minutes. If the selected reviewer rejects the response, the connection fails after accepting the request, or the review deadline expires, the plugin withholds the draft rather than releasing it. If no OPERATOR listener exists, the plugin passes responses through unchanged by default; with `OPERATOR_HERMES_REVIEW_REQUIRED=1`, it withholds them instead. A single `/review off` terminal does not bypass another terminal's active reviewer; pass-through occurs only when every registered terminal has review disabled.

This gate covers the **final assistant response**. Hermes commentary between tool calls is not passed through this transform and is hidden on held turns. Tool progress and reasoning are separate display surfaces and are not covered by this approval gate; adjust Hermes' `display.tool_progress` and `display.show_reasoning` settings separately if those surfaces also need to be hidden.

## Requirements

- Debian or another Unix-like OS with a PTY implementation.
- Rust stable toolchain (install with [rustup](https://rustup.rs/)).
- A terminal with mouse support for wheel scrolling and OSC 52 clipboard access for selection copying (support varies by terminal emulator and SSH client).
- `$SHELL` set to a valid interactive shell, or `/bin/bash` available.
- For SSH login-shell integration, `/bin/bash` must be available and the configured external SFTP server path must match sshd's `Subsystem sftp` setting.

## Persistent settings

OPERATOR stores visual and SSH-transfer preferences together in:

```text
~/.config/operator/config.json
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
~/.local/share/operator/journal.jsonl
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

The palettes are built into OPERATOR and require no external theme package or configuration download. F1 opens a full-screen interactive terminal workspace with a separate PTY and shell process; press F1 again to return. The primary OPERATOR shell continues running independently. F12 shows or hides the animated, full-screen credits splash.

## Commands

| Input | Action |
|---|---|
| `shell command` + Enter | Run command in the embedded Debian shell (TERMINAL mode) |
| `ordinary text` + Enter | Append a note (NOTES mode) |
| `Ctrl+T` | Toggle TERMINAL / NOTES input mode |
| `Ctrl+W` | Toggle transcript word wrap on/off |
| `/note text` | Save a note without switching mode |
| `!command` | Run a shell command from NOTES mode |
| `/search phrase` | Search the full archive; JSONL scans in the background and PostgreSQL uses full-history indexes |
| `/search --literal TEXT` | Literal case-insensitive substring search across the archive |
| `/search --phrase TEXT` | PostgreSQL phrase search |
| `/search --tsquery QUERY` | Advanced PostgreSQL `tsquery` syntax |
| `/search --fuzzy TEXT` | Trigram similarity search |
| Search filters | PostgreSQL supports `kind:`, `session:`, `conversation:`, `after:`, `before:`; both backends support `limit:` and `offset:` |
| `/view` | Open the full body of the selected Hermes event |
| `/threads` | Browse expandable Hermes conversations and their complete tool/assistant timeline |
| `n` / `N` | Next / previous search result |
| `/clear` | Clear search and return to the live tail; does not delete data |
| `/save` | Retry pending writes and sync the journal |
| `/export [path]` | Export the complete journal as readable text |
| `/export-search [path]` | Export full records from the currently displayed search-results page only; defaults to `operator-search-results.txt` |
| `/export-results [path]` | Alias for `/export-search` |
| `/stats` | Show the 30-day hot-cache entry count/bytes, pending writes, and hit count |
| `/path` | Show the journal file path |
| `/scp on|off|status` | Enable, disable, or inspect SSH SCP/SFTP passthrough (persistent; enabled by default) |
| `/mode [shell|notes]` | Switch mode; no argument toggles |
| `F1` | Enter/exit the full-screen interactive terminal workspace |
| `F12` | Show or hide the animated full-screen credits splash |
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
| `Esc` | Clear input; in PostgreSQL search results return to the live transcript |
| `Ctrl+Q` | Quit OPERATOR |
| `F12` | Show or hide the animated full-screen credits splash |

## Journal format

Each line in `journal.jsonl` is an independent JSON object. Example:

```json
{"timestamp":"2026-10-31T17:30:00-04:00","kind":"terminal_command","text":"uname -a"}
{"timestamp":"2026-10-31T17:30:01-04:00","kind":"terminal_output","text":"Linux debian 6.1.0-amd64 ..."}
{"timestamp":"2026-10-31T17:31:00-04:00","kind":"note","text":"Investigate kernel update"}
```

The transcript renders each stored RFC 3339 timestamp as local `YYYY-MM-DD HH:MM:SS ±HH:MM`, so the date, time and UTC offset are visible for every row.

Kinds include `note`, `terminal_command`, `terminal_output`, `system`, `hermes_user`, `hermes_assistant`, `hermes_tool_call`, `hermes_tool_result`, `hermes_stream_start`, `hermes_stream_delta`, `hermes_stream_end`, `hermes_history`, `hermes_interim`, and `hermes_session`. Hermes records also carry optional session, turn, tool-call, and iteration identifiers. Old records without `kind` are treated as notes. The journal is append-only from the application's perspective; malformed lines are skipped at load and counted in the startup status. It is never truncated by OPERATOR. Input entered while terminal echo is off is intentionally not included.

## Hot transcript and complete archive

OPERATOR separates the live transcript cache from the durable journal:

- **Hot window:** the transcript retains entries from the trailing 30 × 24 hours (a rolling 30-day window, not a calendar month). Expired rows are pruned from memory at startup and during the live session; the running TUI checks for expiry once per minute.
- **Complete history:** the JSONL journal is append-only and is never truncated by hot-cache expiry. PostgreSQL stores and indexes the complete history when configured. Expired records remain available to archive search and export.
- **JSONL archive search:** `/search` runs in a background worker that scans the durable file sequentially. Literal matching is case-insensitive. It keeps only the requested result page as snippets and byte ranges; it does not build a second full-history `Vec<Entry>`. Results are newest-first. Use `limit:` and `offset:` to page through matches. Enter loads one selected historical record from its byte range instead of keeping every historical body in memory.
- **Search-result export:** `/export-search [path]` exports the full records represented by the currently displayed results page, preserving search order; it does not export the complete journal or only the shortened preview snippets. This works with both JSONL and PostgreSQL searches. When no path is supplied, the destination is `operator-search-results.txt` beside the journal path.
- **Hermes streams:** adjacent stream deltas are joined while scanning so a literal phrase can match across delta boundaries. The original durable event rows are preserved.
- **PostgreSQL:** startup hydrates only the last 30 days into the TUI; the database's full-text, literal, phrase, tsquery, and trigram indexes cover the complete archive. JSONL migration import and JSONL export are streamed in batches rather than materializing the entire file/database at once.
- **Persistence:** new journal records are written immediately or queued for retry. PTY output is collected in the background and batched to avoid forcing an fsync for every line during large command output. `/save` retries pending writes and waits for PostgreSQL commits.

The archive remains durable and searchable while the in-memory transcript stays bounded by recency. This is a rolling time window, not a record-count limit; exceptionally high activity during a 30-day period can still require substantial memory.

## Security and privacy

Commands run with the permissions of the user who launched OPERATOR. Terminal output is saved to the journal, which may include paths, host information, command output, or other sensitive data. When the PTY's ECHO flag is off (as with normal `sudo` password prompts), OPERATOR masks the footer and does not save the submitted input to the command journal or input history. This is a safeguard, not a complete secret-management system: avoid entering secrets as ordinary command arguments, and remember that a program can itself print sensitive data to its output. The journal file contains terminal and Hermes activity; protect it with owner-only permissions and restrict access to its containing directory.

## Implementation notes

- PTY stdout and stderr are merged by design; OPERATOR records what the shell terminal visibly emits.
- ANSI escape sequences are removed before storage. This keeps the transcript searchable and avoids injecting terminal control sequences into the UI.
- The full timestamp is retained in the journal and shown in local time with its UTC offset. Mouse selection operates on rendered transcript columns; OSC 52 copying depends on terminal clipboard support.
- PTY data can be emitted in partial chunks. An unfinished visible prompt/line is flushed after a brief idle interval and again when the shell exits.
- UTF-8 characters split across PTY reads are reassembled; invalid byte sequences are represented with U+FFFD.
- Full-screen applications are displayed directly by the host terminal while the independent workspace PTY is active. Their alternate-screen redraws are intentionally excluded from the readable transcript; ordinary output is journaled.


## PostgreSQL journal backend

OPERATOR can use PostgreSQL as its authoritative journal and indexed search backend. The original JSONL format remains supported as a migration/import and export format.

### Enable PostgreSQL

Install PostgreSQL 15 or newer (on Debian, for example, install `postgresql` and `postgresql-contrib`) and create a dedicated, non-superuser database login role. For example, using a local PostgreSQL administrator:

```sql
CREATE ROLE operator LOGIN PASSWORD 'replace-with-a-long-random-password';
CREATE DATABASE operator OWNER operator;
```

The first OPERATOR connection applies the schema in `migrations/0001_postgres_journal.sql` and enables the `pg_trgm` extension. The database role therefore needs permission to create the application tables and the extension in the target database. If your administrator manages extensions centrally, create `pg_trgm` first and grant the role the required table/schema permissions.

Set the connection URL in the process environment (do not commit credentials):

```bash
export OPERATOR_DATABASE_URL='postgresql://operator:password@127.0.0.1/operator?sslmode=disable'
operator
```

Alternatively, pass `--database-url 'postgresql://...'` on startup. The command-line option takes precedence over `OPERATOR_DATABASE_URL`. Use PostgreSQL TLS (for example, `sslmode=require`) when connecting across hosts; the `sslmode=disable` example is for a trusted local connection only. OPERATOR removes `OPERATOR_DATABASE_URL` from the environment of its interactive shell so the database credential is not inherited by shell commands.

When PostgreSQL is configured:

- PostgreSQL is the authoritative store. New notes, commands, terminal output, system records, and Hermes events are written to the database by a dedicated background worker so database operations do not block the TUI event loop.
- On the first connection only, if the database contains no journal records, OPERATOR imports valid records from the legacy JSONL file selected by `--file` (or the default `~/.local/share/operator/journal.jsonl`). Import preserves record IDs, event IDs, original timestamps, and Hermes metadata. An already populated database is not silently merged with a local JSONL file.
- Search uses PostgreSQL GIN full-text and `pg_trgm` indexes. Modes include `websearch_to_tsquery`, `phraseto_tsquery`, `to_tsquery`, literal `LIKE`, and trigram similarity ranking. Results include snippets and relevance scores where applicable, and support kind/session/conversation/date filters plus `limit:`/`offset:` pagination. Streamed Hermes deltas are indexed as a reconstructed logical message so searches can span delta boundaries. Searches are submitted asynchronously and replace the transcript window until Escape returns to LIVE.
- `LISTEN/NOTIFY` wakes connected OPERATOR instances after commits. Each instance also polls the monotonically increasing database ID as a recovery path if a notification is missed or its listener disconnects.
- Notes and Hermes events synchronize live between connected OPERATOR instances, matching the existing JSONL behavior. Shell commands, raw PTY output, and system notices remain local to their originating TUI during live sessions; all records are persisted centrally, but only rows from the trailing 30 days are hydrated into each terminal's hot transcript cache. Older rows remain in PostgreSQL and are searchable.
- Enter on a PostgreSQL result loads that record's full indexed body on demand, including a reconstructed Hermes stream message when the hit spans multiple delta rows. This works for historical rows and records written by other terminals; it does not require those bodies to be resident in the local hot cache. Treat database access as access to the complete shared journal; do not share one database between users who should not see one another's commands, output, notes, or Hermes activity.
- `/save` waits for queued database writes to commit. `/export-jsonl [FILE]` exports the original, uncoalesced event stream. `/export [FILE]` continues to produce readable plain text.
- `/path` reports the selected backend and the legacy JSONL import/export path.

Without `OPERATOR_DATABASE_URL` or `--database-url`, OPERATOR continues using its original local append-only JSONL journal. No data is deleted from the JSONL file by enabling PostgreSQL. Keep regular database backups and test restores; JSONL export is a portable additional recovery format, not a substitute for database backup/PITR.
