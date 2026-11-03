use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, InputMode},
    storage::{Entry, EntryKind},
};

const BG: Color = Color::Rgb(12, 16, 24);
const PANEL: Color = Color::Rgb(18, 25, 36);
const BORDER: Color = Color::Rgb(53, 73, 94);
const MUTED: Color = Color::Rgb(124, 145, 166);
const TEXT: Color = Color::Rgb(220, 230, 240);
const CYAN: Color = Color::Rgb(85, 220, 220);
const GREEN: Color = Color::Rgb(135, 220, 160);
const YELLOW: Color = Color::Rgb(255, 210, 100);
const BLUE: Color = Color::Rgb(125, 175, 255);

pub fn draw(frame: &mut Frame, app: &mut App) {
    frame.render_widget(Block::default().style(Style::default().bg(BG)), frame.area());

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

    draw_banner(frame, chunks[0]);
    draw_status(frame, chunks[1], app);
    draw_feed(frame, chunks[2], app);
    draw_draft(frame, chunks[3], app);
    draw_input(frame, chunks[4], app);
    draw_hints(frame, chunks[5]);

    if app.show_help {
        draw_help(frame, frame.area());
    }
}

fn draw_banner(frame: &mut Frame, area: Rect) {
    let art = vec![
        Line::from(Span::styled(
            "  ____  _____ ____  _____  _____ _   _  ___ _____ _____ ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " / ___|| |_ _|  _ \\| ____|| ____| \\ | |/ _ \\_   _| ____|",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " \\___ \\  | | | |_) |  _|  |  _| |  \\| | | | || | |  _|",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  ___) | | | |  _ <| |___ | |___| |\\  | |_| || | | |___",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " |____/  |_| |_| \\_\\_____||_____|_| \\_|\\___/ |_| |_____|  // SHELL · NOTES · JOURNAL",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
    ];
    frame.render_widget(Paragraph::new(art).style(Style::default().bg(BG)), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let mode_color = match app.input_mode {
        InputMode::Terminal => GREEN,
        InputMode::Notes => YELLOW,
    };
    let mode = if app.follow_tail { "LIVE" } else { "SCROLLBACK" };
    let current = app.current_top().saturating_add(1);
    let privacy_hint = if app.secret_input { "  ·  HIDDEN INPUT" } else { "" };
    let status = Line::from(vec![
        Span::styled(" ● ", Style::default().fg(mode_color)),
        Span::styled(mode, Style::default().fg(mode_color).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "  {}{}  ·  {} entries  ·  {} B cached  ·  view @ {}  ·  {} hit(s)  │  ",
                app.input_mode.label(),
                privacy_hint,
                app.journal.entries.len(),
                app.journal.total_text_bytes,
                current,
                app.matches.len()
            ),
            Style::default().fg(MUTED),
        ),
        Span::styled(app.status.clone(), Style::default().fg(TEXT)),
    ]);
    frame.render_widget(Paragraph::new(status), area);
}

fn draw_feed(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = format!(
        " TRANSCRIPT  ·  {} RECORDS  ·  UNWRAPPED  ·  SEARCHABLE ",
        app.journal.entries.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            title,
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
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
        .map(|(index, entry)| entry_line(entry, index, app))
        .collect();

    let lines = if visible.is_empty() {
        vec![Line::from(vec![
            Span::styled("  No terminal output yet. ", Style::default().fg(MUTED)),
            Span::styled(
                "Run a Debian command below, or Ctrl+T to capture notes.",
                Style::default().fg(TEXT),
            ),
        ])]
    } else {
        visible
    };

    frame.render_widget(Paragraph::new(lines).scroll((0, app.feed_hscroll)), inner);
}

fn entry_line(entry: &Entry, index: usize, app: &App) -> Line<'static> {
    let timestamp = format_timestamp(&entry.timestamp);
    let number = format!("{:>6} ", index + 1);
    let active = app.is_active_match(index);
    let hit = app.is_match(index);
    let row_style = if active {
        Style::default().bg(Color::Rgb(42, 51, 62))
    } else {
        Style::default()
    };
    let (tag, tag_color) = match entry.kind {
        EntryKind::Note => ("NOTE", YELLOW),
        EntryKind::TerminalCommand => ("CMD ", YELLOW),
        EntryKind::TerminalOutput => ("OUT ", BLUE),
        EntryKind::System => ("SYS ", GREEN),
    };

    let mut spans = vec![
        Span::styled(number, Style::default().fg(MUTED).patch(row_style)),
        Span::styled(format!("{} ", timestamp), Style::default().fg(GREEN).patch(row_style)),
        Span::styled(format!("{tag} "), Style::default().fg(tag_color).add_modifier(Modifier::BOLD).patch(row_style)),
        Span::styled("│ ", Style::default().fg(BORDER).patch(row_style)),
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
                    Style::default().fg(TEXT).patch(row_style),
                ));
                spans.push(Span::styled(
                    entry.text[start..end].to_string(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(YELLOW)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    entry.text[end..].to_string(),
                    Style::default().fg(TEXT).patch(row_style),
                ));
            } else {
                spans.push(Span::styled(
                    entry.text.clone(),
                    Style::default().fg(TEXT).patch(row_style),
                ));
            }
        } else {
            spans.push(Span::styled(
                entry.text.clone(),
                Style::default().fg(TEXT).patch(row_style),
            ));
        }
    } else {
        spans.push(Span::styled(
            entry.text.clone(),
            Style::default().fg(TEXT).patch(row_style),
        ));
    }

    Line::from(spans)
}

