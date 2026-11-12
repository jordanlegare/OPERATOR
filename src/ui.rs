use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    buffer::CellDiffOption,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{App, ConversationView, InputMode},
    conversations::{collapse_live_streams, ConversationThread},
    storage::{entry_display_text, entry_transcript_text, format_timestamp, Entry, EntryKind},
    theme::{Theme, THEMES},
};


pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = THEMES.get(app.theme_index.saturating_sub(1)).copied().unwrap_or(THEMES[0]);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), frame.area());

    // The application stays fully rendered underneath the optional splash.
    // F12 simply toggles App::show_banner; the overlay never changes input
    // handling, terminal state, or the persisted banner preference.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_status(frame, chunks[0], app, theme);
    draw_feed(frame, chunks[1], app, theme);
    draw_input(frame, chunks[2], app, theme);
    draw_hints(frame, chunks[3], app, theme);

    if let Some(index) = app.message_view_index {
        if let Some(entry) = app.journal.entries.get(index) {
            draw_hermes_message(frame, entry, index, app.message_view_scroll, theme);
        }
    }
    if let Some(entry) = app.archive_detail_entry.as_ref() {
        draw_hermes_message(
            frame,
            entry,
            app.archive_detail_line_number.saturating_sub(1),
            app.message_view_scroll,
            theme,
        );
    }
    if let Some(view) = app.conversation_view.clone() {
        match view {
            ConversationView::List => draw_conversation_list(frame, app, theme),
            ConversationView::Thread { id } => draw_conversation_thread(frame, app, &id, theme),
        }
    }
    if app.pending_review.is_some() {
        draw_hermes_review(frame, app, theme);
    }
    if app.show_help
        && app.pending_review.is_none()
        && app.message_view_index.is_none()
        && app.conversation_view.is_none()
    {
        draw_help(frame, frame.area(), theme);
    }

    // Render last so the splash is a true foreground overlay. F12 removes it
    // immediately and reveals the live application underneath.
    if app.show_banner {
        draw_banner(frame, frame.area(), theme);
    }
    if app.search_export_dialog_open && app.pending_review.is_none() {
        draw_search_export_dialog(frame, app, theme);
    }
}

fn review_editor_lines(app: &App, theme: Theme) -> Vec<Line<'static>> {
    let selection = app.review_selected_range();
    let normal_style = Style::default().fg(theme.text).bg(theme.panel);
    let selected_style = Style::default()
        .fg(theme.bg)
        .bg(theme.info)
        .add_modifier(Modifier::BOLD);
    let mut result = Vec::new();
    let mut absolute_line_start = 0usize;

    for line in app.review_text.split('\n') {
        let mut spans = Vec::new();
        let mut run = String::new();
        let mut run_selected: Option<bool> = None;

        for (offset, character) in line.char_indices() {
            let absolute_offset = absolute_line_start + offset;
            let is_selected = selection
                .is_some_and(|(start, end)| absolute_offset >= start && absolute_offset < end);
            if run_selected.is_some_and(|previous| previous != is_selected) {
                let style = if run_selected == Some(true) {
                    selected_style
                } else {
                    normal_style
                };
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            run_selected = Some(is_selected);
            run.push(character);
        }

        if !run.is_empty() {
            let style = if run_selected == Some(true) {
                selected_style
            } else {
                normal_style
            };
            spans.push(Span::styled(run, style));
        }
        result.push(Line::from(spans));
        absolute_line_start = absolute_line_start.saturating_add(line.len()).saturating_add(1);
    }

    result
}

