use ratatui::{
    buffer::CellDiffOption,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{App, ConversationView, InputMode},
    conversations::ConversationThread,
    storage::{entry_display_text, entry_transcript_text, format_timestamp, Entry, EntryKind},
    theme::{Theme, THEMES},
};


pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = THEMES.get(app.theme_index.saturating_sub(1)).copied().unwrap_or(THEMES[0]);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), frame.area());

    let mut constraints = Vec::with_capacity(5);
    if app.show_banner {
        constraints.push(Constraint::Length(5));
    }
    constraints.extend([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ]);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    let offset = if app.show_banner { 1 } else { 0 };
    if app.show_banner {
        draw_banner(frame, chunks[0], theme);
    }
    draw_status(frame, chunks[offset], app, theme);
    draw_feed(frame, chunks[offset + 1], app, theme);
    draw_input(frame, chunks[offset + 2], app, theme);
    draw_hints(frame, chunks[offset + 3], app, theme);

    if let Some(index) = app.message_view_index {
        if let Some(entry) = app.journal.entries.get(index) {
            draw_hermes_message(frame, entry, index, app.message_view_scroll, theme);
        }
    }
    if let Some(view) = app.conversation_view.clone() {
        match view {
            ConversationView::List => draw_conversation_list(frame, app, theme),
            ConversationView::Thread { id } => draw_conversation_thread(frame, app, &id, theme),
        }
    }
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
        _ => " HERMES MESSAGE ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
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
        .border_style(Style::default().fg(theme.primary))
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
    let completed_iterations = entries.iter()
        .filter_map(|(_, entry)| {
            (entry.kind == EntryKind::HermesAssistant)
                .then(|| entry.turn_id.clone().map(|turn| (turn, entry.iteration)))
                .flatten()
        })
        .collect::<std::collections::HashSet<_>>();

    let mut lines = Vec::new();
    let mut previous_turn: Option<String> = None;
    for (index, entry) in entries {
        if matches!(entry.kind, EntryKind::HermesStreamStart | EntryKind::HermesStreamEnd) {
            continue;
        }
        if entry.kind == EntryKind::HermesStreamDelta
            && entry.turn_id.as_ref().is_some_and(|turn| {
                completed_iterations.contains(&(turn.clone(), entry.iteration))
            })
        {
            continue;
        }
        if entry.turn_id.is_some() && previous_turn != entry.turn_id {
            if !lines.is_empty() {
                lines.push(Line::from(Span::styled("  ─────────────────────────────────────────────────────────────", Style::default().fg(theme.border))));
            }
            previous_turn = entry.turn_id.clone();
        }

        let (label, color) = match entry.kind {
            EntryKind::HermesUser => ("USER", theme.info),
            EntryKind::HermesAssistant => ("ASSISTANT", theme.success),
            EntryKind::HermesToolCall => ("TOOL CALL", theme.warning),
            EntryKind::HermesToolResult => ("TOOL RESULT", theme.accent),
            EntryKind::HermesStreamDelta => ("ASSISTANT · LIVE STREAM", theme.primary),
            _ => ("HERMES", theme.muted),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label}  "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{}  ·  row {}", format_timestamp(&entry.timestamp), index + 1), Style::default().fg(theme.muted)),
        ]));
        for body_line in entry.text.lines() {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(body_line.to_string(), Style::default().fg(theme.text)),
            ]));
        }
        if entry.text.is_empty() {
            lines.push(Line::from("  "));
        }
        lines.push(Line::from(""));
    }

    let state = if streaming { "STREAMING" } else { "CAPTURED" };
    let id_label = thread.id.strip_prefix("session:").unwrap_or(&thread.id);
    let short_id = id_label.chars().take(12).collect::<String>();
    let popup = centered_rect(94, 90, frame.area());
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            format!(" HERMES THREAD {short_id} · {turn_count} TURN(S) · {tool_call_count} TOOL(S) · {state} "),
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    let inner = block.inner(popup);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    app.conversation_content_height = lines.len();
    app.conversation_viewport_height = sections[0].height.max(1) as usize;
    let max_scroll = lines.len().saturating_sub(app.conversation_viewport_height);
    let scroll = if app.conversation_follow_tail {
        max_scroll
    } else {
        app.conversation_scroll.min(max_scroll)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(" ↑/↓ scroll · PgUp/PgDn page · Home/End · Ctrl+C copy thread · Esc close ")
            .style(Style::default().fg(theme.muted).bg(theme.panel)),
        sections[1],
    );
}

