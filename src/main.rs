mod app;
mod audit;
mod archive;
mod paths;
mod postgres;
mod conversations;
mod hermes_bridge;
mod hermes_review;
mod shell;
mod settings;
mod storage;
mod theme;
mod ui;

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, stdout, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::fs::{OpenOptionsExt, PermissionsExt}};

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
use unicode_width::UnicodeWidthStr;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Terminal,
};

use app::{App, ConversationView, InputMode};
use hermes_bridge::{HermesBridge, HERMES_INBOX_ENV};
use hermes_review::HermesReviewServer;
use shell::ShellSession;
use storage::Journal;

#[derive(Debug, Parser)]
#[command(name = "operator", version, about = "A live Debian shell transcript and unwrapped note stream")]
struct Args {
    /// Legacy JSONL import/export path. Defaults to ~/.local/share/operator/journal.jsonl
    #[arg(short, long)]
    file: Option<PathBuf>,
    /// PostgreSQL connection URL. Also read from OPERATOR_DATABASE_URL.
    #[arg(long)]
    database_url: Option<String>,
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
    dispatch_ssh_invocation();
    let args = Args::parse();
    let journal_path = args.file.unwrap_or_else(default_journal_path);
    let database_url = args.database_url.or_else(|| std::env::var("OPERATOR_DATABASE_URL").ok());
    let (journal, startup_message) = if let Some(database_url) = database_url {
        let (journal, imported_count) = Journal::open_postgres(journal_path, database_url)?;
        let message = if imported_count > 0 {
            format!(
                "PostgreSQL journal connected · loaded {} entries · imported {} legacy JSONL records.",
                journal.entries.len(),
                imported_count
            )
        } else {
            format!(
                "PostgreSQL journal connected · loaded {} entries · PostgreSQL is authoritative.",
                journal.entries.len()
            )
        };
        (journal, message)
    } else {
        let journal = Journal::open(journal_path)?;
        let message = if journal.skipped_corrupt_lines > 0 {
            format!(
                "Loaded {} entries. Skipped {} malformed journal line(s); original file was left untouched.",
                journal.entries.len(), journal.skipped_corrupt_lines
            )
        } else {
            format!("Loaded {} entries from JSONL journal.", journal.entries.len())
        };
        (journal, message)
    };

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    show_access_disclaimer(&mut terminal)?;
    let size = terminal.size()?;

    // Start review routing before the PTY so its identity can be inherited by
    // the interactive shell and Hermes' pre_llm_call capture hook.
    let mut review_server = HermesReviewServer::start()?;
    paths::set_compatible_env(
        "OPERATOR_HERMES_REVIEW_SOCKET",
        hermes_review::default_review_socket_path(),
    );
    if let Some(instance_id) = review_server.instance_id() {
        if let Some(registry_dir) = review_server.session_registry_dir() {
            paths::set_compatible_env("OPERATOR_HERMES_REVIEW_SESSION_DIR", &registry_dir);
            // Each SSH-launched TUI consumes its own inbox fan-out. The capture
            // hook writes every Hermes event to every live instance's inbox.
            let instance_inbox = registry_dir.join("inboxes").join(instance_id);
            paths::set_compatible_env(HERMES_INBOX_ENV, instance_inbox);
        }
    }

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
        Some(session) => {
            let client = ssh_client_ip()
                .map(|ip| format!("Client {ip} connected · "))
                .unwrap_or_default();
            app.record_system(format!(
                "{client}Connected to interactive shell ({}) · commands are recorded once; PTY input echo is disabled.",
                session.shell_program().display()
            ));
        }
        None => {
            app.set_mode(InputMode::Notes);
            app.record_system(
                "Shell unavailable. Notes mode is active; install /bin/bash or set OPERATOR_SHELL.".to_string(),
            );
        }
    }

    let mut hermes_bridge = HermesBridge::default();
    review_server.set_enabled(app.review_hermes_responses);
    let result = run(
        &mut terminal,
        &mut app,
        &mut shell,
        &mut hermes_bridge,
        &mut review_server,
    );
    // Import any Hermes events queued before shutdown, then drain shell output.
    if let Err(error) = drain_shared_journal_updates(&mut app, &mut hermes_bridge) {
        app.status = format!("Hermes journal import pending: {error}");
    }
    drain_shell(&mut app, &mut shell)?;
    let save_result = app.journal.save_pending();
    drop(shell);
    drop(terminal);
    drop(_guard);
    // Dropping App closes any pending one-shot decision channel. The review
    // listener then returns a fail-closed rejection before it is shut down.
    drop(app);
    drop(review_server);

    result?;
    save_result?;
    Ok(())
}


/// Display and require acknowledgement of the operator access warning before
/// starting the shell or accepting application input.
fn show_access_disclaimer(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut page_index = 0usize;
    let mut animation_phase = 0usize;
    let mut last_animation_step = Instant::now();
    let mut letter_flash_phase = 0usize;
    let mut last_letter_flash_step = Instant::now();

    loop {
        // Drift the camouflage slowly while the warning lettering runs its
        // own red/orange/yellow signal cycle.
        if last_animation_step.elapsed() >= Duration::from_millis(420) {
            animation_phase = animation_phase.wrapping_add(1);
            last_animation_step = Instant::now();
        }
        if last_letter_flash_step.elapsed() >= Duration::from_millis(180) {
            letter_flash_phase = (letter_flash_phase + 1) % 6;
            last_letter_flash_step = Instant::now();
        }
        let current_animation_phase = animation_phase;
        let current_letter_flash_phase = letter_flash_phase;

        let mut page_count = 1usize;
        let mut can_continue = false;

        terminal.draw(|frame| {
            let area = frame.area();
            let popup = area;

            // Woodland palette for the complete access screen.
            let background = Color::Rgb(20, 27, 17);
            let accent = Color::Rgb(112, 128, 72); // olive drab
            let warning = Color::Rgb(198, 177, 119); // khaki
            let critical = Color::Rgb(255, 24, 24); // bright warning red
            let normal = Color::Rgb(218, 213, 187); // field canvas

            let base_block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent).add_modifier(Modifier::BOLD))
                .style(Style::default().fg(normal).bg(background));
            let inner = base_block.inner(popup);
            let available_text_rows = inner.height.saturating_sub(1);

            let too_small = inner.width < 24 || available_text_rows < 4;
            let pages = if too_small {
                Vec::new()
            } else {
                paginate_access_notice(
                    access_notice_policy(
                        critical,
                        warning,
                        normal,
                        accent,
                        inner.width,
                        current_animation_phase,
                        current_letter_flash_phase,
                    ),
                    inner.width,
                    usize::from(available_text_rows),
                )
            };

            if too_small {
                page_count = 1;
                page_index = 0;
                can_continue = false;
            } else {
                page_count = pages.len().max(1);
                page_index = page_index.min(page_count.saturating_sub(1));
                can_continue = page_index + 1 == page_count;
            }

            let title = if too_small {
                " RESTRICTED SYSTEM TERMS ".to_string()
            } else {
                format!(
                    " RESTRICTED SYSTEM TERMS // PAGE {} OF {} ",
                    page_index + 1,
                    page_count
                )
            };
            let block = base_block.title(Span::styled(
                title,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));

            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);

            if too_small {
                let message = vec![
                    Line::from(Span::styled(
                        "TERMINAL WINDOW TOO SMALL",
                        Style::default().fg(critical).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from("Increase terminal width and height to read"),
                    Line::from("the complete operator policy."),
                    Line::from(""),
                    Line::from(Span::styled(
                        "[ ESC ] EXIT",
                        Style::default().fg(warning).add_modifier(Modifier::BOLD),
                    )),
                ];
                let height = (message.len() as u16).min(inner.height);
                let top = inner.y.saturating_add(inner.height.saturating_sub(height) / 2);
                frame.render_widget(
                    Paragraph::new(message)
                        .alignment(Alignment::Center)
                        .style(Style::default().fg(normal).bg(background)),
                    Rect::new(inner.x, top, inner.width, height),
                );
                return;
            }

            let current_page = &pages[page_index];
            let text_height = (current_page.len() as u16).min(available_text_rows);
            let top_padding = available_text_rows.saturating_sub(text_height) / 2;
            let text_area = Rect::new(
                inner.x,
                inner.y.saturating_add(top_padding),
                inner.width,
                text_height,
            );

            frame.render_widget(
                Paragraph::new(current_page.clone())
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(normal).bg(background)),
                text_area,
            );

            let footer_y = inner.y.saturating_add(inner.height.saturating_sub(1));
            let footer = if page_count > 1 && !can_continue {
                "[PGDN / RIGHT / SPACE] NEXT   [PGUP / LEFT] PREVIOUS   [ESC] EXIT"
            } else if page_count > 1 {
                "[PGUP / LEFT] PREVIOUS   [ENTER] ACKNOWLEDGE   [ESC] EXIT"
            } else {
                "[ENTER] I ACKNOWLEDGE — CONTINUE     [ESC] EXIT"
            };
            frame.render_widget(
                Paragraph::new(footer)
                    .alignment(Alignment::Center)
                    .style(
                        Style::default()
                            .fg(warning)
                            .bg(background)
                            .add_modifier(Modifier::BOLD),
                    ),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        })?;

        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Enter if can_continue => return Ok(()),
                KeyCode::PageDown | KeyCode::Right | KeyCode::Down | KeyCode::Char(' ') => {
                    if page_index + 1 < page_count {
                        page_index += 1;
                    }
                }
                KeyCode::PageUp | KeyCode::Left | KeyCode::Up => {
                    page_index = page_index.saturating_sub(1);
                }
                KeyCode::Esc => {
                    return Err(
                        "Operator declined the access notice; OPERATOR was not started.".into(),
                    );
                }
                _ => {}
            }
        }
    }
}