fn draw_hermes_review(frame: &mut Frame, app: &mut App, theme: Theme) {
    let Some((request_id, session_id, model, platform)) = app.pending_review.as_ref().map(|request| (
        request.request_id.clone(),
        request.session_id.clone(),
        request.model.clone(),
        request.platform.clone(),
    )) else {
        return;
    };

    let popup = centered_rect(96, 94, frame.area());
    frame.render_widget(Clear, popup);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(255, 32, 32)))
        .title(Span::styled(
            " HERMES FINAL RESPONSE · HELD BEFORE DELIVERY · OPERATOR REVIEW REQUIRED ",
            Style::default().fg(Color::Rgb(255, 64, 32)).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(inner);

    let model_label = if model.is_empty() { "unknown" } else { &model };
    let platform_label = if platform.is_empty() { "unknown" } else { &platform };
    let session_label = if session_id.is_empty() {
        "unknown".to_string()
    } else {
        session_id.chars().take(20).collect::<String>()
    };
    let metadata = vec![
        Line::from(vec![
            Span::styled(" MODEL ", Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
            Span::styled(model_label.to_string(), Style::default().fg(theme.text)),
            Span::styled("   PLATFORM ", Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
            Span::styled(platform_label.to_string(), Style::default().fg(theme.text)),
            Span::styled("   SESSION ", Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
            Span::styled(session_label, Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled(" REVIEW ID ", Style::default().fg(theme.muted).add_modifier(Modifier::BOLD)),
            Span::styled(request_id, Style::default().fg(theme.muted)),
            Span::styled("   Edit the answer below. Nothing is delivered until you release it.", Style::default().fg(theme.warning)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(metadata).style(Style::default().bg(theme.panel)),
        sections[0],
    );

    let cursor = app.review_cursor.min(app.review_text.len());
    let before_cursor = &app.review_text[..cursor];
    let line_index = before_cursor.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = before_cursor.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let cursor_column = UnicodeWidthStr::width(&app.review_text[line_start..cursor]);
    let body_height = sections[1].height.max(1) as usize;
    let body_width = sections[1].width.max(1) as usize;

    if line_index < app.review_vertical_scroll as usize {
        app.review_vertical_scroll = line_index as u16;
    } else if line_index >= app.review_vertical_scroll as usize + body_height {
        app.review_vertical_scroll = line_index
            .saturating_add(1)
            .saturating_sub(body_height)
            .min(u16::MAX as usize) as u16;
    }
    if cursor_column < app.review_horizontal_scroll as usize {
        app.review_horizontal_scroll = cursor_column as u16;
    } else if cursor_column >= app.review_horizontal_scroll as usize + body_width {
        app.review_horizontal_scroll = cursor_column
            .saturating_add(1)
            .saturating_sub(body_width)
            .min(u16::MAX as usize) as u16;
    }

    let editor_title = format!(
        " EDIT RESPONSE · {} CHARACTERS · {} LINES · {} BYTES ",
        app.review_text.chars().count(),
        app.review_text.lines().count().max(1),
        app.review_text.len()
    );
    let editor_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            editor_title,
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let editor_area = sections[1];
    let editor_inner = editor_block.inner(editor_area);
    frame.render_widget(
        Paragraph::new(review_editor_lines(app, theme))
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .scroll((app.review_vertical_scroll, app.review_horizontal_scroll))
            .block(editor_block),
        editor_area,
    );

    let cursor_x = editor_inner
        .x
        .saturating_add(cursor_column.saturating_sub(app.review_horizontal_scroll as usize) as u16);
    let cursor_y = editor_inner
        .y
        .saturating_add(line_index.saturating_sub(app.review_vertical_scroll as usize) as u16);
    if cursor_x < editor_inner.x.saturating_add(editor_inner.width)
        && cursor_y < editor_inner.y.saturating_add(editor_inner.height)
    {
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }

    let footer = vec![
        Line::from(vec![
            Span::styled(" STATUS ", Style::default().fg(theme.bg).bg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::styled(app.status.clone(), Style::default().fg(theme.text)),
        ]),
        Line::from(vec![
            Span::styled(" F2 ", Style::default().fg(theme.bg).bg(theme.success).add_modifier(Modifier::BOLD)),
            Span::styled(" RELEASE ORIGINAL    ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::styled(" F3 ", Style::default().fg(theme.bg).bg(theme.info).add_modifier(Modifier::BOLD)),
            Span::styled(" RELEASE EDITED    ", Style::default().fg(theme.info).add_modifier(Modifier::BOLD)),
            Span::styled(" F4 ", Style::default().fg(theme.bg).bg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::styled(" WITHHOLD RESPONSE", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(Span::styled(
            "Ctrl+A select all · Ctrl+V clipboard · Shift+Insert / terminal Paste inserts text · type to replace · Esc keeps held",
            Style::default().fg(theme.muted),
        )),
    ];
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().bg(theme.panel)),
        sections[2],
    );
}

fn draw_hermes_message(
    frame: &mut Frame,
    entry: &Entry,
    index: usize,
    scroll: u16,
    theme: Theme,
) {
    let popup = centered_rect(90, 88, frame.area());
    frame.render_widget(Clear, popup);

    let title = match entry.kind {
        EntryKind::Note => format!(
            " JOURNAL NOTE · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::TerminalCommand => format!(
            " TERMINAL COMMAND · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::TerminalOutput => format!(
            " TERMINAL OUTPUT · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::System => format!(
            " SYSTEM EVENT · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesUser => format!(
            " HERMES USER MESSAGE · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesAssistant => format!(
            " HERMES ASSISTANT RESPONSE · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesToolCall => format!(
            " HERMES TOOL CALL · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesToolResult => format!(
            " HERMES TOOL RESULT · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesHistory => format!(
            " HERMES PRIOR CONVERSATION CONTEXT · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesInterim => format!(
            " HERMES INTERIM MESSAGE · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::HermesSession => format!(
            " HERMES SESSION EVENT · ROW {} · {} ",
            index + 1,
            format_timestamp(&entry.timestamp)
        ),
        EntryKind::OperatorSessionStart => format!(" OPERATOR SESSION START · ROW {} · {} ", index + 1, format_timestamp(&entry.timestamp)),
        EntryKind::OperatorSessionHeartbeat => format!(" OPERATOR SESSION HEARTBEAT · ROW {} · {} ", index + 1, format_timestamp(&entry.timestamp)),
        EntryKind::OperatorSessionEnd => format!(" OPERATOR SESSION END · ROW {} · {} ", index + 1, format_timestamp(&entry.timestamp)),
        EntryKind::OperatorWorkspaceStart => format!(" F1 WORKSPACE START · ROW {} · {} ", index + 1, format_timestamp(&entry.timestamp)),
        EntryKind::OperatorWorkspaceEnd => format!(" F1 WORKSPACE END · ROW {} · {} ", index + 1, format_timestamp(&entry.timestamp)),
        _ => " HERMES MESSAGE ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightYellow))
        .title(Span::styled(
            title,
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(entry.text.clone())
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(" ↑/↓ scroll · PgUp/PgDn page · Home/End · Ctrl+C copy · Esc close ")
            .style(Style::default().fg(theme.muted).bg(theme.panel)),
        sections[1],
    );
}

fn draw_conversation_list(frame: &mut Frame, app: &App, theme: Theme) {
    let threads = app.conversation_threads();
    let popup = centered_rect(88, 82, frame.area());
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(255, 255, 0)))
        .title(Span::styled(
            format!(" HERMES CONVERSATIONS · {} THREAD(S) · ENTER EXPANDS · ESC CLOSES ", threads.len()),
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let height = inner.height.max(1) as usize;
    let start = app.conversation_list_selected
        .saturating_sub(height / 2)
        .min(threads.len().saturating_sub(height));
    let lines = threads
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, thread)| conversation_summary_line(thread, index == app.conversation_list_selected, theme))
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn conversation_summary_line(thread: &ConversationThread, selected: bool, theme: Theme) -> Line<'static> {
    let state = if thread.streaming { "LIVE" } else { "" };
    let id = thread.id.strip_prefix("session:").unwrap_or(&thread.id);
    let short_id = id.chars().take(10).collect::<String>();
    let mut style = Style::default().fg(theme.text);
    if selected {
        style = style.bg(theme.selected_row).add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(if selected { " ▶ " } else { "   " }, Style::default().fg(theme.primary).patch(style)),
        Span::styled(format!("{short_id:<10} "), Style::default().fg(theme.info).patch(style)),
        Span::styled(format!("{:>2} turn(s) · {:>2} tool(s)  ", thread.turn_count, thread.tool_call_count), Style::default().fg(theme.muted).patch(style)),
        Span::styled(state.to_string(), Style::default().fg(if thread.streaming { theme.success } else { theme.muted }).add_modifier(Modifier::BOLD).patch(style)),
        Span::styled(format!("  {}", thread.title), style),
    ])
}


fn draw_conversation_thread(frame: &mut Frame, app: &mut App, id: &str, theme: Theme) {
    let (entry_indices, turn_count, tool_call_count, streaming) = {
        let threads = app.conversation_threads();
        let Some(thread) = threads.iter().find(|thread| thread.id.as_str() == id) else {
            return;
        };
        (thread.entries.clone(), thread.turn_count, thread.tool_call_count, thread.streaming)
    };
    let entries = entry_indices.iter()
        .filter_map(|index| app.journal.entries.get(*index).map(|entry| (*index, entry)))
        .collect::<Vec<_>>();
    // Keep streamed text visible during tool activity; once a final assistant
    // event follows the latest delta and tool event, it replaces the live block.
    let collapsed_streams = collapse_live_streams(&entries);
    let replaced_assistant_entries = &collapsed_streams.replaced_assistant_indices;
    let focus_entry = app.conversation_focus_entry.take();
    let query = app.search_query.as_deref().filter(|query| !query.is_empty());
    let needle = query.map(str::to_ascii_lowercase);

    let mut lines = Vec::new();
    let mut focus_line_index = None;
    let mut previous_turn: Option<String> = None;
    for (index, entry) in entries {
        if matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd) {
            continue;
        }
        if replaced_assistant_entries.contains(&index)
            || collapsed_streams.hidden_delta_indices.contains(&index)
        {
            continue;
        }
        if entry.turn_id.is_some() && previous_turn != entry.turn_id {
            if !lines.is_empty() {
                lines.push(Line::from(Span::styled("  ─────────────────────────────────────────────────────────────", Style::default().fg(theme.border))));
            }
            previous_turn = entry.turn_id.clone();
        }

        let (label, color, body) = if let Some(stream_text) =
            collapsed_streams.text_by_first_index.get(&index)
        {
            ("ASSISTANT - LIVE STREAM", theme.primary, stream_text.as_str())
        } else {
            let (label, color) = match entry.kind {
                EntryKind::HermesUser => ("USER", theme.info),
                EntryKind::HermesAssistant => ("ASSISTANT", theme.success),
                EntryKind::HermesToolCall => ("TOOL CALL", theme.warning),
                EntryKind::HermesToolResult => ("TOOL RESULT", theme.accent),
                EntryKind::HermesHistory => ("PRIOR CONTEXT", theme.info),
                EntryKind::HermesInterim => ("INTERIM MESSAGE", theme.primary),
                EntryKind::HermesSession => ("SESSION EVENT", theme.muted),
                EntryKind::OperatorSessionStart => ("OPERATOR SESSION START", theme.success),
                EntryKind::OperatorSessionHeartbeat => ("OPERATOR SESSION HEARTBEAT", theme.muted),
                EntryKind::OperatorSessionEnd => ("OPERATOR SESSION END", theme.success),
                EntryKind::OperatorWorkspaceStart => ("F1 WORKSPACE START", theme.info),
                EntryKind::OperatorWorkspaceEnd => ("F1 WORKSPACE END", theme.info),
                EntryKind::HermesStreamDelta => ("ASSISTANT - LIVE STREAM", theme.primary),
                _ => ("HERMES", theme.muted),
            };
            (label, color, entry.text.as_str())
        };
        let active = app.is_active_match(index);
        let message_style = if active {
            Style::default().bg(theme.selected_row)
        } else {
            Style::default()
        };
        let header_line = lines.len();
        if focus_entry == Some(index) {
            focus_line_index = Some(header_line);
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label}  "),
                Style::default().fg(color).add_modifier(Modifier::BOLD).patch(message_style),
            ),
            Span::styled(
                format!("{}  ·  row {}", format_timestamp(&entry.timestamp), index + 1),
                Style::default().fg(theme.muted).patch(message_style),
            ),
        ]));

        for body_line in body.lines() {
            if focus_entry == Some(index)
                && focus_line_index == Some(header_line)
                && needle.as_ref().is_some_and(|needle| body_line.to_ascii_lowercase().contains(needle.as_str()))
            {
                focus_line_index = Some(lines.len());
            }
            let body_style = Style::default().fg(theme.text).patch(message_style);
            let mut spans = vec![Span::styled("  ", body_style)];
            spans.extend(search_highlighted_spans(body_line, query, theme, body_style));
            lines.push(Line::from(spans));
        }
        if body.is_empty() {
            lines.push(Line::from(Span::styled("  ", message_style)));
        }
        lines.push(Line::from(""));
    }

    let state = if streaming { "STREAMING" } else { "CAPTURED" };
    let id_label = id.strip_prefix("session:").unwrap_or(id);
    let short_id = id_label.chars().take(12).collect::<String>();
    let popup = centered_rect(94, 90, frame.area());
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(255, 255, 0)))
        .title(Span::styled(
            format!(" HERMES THREAD {short_id} · {turn_count} TURN(S) · {tool_call_count} TOOL(S) · {state} "),
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    // Ratatui counts wrapped visual rows, so position the selected search hit
    // using the actual thread width rather than the unwrapped Line index.
    let paragraph_width = sections[0].width.max(1);
    let focus_scroll = focus_line_index.map(|line_index| {
        Paragraph::new(lines.iter().take(line_index).cloned().collect::<Vec<_>>())
            .wrap(Wrap { trim: false })
            .line_count(paragraph_width)
            .saturating_sub(1)
    });
    let paragraph = Paragraph::new(lines)
        .style(Style::default().fg(theme.text).bg(theme.panel))
        .wrap(Wrap { trim: false });
    app.conversation_content_height = paragraph.line_count(paragraph_width);
    app.conversation_viewport_height = sections[0].height.max(1) as usize;
    let max_scroll = app.conversation_content_height
        .saturating_sub(app.conversation_viewport_height);
    let scroll = if let Some(focus_scroll) = focus_scroll {
        let scroll = focus_scroll.min(max_scroll);
        app.conversation_scroll = scroll;
        app.conversation_follow_tail = false;
        scroll
    } else if app.conversation_follow_tail {
        max_scroll
    } else {
        app.conversation_scroll.min(max_scroll)
    };
    frame.render_widget(
        paragraph.scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        sections[0],
    );
    let footer = if app.matches.is_empty() {
        " ↑/↓ scroll · PgUp/PgDn page · Home/End · Ctrl+C copy thread · Esc close "
    } else {
        " ↑/↓ scroll · n/N next/previous search hit · Ctrl+C copy thread · Esc close "
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(theme.muted).bg(theme.panel)),
        sections[1],
    );
}

fn draw_banner(frame: &mut Frame, area: Rect, theme: Theme) {
    const LOGO: [&str; 5] = [
        r"  ____  ____  _____ ____    _  _____ ___  ____  ",
        r" / __ \|  _ \| ____|  _ \  / \|_   _/ _ \|  _ \ ",
        r"| |  | | |_) |  _| | |_) |/ _ \ | || | | | |_) |",
        r"| |__| |  __/| |___|  _ </ ___ \| || |_| |  _ < ",
        r" \____/|_|   |_____|_| \_/_/   \_\_| \___/|_| \_\",
    ];

    const POPPY: [&str; 33] = [
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⡠⠖⠛⠢⠤⠤⢤⣀⡀⢀⣠⣶⣲⠀⠈⠉⠉⠑⠒⣦⠤⢄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⣀⣤⠔⠚⠻⠤⠤⣀⡀⠀⠀⠀⠀⢫⢿⠿⣌⡇⠀⠀⠀⠀⠀⠀⡏⠀⠀⠉⠢⣄⠀⠀⢀⣀⣀⣀⣀⡀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⣴⣾⠟⠁⠀⣄⠀⠀⠀⡀⠀⠉⠳⣄⠀⠀⠀⢿⠀⠘⢳⠀⡀⠀⠀⠀⢸⡇⢀⢠⠃⠀⡨⠗⢉⣟⡁⠀⠀⠀⠉⠉⢲⢤⡀⠀⠀⠀",
        r"⠀⢰⠀⠈⠳⣄⢄⠈⢦⡀⠀⠘⣦⠀⠀⢫⣣⠀⠀⢸⠀⡆⠀⣟⡇⠀⠀⢀⡿⠀⣜⢇⡴⠋⠀⠀⣎⠞⠀⠀⠀⠀⠀⣰⣤⠾⠛⠻⢶⡄",
        r"⠀⢸⠀⠀⠀⠘⢿⣷⢄⠘⣶⢄⠘⢷⡀⠀⢳⣧⡀⢸⡈⠇⠀⢸⡇⠀⢰⢸⡁⢰⣹⠎⠀⠀⠀⣸⡎⢀⠞⠀⣠⢶⡟⠋⠀⠀⠀⠀⡌⠋",
        r"⠀⢸⠀⠀⠀⢀⠈⠻⣷⣝⠮⣳⣄⠀⠻⣆⠘⠸⣧⢀⢿⠀⠀⠈⡄⠀⡞⢸⡇⠀⡟⠀⠀⠀⢰⡟⣴⠃⢠⠞⠀⣼⠀⠀⠰⠀⠀⢀⠂⠀",
        r"⠀⠈⢧⠀⠀⠈⠳⣄⠈⠻⣧⣌⠻⣷⡀⠹⣻⡆⠘⡷⠱⣧⠀⣠⣷⣀⣧⣤⣧⣸⠁⠀⠀⢀⣿⡿⢁⣴⠏⠀⢰⠃⠀⢀⠀⠀⢀⠿⢒⠏",
        r"⠀⢀⣈⡆⠀⠀⠀⠈⠓⢤⡈⠛⢷⣄⠻⡄⢹⣷⡀⣸⣾⣿⣿⣿⣾⣿⣿⣿⣿⣥⣧⣗⣀⣾⢿⣾⢿⠋⠀⣠⠇⠀⠀⠈⣼⣸⠫⣠⡾⠀",
        r"⠞⢁⠀⠹⡀⠀⠀⠀⠀⠀⠙⢦⣀⠙⠢⡹⣄⠹⣷⣟⣿⢿⣼⣿⠿⠙⢛⣛⠷⣾⣧⣿⣿⣷⠋⡡⠋⣠⣪⣿⠀⠀⠀⢰⣿⠃⣦⡞⠁⠀",
        r"⣇⠀⠱⣄⢙⡦⡀⠀⠀⠀⠀⠄⣙⠷⣢⣌⣮⡢⣬⣽⣷⣿⣿⣿⡀⠸⢷⡤⢀⣿⣿⣿⣭⣿⠏⠀⢀⣿⡿⣝⣀⢤⣶⣞⣣⡼⠟⠀⠀⠀",
        r"⡇⠀⠙⠚⠵⣍⣛⠳⢤⣀⣤⣤⣌⣉⣒⣯⣝⣛⣻⣿⢿⣿⣿⣟⣿⣷⣶⣾⣿⣿⣟⡿⣯⣸⣀⣴⣿⠵⢺⡷⠋⠉⠀⣠⠎⠀⠀⠀⠀⠀",
        r"⠹⣄⠀⠀⢠⠀⢉⡳⢆⣨⠽⠖⣦⣤⣬⣭⣭⣿⣿⣿⣿⣟⣛⣿⣿⣿⣿⣿⣿⣿⣤⣤⣿⣿⣿⡿⠗⠀⢸⠀⠀⢀⡔⠁⠀⠀⠀⠀⠀⠀",
        r"⠀⠈⠓⠦⣼⣄⠀⠀⠀⠀⠉⠻⠥⠤⣴⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣽⣿⣿⣟⣿⣿⣿⡿⠟⠉⠀⠀⣼⠁⠀⣠⠊⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⣶⣾⡷⣄⠀⠀⠀⠀⠀⠀⠀⠀⠘⢛⣿⣿⣟⣿⣿⣿⣿⡏⠙⡟⣿⢿⣿⠏⠀⠀⠀⠀⣶⠏⢀⠔⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠙⠷⣤⣽⢶⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠰⠟⢻⡿⠋⡟⠃⣀⡧⠽⠿⠧⠖⢊⣵⣶⡶⠿⠖⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠈⠉⠛⠛⠓⠲⠶⣶⣦⢦⡤⣀⡠⠤⠯⠤⠤⠛⠉⠀⠀⠀⣠⣶⢞⠿⠃⣲⣤⢤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⣀⡴⠒⠲⣶⠾⡏⡟⠙⣦⠈⠿⡛⢾⢷⣄⣠⣴⣦⣀⣠⣶⠶⠛⡽⢳⠏⣴⣟⡵⠷⢶⡾⠂⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠾⣢⡝⢁⣴⣷⠞⣧⣠⢤⡇⠓⠤⠼⣆⠃⠰⡄⠉⠉⢹⣯⣿⣠⠘⢠⢯⢞⣛⡶⢾⣗⣮⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠈⠩⢶⣿⣾⣵⡂⠸⣅⣾⣤⣴⣚⣛⠾⢷⣄⢙⠦⣀⣸⣿⣿⠤⠤⢶⣭⣄⡀⠒⠲⢶⣮⣝⣣⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠘⠋⡾⢻⣄⣤⣿⠟⠁⠀⠙⢦⣀⠀⠈⢋⣱⣾⠋⣡⠆⠀⠀⡄⢳⢹⡿⠲⢄⠹⡮⠑⢯⡣⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠀⠀⠀⠀⠀⠀⠀⠀⠈⠉⠒⠯⢅⣿⢞⣼⡷⢳⡀⢸⠀⣷⡷⡄⠀⠘⣇⠈⠠⣝⣿⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⣿⣿⡇⠀⡷⠃⠀⢹⠹⣽⣆⠀⠙⠀⠀⠈⠻⠦⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣟⡇⡼⠷⡾⠃⡏⢧⣿⠛⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⡇⠀⢺⡏⣼⡇⠈⠻⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⡇⠀⠘⠊⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⡏⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢻⣧⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢸⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢹⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        r"⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    ];

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // Colour travels from right to left across every character; after crossing
    // the left boundary the spectrum wraps and re-enters from the far right.
    // Position never changes, so the motion is entirely chromatic.
    let phase = (now_ms % 6_480) as f32 / 18.0;
    let popup = centered_rect(96, 96, area);
    frame.render_widget(Clear, popup);

    let border = spectrum_color(phase + 180.0);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border).add_modifier(Modifier::BOLD))
        .title(Span::styled(
            " ✧ O P E R A T O R  //  SYSTEM CREDITS ✧ ",
            Style::default().fg(spectrum_color(phase + 35.0)).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if inner.height < 8 || inner.width < 24 {
        frame.render_widget(
            Paragraph::new("✦ OPERATOR · F12 HIDE ✦")
                .alignment(Alignment::Left)
                .style(Style::default().fg(spectrum_color(phase)).bg(theme.panel)),
            inner,
        );
        return;
    }

    let quote = "- NEVER AGAIN";
    let credit = "Released with love by Jordan Légaré - 11/11/2026";
    let quote_width = UnicodeWidthStr::width(quote);
    let credit_width = UnicodeWidthStr::width(credit);
    let footer_height = if inner.width as usize >= quote_width + credit_width + 1 {
        1usize
    } else {
        2usize
    };

    // Reserve enough vertical space for the complete poem before assigning
    // remaining rows to the poppy. On compact terminals the artwork contracts,
    // never the poem or its line breaks.
    const POEM_HEIGHT: usize = 26;
    let total_height = inner.height as usize;
    let art_height = (total_height / 3)
        .min(total_height.saturating_sub(footer_height + POEM_HEIGHT))
        .min(POPPY.len());
    let main_height = total_height.saturating_sub(art_height + footer_height);

    let main_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: main_height as u16,
    };
    let art_area = Rect {
        x: inner.x,
        y: inner.y.saturating_add(main_height as u16),
        width: inner.width,
        height: art_height as u16,
    };
    let footer_area = Rect {
        x: inner.x,
        y: inner.y.saturating_add((main_height + art_height) as u16),
        width: inner.width,
        height: footer_height as u16,
    };

    // The left column keeps the identity and command guide; the right column
    // is reserved for the poem. A fixed split avoids collisions as colours roll.
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(main_area);
    let left_area = columns[0];
    let right_area = columns[1];

    const GUIDE: [&str; 13] = [
        "PTY SHELL    Interactive shell; commands and output enter the journal.",
        "NOTES        Ctrl+T switches Terminal / Notes; /note TEXT saves a note.",
        "SEARCH       /search WORDS scans records and Hermes conversations.",
        "HERMES       Prompts, streams, tools, threads, and response review.",
        "THREADS      /threads browses conversations; Enter expands a thread.",
        "REVIEW       /review on|off|status controls answer release.",
        "COMMANDS     /view · /save · /export · Ctrl+S SAVE AS · /stats · /path",
        "CONTROL      Tab completion · Ctrl+P/N history · Ctrl+W wrap · Ctrl+S save / search SAVE AS.",
        "THEMES       F2–F10 changes palettes; F12 hides or restores this splash.",
        "HELP         /help opens the complete command and keyboard reference.",
        "SSH          F1 opens the terminal workspace; SCP/SFTP is configurable.",
        "JOURNAL      Shared indexed journal; JSONL import/export remains available.",
        "FIELD NOTE   The work continues. The record remains. Never again.",
    ];

    let mut content = Vec::new();
    if left_area.width >= 56 && main_area.height >= 7 {
        for (row, line) in LOGO.iter().enumerate() {
            content.push(psychedelic_line(line, phase, row, true));
        }
        content.push(psychedelic_line(
            "✦ SHELL  ·  NOTES  ·  JOURNAL  ·  HERMES  ·  OPERATOR ✦",
            phase + 80.0,
            6,
            true,
        ));
    } else {
        content.push(psychedelic_line("✦ O P E R A T O R ✦", phase + 80.0, 0, true));
        content.push(psychedelic_line(
            "SHELL  ·  NOTES  ·  JOURNAL  ·  HERMES",
            phase + 110.0,
            1,
            true,
        ));
    }
    content.push(Line::from(""));
    let guide_slots = (left_area.height as usize).saturating_sub(content.len());
    for (row, line) in GUIDE.iter().take(guide_slots).enumerate() {
        content.push(psychedelic_line(line, phase + 145.0, row + 8, row == 0));
    }

    let content_height = content.len().min(left_area.height as usize);
    let top_padding = (left_area.height as usize).saturating_sub(content_height) / 2;
    let content_area = Rect {
        x: left_area.x,
        y: left_area.y.saturating_add(top_padding as u16),
        width: left_area.width,
        height: content_height as u16,
    };
    frame.render_widget(
        Paragraph::new(content)
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.text).bg(theme.panel)),
        content_area,
    );

    // Keep the poem in the right-hand column, without a vertical divider.
    // Each line is centred horizontally while the complete poem stays centred
    // vertically in the available space.
    let poem_block = Block::default().style(Style::default().bg(theme.panel));
    let poem_inner = poem_block.inner(right_area);
    frame.render_widget(poem_block, right_area);

    const POEM: [&str; POEM_HEIGHT] = [
        "-=- Snowflakes and Ashes -=-",
        "",
        "All that's left are minerals",
        "the frost succession",
        "All that's left are the funerals",
        "the body processions",
        "",
        "One by one they fall",
        "as white and grey they were",
        "One by one they crawl",
        "as white and grey matter",
        "",
        "For winter has brought a dream",
        "many awake, few sleeping",
        "For their dreams turned to ashes",
        "a restless, relentless; struggling",
        "",
        "I closed my eyes to a headache",
        "blinded by the light, Awake",
        "They carry out burden and ache",
        "blinded by the snowflake, awake",
        "",
        "I release you in heaven",
        "White, grey a raven",
        "on a windy day",
        "I got carried away.",
    ];

    let compact_poem = poem_inner.height < POEM_HEIGHT as u16;
    let mut poem_lines = Vec::with_capacity(POEM_HEIGHT);
    for (row, text) in POEM.iter().enumerate() {
        if text.is_empty() {
            if !compact_poem {
                poem_lines.push(Line::from(""));
            }
            continue;
        }

        if row == 0 {
            poem_lines.push(psychedelic_line(
                "❄  -= Snowflakes and Ashes =-  ❄",
                phase + 230.0,
                row,
                true,
            ));
        } else {
            let mut line = psychedelic_line(text, phase + 210.0, row, false);
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::ITALIC);
            }
            poem_lines.push(line);
        }
    }

    let poem_height = poem_lines.len().min(poem_inner.height as usize);
    let poem_top = (poem_inner.height as usize).saturating_sub(poem_height) / 2;
    let poem_area = Rect {
        x: poem_inner.x,
        y: poem_inner.y.saturating_add(poem_top as u16),
        width: poem_inner.width,
        height: poem_height as u16,
    };
    frame.render_widget(
        Paragraph::new(poem_lines)
            .alignment(Alignment::Center)
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .wrap(Wrap { trim: false }),
        poem_area,
    );

    // Keep the poppy at bottom-left, separate from the right-hand poem.
    let poppy_width = columns[0].width;
    let art_area = Rect {
        x: inner.x,
        y: art_area.y,
        width: poppy_width,
        height: art_area.height,
    };
    let mut poppy = Vec::with_capacity(art_height);
    for row in 0..art_height {
        let source_row = if art_height <= 1 {
            0
        } else {
            row.saturating_mul(POPPY.len() - 1) / (art_height - 1)
        };
        let fitted = fit_poppy_row(POPPY[source_row], art_area.width as usize);
        poppy.push(psychedelic_line(&fitted, phase + 205.0, row, row % 3 == 0));
    }
    frame.render_widget(
        Paragraph::new(poppy)
            .alignment(Alignment::Left)
            .style(Style::default().bg(theme.panel)),
        art_area,
    );

    let mut footer_lines = Vec::with_capacity(footer_height);
    if footer_height == 1 {
        let spacer_width = (inner.width as usize)
            .saturating_sub(quote_width + credit_width);
        let mut spans = psychedelic_line(quote, phase + 15.0, 0, true).spans;
        spans.push(Span::raw(" ".repeat(spacer_width)));
        spans.extend(psychedelic_line(credit, phase + 190.0, 1, true).spans);
        footer_lines.push(Line::from(spans));
    } else {
        footer_lines.push(psychedelic_line(quote, phase + 15.0, 0, true));
        let left_padding = (inner.width as usize).saturating_sub(credit_width);
        let mut spans = vec![Span::raw(" ".repeat(left_padding))];
        spans.extend(psychedelic_line(credit, phase + 190.0, 1, true).spans);
        footer_lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(footer_lines)
            .alignment(Alignment::Left)
            .style(Style::default().bg(theme.panel)),
        footer_area,
    );
}