fn draw_banner(frame: &mut Frame, area: Rect, theme: Theme) {
    let art = vec![
        Line::from(Span::styled(
            "  ____  _____ ____  _____  _____ _   _  ___ _____ _____ ",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " / ___|| |_ _|  _ \\| ____|| ____| \\ | |/ _ \\_   _| ____|",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " \\___ \\  | | | |_) |  _|  |  _| |  \\| | | | || | |  _|",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  ___) | | | |  _ <| |___ | |___| |\\  | |_| || | | |___",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " |____/  |_| |_| \\_\\_____||_____|_| \\_|\\___/ |_| |_____|  // SHELL · NOTES · JOURNAL",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
    ];
    frame.render_widget(Paragraph::new(art).style(Style::default().bg(theme.bg)), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    let mode_color = match app.input_mode {
        InputMode::Terminal => theme.success,
        InputMode::Notes => theme.warning,
    };
    let mode = if app.follow_tail { "LIVE" } else { "SCROLLBACK" };
    let current = app.current_top().saturating_add(1);
    let privacy_hint = if app.secret_input { "  ·  HIDDEN INPUT" } else { "" };
    let status = Line::from(vec![
        Span::styled(" ● ", Style::default().fg(mode_color)),
        Span::styled(mode, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "  {}{}  ·  theme {}/9: {}  ·  {} entries  ·  {} B cached  ·  view @ {}  ·  {} hit(s)  │  ",
                app.input_mode.label(),
                privacy_hint,
                app.theme_index,
                theme.name,
                app.journal.entries.len(),
                app.journal.total_text_bytes,
                current,
                app.matches.len()
            ),
            Style::default().fg(theme.muted),
        ),
        Span::styled(app.status.clone(), Style::default().fg(theme.text)),
    ]);
    frame.render_widget(Paragraph::new(status), area);
}

fn draw_feed(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    let title = format!(
        " TRANSCRIPT  ·  {} RECORDS  ·  {}  ·  SEARCHABLE ",
        app.journal.entries.len(),
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
    let visible: Vec<Line> = app
        .journal
        .entries
        .iter()
        .enumerate()
        .skip(top)
        .map(|(index, entry)| entry_line(entry, index, app, theme))
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
        let visual_rows: usize = app.journal.entries.iter().skip(top).map(|entry| {
            let width = UnicodeWidthStr::width(entry_transcript_text(entry, 0).as_str());
            width.max(1).div_ceil(inner.width.max(1) as usize)
        }).sum();
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
    };

    let mut spans = vec![
        Span::styled(number, Style::default().fg(theme.muted).patch(row_style)),
        Span::styled(format!("{} ", timestamp), Style::default().fg(theme.success).patch(row_style)),
        Span::styled(format!("{tag} "), Style::default().fg(tag_color).add_modifier(Modifier::BOLD).patch(row_style)),
        Span::styled("│ ", Style::default().fg(theme.border).patch(row_style)),
    ];

    if let Some(query) = app.search_query.as_deref().filter(|_| hit) {
        let needle = query.to_ascii_lowercase();
        let haystack = display_text.to_ascii_lowercase();
        if let Some(start) = haystack.find(needle.as_str()) {
            let end = start + query.len();
            if start <= display_text.len()
                && end <= display_text.len()
                && display_text.is_char_boundary(start)
                && display_text.is_char_boundary(end)
            {
                spans.push(Span::styled(
                    display_text[..start].to_string(),
                    Style::default().fg(theme.text).patch(row_style),
                ));
                spans.push(Span::styled(
                    display_text[start..end].to_string(),
                    Style::default()
                        .patch(row_style)
                        .fg(theme.search_fg)
                        .bg(theme.search_bg)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    display_text[end..].to_string(),
                    Style::default().fg(theme.text).patch(row_style),
                ));
            } else {
                spans.push(Span::styled(
                    display_text.clone(),
                    Style::default().fg(theme.text).patch(row_style),
                ));
            }
        } else {
            spans.push(Span::styled(
                display_text.clone(),
                Style::default().fg(theme.text).patch(row_style),
            ));
        }
    } else {
        spans.push(Span::styled(
            display_text.clone(),
            Style::default().fg(theme.text).patch(row_style),
        ));
    }

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
    let title = if app.secret_input {
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
        Span::styled("drag ", Style::default().fg(theme.primary)),
        Span::styled("select  ", Style::default().fg(theme.muted)),
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
    let text = vec![
        Line::from(Span::styled(
            "STREAMNOTE · DEBIAN SHELL + NOTE JOURNAL",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("TERMINAL  ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Starts an interactive Debian shell in a PTY at startup. Type commands and press Enter."),
        ]),
        Line::from(vec![
            Span::styled("TRANSCRIPT", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("  Every submitted command and captured stdout/stderr line is added to the JSONL journal."),
        ]),
        Line::from(vec![
            Span::styled("NOTES     ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("Ctrl+T toggles Notes / Terminal. In terminal mode, /note text saves a note; !command runs a shell command from Notes mode."),
        ]),
        Line::from(vec![
            Span::styled("SEARCH    ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
            Span::raw("/search words searches notes, commands, and terminal output across the full journal."),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("LOCAL COMMANDS", Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))]),
        Line::from(vec![Span::styled("/search WORDS", Style::default().fg(theme.warning)), Span::raw("  Search the entire journal and jump to the latest hit with context.")]),
        Line::from(vec![Span::styled("/view", Style::default().fg(theme.warning)), Span::raw("           Open the full Markdown body of a selected Hermes event.")]),
        Line::from(vec![Span::styled("/threads", Style::default().fg(theme.warning)), Span::raw("        Browse Hermes conversations; Enter expands a complete live thread.")]),
        Line::from(vec![Span::styled("n / N", Style::default().fg(theme.warning)), Span::raw("          Next / previous search result when input is empty.")]),
        Line::from(vec![Span::styled("/mode [shell|notes]", Style::default().fg(theme.warning)), Span::raw("  Switch input mode; no argument toggles.")]),
        Line::from(vec![Span::styled("/note TEXT", Style::default().fg(theme.warning)), Span::raw("      Save a note without leaving terminal mode; /note alone switches to Notes mode.")]),
        Line::from(vec![Span::styled("/save", Style::default().fg(theme.warning)), Span::raw("          Retry pending writes and sync the journal.")]),
        Line::from(vec![Span::styled("/export [FILE]", Style::default().fg(theme.warning)), Span::raw(" Export every journal record as readable text.")]),
        Line::from(vec![Span::styled("/stats · /path", Style::default().fg(theme.warning)), Span::raw("  Show journal statistics or path.")]),
        Line::from(vec![Span::styled("/scp on|off|status", Style::default().fg(theme.warning)), Span::raw("  Toggle persistent SSH SCP/SFTP file-transfer passthrough.")]),
        Line::from(vec![Span::styled("/canada [on|off|status]", Style::default().fg(theme.warning)), Span::raw("  Toggle the persistent Canadian maple-leaf transcript watermark.")]),
        Line::from(vec![Span::styled("/quit", Style::default().fg(theme.warning)), Span::raw("          Save pending records and exit.")]),
        Line::from(""),
        Line::from(vec![Span::styled("KEYS", Style::default().fg(theme.success).add_modifier(Modifier::BOLD))]),
        Line::from("↑/↓ select rows and load CMD entries into input · PgUp/PgDn page · Home/End select first/last row"),
        Line::from("Click a CMD row to load its command into input; drag to select text across any rows."),
        Line::from("Ctrl+C copies a transcript selection through OSC 52; without a selection it keeps its normal behavior."),
        Line::from("Each transcript row displays the full local date, time, seconds and UTC offset."),
        Line::from("Tab completes shell commands and paths · four spaces in Notes mode"),
        Line::from(format!("F2–F10 switches high-contrast palettes (F2 = 1, F10 = 9) · active: {}", theme.name)),
        Line::from("Ctrl+W toggles transcript word wrap · Alt+←/→ horizontal pan · Ctrl+P/N history · Ctrl+S save · Ctrl+T mode"),
        Line::from("Ctrl+L / Esc clears the input line · F12 shows or hides the ASCII-art banner"),
        Line::from("Ctrl+C sends SIGINT to the shell · Ctrl+D sends EOF · Ctrl+Q quits StreamNote"),
        Line::from(""),
        Line::from(Span::styled(
            "Transcript mode strips ANSI control sequences. Full-screen programs (vim, top, etc.) are not screen-passthrough applications.",
            Style::default().fg(theme.muted),
        )),
        Line::from(Span::styled("F1 or Esc closes help", Style::default().fg(theme.muted))),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(Span::styled(
            " HELP ",
            Style::default().fg(theme.primary).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.panel));
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.text).bg(theme.panel))
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
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