/// Expanded operator terms. These govern system use; they do not transfer
/// ownership of files or replace a separately executed proprietary licence.
fn access_notice_policy(
    critical: Color,
    warning: Color,
    normal: Color,
    accent: Color,
    width: u16,
    animation_phase: usize,
    letter_flash_phase: usize,
) -> Vec<Line<'static>> {
    let critical_style = Style::default()
        .fg(critical)
        .add_modifier(Modifier::BOLD);
    let warning_style = Style::default()
        .fg(warning)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(normal).add_modifier(Modifier::BOLD);

    let mut rows = restricted_access_splash(
        width,
        warning,
        accent,
        animation_phase,
        letter_flash_phase,
    );
    rows.push(Line::from(Span::styled("", body_style)));

    let policy_rows: Vec<(&str, Style)> = vec![
        ("", body_style),
        ("1. AUTHORIZATION, IDENTITY, AND SCOPE", warning_style),
        ("Access is permitted only to an individual who has received express authorization", body_style),
        ("from the system owner or an administrator acting with delegated authority.", body_style),
        ("Authorization is limited to the account, device, network, time period, purpose,", body_style),
        ("and resources assigned to that operator. Access to one folder or service does not", body_style),
        ("imply permission to inspect other users' accounts, private files, or system areas.", body_style),
        ("Operators must use their own assigned identity and must not impersonate another", body_style),
        ("person, share accounts, lend credentials, or conceal the identity of an activity.", body_style),
        ("Any access beyond the granted scope requires prior approval from the responsible", body_style),
        ("owner. A successful login, visible file, readable directory, or available command", body_style),
        ("is not, by itself, evidence that the operator is authorized to use that resource.", body_style),
        ("", body_style),
        ("2. RESPONSIBLE AND LAWFUL USE", warning_style),
        ("Use the system solely for legitimate, approved purposes and in accordance with", body_style),
        ("applicable law, organizational rules, contractual duties, and written instructions.", body_style),
        ("Operators must exercise care, professional judgment, and restraint at all times.", body_style),
        ("Do not bypass authentication, access controls, quotas, monitoring, security tools,", body_style),
        ("or administrative restrictions. Do not probe, scan, exploit, damage, degrade, or", body_style),
        ("disrupt systems or networks without specific written authorization and scope.", body_style),
        ("Do not introduce malware, malicious scripts, unauthorized persistence, destructive", body_style),
        ("commands, denial-of-service activity, or tools intended to evade detection.", body_style),
        ("Do not use the system for harassment, threats, stalking, fraud, unlawful surveillance,", body_style),
        ("copyright infringement, privacy violations, or any other unlawful or abusive conduct.", body_style),
        ("Do not use stored information to discriminate, intimidate, retaliate, or gain an", body_style),
        ("improper advantage. Stop work when authorization is unclear or appears withdrawn.", body_style),
        ("", body_style),
        ("3. FILE OWNERSHIP, COPYRIGHT, AND PROPRIETARY LICENSING", warning_style),
        ("Files, notes, source code, documents, datasets, credentials, and other materials", body_style),
        ("remain owned by their respective authors, employers, licensors, or rights-holders.", body_style),
        ("Placing a file on this system, indexing it, backing it up, or viewing it here does", body_style),
        ("not transfer ownership, copyright, title, or any broader intellectual-property right.", body_style),
        ("No implied licence is granted merely because a file is present, accessible, or stored.", body_style),
        ("The system owner does not claim ownership of user or third-party content solely", body_style),
        ("because that content is processed, cached, journaled, transmitted, or backed up here.", body_style),
        ("Proprietary or confidential materials may be accessed only where the rights-holder", body_style),
        ("or authorized representative has granted permission for the specific intended use.", body_style),
        ("Where a proprietary file licence is required, the preferred permission model is", body_style),
        ("a separate written, non-exclusive, purpose-limited, non-transferable, and", body_style),
        ("non-sublicensable licence. It should identify the licensor, permitted operator,", body_style),
        ("covered files, allowed purpose, duration, territory where relevant, and any limits", body_style),
        ("on copying, modification, export, disclosure, retention, and deletion.", body_style),
        ("The licence must be granted by a party that actually holds or controls the rights.", body_style),
        ("It must not purport to grant rights that the licensor does not possess.", body_style),
        ("No operator may copy, publish, distribute, adapt, reverse engineer, sell, sublicense,", body_style),
        ("or disclose proprietary material except to the extent expressly permitted in writing", body_style),
        ("or allowed by applicable law. Any permitted working copy must remain within the", body_style),
        ("approved scope and be protected against access by unauthorized persons.", body_style),
        ("Third-party notices, open-source licences, commercial terms, confidentiality duties,", body_style),
        ("database rights, privacy requirements, and contractual restrictions continue to apply.", body_style),
        ("Where terms conflict or permissions are uncertain, stop the relevant activity and", body_style),
        ("obtain clarification from the rights-holder or responsible administrator.", body_style),
        ("This operator notice is not itself a transfer of copyright or a complete proprietary", body_style),
        ("file licence. A separate signed or otherwise valid written agreement may be needed.", body_style),
        ("OPERATOR application source code is dedicated to the public domain under CC0 1.0", body_style),
        ("Universal, as set out in the repository LICENSE file. That dedication does not grant", body_style),
        ("rights to third-party files, user data, credentials, or content stored in the journal.", body_style),
        ("", body_style),
        ("4. CONFIDENTIALITY AND NEED-TO-KNOW HANDLING", warning_style),
        ("Treat material marked confidential, proprietary, private, restricted, or sensitive", body_style),
        ("as protected information. Apply the same care to material that a reasonable operator", body_style),
        ("should recognize as sensitive even when it has no visible classification label.", body_style),
        ("Access only the minimum information needed for the approved task. Do not browse", body_style),
        ("curiously, collect unrelated personal information, or inspect another user's work", body_style),
        ("without a documented operational need and appropriate authorization.", body_style),
        ("Do not disclose protected information in chat rooms, public repositories, screenshots,", body_style),
        ("support tickets, demonstrations, issue reports, or external services unless the", body_style),
        ("recipient is authorized and the disclosure method is approved.", body_style),
        ("Before sharing diagnostic output, remove passwords, tokens, private keys, personal", body_style),
        ("information, internal hostnames, sensitive paths, and unrelated user content.", body_style),
        ("Confidentiality obligations continue after access ends, to the extent required by", body_style),
        ("the applicable agreement, policy, and law. Return or securely delete material when", body_style),
        ("the authorized purpose ends or when the rights-holder or administrator directs.", body_style),
        ("", body_style),
        ("5. JOURNALS, TRANSCRIPTS, STORAGE, AND DATA RETENTION", warning_style),
        ("OPERATOR may record commands, notes, system messages, and terminal output in a", body_style),
        ("local journal. Output can reveal usernames, host details, directory paths, network", body_style),
        ("addresses, filenames, application data, personal information, or business secrets.", body_style),
        ("A command may cause a program to print sensitive material even when the operator", body_style),
        ("did not intend to save it. Review output before sharing, exporting, or retaining it.", body_style),
        ("Hidden-input handling is a safeguard, not a guarantee that all secrets are protected.", body_style),
        ("Avoid entering passwords, tokens, private keys, recovery codes, or confidential data", body_style),
        ("as ordinary command arguments or notes. Use purpose-built secret-management tools.", body_style),
        ("Journal files, exports, backups, temporary files, and copied records must be stored", body_style),
        ("only in approved locations with access permissions appropriate to their contents.", body_style),
        ("Do not weaken file permissions, expose journals through public web servers, commit", body_style),
        ("sensitive data to public version control, or place restricted content in unapproved", body_style),
        ("cloud storage, synchronization services, removable media, or personal accounts.", body_style),
        ("Retain information only for an authorized operational, contractual, or legal purpose.", body_style),
        ("Follow the applicable retention schedule and any preservation or deletion hold.", body_style),
        ("Do not destroy, alter, conceal, or selectively remove records to obstruct an audit,", body_style),
        ("investigation, legal obligation, or authorized review.", body_style),
        ("", body_style),
        ("6. CREDENTIALS, DEVICES, AND SECURITY CONTROLS", warning_style),
        ("Keep passwords, access tokens, private keys, certificates, and recovery codes secret.", body_style),
        ("Use approved authentication methods and unique credentials. Enable multi-factor", body_style),
        ("authentication where available and required by the system owner.", body_style),
        ("Lock unattended sessions and protect devices from theft, shared access, and shoulder", body_style),
        ("surfing. Do not store credentials in scripts, shell history, notes, or journal files.", body_style),
        ("Do not disable endpoint protection, firewall rules, backups, audit functions, or", body_style),
        ("security updates. Do not install packages, extensions, agents, or remote-access tools", body_style),
        ("unless the change is authorized and obtained from a trusted source.", body_style),
        ("Use secure connections and verify destinations before transferring protected data.", body_style),
        ("Limit file and directory permissions to the minimum required. Protect backup copies", body_style),
        ("to the same standard as the originals, including encryption where appropriate.", body_style),
        ("", body_style),
        ("7. MONITORING, AUDIT, AND ADMINISTRATIVE ACCESS", warning_style),
        ("To the extent permitted by applicable law and disclosed organizational policy,", body_style),
        ("system activity may be logged, monitored, retained, or reviewed for security,", body_style),
        ("reliability, troubleshooting, compliance, and incident response.", body_style),
        ("Records may include login events, command history, journal entries, file access,", body_style),
        ("network activity, configuration changes, and administrative actions.", body_style),
        ("Authorized administrators may investigate suspected misuse, preserve relevant", body_style),
        ("records, restrict sessions, or disable accounts where reasonably necessary.", body_style),
        ("Monitoring is not a promise that every action is observed, nor does it authorize", body_style),
        ("operators to monitor other people or access their content without permission.", body_style),
        ("", body_style),
        ("8. INCIDENT REPORTING AND RESPONSE", warning_style),
        ("Promptly report suspected credential compromise, accidental disclosure, malware,", body_style),
        ("lost devices, unauthorized access, corrupted records, or policy violations to the", body_style),
        ("designated system owner or security contact using the approved reporting channel.", body_style),
        ("Include relevant facts and timing, but do not circulate exposed secrets or copy", body_style),
        ("sensitive evidence to unapproved destinations. Preserve relevant records when safe.", body_style),
        ("Do not attempt an independent investigation that could expand access, destroy evidence,", body_style),
        ("alert an attacker, or worsen the incident. Follow administrator instructions.", body_style),
        ("", body_style),
        ("9. THIRD-PARTY DATA AND PRIVACY", warning_style),
        ("Access personal information only for a legitimate authorized purpose and only to", body_style),
        ("the extent necessary. Respect applicable privacy, employment, confidentiality, and", body_style),
        ("data-protection obligations. Do not infer consent from technical accessibility.", body_style),
        ("Do not upload another person's information to an external AI, hosting, analytics,", body_style),
        ("translation, or synchronization service unless the data owner and responsible", body_style),
        ("administrator have approved that specific service and transfer.", body_style),
        ("Respect data residency, cross-border transfer, retention, deletion, and breach", body_style),
        ("notification requirements that apply to the data and the responsible organization.", body_style),
        ("", body_style),
        ("10. ENFORCEMENT AND CONSEQUENCES", warning_style),
        ("A suspected violation may result in suspension or revocation of access, removal of", body_style),
        ("sessions, preservation of relevant records, internal review, or referral to the", body_style),
        ("appropriate rights-holder, employer, administrator, regulator, or law-enforcement", body_style),
        ("authority, as permitted or required by applicable law.", body_style),
        ("Unauthorized access, disclosure, copying, damage, or misuse may create civil,", body_style),
        ("contractual, employment, or criminal consequences depending on the facts and law.", body_style),
        ("No penalty or legal outcome is predetermined by this notice; competent authorities", body_style),
        ("and courts determine consequences under the applicable rules.", body_style),
        ("", body_style),
        ("11. ACKNOWLEDGMENT AND LIMITS OF THIS NOTICE", warning_style),
        ("By selecting ACKNOWLEDGE, the operator confirms that they have read this notice,", body_style),
        ("understand the stated operating rules, and will stop and seek clarification where", body_style),
        ("authority, ownership, licence scope, or permitted use is uncertain.", body_style),
        ("Acknowledgment does not create rights in another person's files, replace a required", body_style),
        ("proprietary licence, override third-party terms, or waive rights that cannot lawfully", body_style),
        ("be waived. It does not replace a separate employment, confidentiality, data-processing,", body_style),
        ("acceptable-use, or software agreement where one is required.", body_style),
        ("The system owner should publish and maintain the applicable contact, retention", body_style),
        ("schedule, security procedure, and any additional written terms outside this notice.", body_style),
        ("", body_style),
        ("UNAUTHORIZED PERSONNEL: DISCONNECT NOW. DO NOT PROCEED.", critical_style),
    ];
    rows.extend(
        policy_rows
            .into_iter()
            .map(|(text, style)| Line::from(Span::styled(text, style))),
    );
    rows
}