fn fit_poppy_row(source: &str, target_width: usize) -> String {
    let characters = source.chars().collect::<Vec<_>>();
    if characters.len() <= target_width {
        return source.to_string();
    }
    if target_width == 0 {
        return String::new();
    }
    (0..target_width)
        .map(|column| characters[column.saturating_mul(characters.len()) / target_width])
        .collect()
}

fn psychedelic_line(
    source: &str,
    phase: f32,
    row: usize,
    bold: bool,
) -> Line<'static> {
    let spans = source
        .chars()
        .enumerate()
        .map(|(column, character)| {
            let hue = phase + column as f32 * 8.5 + row as f32 * 19.0;
            let mut style = Style::default().fg(spectrum_color(hue));
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(character.to_string(), style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn spectrum_color(hue: f32) -> Color {
    let hue = hue.rem_euclid(360.0);
    let sector = (hue / 60.0).floor() as u8;
    let fraction = (hue / 60.0) - sector as f32;
    let rising = fraction * 255.0;
    let falling = (1.0 - fraction) * 255.0;
    let (red, green, blue) = match sector % 6 {
        0 => (255.0, rising, 0.0),
        1 => (falling, 255.0, 0.0),
        2 => (0.0, 255.0, rising),
        3 => (0.0, falling, 255.0),
        4 => (rising, 0.0, 255.0),
        _ => (255.0, 0.0, falling),
    };
    Color::Rgb(red as u8, green as u8, blue as u8)
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    let mode_color = match app.input_mode {
        InputMode::Terminal => theme.success,
        InputMode::Notes => theme.warning,
    };
    let mode = if app.search_view { "SEARCH" } else if app.follow_tail { "LIVE" } else { "SCROLLBACK" };
    let privacy_hint = if app.secret_input { "  ·  HIDDEN INPUT" } else { "" };
    let reviewer_label = if app.active_reviewer_count == 1 {
        "reviewer"
    } else {
        "reviewers"
    };
    let handled_label = if app.reviews_handled == 1 {
        "review handled here"
    } else {
        "reviews handled here"
    };
    let status = Line::from(vec![
        Span::styled(" ● ", Style::default().fg(mode_color)),
        Span::styled(mode, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "  {}{}  ·  theme {}/9: {}  ·  {} 30d rows  ·  {} B hot cache  ·  {} {} active  ·  {} {}  │  ",
                app.input_mode.label(),
                privacy_hint,
                app.theme_index,
                theme.name,
                app.journal.entries.len(),
                app.journal.total_text_bytes,
                app.active_reviewer_count,
                reviewer_label,
                app.reviews_handled,
                handled_label
            ),
            Style::default().fg(theme.muted),
        ),
        Span::styled(app.status.clone(), Style::default().fg(theme.text)),
    ]);
    frame.render_widget(Paragraph::new(status), area);
}

fn draw_feed(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    if app.search_view {
        draw_search_results_feed(frame, area, app, theme);
        return;
    }

    let title = format!(
        " TRANSCRIPT  ·  {} ROWS  ·  30-DAY WINDOW  ·  {}  ·  SEARCHABLE ",
        app.visible_transcript_count(),
        if app.wrap_transcript { "WORD WRAP" } else { "UNWRAPPED" }
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(
            title,
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    app.feed_area = inner;
    app.feed_viewport_height = inner.height.max(1) as usize;
    frame.render_widget(block, area);

    let top = app.current_top();
    let visible_indices = app.visible_transcript_indices();
    let visible: Vec<Line> = visible_indices
        .iter()
        .skip(top)
        .filter_map(|index| {
            app.journal
                .entries
                .get(*index)
                .map(|entry| entry_line(entry, *index, app, theme))
        })
        .collect();

    let lines = if visible.is_empty() {
        vec![Line::from(vec![
            Span::styled("  No terminal output yet. ", Style::default().fg(theme.muted)),
            Span::styled(
                "Run a Debian command below, or Ctrl+T to capture notes.",
                Style::default().fg(theme.text),
            ),
        ])]
    } else {
        visible
    };

    let paragraph = Paragraph::new(lines);
    if app.wrap_transcript {
        let visual_rows: usize = visible_indices
            .iter()
            .skip(top)
            .filter_map(|index| app.journal.entries.get(*index))
            .map(|entry| {
                let width = UnicodeWidthStr::width(entry_transcript_text(entry, 0).as_str());
                width.max(1).div_ceil(inner.width.max(1) as usize)
            })
            .sum();
        let vertical_scroll = if app.follow_tail {
            visual_rows.saturating_sub(inner.height as usize)
        } else {
            0
        };
        frame.render_widget(
            paragraph.wrap(Wrap { trim: false }).scroll((vertical_scroll.min(u16::MAX as usize) as u16, 0)),
            inner,
        );
    } else {
        frame.render_widget(paragraph.scroll((0, app.feed_hscroll)), inner);
    }

    // Paint the watermark only into unused cells, so transcript text, selections,
    // and wide-character continuation cells always remain untouched.

    if app.show_canada_watermark {
        draw_maple_watermark(frame, inner, theme);
    }
}

fn draw_search_results_feed(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    let query = app.search_query.as_deref().unwrap_or("");
    let title = format!(
        " {} · {} RESULTS · OFF {} / LIM {} · CTRL+S SAVE AS · ESC LIVE ",
        app.search_mode_label,
        app.search_total,
        app.search_page_offset,
        app.search_page_limit
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.info))
        .title(Span::styled(title, Style::default().fg(theme.primary).add_modifier(Modifier::BOLD)))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    app.feed_area = inner;
    app.feed_viewport_height = inner.height.max(1) as usize;
    frame.render_widget(block, area);

    let mut lines = Vec::new();
    let mut previous_audit_session: Option<String> = None;
    if let Some(error) = app.search_error.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("  Search error: {error}"),
            Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
        )));
    } else if app.pending_postgres_search_is_pending() {
        lines.push(Line::from(Span::styled(
            format!("  Searching {}: {query}", app.search_mode_label),
            Style::default().fg(theme.info),
        )));
    } else if app.search_hits.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  No results for {query:?}. Press Esc to return to the live transcript."),
            Style::default().fg(theme.muted),
        )));
    } else {
        for (position, hit) in app.search_hits.iter().enumerate() {
            if let Some(session_id) = hit.operator_session_id.as_deref() {
                if previous_audit_session.as_deref() != Some(session_id) {
                    let username = hit.ssh_username.as_deref().unwrap_or("unknown");
                    let ip = hit.ssh_client_ip.as_deref().unwrap_or("local / unknown IP");
                    let short_id = session_id.chars().take(18).collect::<String>();
                    lines.push(Line::from(Span::styled(
                        format!("  ── OPERATOR SESSION {short_id} · {username}@{ip} · UID {} ──", hit.unix_uid.map(|uid| uid.to_string()).unwrap_or_else(|| "?".to_string())),
                        Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
                    )));
                    previous_audit_session = Some(session_id.to_string());
                }
            } else if previous_audit_session.is_some() {
                previous_audit_session = None;
            }
            let index = app.search_hit_entry_indices.get(position).copied().flatten();
            let selected = app.active_match == Some(position);
            let row_style = if selected {
                Style::default().bg(theme.selected_row).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let kind = match hit.kind.as_str() {
                "note" => "NOTE",
                "terminal_command" => "COMMAND",
                "terminal_output" => "OUTPUT",
                "system" => "SYSTEM",
                "hermes_user" => "HERMES USER",
                "hermes_assistant" => "HERMES ASSISTANT",
                "hermes_tool_call" => "HERMES TOOL",
                "hermes_tool_result" => "HERMES RESULT",
                "hermes_stream_start" => "STREAM START",
                "hermes_stream_delta" => "STREAM DELTA",
                "hermes_stream_end" => "STREAM END",
                "hermes_history" => "HERMES HISTORY",
                "hermes_interim" => "HERMES INTERIM",
                "hermes_session" => "HERMES SESSION",
                "operator_session_start" => "SESSION START",
                "operator_session_heartbeat" => "SESSION PULSE",
                "operator_session_end" => "SESSION END",
                "operator_workspace_start" => "WORKSPACE START",
                "operator_workspace_end" => "WORKSPACE END",
                other => other,
            };
            let row_label = app.archive_search_hits.get(position)
                .map(|archive_hit| format!("[ARCHIVE LINE {}]", archive_hit.line_number))
                .or_else(|| index.map(|row| format!("[ROW {}]", row + 1)))
                .unwrap_or_else(|| format!("[DB {}]", hit.record_id.chars().take(10).collect::<String>()));
            let score_label = if app.archive_search_hits.is_empty() && hit.operator_session_id.is_none() {
                format!(" score:{:.3}", hit.rank)
            } else {
                String::new()
            };
            let attribution = if hit.operator_session_id.is_some() {
                format!(
                    "  {} · {}",
                    hit.operator_surface.as_deref().unwrap_or("transcript"),
                    hit.operator_session_id.as_deref().unwrap_or("")
                )
            } else {
                "  UNATTRIBUTED / LEGACY".to_string()
            };
            let preview = hit.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
            let preview = preview.chars().take(320).collect::<String>();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} {:>5}  ", if selected { "▶" } else { " " }, app.search_page_offset + position + 1),
                    Style::default().fg(theme.primary).patch(row_style),
                ),
                Span::styled(
                    format!("{}  ", format_timestamp(&hit.occurred_at)),
                    Style::default().fg(theme.success).patch(row_style),
                ),
                Span::styled(
                    format!("{kind:<16}"),
                    Style::default().fg(theme.info).add_modifier(Modifier::BOLD).patch(row_style),
                ),
                Span::styled(
                    format!("  {row_label}{score_label}{attribution}"),
                    Style::default().fg(theme.muted).patch(row_style),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("       ", Style::default().patch(row_style)),
                Span::styled(preview, Style::default().fg(theme.text).patch(row_style)),
            ]));
        }
        lines.push(Line::from(Span::styled(
            format!("  Query: {query}"),
            Style::default().fg(theme.muted),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .scroll(((app.search_scroll.saturating_mul(2)).min(u16::MAX as usize) as u16, app.feed_hscroll)),
        inner,
    );
}

fn draw_maple_watermark(frame: &mut Frame, area: Rect, theme: Theme) {
    const WATERMARK_ART: [&str; 17] = [
        "-------------------------------------------------------------------",
        "XWXWXWXWXWXWXWXW                                   WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX                 A                 XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW                AWA                WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX           AA  AWXWA  AA           XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW            VWXWXWXWXWV            WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX        AA   VWXWXWXWV   AA        XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW    VWXWXWXA  VWXWXWV  AXWXWXWV    WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX     XWXWXWXWXWXWXWXWXWXWXWXWX     XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW   AXWXWXWXWXWXWXWXWXWXWXWXWXWXA   WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX      VXWXWXWXWXWXWXWXWXWXWXV      XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW         VXWXWXWXWXWXWXWXV         WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX           XWXWXWXWXWXWX           XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW         AXWXWXWXWXWXWXWXA         WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX                 I                 XWXWXWXWXWXWXWXW",
        "XWXWXWXWXWXWXWXW                 I                 WXWXWXWXWXWXWXWX",
        "WXWXWXWXWXWXWXWX                 I                 XWXWXWXWXWXWXWXW",
    ];

    if area.width < 24 || area.height < 8 {
        return;
    }

    // Keep the watermark centered and within 80% of the transcript viewport.
    let target_width = (area.width.saturating_mul(4) / 5).max(1);
    let target_height = (area.height.saturating_mul(4) / 5).max(1);
    let left = area.x + (area.width - target_width) / 2;
    let top = area.y + (area.height - target_height) / 2;
    let source_width = WATERMARK_ART.iter().map(|line| line.len()).max().unwrap_or(1);
    let watermark_color = watermark_tint(theme.panel, theme.border);
    let buffer = frame.buffer_mut();

    for y in 0..target_height {
        let source_y = (y as usize * WATERMARK_ART.len() / target_height as usize)
            .min(WATERMARK_ART.len() - 1);
        let source_line = WATERMARK_ART[source_y].as_bytes();

        for x in 0..target_width {
            let source_x = (x as usize * source_width / target_width as usize)
                .min(source_width - 1);
            let Some(&byte) = source_line.get(source_x) else {
                continue;
            };
            if byte == b' ' {
                continue;
            }

            let position = (left + x, top + y);
            if let Some(cell) = buffer.cell_mut(position) {
                if cell.symbol() == " "
                    && cell.diff_option == CellDiffOption::None
                    && cell.bg != theme.selected_row
                    && cell.bg != theme.search_bg
                {
                    cell.set_char(byte as char).set_fg(watermark_color);
                }
            }
        }
    }
}

/// Split text into styled spans and highlight every case-insensitive occurrence
/// of the active search query. ASCII folding preserves byte offsets for slicing.
fn search_highlighted_spans(
    text: &str,
    query: Option<&str>,
    theme: Theme,
    base_style: Style,
) -> Vec<Span<'static>> {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return vec![Span::styled(text.to_string(), base_style)];
    };

    let needle = query.to_ascii_lowercase();
    let haystack = text.to_ascii_lowercase();
    let highlight_style = Style::default()
        .fg(theme.search_fg)
        .bg(theme.search_bg)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut cursor = 0usize;

    while let Some(relative_start) = haystack[cursor..].find(needle.as_str()) {
        let start = cursor + relative_start;
        let end = start + needle.len();
        if start > cursor {
            spans.push(Span::styled(text[cursor..start].to_string(), base_style));
        }
        spans.push(Span::styled(text[start..end].to_string(), highlight_style));
        cursor = end;
    }

    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base_style));
    }
    spans
}