fn format_timestamp(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| timestamp.chars().take(8).collect())
}

fn draw_draft(frame: &mut Frame, area: Rect, app: &App) {
    let prompt = app.input_mode.prompt();
    let preview = if app.input.is_empty() {
        Line::from(Span::styled(
            format!("  {prompt}waiting for input…"),
            Style::default().fg(MUTED),
        ))
    } else {
        Line::from(vec![
            Span::styled("  ", Style::default().fg(MUTED)),
            Span::styled(prompt, Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
            Span::styled(visible_input(app), Style::default().fg(TEXT)),
        ])
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" LIVE INPUT PREVIEW ", Style::default().fg(YELLOW)));
    frame.render_widget(
        Paragraph::new(preview)
            .scroll((0, app.input_hscroll as u16))
            .block(block),
        area,
    );
}

fn draw_input(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = if app.secret_input {
        " HIDDEN INPUT  ·  ENTER TO SEND WITHOUT LOGGING  ·  CTRL+C TO INTERRUPT "
    } else {
        match app.input_mode {
            InputMode::Terminal => " DEBIAN TERMINAL  ·  ENTER TO EXECUTE  ·  CTRL+T FOR NOTES ",
            InputMode::Notes => " NOTE CAPTURE  ·  ENTER TO SAVE  ·  CTRL+T FOR TERMINAL ",
        }
    };
    let mode_color = if app.secret_input {
        YELLOW
    } else if app.input_mode == InputMode::Terminal {
        CYAN
    } else {
        YELLOW
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(mode_color))
        .title(Span::styled(
            title,
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL));
    let inner = block.inner(area);
    let prompt = app.input_mode.prompt();
    let prompt_width = UnicodeWidthStr::width(prompt);
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
        Span::styled(visible_input(app), Style::default().fg(TEXT)),
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

fn draw_hints(frame: &mut Frame, area: Rect) {
    let hints = Line::from(vec![
        Span::styled(" Ctrl+T ", Style::default().fg(CYAN)),
        Span::styled("mode  ", Style::default().fg(MUTED)),
        Span::styled("↑/↓ ", Style::default().fg(CYAN)),
        Span::styled("scroll  ", Style::default().fg(MUTED)),
        Span::styled("wheel ", Style::default().fg(CYAN)),
        Span::styled("mouse  ", Style::default().fg(MUTED)),
        Span::styled("Tab ", Style::default().fg(CYAN)),
        Span::styled("complete  ", Style::default().fg(MUTED)),
        Span::styled("Ctrl+P/N ", Style::default().fg(CYAN)),
        Span::styled("history  ", Style::default().fg(MUTED)),
        Span::styled("/search ", Style::default().fg(CYAN)),
        Span::styled("journal  ", Style::default().fg(MUTED)),
        Span::styled("F1 ", Style::default().fg(CYAN)),
        Span::styled("help  ", Style::default().fg(MUTED)),
        Span::styled("Ctrl+Q ", Style::default().fg(CYAN)),
        Span::styled("quit", Style::default().fg(MUTED)),
    ]);
    frame.render_widget(Paragraph::new(hints), area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(82, 84, area);
    frame.render_widget(Clear, popup);
    let text = vec![
        Line::from(Span::styled(
            "STREAMNOTE · DEBIAN SHELL + NOTE JOURNAL",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("TERMINAL  ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::raw("Starts an interactive Debian shell in a PTY at startup. Type commands and press Enter."),
        ]),
        Line::from(vec![
            Span::styled("TRANSCRIPT", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::raw("  Every submitted command and captured stdout/stderr line is added to the JSONL journal."),
        ]),
        Line::from(vec![
            Span::styled("NOTES     ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::raw("Ctrl+T toggles Notes / Terminal. In terminal mode, /note text saves a note; !command runs a shell command from Notes mode."),
        ]),
        Line::from(vec![
            Span::styled("SEARCH    ", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::raw("/search words searches notes, commands, and terminal output across the full journal."),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled("LOCAL COMMANDS", Style::default().fg(YELLOW).add_modifier(Modifier::BOLD))]),
        Line::from(vec![Span::styled("/search WORDS", Style::default().fg(YELLOW)), Span::raw("  Search the entire journal and jump to the latest hit with context.")]),
        Line::from(vec![Span::styled("n / N", Style::default().fg(YELLOW)), Span::raw("          Next / previous search result when input is empty.")]),
        Line::from(vec![Span::styled("/mode [shell|notes]", Style::default().fg(YELLOW)), Span::raw("  Switch input mode; no argument toggles.")]),
        Line::from(vec![Span::styled("/note TEXT", Style::default().fg(YELLOW)), Span::raw("      Save a note without leaving terminal mode; /note alone switches to Notes mode.")]),
        Line::from(vec![Span::styled("/save", Style::default().fg(YELLOW)), Span::raw("          Retry pending writes and sync the journal.")]),
        Line::from(vec![Span::styled("/export [FILE]", Style::default().fg(YELLOW)), Span::raw(" Export every journal record as readable text.")]),
        Line::from(vec![Span::styled("/stats · /path", Style::default().fg(YELLOW)), Span::raw("  Show journal statistics or path.")]),
        Line::from(vec![Span::styled("/quit", Style::default().fg(YELLOW)), Span::raw("          Save pending records and exit.")]),
        Line::from(""),
        Line::from(vec![Span::styled("KEYS", Style::default().fg(GREEN).add_modifier(Modifier::BOLD))]),
        Line::from("↑/↓ scroll · PgUp/PgDn page · Home oldest · End live tail · mouse wheel scroll"),
        Line::from("Tab completes shell commands and paths · four spaces in Notes mode"),
        Line::from("Alt+←/→ horizontal pan · Ctrl+P/N command history · Ctrl+S save · Ctrl+T mode"),
        Line::from("Ctrl+C sends SIGINT to the shell · Ctrl+D sends EOF · Ctrl+Q quits StreamNote"),
        Line::from(""),
        Line::from(Span::styled(
            "Transcript mode strips ANSI control sequences. Full-screen programs (vim, top, etc.) are not screen-passthrough applications.",
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled("F1 or Esc closes help", Style::default().fg(MUTED))),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CYAN))
        .title(Span::styled(
            " HELP ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(PANEL));
    frame.render_widget(
        Paragraph::new(text)
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