/// Build a single framed Restricted Access banner. No BBS node header or
/// redundant connection headline is placed above the warning artwork.
fn restricted_access_splash(
    width: u16,
    warning: Color,
    accent: Color,
    animation_phase: usize,
    letter_flash_phase: usize,
) -> Vec<Line<'static>> {
    let frame_width = usize::from(width).clamp(24, 76);
    let letter_color = restricted_access_flash_color(letter_flash_phase);
    let border_style = Style::default()
        .fg(accent)
        .bg(Color::Rgb(25, 34, 21))
        .add_modifier(Modifier::BOLD);
    let warning_style = Style::default()
        .fg(warning)
        .add_modifier(Modifier::BOLD);

    let mut rows = vec![Line::from(Span::styled(
        bbs_rule(frame_width, '╔', '╗'),
        border_style,
    ))];

    if frame_width >= 66 {
        // The artwork fills the frame edge-to-edge. Letter pixels flash red,
        // orange, and yellow; the camo pattern fills every surrounding cell.
        let canvas_width = frame_width.saturating_sub(6);
        let mut art_row = 0usize;

        for glyph_row in compact_notice_word("RESTRICTED") {
            rows.push(camouflage_art_line(
                &glyph_row,
                canvas_width,
                art_row,
                17,
                letter_color,
                accent,
                animation_phase,
            ));
            art_row += 1;
        }
        rows.push(camouflage_art_line(
            "",
            canvas_width,
            art_row,
            29,
            letter_color,
            accent,
            animation_phase,
        ));
        art_row += 1;
        for glyph_row in compact_notice_word("ACCESS") {
            rows.push(camouflage_art_line(
                &glyph_row,
                canvas_width,
                art_row,
                41,
                letter_color,
                accent,
                animation_phase,
            ));
            art_row += 1;
        }
    } else {
        rows.push(Line::from(Span::styled(
            centered_bbs_line("RESTRICTED ACCESS", frame_width),
            Style::default().fg(letter_color).add_modifier(Modifier::BOLD),
        )));
    }

    rows.push(Line::from(Span::styled(
        bbs_rule(frame_width, '╟', '╢'),
        border_style,
    )));
    let authorization = if frame_width >= 30 {
        "AUTHORIZED PERSONNEL ONLY"
    } else {
        "AUTHORIZED ONLY"
    };
    rows.push(Line::from(Span::styled(
        centered_bbs_line(authorization, frame_width),
        warning_style,
    )));
    if frame_width >= 52 {
        rows.push(Line::from(Span::styled(
            centered_bbs_line("SESSION MONITORED  //  READ OPERATOR TERMS", frame_width),
            warning_style,
        )));
    }
    rows.push(Line::from(Span::styled(
        bbs_rule(frame_width, '╚', '╝'),
        border_style,
    )));
    rows
}

/// Repeating warning-light cycle: strong red, short orange flash,
/// strong yellow, then another short orange flash before returning to red.
fn restricted_access_flash_color(phase: usize) -> Color {
    const RED: Color = Color::Rgb(255, 24, 24);
    const ORANGE: Color = Color::Rgb(255, 96, 0);
    const YELLOW: Color = Color::Rgb(255, 255, 0);

    match phase % 6 {
        0 | 1 => RED,
        2 | 5 => ORANGE,
        _ => YELLOW,
    }
}

/// Fill the full art canvas with woodland camouflage. The flashing bitmap
/// letters remain readable against the dark olive/brown cell backgrounds.
fn camouflage_art_line(
    glyph_row: &str,
    canvas_width: usize,
    y: usize,
    seed: usize,
    letter_color: Color,
    border_color: Color,
    animation_phase: usize,
) -> Line<'static> {
    let dark_palette = [
        Color::Rgb(31, 43, 29), // deep field green
        Color::Rgb(48, 59, 34), // olive
        Color::Rgb(64, 57, 37), // earth brown
        Color::Rgb(41, 52, 36), // woodland green
        Color::Rgb(72, 61, 41), // muted soil
        Color::Rgb(37, 42, 31), // shadow
    ];
    let texture_palette = [
        Color::Rgb(76, 91, 49),
        Color::Rgb(91, 104, 57),
        Color::Rgb(111, 99, 66),
        Color::Rgb(66, 83, 52),
        Color::Rgb(120, 104, 70),
        Color::Rgb(83, 88, 58),
    ];

    let glyph: Vec<char> = glyph_row.chars().collect();
    let left_pad = canvas_width.saturating_sub(glyph.len()) / 2;
    let mut spans = Vec::with_capacity(canvas_width + 2);
    let border_style = Style::default()
        .fg(border_color)
        .bg(dark_palette[0])
        .add_modifier(Modifier::BOLD);
    spans.push(Span::styled("║  ".to_string(), border_style));

    for x in 0..canvas_width {
        let glyph_x = x as isize - left_pad as isize;
        let is_letter = glyph_x >= 0
            && glyph.get(glyph_x as usize).is_some_and(|character| *character == '█');

        let drifted_x = x.saturating_add(animation_phase);
        let background_index = camouflage_index(
            drifted_x,
            y,
            seed,
            dark_palette.len(),
        );
        let background = dark_palette[background_index];

        let (character, foreground) = if is_letter {
            ('█', letter_color)
        } else {
            let index = camouflage_index(
                drifted_x.saturating_add(7),
                y.saturating_add(2),
                seed.wrapping_add(2),
                texture_palette.len(),
            );
            let texture = [' ', '░', ' ', '▒', '·', ' ', '▓'];
            (texture[index % texture.len()], texture_palette[index])
        };

        spans.push(Span::styled(
            character.to_string(),
            Style::default()
                .fg(foreground)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::styled("  ║".to_string(), border_style));
    Line::from(spans)
}

/// Deterministic low-frequency noise creates irregular woodland patches.
/// animation_phase shifts the texture gradually without blinking the lettering.
fn camouflage_index(
    x: usize,
    y: usize,
    seed: usize,
    palette_len: usize,
) -> usize {
    let shifted_x = x.saturating_add((y.wrapping_mul(7).wrapping_add(seed)) % 5);
    let shifted_y = y.saturating_add((x.wrapping_mul(3).wrapping_add(seed)) % 3);
    let region_x = shifted_x / 4;
    let region_y = shifted_y / 2;

    let mut noise = region_x.wrapping_mul(0x9E37_79B1)
        ^ region_y.wrapping_mul(0x85EB_CA77)
        ^ seed.wrapping_mul(0xC2B2_AE3D);
    noise ^= noise >> 16;
    noise = noise.wrapping_mul(0x7FEB_352D);
    noise ^= noise >> 15;
    noise % palette_len.max(1)
}

fn bbs_rule(width: usize, left: char, right: char) -> String {
    let width = width.max(2);
    format!("{left}{}{right}", "═".repeat(width - 2))
}

fn centered_bbs_line(text: &str, width: usize) -> String {
    let width = width.max(2);
    let interior = width - 2;
    let text_width = UnicodeWidthStr::width(text);
    let padding = interior.saturating_sub(text_width);
    let left = padding / 2;
    let right = padding.saturating_sub(left);
    format!("║{}{}{}║", " ".repeat(left), text, " ".repeat(right))
}

/// Reduce the bitmap to five deliberate horizontal bands so both words,
/// the authorization line, and the complete frame fit together on a typical
/// 20-row SSH terminal window.
fn compact_notice_word(word: &str) -> Vec<String> {
    large_notice_word(word)
        .into_iter()
        .enumerate()
        .filter_map(|(index, row)| matches!(index, 0 | 1 | 3 | 5 | 6).then_some(row))
        .collect()
}

/// Render a word in a 5x7 block-letter bitmap. This gives the terminal
/// notice a genuinely oversized visual masthead without changing the
/// terminal emulator's user-controlled font settings.
fn large_notice_word(word: &str) -> Vec<String> {
    let mut output = vec![String::new(); 7];

    for character in word.chars() {
        let glyph: [&str; 7] = match character {
            'A' => ["01110", "10001", "10001", "11111", "10001", "10001", "10001"],
            'C' => ["01111", "10000", "10000", "10000", "10000", "10000", "01111"],
            'D' => ["11110", "10001", "10001", "10001", "10001", "10001", "11110"],
            'E' => ["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
            'I' => ["11111", "00100", "00100", "00100", "00100", "00100", "11111"],
            'R' => ["11110", "10001", "10001", "11110", "10100", "10010", "10001"],
            'S' => ["01111", "10000", "10000", "01110", "00001", "00001", "11110"],
            'T' => ["11111", "00100", "00100", "00100", "00100", "00100", "00100"],
            _ => ["00000", "00000", "00000", "00000", "00000", "00000", "00000"],
        };

        for (line, glyph_row) in output.iter_mut().zip(glyph) {
            for bit in glyph_row.chars() {
                line.push(if bit == '1' { '█' } else { ' ' });
            }
            line.push(' ');
        }
    }

    output
}

/// Wraps policy paragraphs to terminal width without discarding any wording.
fn wrap_access_notice(
    rows: Vec<Line<'static>>,
    width: u16,
) -> Vec<Line<'static>> {
    let max_width = usize::from(width.max(1));
    let mut wrapped = Vec::new();

    for row in rows {
        // Multi-span rows are the per-cell camouflage mosaic. Framed masthead
        // rows (including single-span horizontal borders and centered labels)
        // must also retain their exact width and spacing.
        let is_framed = row.spans.iter().any(|span| {
            span.content.chars().any(|character| {
                matches!(character, '║' | '╔' | '╗' | '╚' | '╝' | '╟' | '╢')
            })
        });
        if row.spans.len() > 1 || is_framed {
            wrapped.push(row);
            continue;
        }

        let Some(span) = row.spans.first() else {
            wrapped.push(Line::from(""));
            continue;
        };
        let row_text = span.content.to_string();
        let style = span.style;

        if row_text.trim().is_empty() {
            wrapped.push(Line::from(""));
            wrapped.push(Line::from(""));
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;

        for word in row_text.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            let separator_width = usize::from(!current.is_empty());

            if !current.is_empty()
                && current_width.saturating_add(separator_width).saturating_add(word_width)
                    > max_width
            {
                wrapped.push(Line::from(Span::styled(
                    std::mem::take(&mut current),
                    style,
                )));
                current_width = 0;
            }

            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width = current_width.saturating_add(word_width);
        }

        if !current.is_empty() {
            let is_section_heading = current
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
                && current.contains(". ");
            wrapped.push(Line::from(Span::styled(current, style)));
            if is_section_heading {
                wrapped.push(Line::from(Span::styled(
                    "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
                    style,
                )));
                wrapped.push(Line::from(""));
            }
        }
    }

    wrapped
}

/// Splits wrapped policy lines into pages sized to the current modal viewport.
fn paginate_access_notice(
    rows: Vec<Line<'static>>,
    width: u16,
    page_height: usize,
) -> Vec<Vec<Line<'static>>> {
    let lines = wrap_access_notice(rows, width);
    let capacity = page_height.max(1);
    let mut pages = Vec::new();
    let mut start = 0usize;

    while start < lines.len() {
        let mut end = start.saturating_add(capacity).min(lines.len());

        // Prefer a blank separator as the page boundary so a section heading
        // and its divider are not stranded at the bottom of a page.
        if end < lines.len() {
            let mut preferred_end = end;
            while preferred_end > start
                && !lines[preferred_end - 1]
                    .spans
                    .iter()
                    .all(|span| span.content.trim().is_empty())
            {
                preferred_end -= 1;
            }

            if preferred_end > start {
                end = preferred_end;
            }
        }

        if end <= start {
            end = start.saturating_add(capacity).min(lines.len());
        }

        pages.push(lines[start..end].to_vec());
        start = end;
    }

    pages
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileTransferRequest {
    Sftp,
    LegacyScp,
}

/// Return the peer address supplied by OpenSSH for this login session.
/// SSH_CONNECTION contains: client_ip client_port server_ip server_port.
/// SSH_CLIENT is retained as a compatibility fallback.
fn ssh_client_ip() -> Option<std::net::IpAddr> {
    let ssh_connection = std::env::var("SSH_CONNECTION").ok();
    let ssh_client = std::env::var("SSH_CLIENT").ok();
    parse_ssh_client_ip(ssh_connection.as_deref(), ssh_client.as_deref())
}

fn parse_ssh_client_ip(
    ssh_connection: Option<&str>,
    ssh_client: Option<&str>,
) -> Option<std::net::IpAddr> {
    fn first_address(value: &str) -> Option<std::net::IpAddr> {
        value.split_whitespace().next()?.parse().ok()
    }

    ssh_connection
        .and_then(first_address)
        .or_else(|| ssh_client.and_then(first_address))
}

/// Dispatch sshd's non-interactive login-shell invocations before the TUI
/// initializes. Transfer data retains the original SSH channel file descriptors.
#[cfg(unix)]
fn dispatch_ssh_invocation() {
    use std::{ffi::OsStr, os::unix::process::CommandExt};

    // Do not reinterpret ordinary local invocations of the executable.
    if std::env::var_os("SSH_CONNECTION").is_none() {
        return;
    }

    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(OsStr::new("-c")) {
        return;
    }

    let Some(command) = arguments.next() else {
        eprintln!("OPERATOR: SSH supplied -c without a command.");
        std::process::exit(2);
    };
    let command_text = command.to_string_lossy();

    if let Some(request) = detect_file_transfer_request(&command_text) {
        if !settings::Settings::load().scp_sftp_passthrough {
            eprintln!("OPERATOR: SCP/SFTP passthrough is disabled. Enable it with /scp on.");
            std::process::exit(1);
        }

        match request {
            FileTransferRequest::Sftp => {
                // The subsystem path comes from sshd configuration. Execute
                // it directly so no shell startup output can corrupt SFTP.
                let words = match split_command_words(&command_text) {
                    Some(words) if !words.is_empty() => words,
                    _ => {
                        eprintln!("OPERATOR: invalid SFTP subsystem command.");
                        std::process::exit(2);
                    }
                };
                let program_index = if words.first().is_some_and(|word| word == "exec") {
                    1
                } else {
                    0
                };
                let Some(program) = words.get(program_index) else {
                    eprintln!("OPERATOR: invalid SFTP subsystem command.");
                    std::process::exit(2);
                };
                let mut server = Command::new(program);
                server.args(&words[program_index + 1..]);
                let error = server.exec();
                eprintln!("OPERATOR: could not execute SFTP subsystem: {error}");
                std::process::exit(127);
            }
            FileTransferRequest::LegacyScp => {
                // Legacy SCP uses shell-quoted remote paths; delegate the
                // original command string to Bash without allocating a PTY.
                let error = Command::new("/bin/bash")
                    .arg("-c")
                    .arg(&command)
                    .env("SHELL", "/bin/bash")
                    .exec();
                eprintln!("OPERATOR: could not execute legacy SCP command: {error}");
                std::process::exit(127);
            }
        }
    }

    // Ordinary remote SSH commands retain normal non-interactive shell behavior.
    let error = Command::new("/bin/bash")
        .arg("-c")
        .arg(&command)
        .arg("operator-ssh")
        .args(arguments)
        .env("SHELL", "/bin/bash")
        .exec();
    eprintln!("OPERATOR: could not execute remote command through /bin/bash: {error}");
    std::process::exit(127);
}

#[cfg(not(unix))]
fn dispatch_ssh_invocation() {}

fn detect_file_transfer_request(command: &str) -> Option<FileTransferRequest> {
    let words = split_command_words(command)?;
    let start = if words.first().is_some_and(|word| word == "exec") {
        1
    } else {
        0
    };
    let executable = words.get(start)?;
    let name = std::path::Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())?;

    if name == "sftp-server" {
        return Some(FileTransferRequest::Sftp);
    }

    if name == "scp" {
        let is_server_mode = words.iter().skip(start + 1).any(|argument| {
            matches!(argument.as_str(), "-t" | "-f")
                || (argument.starts_with('-')
                    && !argument.starts_with("--")
                    && argument[1..].chars().any(|flag| flag == 't' || flag == 'f'))
        });
        if is_server_mode {
            return Some(FileTransferRequest::LegacyScp);
        }
    }

    None
}