/// Blend the border color toward the panel color to keep the watermark subtle.
fn watermark_tint(panel: Color, border: Color) -> Color {
    match (panel, border) {
        (Color::Rgb(pr, pg, pb), Color::Rgb(br, bg, bb)) => Color::Rgb(
            ((u16::from(pr) * 70 + u16::from(br) * 30) / 100) as u8,
            ((u16::from(pg) * 70 + u16::from(bg) * 30) / 100) as u8,
            ((u16::from(pb) * 70 + u16::from(bb) * 30) / 100) as u8,
        ),
        (_, border) => border,
    }
}

fn entry_line(entry: &Entry, index: usize, app: &App, theme: Theme) -> Line<'static> {
    let timestamp = format_timestamp(&entry.timestamp);
    let number = format!("{:>6} ", index + 1);
    let row_text = entry_transcript_text(entry, index);
    let display_text = entry_display_text(entry);
    let selected_columns =
        app.selection_columns_for_row(index, UnicodeWidthStr::width(row_text.as_str()));
    let active = app.is_active_match(index);
    let keyboard_selected = app.selected_row == Some(index);
    let hit = app.is_match(index);
    let row_style = if active || keyboard_selected {
        Style::default().bg(theme.selected_row)
    } else {
        Style::default()
    };
    let (tag, tag_color) = match entry.kind {
        EntryKind::Note => ("NOTE", theme.accent),
        EntryKind::TerminalCommand => ("CMD ", theme.warning),
        EntryKind::TerminalOutput => ("OUT ", theme.info),
        EntryKind::System => ("SYS ", theme.success),
        EntryKind::HermesUser => ("HUSR", theme.info),
        EntryKind::HermesAssistant => ("HAI ", theme.success),
        EntryKind::HermesToolCall => ("HCAL", theme.warning),
        EntryKind::HermesToolResult => ("HRES", theme.success),
        EntryKind::HermesStreamStart => ("HSTR", theme.primary),
        EntryKind::HermesStreamDelta => ("HDEL", theme.success),
        EntryKind::HermesStreamEnd => ("HEND", theme.muted),
        EntryKind::HermesHistory => ("HCTX", theme.info),
        EntryKind::HermesInterim => ("HINT", theme.primary),
        EntryKind::HermesSession => ("HSES", theme.muted),
        EntryKind::OperatorSessionStart => ("OPST", theme.success),
        EntryKind::OperatorSessionHeartbeat => ("OPHB", theme.muted),
        EntryKind::OperatorSessionEnd => ("OPEN", theme.success),
        EntryKind::OperatorWorkspaceStart => ("WKST", theme.info),
        EntryKind::OperatorWorkspaceEnd => ("WKED", theme.info),
    };

    let mut spans = vec![
        Span::styled(number, Style::default().fg(theme.muted).patch(row_style)),
        Span::styled(format!("{} ", timestamp), Style::default().fg(theme.success).patch(row_style)),
        Span::styled(format!("{tag} "), Style::default().fg(tag_color).add_modifier(Modifier::BOLD).patch(row_style)),
        Span::styled("│ ", Style::default().fg(theme.border).patch(row_style)),
    ];

    spans.extend(search_highlighted_spans(
        &display_text,
        app.search_query.as_deref().filter(|_| hit),
        theme,
        Style::default().fg(theme.text).patch(row_style),
    ));

    apply_selection(Line::from(spans), selected_columns, theme)
}

