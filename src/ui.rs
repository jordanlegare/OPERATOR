use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{App, InputMode},
    storage::{entry_transcript_text, format_timestamp, Entry, EntryKind},
    theme::{Theme, THEMES},
};


pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = THEMES.get(app.theme_index.saturating_sub(1)).copied().unwrap_or(THEMES[0]);
    frame.render_widget(Block::default().style(Style::default().bg(theme.bg)), frame.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_banner(frame, chunks[0], theme);
    draw_status(frame, chunks[1], app, theme);
    draw_feed(frame, chunks[2], app, theme);
    draw_draft(frame, chunks[3], app, theme);
    draw_input(frame, chunks[4], app, theme);
    draw_hints(frame, chunks[5], theme);

    if app.show_help {
        draw_help(frame, frame.area(), theme);
    }
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
        " TRANSCRIPT  ·  {} RECORDS  ·  UNWRAPPED  ·  SEARCHABLE ",
        app.journal.entries.len()
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
        .take(inner.height as usize)
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

    frame.render_widget(Paragraph::new(lines).scroll((0, app.feed_hscroll)), inner);
}

fn entry_line(entry: &Entry, index: usize, app: &App, theme: Theme) -> Line<'static> {
    let timestamp = format_timestamp(&entry.timestamp);
    let number = format!("{:>6} ", index + 1);
    let row_text = entry_transcript_text(entry, index);
    let selected_columns =
        app.selection_columns_for_row(index, UnicodeWidthStr::width(row_text.as_str()));
    let active = app.is_active_match(index);
    let hit = app.is_match(index);
    let row_style = if active {
        Style::default().bg(theme.selected_row)
    } else {
        Style::default()
    };
    let (tag, tag_color) = match entry.kind {
        EntryKind::Note => ("NOTE", theme.accent),
        EntryKind::TerminalCommand => ("CMD ", theme.warning),
        EntryKind::TerminalOutput => ("OUT ", theme.info),
        EntryKind::System => ("SYS ", theme.success),
    };

    let mut spans = vec![
        Span::styled(number, Style::default().fg(theme.muted).patch(row_style)),
        Span::styled(format!("{} ", timestamp), Style::default().fg(theme.success).patch(row_style)),
        Span::styled(format!("{tag} "), Style::default().fg(tag_color).add_modifier(Modifier::BOLD).patch(row_style)),
        Span::styled("│ ", Style::default().fg(theme.border).patch(row_style)),
    ];

    if let Some(query) = app.search_query.as_deref().filter(|_| hit) {
        let needle = query.to_ascii_lowercase();
        let haystack = entry.text.to_ascii_lowercase();
        if let Some(start) = haystack.find(needle.as_str()) {
            let end = start + query.len();
            if start <= entry.text.len()
                && end <= entry.text.len()
                && entry.text.is_char_boundary(start)
                && entry.text.is_char_boundary(end)
            {
                spans.push(Span::styled(
                    entry.text[..start].to_string(),
                    Style::default().fg(theme.text).patch(row_style),
                ));
                spans.push(Span::styled(
                    entry.text[start..end].to_string(),
                    Style::default()
                        .fg(theme.search_fg)
                        .bg(theme.search_bg)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    entry.text[end..].to_string(),
                    Style::default().fg(theme.text).patch(row_style),
                ));
            } else {
                spans.push(Span::styled(
                    entry.text.clone(),
                    Style::default().fg(theme.text).patch(row_style),
                ));
            }
        } else {
            spans.push(Span::styled(
                entry.text.clone(),
                Style::default().fg(theme.text).patch(row_style),
            ));
        }
    } else {
        spans.push(Span::styled(
            entry.text.clone(),
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

fn draw_draft(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    let prompt = app.input_prompt().to_string();
    let preview = if app.input.is_empty() {
        Line::from(Span::styled(
            format!("  {prompt}waiting for input…"),
            Style::default().fg(theme.muted),
        ))
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.muted)),
            Span::styled(prompt, Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
            Span::styled(visible_input(app), Style::default().fg(theme.text)),
        ])
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme.border))
        .title(Span::styled(" LIVE INPUT PREVIEW ", Style::default().fg(theme.warning)));
    frame.render_widget(
        Paragraph::new(preview)
            .scroll((0, app.input_hscroll as u16))
            .block(block),
        area,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    let title = if app.secret_input {
        " HIDDEN INPUT  ·  ENTER TO SEND WITHOUT LOGGING  ·  CTRL+C TO INTERRUPT "
    } else {
        match app.input_mode {
            InputMode::Terminal => " DEBIAN TERMINAL  ·  ENTER TO EXECUTE  ·  CTRL+T FOR NOTES ",
            InputMode::Notes => " NOTE CAPTURE  ·  ENTER TO SAVE  ·  CTRL+T FOR TERMINAL ",
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
    if !app.show_help && cursor_x < inner.x.saturating_add(inner.width) {
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

fn draw_hints(frame: &mut Frame, area: Rect, theme: Theme) {
    let hints = Line::from(vec![
        Span::styled(" Ctrl+T ", Style::default().fg(theme.primary)),
        Span::styled("mode  ", Style::default().fg(theme.muted)),
        Span::styled(" F2–F10 ", Style::default().fg(theme.primary)),
        Span::styled("theme  ", Style::default().fg(theme.muted)),
        Span::styled("↑/↓ ", Style::default().fg(theme.primary)),
        Span::styled("scroll  ", Style::default().fg(theme.muted)),
        Span::styled("wheel ", Style::default().fg(theme.primary)),
        Span::styled("mouse  ", Style::default().fg(theme.muted)),
        Span::styled("Tab ", Style::default().fg(theme.primary)),
        Span::styled("complete  ", Style::default().fg(theme.muted)),
        Span::styled("Ctrl+P/N ", Style::default().fg(theme.primary)),
        Span::styled("history  ", Style::default().fg(theme.muted)),
        Span::styled("/search ", Style::default().fg(theme.primary)),
        Span::styled("journal  ", Style::default().fg(theme.muted)),
        Span::styled("F1 ", Style::default().fg(theme.primary)),
        Span::styled("help  ", Style::default().fg(theme.muted)),
        Span::styled("F12 ", Style::default().fg(theme.primary)),
        Span::styled("key test  ", Style::default().fg(theme.muted)),
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
        Line::from(vec![Span::styled("n / N", Style::default().fg(theme.warning)), Span::raw("          Next / previous search result when input is empty.")]),
        Line::from(vec![Span::styled("/mode [shell|notes]", Style::default().fg(theme.warning)), Span::raw("  Switch input mode; no argument toggles.")]),
        Line::from(vec![Span::styled("/note TEXT", Style::default().fg(theme.warning)), Span::raw("      Save a note without leaving terminal mode; /note alone switches to Notes mode.")]),
        Line::from(vec![Span::styled("/save", Style::default().fg(theme.warning)), Span::raw("          Retry pending writes and sync the journal.")]),
        Line::from(vec![Span::styled("/export [FILE]", Style::default().fg(theme.warning)), Span::raw(" Export every journal record as readable text.")]),
        Line::from(vec![Span::styled("/stats · /path", Style::default().fg(theme.warning)), Span::raw("  Show journal statistics or path.")]),
        Line::from(vec![Span::styled("/quit", Style::default().fg(theme.warning)), Span::raw("          Save pending records and exit.")]),
        Line::from(""),
        Line::from(vec![Span::styled("KEYS", Style::default().fg(theme.success).add_modifier(Modifier::BOLD))]),
        Line::from("↑/↓ scroll · PgUp/PgDn page · Home oldest · End live tail · mouse wheel scroll"),
        Line::from("Click a transcript row to select it and load its text into the input; drag to select text across rows."),
        Line::from("Ctrl+C copies a transcript selection through OSC 52; without a selection it keeps its normal behavior."),
        Line::from("Each transcript row displays the full local date, time, seconds and UTC offset."),
        Line::from("Tab completes shell commands and paths · four spaces in Notes mode"),
        Line::from(format!("F2–F10 switches high-contrast palettes (F2 = 1, F10 = 9) · active: {}", theme.name)),
        Line::from("Alt+←/→ horizontal pan · Ctrl+P/N command history · Ctrl+S save · Ctrl+T mode"),
        Line::from("F12 then press a key to display its Crossterm key code and modifiers for diagnostics"),
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