/// Split the simple executable/argument form used by sshd's Subsystem setting.
/// Handles quotes and backslash escapes without shell expansion.
fn split_command_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut in_word = false;

    for character in command.chars() {
        if escaped {
            current.push(character);
            in_word = true;
            escaped = false;
            continue;
        }

        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else if character == '\\' && delimiter == '"' {
                escaped = true;
            } else {
                current.push(character);
            }
            in_word = true;
            continue;
        }

        match character {
            '\'' | '"' => {
                quote = Some(character);
                in_word = true;
            }
            '\\' => {
                escaped = true;
                in_word = true;
            }
            whitespace if whitespace.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                current.push(other);
                in_word = true;
            }
        }
    }

    if escaped || quote.is_some() {
        return None;
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

const REVIEWER_COUNT_REFRESH_INTERVAL: Duration = Duration::from_millis(300);

/// Refresh from the shared per-session registry so all SSH terminals display
/// the same count shortly after any operator runs /review on or /review off.
fn refresh_active_reviewer_count(
    app: &mut App,
    review_server: &HermesReviewServer,
    last_refresh: &mut Instant,
) {
    if last_refresh.elapsed() >= REVIEWER_COUNT_REFRESH_INTERVAL {
        app.active_reviewer_count = review_server.active_reviewer_count();
        *last_refresh = Instant::now();
    }
}

/// Import Hermes inbox events and mirror peer notes while keeping a highlighted
/// live-tail row attached to the newest imported record.
fn drain_shared_journal_updates(
    app: &mut App,
    hermes_bridge: &mut HermesBridge,
) -> io::Result<usize> {
    let was_tracking_tail = app.is_tracking_selected_tail();
    let previous_len = app.journal.entries.len();
    let inbox_imported = hermes_bridge.drain(&mut app.journal)?;
    // Private inboxes provide reliable per-instance Hermes delivery. The
    // shared journal tail also mirrors notes written by any peer terminal.
    let shared_journal_imported = app.journal.refresh_external_entries()?;
    app.apply_archive_search_results();
    app.apply_postgres_search_results();
    app.apply_postgres_archive_record_results();
    if app.journal.entries.len() > previous_len {
        app.update_selected_tail_after_append(was_tracking_tail);
    }
    Ok(inbox_imported + shared_journal_imported)
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    shell: &mut Option<ShellSession>,
    hermes_bridge: &mut HermesBridge,
    review_server: &mut HermesReviewServer,
) -> Result<(), Box<dyn std::error::Error>> {
    app.active_reviewer_count = review_server.active_reviewer_count();
    let mut last_reviewer_count_refresh = Instant::now();
    let mut last_hot_cache_prune = Instant::now();

    loop {
        if last_hot_cache_prune.elapsed() >= Duration::from_secs(60) {
            app.prune_hot_cache();
            last_hot_cache_prune = Instant::now();
        }
        app.expire_hermes_review_if_needed();
        review_server.set_enabled(app.review_hermes_responses);
        refresh_active_reviewer_count(app, review_server, &mut last_reviewer_count_refresh);
        if app.pending_review.is_none() {
            if let Some(request) = review_server.try_recv() {
                app.begin_hermes_review(request);
            }
        }
        drain_shell(app, shell)?;
        match drain_shared_journal_updates(app, hermes_bridge) {
            Ok(imported) if imported > 0 => {
                app.status = format!("Synced {imported} shared journal row(s).");
            }
            Ok(_) => {}
            Err(error) => {
                app.status = format!("Hermes journal import pending: {error}");
            }
        }
        if let Some(session) = shell.as_ref() {
            app.set_terminal_prompt(session.prompt_label());
            app.set_terminal_working_directory(session.current_working_directory());
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
                if key.kind == KeyEventKind::Press && key.code == KeyCode::F(1) && app.show_help {
                    app.show_help = false;
                } else if key.kind == KeyEventKind::Press && key.code == KeyCode::F(1) {
                    let size = terminal.size()?;
                    match ShellSession::spawn(size.width, size.height) {
                        Ok(mut workspace_session) => {
                            run_terminal_workspace(
                                terminal,
                                app,
                                &mut workspace_session,
                                hermes_bridge,
                                review_server,
                            )?;
                            // Dropping this session closes only the workspace shell.
                            // OPERATOR's primary transcript shell remains untouched.
                            terminal.clear()?;
                        }
                        Err(error) => {
                            app.status = format!("Could not start independent terminal workspace: {error}");
                        }
                    }
                } else {
                    let should_exit = handle_key(key.code, key.modifiers, app, shell)?;
                    // Publish /review on|off changes immediately so the
                    // shared Hermes dispatcher sees the live reviewer pool
                    // state before the next poll iteration.
                    review_server.set_enabled(app.review_hermes_responses);
                    if should_exit {
                        break;
                    }
                }
            }
            Event::Mouse(_) if app.pending_review.is_some() => {}
            Event::Mouse(_) if app.search_export_dialog_open => {}
            Event::Mouse(mouse) if app.search_view => match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(if mouse.modifiers.contains(KeyModifiers::SHIFT) { 8 } else { 3 }),
                MouseEventKind::ScrollDown => app.scroll_down(if mouse.modifiers.contains(KeyModifiers::SHIFT) { 8 } else { 3 }),
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Right)
                    if app.message_view_index.is_none()
                        && app.conversation_view.is_none()
                        && mouse.column >= app.input_area.x
                        && mouse.column < app.input_area.x.saturating_add(app.input_area.width)
                        && mouse.row >= app.input_area.y
                        && mouse.row < app.input_area.y.saturating_add(app.input_area.height) =>
                {
                    paste_from_system_clipboard(app);
                }
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
            Event::Paste(text) => {
                if app.pending_review.is_some() {
                    insert_review_terminal_paste(app, &text);
                } else if app.search_export_dialog_open {
                    app.search_export_insert_text(&text);
                } else {
                    paste_clipboard_text(app, &text, "terminal paste event");
                }
            },
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
/// F1 returns to the OPERATOR transcript workspace.
fn run_terminal_workspace(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    session: &mut ShellSession,
    hermes_bridge: &mut HermesBridge,
    review_server: &mut HermesReviewServer,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
    let size = terminal.size()?;
    session.resize(size.width, size.height)?;
    session.set_echo(true)?;

    // Stop Crossterm's enhanced-key parser from consuming terminal replies
    // while the child PTY owns input. The raw Unix pump below forwards every
    // byte except the reserved F1 workspace-exit sequence.
    execute!(
        out,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture,
        crossterm::cursor::Show
    )?;
    out.flush()?;

    let result = run_terminal_workspace_loop(terminal, app, session, hermes_bridge, review_server);
    let echo_result = session.set_echo(false);

    // Restore the input modes expected by OPERATOR. A nested full-screen
    // child may have changed bracketed-paste or mouse modes while it owned TTY.
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        ),
        crossterm::cursor::Hide
    )?;
    terminal.clear()?;

    if let Err(error) = echo_result {
        return Err(Box::new(error));
    }
    result
}