fn apply_selection(
    line: Line<'static>,
    selection: Option<(usize, usize)>,
    theme: Theme,
) -> Line<'static> {
    let Some((selection_start, selection_end)) = selection else {
        return line;
    };

    let mut result = Vec::new();
    let mut display_column = 0usize;
    let mut previous_cell_selected = false;

    for span in line.spans {
        let base_style = span.style;
        let mut chunk = String::new();
        let mut chunk_selected: Option<bool> = None;

        for character in span.content.chars() {
            let width = UnicodeWidthChar::width(character).unwrap_or(0);
            let selected = if width == 0 {
                previous_cell_selected
            } else {
                display_column < selection_end
                    && display_column.saturating_add(width) > selection_start
            };

            if chunk_selected.is_some_and(|current| current != selected) {
                let style = if chunk_selected == Some(true) {
                    base_style.bg(theme.selected_row)
                } else {
                    base_style
                };
                result.push(Span::styled(std::mem::take(&mut chunk), style));
                chunk_selected = None;
            }
            if chunk_selected.is_none() {
                chunk_selected = Some(selected);
            }
            chunk.push(character);

            if width > 0 {
                previous_cell_selected = selected;
                display_column = display_column.saturating_add(width);
            }
        }

        if !chunk.is_empty() {
            let style = if chunk_selected == Some(true) {
                base_style.bg(theme.selected_row)
            } else {
                base_style
            };
            result.push(Span::styled(chunk, style));
        }
    }

    Line::from(result)
}