#[cfg(unix)]
fn run_terminal_workspace_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    session: &mut ShellSession,
    hermes_bridge: &mut HermesBridge,
    review_server: &mut HermesReviewServer,
) -> Result<(), Box<dyn std::error::Error>> {
    const INPUT_POLL: Duration = Duration::from_millis(12);
    const ESCAPE_SEQUENCE_GRACE: Duration = Duration::from_millis(60);
    const RESIZE_POLL: Duration = Duration::from_millis(200);

    let mut tty = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty")?;
    let mut pending_escape = Vec::with_capacity(8);
    let mut pending_since: Option<Instant> = None;
    let mut last_hot_cache_prune = Instant::now();
    let mut last_resize_check = Instant::now();
    let mut last_size = terminal.size()?;

    loop {
        if last_hot_cache_prune.elapsed() >= Duration::from_secs(60) {
            app.prune_hot_cache();
            last_hot_cache_prune = Instant::now();
        }
        app.expire_hermes_review_if_needed();
        review_server.set_enabled(app.review_hermes_responses);
        if app.pending_review.is_none() {
            if let Some(request) = review_server.try_recv() {
                app.begin_hermes_review(request);
            }
        }
        if app.pending_review.is_some() {
            run_workspace_review_overlay(terminal, app)?;
            continue;
        }

        // Keep importing Hermes hook records while the F1 PTY is active.
        match drain_shared_journal_updates(app, hermes_bridge) {
            Ok(imported) if imported > 0 => {
                app.status = format!(
                    "Synced {imported} shared journal row(s) while F1 workspace was active."
                );
            }
            Ok(_) => {}
            Err(error) => {
                app.status = format!("Hermes journal import pending: {error}");
            }
        }

        let raw = session.drain_raw_output();
        if !raw.is_empty() {
            // Intentionally byte-for-byte: nested OSC 52 set/query sequences
            // must reach the actual client terminal unchanged.
            let mut output = stdout();
            output.write_all(&raw)?;
            output.flush()?;
        }
        if session.check_exit()?.is_some() {
            break;
        }

        if let Some(input) = poll_terminal_input(&mut tty, INPUT_POLL)? {
            let mut forwarded = Vec::with_capacity(input.len());
            let leave_workspace =
                filter_workspace_input(&mut pending_escape, &input, &mut forwarded);

            if !forwarded.is_empty() {
                session.send_bytes(&forwarded)?;
            }
            if leave_workspace {
                break;
            }
            if pending_escape.is_empty() {
                pending_since = None;
            } else if pending_since.is_none() {
                pending_since = Some(Instant::now());
            }
        }

        // ESC can also be a standalone key (or the beginning of a sequence
        // that is not F1). Do not hold it indefinitely.
        if pending_since.is_some_and(|started| started.elapsed() >= ESCAPE_SEQUENCE_GRACE) {
            let pending = std::mem::take(&mut pending_escape);
            if !pending.is_empty() {
                session.send_bytes(&pending)?;
            }
            pending_since = None;
        }

        // Crossterm used to deliver Resize events here. In raw passthrough mode
        // poll dimensions directly so remote full-screen apps still resize.
        if last_resize_check.elapsed() >= RESIZE_POLL {
            let size = terminal.size()?;
            if size != last_size {
                session.resize(size.width, size.height)?;
                last_size = size;
            }
            last_resize_check = Instant::now();
        }
    }

    Ok(())
}

/// Temporarily reclaim the client terminal from the F1 PTY while a final
/// Hermes answer is held. The PTY process remains alive and resumes unchanged
/// after the operator releases or rejects the response.
fn run_workspace_review_overlay(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        ),
        crossterm::cursor::Hide
    )?;
    terminal.clear()?;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut last_hot_cache_prune = Instant::now();
        while app.pending_review.is_some() {
            if last_hot_cache_prune.elapsed() >= Duration::from_secs(60) {
                app.prune_hot_cache();
                last_hot_cache_prune = Instant::now();
            }
            app.expire_hermes_review_if_needed();
            if app.pending_review.is_none() {
                break;
            }
            terminal.draw(|frame| ui::draw(frame, app))?;
            if !event::poll(Duration::from_millis(40))? {
                continue;
            }
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
                {
                    handle_review_key(key.code, key.modifiers, app);
                }
                Event::Paste(text) => insert_review_terminal_paste(app, &text),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        Ok(())
    })();

    let cleanup = execute!(
        out,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let clear = terminal.clear();
    result?;
    cleanup?;
    clear?;
    Ok(())
}

/// Paste shortcuts supported inside the review editor.
///
/// Ctrl+V/Ctrl+Shift+V request the SSH client's clipboard through OSC 52.
/// Shift+Insert is a terminal-native alternative. Some older terminal
/// emulators expose Ctrl+V as the raw control byte instead of a modified key.
fn is_clipboard_paste_shortcut(code: KeyCode, modifiers: KeyModifiers) -> bool {
    (modifiers.contains(KeyModifiers::CONTROL)
        && matches!(code, KeyCode::Char('v' | 'V')))
        || (code == KeyCode::Insert && modifiers.contains(KeyModifiers::SHIFT))
        || code == KeyCode::Char('\u{16}')
}

/// Insert text delivered by the terminal's bracketed-paste protocol. The
/// application model replaces an active Ctrl+A selection on the first paste.
fn insert_review_terminal_paste(app: &mut App, text: &str) {
    if text.is_empty() {
        return;
    }
    let count = text.chars().count();
    app.review_insert_text(text);
    app.status = format!("Pasted {count} characters from the SSH client terminal into the held answer.");
}

fn handle_review_key(code: KeyCode, modifiers: KeyModifiers, app: &mut App) {
    if app.pending_review.is_none() {
        return;
    }

    // Some SSH terminal/keyboard-protocol combinations report Ctrl+V as
    // Char('V') (often with SHIFT) rather than Char('v'). Accept both.
    if is_clipboard_paste_shortcut(code, modifiers) {
        paste_review_from_system_clipboard(app);
        return;
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('a') | KeyCode::Char('A') => app.review_select_all(),
            KeyCode::Home => app.review_cursor_document_start(),
            KeyCode::End => app.review_cursor_document_end(),
            KeyCode::Char('q') => {
                app.status = "A held Hermes answer must be released with F2/F3 or withheld with F4.".to_string();
            }
            _ => {}
        }
        return;
    }

    match code {
        KeyCode::F(2) => app.approve_hermes_original(),
        KeyCode::F(3) => app.approve_hermes_edited(),
        KeyCode::F(4) => app.reject_hermes_response(),
        KeyCode::Esc => {
            app.status = "The answer remains held. Use F2 to release original, F3 to release edits, or F4 to withhold.".to_string();
        }
        KeyCode::Left => app.review_cursor_left(),
        KeyCode::Right => app.review_cursor_right(),
        KeyCode::Up => app.review_cursor_up(),
        KeyCode::Down => app.review_cursor_down(),
        KeyCode::PageUp => {
            for _ in 0..10 {
                app.review_cursor_up();
            }
        }
        KeyCode::PageDown => {
            for _ in 0..10 {
                app.review_cursor_down();
            }
        }
        KeyCode::Home => app.review_cursor_home(),
        KeyCode::End => app.review_cursor_end(),
        KeyCode::Backspace => app.review_backspace(),
        KeyCode::Delete => app.review_delete(),
        KeyCode::Enter => app.review_insert_text("\n"),
        KeyCode::Tab => app.review_insert_text("    "),
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::ALT) => {
            app.review_insert_text(&character.to_string());
        }
        _ => {}
    }
}

fn paste_review_from_system_clipboard(app: &mut App) {
    // Prefer the attached SSH client's clipboard. Some terminal emulators
    // permit OSC 52 writes but deny clipboard reads, so continue through the
    // server-side fallbacks rather than silently doing nothing.
    match read_osc52_terminal_clipboard() {
        Ok(Some(text)) if !text.is_empty() => {
            let count = text.chars().count();
            app.review_insert_text(&text);
            app.status = format!("Pasted {count} characters from the SSH client clipboard into the held answer.");
            return;
        }
        Ok(Some(_)) | Ok(None) | Err(_) => {}
    }

    match read_server_clipboard_cache() {
        Ok(text) if !text.is_empty() => {
            let count = text.chars().count();
            app.review_insert_text(&text);
            app.status = format!("Pasted {count} characters from the OPERATOR server clipboard into the held answer.");
            return;
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            app.status = format!("Could not read OPERATOR clipboard cache: {error}");
        }
    }

    match read_system_clipboard() {
        Ok(text) if !text.is_empty() => {
            let count = text.chars().count();
            app.review_insert_text(&text);
            app.status = format!("Pasted {count} characters from the server clipboard into the held answer.");
        }
        Ok(_) => {
            app.status = "No clipboard text found. Use your SSH terminal's Paste command (usually Ctrl+Shift+V) to send text into this editor.".to_string();
        }
        Err(error) => {
            app.status = format!(
                "Clipboard read unavailable. Use the SSH terminal's Paste command (usually Ctrl+Shift+V); {error}"
            );
        }
    }
}

#[cfg(not(unix))]
fn run_terminal_workspace_loop(
    _terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    session: &mut ShellSession,
    hermes_bridge: &mut HermesBridge,
    review_server: &mut HermesReviewServer,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_hot_cache_prune = Instant::now();
    loop {
        if last_hot_cache_prune.elapsed() >= Duration::from_secs(60) {
            app.prune_hot_cache();
            last_hot_cache_prune = Instant::now();
        }
        app.expire_hermes_review_if_needed();
        review_server.set_enabled(app.review_hermes_responses);
        if app.pending_review.is_none() {
            if let Some(request) = review_server.try_recv() {
                app.begin_hermes_review(request);
            }
        }
        if app.pending_review.is_some() {
            run_workspace_review_overlay(_terminal, app)?;
            continue;
        }
        match drain_shared_journal_updates(app, hermes_bridge) {
            Ok(imported) if imported > 0 => {
                app.status = format!(
                    "Synced {imported} shared journal row(s) while F1 workspace was active."
                );
            }
            Ok(_) => {}
            Err(error) => {
                app.status = format!("Hermes journal import pending: {error}");
            }
        }

        let raw = session.drain_raw_output();
        if !raw.is_empty() {
            let mut output = stdout();
            output.write_all(&raw)?;
            output.flush()?;
        }
        if session.check_exit()?.is_some() {
            break;
        }
        if !event::poll(Duration::from_millis(12))? {
            continue;
        }
        match event::read()? {
            Event::Key(key)
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
            {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::F(1) {
                    break;
                }
                let bytes = encode_terminal_key(key.code, key.modifiers);
                if !bytes.is_empty() {
                    session.send_bytes(&bytes)?;
                }
            }
            Event::Paste(text) => session.send_bytes(text.as_bytes())?,
            Event::Resize(cols, rows) => session.resize(cols, rows)?,
            _ => {}
        }
    }
    Ok(())
}

/// Read raw input from the controlling terminal without parsing ANSI sequences.
/// This allows OSC 52 responses from the SSH client to pass through F1 into a
/// nested OPERATOR running on another host.
#[cfg(unix)]
fn poll_terminal_input(
    tty: &mut std::fs::File,
    timeout: Duration,
) -> io::Result<Option<Vec<u8>>> {
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut descriptor = libc::pollfd {
        fd: tty.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if ready == 0 {
        return Ok(None);
    }
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }

    let mut buffer = vec![0u8; 8192];
    match tty.read(&mut buffer) {
        Ok(0) => Ok(None),
        Ok(count) => {
            buffer.truncate(count);
            Ok(Some(buffer))
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(error) => Err(error),
    }
}

/// Forward raw bytes except the two common unmodified F1 encodings. Other CSI,
/// OSC (including OSC 52), mouse, and bracketed-paste sequences pass unchanged.
fn filter_workspace_input(
    pending: &mut Vec<u8>,
    input: &[u8],
    forwarded: &mut Vec<u8>,
) -> bool {
    const F1_SEQUENCES: [&[u8]; 2] = [b"\x1bOP", b"\x1b[11~"];

    for byte in input.iter().copied() {
        if pending.is_empty() {
            if byte == 0x1b {
                pending.push(byte);
            } else {
                forwarded.push(byte);
            }
            continue;
        }

        pending.push(byte);
        if F1_SEQUENCES
            .iter()
            .any(|sequence| pending.as_slice() == *sequence)
        {
            pending.clear();
            return true;
        }

        let is_f1_prefix = F1_SEQUENCES
            .iter()
            .any(|sequence| sequence.starts_with(pending.as_slice()));
        if !is_f1_prefix {
            forwarded.extend(pending.drain(..));
        }
    }

    false
}

#[cfg(test)]
mod workspace_input_tests {
    use super::filter_workspace_input;

    #[test]
    fn forwards_osc52_replies_byte_for_byte() {
        let mut pending = Vec::new();
        let mut forwarded = Vec::new();
        let response = b"\x1b]52;c;SGVsbG8=\x07";

        assert!(!filter_workspace_input(&mut pending, response, &mut forwarded));
        assert!(pending.is_empty());
        assert_eq!(forwarded, response);
    }

    #[test]
    fn forwards_bracketed_paste_and_other_escape_sequences() {
        let mut pending = Vec::new();
        let mut forwarded = Vec::new();
        let input = b"\x1b[200~hello\x1b[201~\x1b[31m";

        assert!(!filter_workspace_input(&mut pending, input, &mut forwarded));
        assert!(pending.is_empty());
        assert_eq!(forwarded, input);
    }

    #[test]
    fn consumes_both_common_f1_sequences() {
        for sequence in [b"\x1bOP".as_slice(), b"\x1b[11~".as_slice()] {
            let mut pending = Vec::new();
            let mut forwarded = Vec::new();
            assert!(filter_workspace_input(&mut pending, sequence, &mut forwarded));
            assert!(pending.is_empty());
            assert!(forwarded.is_empty());
        }
    }

    #[test]
    fn recognizes_f1_split_across_terminal_reads() {
        let mut pending = Vec::new();
        let mut forwarded = Vec::new();

        assert!(!filter_workspace_input(&mut pending, b"\x1b[11", &mut forwarded));
        assert!(forwarded.is_empty());
        assert!(filter_workspace_input(&mut pending, b"~", &mut forwarded));
        assert!(pending.is_empty());
        assert!(forwarded.is_empty());
    }

    #[test]
    fn forwards_unknown_csi_and_ss3_sequences_without_rewriting() {
        for sequence in [b"\x1b[A".as_slice(), b"\x1bOQ".as_slice()] {
            let mut pending = Vec::new();
            let mut forwarded = Vec::new();
            assert!(!filter_workspace_input(&mut pending, sequence, &mut forwarded));
            assert!(pending.is_empty());
            assert_eq!(forwarded, sequence);
        }
    }
}

#[cfg(not(unix))]
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
    // Keep the splash toggle global, even while help, review, or conversation
    // views own keyboard focus.
    if code == KeyCode::F(12) {
        app.toggle_banner();
        return Ok(false);
    }

    // Conversation list and thread viewer own keyboard focus. The thread
    // is rendered from the current journal on every frame, so stream deltas
    // imported by the main loop appear without closing or reopening the modal.
    if app.pending_review.is_some() {
        handle_review_key(code, modifiers, app);
        return Ok(false);
    }

    if app.show_help {
        if code == KeyCode::Esc {
            app.show_help = false;
        }
        return Ok(false);
    }

    // The SAVE AS overlay owns keyboard focus until Enter or Escape. Nothing
    // typed here can leak into the embedded shell or journal input line.
    if app.search_export_dialog_open {
        handle_search_export_dialog_key(code, modifiers, app);
        return Ok(false);
    }

    if let Some(view) = app.conversation_view.clone() {
        match view {
            ConversationView::List => match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.open_selected_conversation();
                }
                KeyCode::Esc => app.close_conversation_view(),
                KeyCode::Up => app.move_conversation_selection(-1),
                KeyCode::Down => app.move_conversation_selection(1),
                KeyCode::PageUp => app.move_conversation_selection(-8),
                KeyCode::PageDown => app.move_conversation_selection(8),
                KeyCode::Home => app.conversation_list_selected = 0,
                KeyCode::End => {
                    let last = app.conversation_threads().len().saturating_sub(1);
                    app.conversation_list_selected = last;
                }
                KeyCode::Enter => app.open_selected_conversation(),
                _ => {}
            },
            ConversationView::Thread { .. } => match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(text) = app.selected_conversation_text() {
                        if text.len() > 60_000 {
                            app.status = "Conversation exceeds the 60 KB terminal clipboard limit.".to_string();
                        } else {
                            match copy_to_terminal_clipboard(&text) {
                                Ok(()) => app.status = format!(
                                    "Copied {} characters from the Hermes conversation to the client and server clipboards.",
                                    text.chars().count()
                                ),
                                Err(error) => app.status = format!("Could not copy Hermes conversation: {error}"),
                            }
                        }
                    }
                }
                KeyCode::Esc => app.close_conversation_view(),
                KeyCode::Backspace => app.collapse_conversation_thread(),
                KeyCode::Char('n') if !app.matches.is_empty() => app.next_match(),
                KeyCode::Char('N') if !app.matches.is_empty() => app.previous_match(),
                KeyCode::Up => app.scroll_conversation_view(-1),
                KeyCode::Down => app.scroll_conversation_view(1),
                KeyCode::PageUp => app.scroll_conversation_view(-(app.conversation_viewport_height.max(1) as i32)),
                KeyCode::PageDown => app.scroll_conversation_view(app.conversation_viewport_height.max(1) as i32),
                KeyCode::Home => app.conversation_view_home(),
                KeyCode::End => app.conversation_view_end(),
                _ => {}
            },
        }
        return Ok(false);
    }

    // Historical archive records are loaded on demand and remain above the
    // search results workspace until the operator closes the record viewer.
    if app.archive_detail_entry.is_some() {
        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                let text = app.archive_detail_entry
                    .as_ref()
                    .map(|entry| entry.text.clone())
                    .unwrap_or_default();
                if text.len() > 60_000 {
                    app.status = "Archive record exceeds the 60 KB terminal clipboard limit.".to_string();
                } else {
                    match copy_to_terminal_clipboard(&text) {
                        Ok(()) => app.status = format!("Copied {} archived characters.", text.chars().count()),
                        Err(error) => app.status = format!("Could not copy archived record: {error}"),
                    }
                }
            }
            KeyCode::Char('s' | 'S') if modifiers.contains(KeyModifiers::CONTROL) => app.open_search_export_dialog(),
            KeyCode::Esc => app.archive_detail_entry = None,
            KeyCode::Up => app.message_view_scroll = app.message_view_scroll.saturating_sub(1),
            KeyCode::Down => app.message_view_scroll = app.message_view_scroll.saturating_add(1),
            KeyCode::PageUp => app.message_view_scroll = app.message_view_scroll.saturating_sub(app.feed_viewport_height.max(1) as u16),
            KeyCode::PageDown => app.message_view_scroll = app.message_view_scroll.saturating_add(app.feed_viewport_height.max(1) as u16),
            KeyCode::Home => app.message_view_scroll = 0,
            KeyCode::End => app.message_view_scroll = u16::MAX,
            _ => {}
        }
        return Ok(false);
    }

    // The full Hermes message viewer owns keyboard focus until dismissed.
    if app.message_view_index.is_some() {
        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                let modal_text = app
                    .message_view_index
                    .and_then(|index| app.journal.entries.get(index))
                    .map(|entry| entry.text.clone());

                if let Some(text) = modal_text {
                    match copy_to_terminal_clipboard(&text) {
                        Ok(()) => {
                            app.status = format!(
                                "Copied all {} characters to the client and server clipboards.",
                                text.chars().count()
                            );
                        }
                        Err(error) => {
                            app.status = format!("Could not copy Hermes modal content: {error}");
                        }
                    }
                } else {
                    app.status = "No Hermes modal content is available to copy.".to_string();
                }
            }
            KeyCode::Esc => app.close_hermes_message(),
            KeyCode::Up => app.scroll_hermes_message(-1),
            KeyCode::Down => app.scroll_hermes_message(1),
            KeyCode::PageUp => {
                app.scroll_hermes_message(-(app.feed_viewport_height.max(1) as i32));
            }
            KeyCode::PageDown => {
                app.scroll_hermes_message(app.feed_viewport_height.max(1) as i32);
            }
            KeyCode::Home => app.message_view_scroll = 0,
            KeyCode::End => app.message_view_scroll = u16::MAX,
            _ => {}
        }
        return Ok(false);
    }

    // The dedicated search workspace owns transcript navigation until Escape.
    // The main event loop continues capturing shell and peer journal activity.
    if app.search_view {
        match code {
            KeyCode::Char('q') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('s' | 'S') if modifiers.contains(KeyModifiers::CONTROL) => app.open_search_export_dialog(),
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                let selected = app.active_match.and_then(|position| {
                    if let Some(hit) = app.archive_search_hits.get(position) {
                        return app.journal.load_archive_hit(hit).ok().map(|entry| (entry.text, true));
                    }
                    let hit = app.search_hits.get(position)?;
                    let local = app.search_hit_entry_indices.get(position).copied().flatten();
                    let text = local
                        .and_then(|index| app.journal.entries.get(index).map(|entry| entry.text.clone()))
                        .unwrap_or_else(|| hit.snippet.clone());
                    Some((text, local.is_some()))
                });
                if let Some((text, full_record)) = selected {
                    if text.len() > 60_000 {
                        app.status = "Search result exceeds the 60 KB terminal clipboard limit.".to_string();
                    } else {
                        match copy_to_terminal_clipboard(&text) {
                            Ok(()) => app.status = format!(
                                "Copied {} characters from {} search result.",
                                text.chars().count(),
                                if full_record { "full" } else { "indexed snippet" }
                            ),
                            Err(error) => app.status = format!("Could not copy search result: {error}"),
                        }
                    }
                } else {
                    app.status = "Could not load the selected archive record.".to_string();
                }
            }
            KeyCode::Esc => app.exit_search_view(),
            KeyCode::Up => {
                let current = app.active_match.unwrap_or(0);
                app.select_search_result(current.saturating_sub(1));
            }
            KeyCode::Down => {
                let current = app.active_match.unwrap_or(0);
                if !app.search_hits.is_empty() {
                    app.select_search_result(current.saturating_add(1).min(app.matches.len() - 1));
                }
            }
            KeyCode::PageUp => app.scroll_up((app.feed_viewport_height / 2).max(1)),
            KeyCode::PageDown => app.scroll_down((app.feed_viewport_height / 2).max(1)),
            KeyCode::Home => app.select_search_result(0),
            KeyCode::End => app.select_search_result(app.search_hits.len().saturating_sub(1)),
            KeyCode::Char('n') => app.next_match(),
            KeyCode::Char('N') => app.previous_match(),
            KeyCode::Enter => app.open_active_search_result(),
            KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => app.scroll_feed_left(8),
            KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => app.scroll_feed_right(8),
            _ => {}
        }
        return Ok(false);
    }


    // Unmodified F2-F10 select the nine built-in color themes.
    if let Some(number) = theme_number_from_key(code, modifiers) {
        app.set_theme(number);
        return Ok(false);
    }

    if modifiers.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') => {
                // Ctrl+C on a focused HUSR prompt opens its complete thread.
                // An explicit drag/row selection still takes precedence and copies.
                if app.open_selected_hermes_user_thread() {
                    return Ok(false);
                }
                if let Some(selected) = app.selected_transcript_text() {
                    if selected.len() > 60_000 {
                        app.status = "Selection exceeds the 60 KB terminal clipboard limit.".to_string();
                    } else {
                        match copy_to_terminal_clipboard(&selected) {
                            Ok(()) => {
                                app.status = format!(
                                    "Copied {} characters to the client and server clipboards via OSC 52.",
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
            KeyCode::Char('v') | KeyCode::Char('V') => paste_from_system_clipboard(app),
            KeyCode::Char('s') => app.save(),
            KeyCode::Char('p') => app.history_previous(),
            KeyCode::Char('n') => app.history_next(),
            KeyCode::Char('l') => {
                app.clear_input();
                app.status = "Input line cleared.".to_string();
            },
            KeyCode::Char('a') => app.cursor_to_start(),
            KeyCode::Char('e') => app.cursor_to_end(),
            KeyCode::Char('f') => app.focus_search_hint(),
            KeyCode::Char('t') => app.toggle_mode(),
            KeyCode::Char('w') => app.toggle_wrap_transcript(),
            _ => {}
        }
        return Ok(false);
    }

    match code {
        KeyCode::Esc => {
            if app.search_view {
                app.exit_search_view();
            } else if !app.input.is_empty() {
                app.clear_input();
                app.status = "Input line cleared.".to_string();
            } else if app.search_query.is_some() {
                app.clear_search();
                app.jump_to_latest();
            } else {
                app.show_help = false;
            }
        }
        KeyCode::Enter => {
            if !app.open_selected_hermes_message() {
                app.submit(shell.as_mut());
            }
        },
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

fn handle_search_export_dialog_key(code: KeyCode, modifiers: KeyModifiers, app: &mut App) {
    match code {
        KeyCode::Esc => app.cancel_search_export_dialog(),
        KeyCode::Enter => app.confirm_search_export(),
        KeyCode::Left => app.search_export_cursor_left(),
        KeyCode::Right => app.search_export_cursor_right(),
        KeyCode::Home => app.search_export_cursor_start(),
        KeyCode::End => app.search_export_cursor_end(),
        KeyCode::Backspace => app.search_export_backspace(),
        KeyCode::Delete => app.search_export_delete(),
        KeyCode::Char('u' | 'U') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_export_clear_path();
        }
        KeyCode::Char('a' | 'A') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_export_cursor_start();
        }
        KeyCode::Char('e' | 'E') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_export_cursor_end();
        }
        KeyCode::Char(character)
            if !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            app.search_export_insert_text(&character.to_string());
        }
        _ => {}
    }
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

const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
const OSC52_READ_TIMEOUT: Duration = Duration::from_millis(1800);

/// Per-user, per-server clipboard cache. This makes copy/paste available to
/// other OPERATOR sessions on the same host even when no desktop is present.
fn server_clipboard_cache_path() -> PathBuf {
    default_journal_path().with_file_name("clipboard.txt")
}

fn write_server_clipboard_cache(text: &str) -> io::Result<()> {
    let path = server_clipboard_cache_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    write_clipboard_cache_to(&path, text)
}

fn write_clipboard_cache_to(path: &Path, text: &str) -> io::Result<()> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("clipboard text exceeds the {} MiB server cache limit", MAX_CLIPBOARD_BYTES / (1024 * 1024)),
        ));
    }

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "clipboard cache has no parent directory")
    })?;
    fs::create_dir_all(parent)?;

    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("clipboard");
    let temporary = parent.join(format!("{file_name}.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&temporary);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options.open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn read_server_clipboard_cache() -> io::Result<String> {
    read_clipboard_cache_from(&server_clipboard_cache_path())
}

fn read_clipboard_cache_from(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    if metadata.len() as usize > MAX_CLIPBOARD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "server clipboard cache exceeds the 1 MiB read limit",
        ));
    }
    String::from_utf8(fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Decode a terminal's OSC 52 clipboard response. Terminals commonly terminate
/// OSC with BEL; some use ST (ESC backslash).
fn parse_osc52_response(response: &[u8]) -> Option<String> {
    let prefix = b"\x1b]52;c;";
    let body = response.strip_prefix(prefix)?;
    let encoded = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))
        ?;
    let decoded = base64_decode(encoded)?;
    if decoded.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    String::from_utf8(decoded).ok()
}