fn draw_input(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    app.input_area = area;
    let title = if app.search_view {
        " SEARCH RESULTS  ·  ↑/↓ SELECT  ·  n/N NEXT/PREVIOUS  ·  ENTER OPEN CONTEXT  ·  ESC RETURN LIVE "
    } else if app.secret_input {
        " HIDDEN INPUT  ·  ENTER TO SEND WITHOUT LOGGING  ·  CTRL+V / RIGHT-CLICK TO PASTE  ·  CTRL+C TO INTERRUPT "
    } else {
        match app.input_mode {
            InputMode::Terminal => " DEBIAN TERMINAL  ·  ENTER TO EXECUTE  ·  CTRL+V / RIGHT-CLICK PASTE  ·  OSC 52 CLIPBOARD  ·  CTRL+T FOR NOTES ",
            InputMode::Notes => " NOTE CAPTURE  ·  ENTER TO SAVE  ·  CTRL+V / RIGHT-CLICK PASTE  ·  OSC 52 CLIPBOARD  ·  CTRL+T FOR TERMINAL ",
        }
    };
    let mode_color = if app.secret_input {
        theme.warning
    } else if app.input_mode == InputMode::Terminal {
        theme.primary
    } else {
        theme.warning
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(mode_color))
        .title(Span::styled(
            title,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(area);
    let prompt = app.input_prompt().to_string();
    let prompt_width = UnicodeWidthStr::width(prompt.as_str());
    let available = inner.width.saturating_sub(prompt_width as u16).max(1) as usize;
    let before_cursor = &app.input[..app.cursor.min(app.input.len())];
    let cursor_text = if app.secret_input {
        "•".repeat(before_cursor.chars().count())
    } else {
        before_cursor.to_string()
    };
    let cursor_column = prompt_width + UnicodeWidthStr::width(cursor_text.as_str());
    if cursor_column < app.input_hscroll {
        app.input_hscroll = cursor_column;
    } else if cursor_column >= app.input_hscroll.saturating_add(available) {
        app.input_hscroll = cursor_column.saturating_sub(available.saturating_sub(1));
    }

    let line = Line::from(vec![
        Span::styled(
            prompt.to_string(),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(visible_input(app), Style::default().fg(theme.text)),
    ]);
    frame.render_widget(
        Paragraph::new(line)
            .scroll((0, app.input_hscroll as u16))
            .block(block),
        area,
    );

    let cursor_x = inner
        .x
        .saturating_add(cursor_column.saturating_sub(app.input_hscroll) as u16);
    let cursor_y = inner.y;
    if cursor_x < inner.x.saturating_add(inner.width) {
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
}

fn visible_input(app: &App) -> String {
    if app.secret_input {
        "•".repeat(app.input.chars().count())
    } else {
        app.input.clone()
    }
}

fn draw_hints(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    if app.search_view {
        let hints = Line::from(vec![
            Span::styled(" ↑/↓ ", Style::default().fg(theme.primary)),
            Span::styled("select result  ", Style::default().fg(theme.muted)),
            Span::styled("PgUp/PgDn ", Style::default().fg(theme.primary)),
            Span::styled("scroll  ", Style::default().fg(theme.muted)),
            Span::styled("n/N ", Style::default().fg(theme.primary)),
            Span::styled("next/previous  ", Style::default().fg(theme.muted)),
            Span::styled("Enter ", Style::default().fg(theme.primary)),
            Span::styled("open context  ", Style::default().fg(theme.muted)),
            Span::styled("Esc ", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::styled("return to LIVE", Style::default().fg(theme.muted)),
        ]);
        frame.render_widget(Paragraph::new(hints), area);
        return;
    }
    let hints = Line::from(vec![
        Span::styled(" Ctrl+T ", Style::default().fg(theme.primary)),
        Span::styled("mode  ", Style::default().fg(theme.muted)),
        Span::styled("/scp ", Style::default().fg(theme.primary).add_modifier(Modifier::BOLD)),
        Span::styled(
            if app.scp_sftp_passthrough { "ON" } else { "OFF" },
            Style::default()
                .fg(if app.scp_sftp_passthrough { theme.success } else { theme.warning })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" on|off  ", Style::default().fg(theme.muted)),
        Span::styled("/canada ", Style::default().fg(theme.primary).add_modifier(Modifier::BOLD)),
        Span::styled(
            if app.show_canada_watermark { "ON" } else { "OFF" },
            Style::default()
                .fg(if app.show_canada_watermark { theme.success } else { theme.warning })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" toggle  ", Style::default().fg(theme.muted)),
        Span::styled(" F2–F10 ", Style::default().fg(theme.primary)),
        Span::styled("theme  ", Style::default().fg(theme.muted)),
        Span::styled("↑/↓ ", Style::default().fg(theme.primary)),
        Span::styled("select row  ", Style::default().fg(theme.muted)),
        Span::styled("wheel ", Style::default().fg(theme.primary)),
        Span::styled("mouse  ", Style::default().fg(theme.muted)),
        Span::styled("Tab ", Style::default().fg(theme.primary)),
        Span::styled("complete  ", Style::default().fg(theme.muted)),
        Span::styled("Ctrl+W ", Style::default().fg(theme.primary)),
        Span::styled(if app.wrap_transcript { "wrap on  " } else { "wrap off  " }, Style::default().fg(theme.muted)),
        Span::styled("Ctrl+P/N ", Style::default().fg(theme.primary)),
        Span::styled("history  ", Style::default().fg(theme.muted)),
        Span::styled("/search ", Style::default().fg(theme.primary)),
        Span::styled("journal  ", Style::default().fg(theme.muted)),
        Span::styled("/threads ", Style::default().fg(theme.primary)),
        Span::styled("Hermes  ", Style::default().fg(theme.muted)),
        Span::styled("F1 ", Style::default().fg(theme.primary)),
        Span::styled("terminal workspace  ", Style::default().fg(theme.muted)),
        Span::styled("F12 ", Style::default().fg(theme.primary)),
        Span::styled("banner  ", Style::default().fg(theme.muted)),
        Span::styled("Ctrl+C ", Style::default().fg(theme.primary)),
        Span::styled("copy  ", Style::default().fg(theme.muted)),
        Span::styled("Ctrl+Q ", Style::default().fg(theme.primary)),
        Span::styled("quit", Style::default().fg(theme.muted)),
    ]);
    frame.render_widget(Paragraph::new(hints), area);
}

fn draw_help(frame: &mut Frame, area: Rect, theme: Theme) {
    let popup = centered_rect(82, 84, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            " HELP ",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    frame.render_widget(
        Paragraph::new(help_lines(theme))
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_search_export_dialog(frame: &mut Frame, app: &App, theme: Theme) {
    let popup = centered_rect(78, 34, frame.area());
    frame.render_widget(Clear, popup);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.info))
        .title(Span::styled(
            " SAVE AS · SEARCH RESULTS ",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = outer.inner(popup);
    frame.render_widget(outer, popup);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    let result_count = app.archive_search_hits.len().max(app.search_hits.len());
    let query = app.search_query.as_deref().unwrap_or("");
    frame.render_widget(
        Paragraph::new(format!(
            "Export {result_count} displayed result(s) for {query:?}. Only this page is included."
        ))
        .style(Style::default().fg(theme.text).bg(theme.panel))
        .alignment(Alignment::Center),
        sections[0],
    );

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            " FILE NAME / DESTINATION ",
            Style::default().fg(theme.success).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let input_area = input_block.inner(sections[1]);
    frame.render_widget(input_block, sections[1]);

    let cursor = app.search_export_cursor.min(app.search_export_path.len());
    let before = &app.search_export_path[..cursor];
    let after = &app.search_export_path[cursor..];
    let cursor_style = Style::default()
        .fg(theme.bg)
        .bg(theme.info)
        .add_modifier(Modifier::BOLD);
    let cursor_span = Span::styled("▏", cursor_style);
    let line = if app.search_export_path.is_empty() {
        Line::from(vec![
            cursor_span,
            Span::styled("operator-search-results.txt", Style::default().fg(theme.muted)),
        ])
    } else {
        Line::from(vec![
            Span::styled(before, Style::default().fg(theme.text)),
            cursor_span,
            Span::styled(after, Style::default().fg(theme.text)),
        ])
    };

    let available_width = input_area.width.saturating_sub(2).max(1) as usize;
    let cursor_width = UnicodeWidthStr::width(before);
    let horizontal_scroll = cursor_width.saturating_sub(available_width.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .scroll((0, horizontal_scroll.min(u16::MAX as usize) as u16)),
        input_area,
    );
    let cursor_x = input_area
        .x
        .saturating_add(1)
        .saturating_add(cursor_width.saturating_sub(horizontal_scroll).min(u16::MAX as usize) as u16)
        .min(input_area.x.saturating_add(input_area.width.saturating_sub(2)));
    let cursor_y = input_area.y.saturating_add(1);
    frame.set_cursor_position(Position::new(cursor_x, cursor_y));

    let footer = app.search_export_error.as_deref().unwrap_or(
        "ENTER EXPORT  ·  ESC CANCEL  ·  CTRL+U CLEAR PATH",
    );
    frame.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(if app.search_export_error.is_some() {
                theme.warning
            } else {
                theme.muted
            }).bg(theme.panel))
            .alignment(Alignment::Center),
        sections[2],
    );
}

fn help_lines(theme: Theme) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "OPERATOR · DEBIAN SHELL + NOTE JOURNAL",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("TERMINAL  ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Starts an interactive Debian shell in a PTY at startup. Type commands and press Enter."),
        ]),
        Line::from(vec![
            Span::styled("TRANSCRIPT", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("  Every submitted command and captured stdout/stderr line is persisted to the configured journal backend."),
        ]),
        Line::from(vec![
            Span::styled("NOTES     ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Ctrl+T toggles Notes / Terminal. In terminal mode, /note text saves a note; !command runs a shell command from Notes mode."),
        ]),
        Line::from(vec![
            Span::styled("SEARCH    ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("/search runs indexed PostgreSQL full-text, phrase, tsquery, literal and trigram searches; Esc returns LIVE."),
        ]),
        Line::from(vec![
            Span::styled("SAVE AS   ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Ctrl+S in search results opens the centered filename dialog; its default is operator-search-results.txt in the live shell cwd shown in the input prompt."),
        ]),
        Line::from(vec![
            Span::styled("SAVE KEYS ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Enter exports full records from the displayed page; Esc cancels. Type or paste a filename; arrows/Home/End move, Backspace/Delete edit, Ctrl+U clears, Ctrl+A/E jump to either end."),
        ]),
        Line::from(vec![
            Span::styled("AUDIT     ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Each login has a distinct OPERATOR session ID. SSH IP, account, UID, ports, TTY, reviewer instance, and activity origin are attached to new local journal entries."),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("LOCAL COMMANDS", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))]),
        Line::from(vec![Span::styled("/search WORDS", Style::default().fg(theme.warning)), Span::raw("  Web search; quotes, OR alternatives and -excluded terms; Esc returns LIVE.")]),
        Line::from(vec![Span::styled("/search --literal TEXT", Style::default().fg(theme.warning)), Span::raw("  Case-insensitive substring matching with escaped SQL wildcards.")]),
        Line::from(vec![Span::styled("/search --phrase TEXT", Style::default().fg(theme.warning)), Span::raw("  PostgreSQL phrase search using phraseto_tsquery.")]),
        Line::from(vec![Span::styled("/search --tsquery QUERY", Style::default().fg(theme.warning)), Span::raw("  Advanced PostgreSQL syntax: &, |, !, <-> and :*.")]),
        Line::from(vec![Span::styled("/search --fuzzy TEXT", Style::default().fg(theme.warning)), Span::raw("  Trigram similarity search, ranked by similarity().")]),
        Line::from(vec![Span::styled("/audit FILTERS", Style::default().fg(theme.warning)), Span::raw("  Search every SSH/OPERATOR session by IP, login, UID, or session ID; results are grouped by session.")]),
        Line::from(vec![Span::styled("Audit filters", Style::default().fg(theme.warning)), Span::raw("  ip:, user:, uid:, key:, operator_session:, kind:, limit:, offset:; PostgreSQL also supports after: and before:.")]),
        Line::from(vec![Span::styled("Search filters", Style::default().fg(theme.warning)), Span::raw("  PostgreSQL also supports session: (Hermes), conversation:, after:, before:, limit: and offset:.")]),
        Line::from(vec![Span::styled("/review on|off|status", Style::default().fg(theme.warning)), Span::raw("  Hold Hermes final answers for operator review before delivery.")]),
        Line::from(vec![Span::styled("/view", Style::default().fg(theme.warning)), Span::raw("           Open the full Markdown body of a selected Hermes event.")]),
        Line::from(vec![Span::styled("/threads", Style::default().fg(theme.warning)), Span::raw("        Browse conversations; Enter or Ctrl+C expands the selected thread.")]),
        Line::from(vec![Span::styled("n / N", Style::default().fg(theme.warning)), Span::raw("          Next / previous search result when input is empty.")]),
        Line::from(vec![Span::styled("/mode [shell|notes]", Style::default().fg(theme.warning)), Span::raw("  Switch input mode; no argument toggles.")]),
        Line::from(vec![Span::styled("/note TEXT", Style::default().fg(theme.warning)), Span::raw("      Save a note without leaving terminal mode; /note alone switches to Notes mode.")]),
        Line::from(vec![Span::styled("/save", Style::default().fg(theme.warning)), Span::raw("          Retry pending writes and sync the journal.")]),
        Line::from(vec![Span::styled("/export [FILE]", Style::default().fg(theme.warning)), Span::raw(" Export every journal record as readable text.")]),
        Line::from(vec![Span::styled("/export-jsonl [FILE]", Style::default().fg(theme.warning)), Span::raw(" Export the original uncoalesced journal events as JSONL.")]),
        Line::from(vec![Span::styled("/stats · /path", Style::default().fg(theme.warning)), Span::raw("  Show journal statistics, backend and import/export path.")]),
        Line::from(vec![Span::styled("/scp on|off|status", Style::default().fg(theme.warning)), Span::raw("  Toggle persistent SSH SCP/SFTP file-transfer passthrough.")]),
        Line::from(vec![Span::styled("/canada [on|off|status]", Style::default().fg(theme.warning)), Span::raw("  Toggle the persistent Canadian maple-leaf transcript watermark.")]),
        Line::from(vec![Span::styled("/quit", Style::default().fg(theme.warning)), Span::raw("          Save pending records and exit.")]),
        Line::from(""),
        Line::from(vec![Span::styled("KEYS", Style::default().fg(theme.success).add_modifier(Modifier::BOLD))]),
        Line::from("↑/↓ select rows and load CMD entries into input · PgUp/PgDn page · Home/End select first/last row"),
        Line::from("Click a CMD row to load its command into input; drag to select text across any rows."),
        Line::from("Ctrl+C copies a selection; on a highlighted HUSR row it opens the Hermes thread."),
        Line::from("Each transcript row displays the full local date, time, seconds and UTC offset."),
        Line::from("F1 Bash workspace commands are captured from shell history; raw keystrokes and hidden password input are never logged."),
        Line::from("Tab completes shell commands and paths · four spaces in Notes mode"),
        Line::from(format!("F2–F10 switches high-contrast palettes (F2 = 1, F10 = 9) · active: {}", theme.name)),
        Line::from("Ctrl+W toggles transcript word wrap · Alt+←/→ horizontal pan · Ctrl+P/N history · Ctrl+S save / search SAVE AS · Ctrl+T mode"),
        Line::from("Ctrl+L / Esc clears the input line · F12 shows or hides the animated full-screen credits splash"),
        Line::from("Ctrl+C in a thread copies its full transcript; elsewhere it sends SIGINT or clears the draft."),
        Line::from(""),
        Line::from(Span::styled(
            "Transcript mode strips ANSI control sequences. Full-screen programs (vim, top, etc.) are not screen-passthrough applications.",
            Style::default().fg(theme.muted),
        )),
        Line::from(Span::styled("F1 or Esc closes help", Style::default().fg(theme.muted))),
    ]
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod conversation_thread_stream_tests {
    use crate::{
        conversations::collapse_live_streams,
        storage::{Entry, EntryKind},
    };

    fn event(kind: EntryKind, turn_id: Option<&str>, iteration: Option<u32>) -> Entry {
        let mut entry = Entry::with_kind(kind, "assistant text".to_string());
        entry.turn_id = turn_id.map(str::to_string);
        entry.iteration = iteration;
        entry
    }

    #[test]
    fn final_assistant_replaces_live_stream_when_iteration_is_omitted() {
        let prompt = event(EntryKind::HermesUser, Some("turn-1"), None);
        let assistant = event(EntryKind::HermesAssistant, Some("turn-1"), None);
        let delta = event(EntryKind::HermesStreamDelta, Some("turn-1"), Some(0));
        let entries = vec![(1, &prompt), (2, &delta), (3, &assistant)];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.text_by_first_index.is_empty());
        assert_eq!(collapsed.hidden_delta_indices, [2].into_iter().collect());
        assert!(collapsed.replaced_assistant_indices.is_empty());
    }

    #[test]
    fn final_assistant_replaces_live_stream_when_post_llm_event_omits_turn_id() {
        let prompt = event(EntryKind::HermesUser, Some("turn-1"), None);
        let delta = event(EntryKind::HermesStreamDelta, Some("turn-1"), Some(0));
        let assistant = event(EntryKind::HermesAssistant, None, None);
        let entries = vec![(1, &prompt), (2, &delta), (3, &assistant)];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.text_by_first_index.is_empty());
        assert_eq!(collapsed.hidden_delta_indices, [2].into_iter().collect());
        assert!(collapsed.replaced_assistant_indices.is_empty());
    }

    #[test]
    fn final_assistant_remains_visible_at_the_streamed_turn_boundary() {
        let first_prompt = event(EntryKind::HermesUser, Some("turn-1"), None);
        let delta = event(EntryKind::HermesStreamDelta, Some("turn-1"), Some(0));
        let first_assistant = event(EntryKind::HermesAssistant, Some("turn-1"), None);
        let second_prompt = event(EntryKind::HermesUser, Some("turn-2"), None);
        let second_assistant = event(EntryKind::HermesAssistant, Some("turn-2"), None);
        let entries = vec![
            (1, &first_prompt),
            (2, &delta),
            (3, &first_assistant),
            (4, &second_prompt),
            (5, &second_assistant),
        ];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.replaced_assistant_indices.is_empty());
        assert_eq!(collapsed.hidden_delta_indices, [2].into_iter().collect());
    }

    #[test]
    fn legacy_events_without_turn_ids_use_the_final_assistant_after_prompt_boundary() {
        let prompt = event(EntryKind::HermesUser, None, None);
        let delta = event(EntryKind::HermesStreamDelta, None, Some(0));
        let assistant = event(EntryKind::HermesAssistant, None, None);
        let entries = vec![(10, &prompt), (11, &delta), (12, &assistant)];

        let collapsed = collapse_live_streams(&entries);
        assert!(collapsed.text_by_first_index.is_empty());
        assert_eq!(collapsed.hidden_delta_indices, [11].into_iter().collect());
        assert!(collapsed.replaced_assistant_indices.is_empty());
    }

    #[test]
    fn assistant_entry_remains_visible_when_no_live_stream_was_captured() {
        let prompt = event(EntryKind::HermesUser, Some("turn-1"), None);
        let assistant = event(EntryKind::HermesAssistant, Some("turn-1"), None);
        let entries = vec![(1, &prompt), (2, &assistant)];

        assert!(collapse_live_streams(&entries).replaced_assistant_indices.is_empty());
    }
}