/// Ask the attached terminal for the client's clipboard using OSC 52 read.
/// This reads /dev/tty directly while the main Crossterm event loop is paused.
/// Clipboard reads are security-sensitive and may be denied by the terminal.
#[cfg(unix)]
fn read_osc52_terminal_clipboard() -> io::Result<Option<String>> {
    let mut tty = OpenOptions::new().read(true).open("/dev/tty")?;

    // Query the clipboard selection ("c"). The response is ESC ] 52 ; c ; BASE64 BEL/ST.
    let mut output = stdout();
    output.write_all(b"\x1b]52;c;?\x1b\\")?;
    output.flush()?;

    let prefix = b"\x1b]52;c;";
    let deadline = Instant::now() + OSC52_READ_TIMEOUT;
    let mut response = Vec::with_capacity(256);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }

        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == 0 {
            return Ok(None);
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        let mut byte = [0u8; 1];
        let count = tty.read(&mut byte)?;
        if count == 0 {
            return Ok(None);
        }
        response.push(byte[0]);

        // Stop immediately if the incoming bytes are not the expected OSC 52
        // response prefix. This prevents consuming an unrelated long sequence.
        if response.len() <= prefix.len() && !prefix.starts_with(&response) {
            return Ok(None);
        }

        if response.ends_with(b"\x07") || response.ends_with(b"\x1b\\") {
            return Ok(parse_osc52_response(&response));
        }
        if response.len() > (MAX_CLIPBOARD_BYTES * 2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OSC 52 clipboard response exceeded the 1 MiB limit",
            ));
        }
    }
}

#[cfg(not(unix))]
fn read_osc52_terminal_clipboard() -> io::Result<Option<String>> {
    Ok(None)
}

/// Read text from the server's desktop clipboard, if one is available.
fn read_system_clipboard() -> Result<String, String> {
    let commands: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-out"]),
        ("xsel", &["--clipboard", "--output"]),
        ("pbpaste", &[]),
        ("powershell.exe", &["-NoProfile", "-Command", "Get-Clipboard"]),
    ];
    let mut failures = Vec::new();

    for (program, args) in commands {
        match Command::new(program)
            .args(*args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout)
                    .trim_end_matches(|ch| ch == '\r' || ch == '\n')
                    .to_string();
                return Ok(text);
            }
            Ok(output) => failures.push(format!("{program} exited with {}", output.status)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }

    let detail = if failures.is_empty() {
        "No clipboard reader is installed (tried wl-paste, xclip, xsel, pbpaste, powershell.exe)."
    } else {
        "Clipboard readers were found but none could read the clipboard."
    };
    Err(detail.to_string())
}

/// Insert clipboard text at the current cursor and mirror it into this server's cache.
fn paste_clipboard_text(app: &mut App, text: &str, source: &str) {
    let chars = text.chars().count();
    app.paste(text);
    match write_server_clipboard_cache(text) {
        Ok(()) => {
            app.status = format!("Pasted {chars} characters from {source}; server clipboard cache synced.");
        }
        Err(error) => {
            app.status = format!("Pasted {chars} characters from {source}, but server cache update failed: {error}");
        }
    }
}

/// Paste from the client terminal first (OSC 52), then the server-local cache,
/// and finally the server desktop clipboard tools.
fn paste_from_system_clipboard(app: &mut App) {
    match read_osc52_terminal_clipboard() {
        Ok(Some(text)) => {
            if text.is_empty() {
                app.status = "Client terminal clipboard is empty.".to_string();
            } else {
                paste_clipboard_text(app, &text, "client OSC 52 clipboard");
            }
            return;
        }
        Ok(None) => {}
        Err(_) => {}
    }

    match read_server_clipboard_cache() {
        Ok(text) => {
            if text.is_empty() {
                app.status = "OPERATOR server clipboard cache is empty.".to_string();
            } else {
                paste_clipboard_text(app, &text, "OPERATOR server clipboard");
            }
            return;
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            app.status = format!("Could not read OPERATOR server clipboard: {error}");
            return;
        }
        Err(_) => {}
    }

    match read_system_clipboard() {
        Ok(text) if text.is_empty() => {
            app.status = "Client OSC 52 read was unavailable and the server clipboard is empty.".to_string();
        }
        Ok(text) => paste_clipboard_text(app, &text, "server OS clipboard"),
        Err(error) => {
            app.status = format!(
                "Could not read clipboard. Enable OSC 52 clipboard reads in your terminal, or use its paste shortcut for bracketed paste. {error}"
            );
        }
    }
}

fn copy_to_terminal_clipboard(text: &str) -> io::Result<()> {
    // Keep a private copy on this host for later pastes and other OPERATOR
    // sessions, then send OSC 52 outward so the attached client clipboard updates.
    write_server_clipboard_cache(text)?;
    let encoded = base64_encode(text.as_bytes());
    let mut output = stdout();
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

fn base64_decode(encoded: &[u8]) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut decoded = Vec::with_capacity(encoded.len().saturating_mul(3) / 4);
    let mut accumulator = 0u32;
    let mut bits = 0u8;

    for byte in encoded.iter().copied() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            break;
        }
        accumulator = (accumulator << 6) | u32::from(value(byte)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push((accumulator >> bits) as u8);
            accumulator &= (1u32 << bits).saturating_sub(1);
            if decoded.len() > MAX_CLIPBOARD_BYTES {
                return None;
            }
        }
    }
    Some(decoded)
}

fn default_journal_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    paths::migrate_legacy_dir(base.join(".local").join("share").join("operator"))
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
mod clipboard_paste_shortcut_tests {
    use super::{is_clipboard_paste_shortcut, KeyCode, KeyModifiers};

    #[test]
    fn ctrl_v_accepts_lowercase_terminal_key_reports() {
        assert!(is_clipboard_paste_shortcut(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn ctrl_v_accepts_uppercase_reports_from_ssh_terminals() {
        assert!(is_clipboard_paste_shortcut(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        ));
        assert!(is_clipboard_paste_shortcut(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL
        ));
    }

    #[test]
    fn shift_insert_is_supported_as_terminal_paste_shortcut() {
        assert!(is_clipboard_paste_shortcut(
            KeyCode::Insert,
            KeyModifiers::SHIFT
        ));
    }

    #[test]
    fn raw_control_v_byte_is_supported() {
        assert!(is_clipboard_paste_shortcut(
            KeyCode::Char('\u{16}'),
            KeyModifiers::NONE
        ));
    }

    #[test]
    fn ordinary_v_is_not_treated_as_a_clipboard_shortcut() {
        assert!(!is_clipboard_paste_shortcut(
            KeyCode::Char('v'),
            KeyModifiers::NONE
        ));
        assert!(!is_clipboard_paste_shortcut(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL
        ));
        assert!(!is_clipboard_paste_shortcut(
            KeyCode::Insert,
            KeyModifiers::NONE
        ));
    }
}

#[cfg(test)]
mod ssh_dispatch_tests {
    use super::{
        detect_file_transfer_request, parse_ssh_client_ip, split_command_words, FileTransferRequest,
    };

    #[test]
    fn extracts_ipv4_and_ipv6_client_addresses_from_ssh_connection() {
        assert_eq!(
            parse_ssh_client_ip(
                Some("192.168.50.125 52341 192.168.50.54 22"),
                None
            )
            .map(|ip| ip.to_string()),
            Some("192.168.50.125".to_string())
        );
        assert_eq!(
            parse_ssh_client_ip(
                Some("2001:db8::25 52341 2001:db8::54 22"),
                None
            )
            .map(|ip| ip.to_string()),
            Some("2001:db8::25".to_string())
        );
    }

    #[test]
    fn falls_back_to_ssh_client_when_ssh_connection_is_unavailable_or_invalid() {
        assert_eq!(
            parse_ssh_client_ip(None, Some("10.0.0.7 52341 22"))
                .map(|ip| ip.to_string()),
            Some("10.0.0.7".to_string())
        );
        assert_eq!(
            parse_ssh_client_ip(Some("not-an-ip malformed"), Some("10.0.0.7 52341 22"))
                .map(|ip| ip.to_string()),
            Some("10.0.0.7".to_string())
        );
    }

    #[test]
    fn rejects_missing_or_invalid_ssh_client_addresses() {
        assert_eq!(parse_ssh_client_ip(None, None), None);
        assert_eq!(parse_ssh_client_ip(Some("bad address"), Some("also-bad")), None);
    }

    #[test]
    fn detects_external_sftp_subsystem() {
        assert_eq!(
            detect_file_transfer_request("/usr/lib/openssh/sftp-server"),
            Some(FileTransferRequest::Sftp)
        );
        assert_eq!(
            detect_file_transfer_request("/usr/lib/openssh/sftp-server -f AUTHPRIV -l INFO"),
            Some(FileTransferRequest::Sftp)
        );
        assert_eq!(
            detect_file_transfer_request("exec /usr/lib/openssh/sftp-server"),
            Some(FileTransferRequest::Sftp)
        );
    }

    #[test]
    fn detects_legacy_scp_upload_and_download_requests() {
        assert_eq!(
            detect_file_transfer_request("scp -t '/home/jordan/a file'"),
            Some(FileTransferRequest::LegacyScp)
        );
        assert_eq!(
            detect_file_transfer_request("/usr/bin/scp -f /tmp/archive.tar"),
            Some(FileTransferRequest::LegacyScp)
        );
    }

    #[test]
    fn does_not_misclassify_interactive_or_unrelated_remote_commands() {
        assert_eq!(detect_file_transfer_request("hostname"), None);
        assert_eq!(detect_file_transfer_request("scp local-file /tmp/file"), None);
        assert_eq!(detect_file_transfer_request("scp -r source destination"), None);
    }

    #[test]
    fn command_word_splitter_preserves_quoted_sftp_arguments() {
        assert_eq!(
            split_command_words("/usr/lib/openssh/sftp-server -d '/home/jordan/My Files'"),
            Some(vec![
                "/usr/lib/openssh/sftp-server".to_string(),
                "-d".to_string(),
                "/home/jordan/My Files".to_string()
            ])
        );
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::{
        base64_decode, base64_encode, parse_osc52_response, read_clipboard_cache_from,
        write_clipboard_cache_to, MAX_CLIPBOARD_BYTES,
    };
    use std::{
        env, fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEST_ID: AtomicUsize = AtomicUsize::new(0);

    fn test_path() -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!("operator-clipboard-{}-{id}.txt", std::process::id()))
    }

    #[test]
    fn base64_encodes_and_decodes_empty_ascii_and_utf8_text() {
        for text in ["", "f", "fo", "foo", "café", "SSH → OPERATOR"] {
            let encoded = base64_encode(text.as_bytes());
            assert_eq!(base64_decode(encoded.as_bytes()).unwrap(), text.as_bytes());
        }
    }

    #[test]
    fn parses_osc52_bel_and_st_responses() {
        let bel = b"\x1b]52;c;SGVsbG8=\x07";
        let st = b"\x1b]52;c;U1NIIHRvIFN0cmVlTm90ZQ==\x1b\\";
        assert_eq!(parse_osc52_response(bel).as_deref(), Some("Hello"));
        assert_eq!(parse_osc52_response(st).as_deref(), Some("SSH to OPERATOR"));
        assert!(parse_osc52_response(b"\x1b]52;p;SGVsbG8=\x07").is_none());
        assert!(parse_osc52_response(b"\x1b]52;c;!!!\x07").is_none());
    }

    #[test]
    fn server_clipboard_cache_round_trips_atomically() {
        let path = test_path();
        write_clipboard_cache_to(&path, "copied on server A").expect("write cache");
        assert_eq!(
            read_clipboard_cache_from(&path).expect("read cache"),
            "copied on server A"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn server_clipboard_cache_rejects_oversized_content() {
        let path = test_path();
        let content = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(write_clipboard_cache_to(&path, &content).is_err());
        assert!(!path.exists());
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


#[cfg(test)]
mod access_splash_tests {
    use super::{
        restricted_access_flash_color, restricted_access_splash, wrap_access_notice, Color,
    };
    use unicode_width::UnicodeWidthStr;

    fn line_text(line: &ratatui::text::Line<'static>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn wide_splash_has_bright_red_lettering_and_army_camouflage_only() {
        let red = Color::Rgb(255, 24, 24);
        let olive = Color::Rgb(112, 128, 72);
        let khaki = Color::Rgb(198, 177, 119);
        let rows = restricted_access_splash(80, khaki, olive, 0, 0);

        assert!(!rows.iter().any(|line| line_text(line).contains("BBS NODE")));
        assert!(!rows.iter().any(|line| line_text(line).contains("SECURE ACCESS GATEWAY")));
        assert!(rows.iter().any(|line| line_text(line).contains("AUTHORIZED PERSONNEL ONLY")));

        let art_rows: Vec<_> = rows.iter().filter(|line| {
            line.spans.iter().any(|span| span.style.fg == Some(red) && span.content.contains('█'))
        }).collect();
        assert_eq!(art_rows.len(), 10, "five bitmap bands per word");
        for row in &art_rows {
            assert!(row.spans.iter().all(|span| span.style.bg.is_some()));
            assert_eq!(UnicodeWidthStr::width(line_text(row).as_str()), 76);
            assert!(row.spans.iter().filter(|span| span.content.contains('█'))
                .all(|span| span.style.fg == Some(red)));
        }

        let colors: std::collections::HashSet<_> = art_rows
            .iter()
            .flat_map(|row| row.spans.iter().filter_map(|span| span.style.bg))
            .collect();
        assert!(colors.len() >= 4, "camouflage should fill the artwork with several field colours");
        assert!(rows.iter().any(|line| line.spans.iter().any(|span| span.style.fg == Some(olive))));
        assert!(rows.iter().any(|line| line.spans.iter().any(|span| span.style.fg == Some(khaki))));

        // All parts of the frame, including centered labels, stay aligned.
        assert!(rows.iter().all(|line| UnicodeWidthStr::width(line_text(line).as_str()) == 76));
        let wrapped = wrap_access_notice(rows, 76);
        assert!(wrapped.iter().all(|line| UnicodeWidthStr::width(line_text(line).as_str()) == 76));
    }

    #[test]
    fn camouflage_pattern_moves_between_animation_frames_but_red_text_stays_fixed() {
        let red = Color::Rgb(255, 24, 24);
        let khaki = Color::Rgb(198, 177, 119);
        let olive = Color::Rgb(112, 128, 72);
        let first = restricted_access_splash(80, khaki, olive, 0, 0);
        let later = restricted_access_splash(80, khaki, olive, 4, 0);

        let letter_mask = |rows: &[ratatui::text::Line<'static>]| {
            rows.iter()
                .filter(|line| line.spans.iter().any(|span| span.content.contains('█')))
                .map(|line| {
                    line.spans
                        .iter()
                        .enumerate()
                        .filter_map(|(index, span)| {
                            (span.style.fg == Some(red) && span.content.contains('█'))
                                .then_some(index)
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(letter_mask(&first), letter_mask(&later), "red letter silhouettes remain stationary");

        let first_backgrounds: Vec<_> = first.iter().filter(|line| line.spans.iter().any(|span| span.content.contains('█')))
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.bg)).collect();
        let later_backgrounds: Vec<_> = later.iter().filter(|line| line.spans.iter().any(|span| span.content.contains('█')))
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.bg)).collect();
        assert_ne!(first_backgrounds, later_backgrounds, "camouflage texture should drift");
    }

    #[test]
    fn access_warning_flashes_red_orange_yellow_orange() {
        let red = Color::Rgb(255, 24, 24);
        let orange = Color::Rgb(255, 96, 0);
        let yellow = Color::Rgb(255, 255, 0);

        assert_eq!(restricted_access_flash_color(0), red);
        assert_eq!(restricted_access_flash_color(1), red);
        assert_eq!(restricted_access_flash_color(2), orange);
        assert_eq!(restricted_access_flash_color(3), yellow);
        assert_eq!(restricted_access_flash_color(4), yellow);
        assert_eq!(restricted_access_flash_color(5), orange);
        assert_eq!(restricted_access_flash_color(6), red);
    }

    #[test]
    fn splash_lettering_uses_each_flash_colour() {
        let warning = Color::Rgb(198, 177, 119);
        let olive = Color::Rgb(112, 128, 72);
        let red = Color::Rgb(255, 24, 24);
        let orange = Color::Rgb(255, 96, 0);
        let yellow = Color::Rgb(255, 255, 0);

        for (phase, expected) in [(0, red), (2, orange), (3, yellow), (5, orange)] {
            let rows = restricted_access_splash(80, warning, olive, 0, phase);
            assert!(rows.iter().any(|line| {
                line.spans.iter().any(|span| span.style.fg == Some(expected) && span.content.contains('█'))
            }), "phase {phase} should render its warning colour in the large lettering");
        }
    }

    #[test]
    fn narrow_terminal_gets_compact_bright_red_warning() {
        let red = Color::Rgb(255, 24, 24);
        let rows = restricted_access_splash(
            24,
            Color::Rgb(198, 177, 119),
            Color::Rgb(112, 128, 72),
            0,
            0,
        );

        assert!(!rows.iter().any(|line| line_text(line).contains('█')));
        assert!(rows.iter().any(|line| {
            line_text(line).contains("RESTRICTED ACCESS")
                && line.spans.iter().any(|span| span.style.fg == Some(red))
        }));
        assert!(rows.iter().all(|line| UnicodeWidthStr::width(line_text(line).as_str()) <= 76));
    }
}
