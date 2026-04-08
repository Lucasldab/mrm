use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::App;
use crate::types::Status;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.size();

    // Split: main list | right sidebar (keybinds)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(26)])
        .split(area);

    // Split left: search bar (if active) | list | status bar
    let search_height = if app.search_active || !app.search_query.is_empty() { 3 } else { 0 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(search_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(cols[0]);

    // Search bar
    if search_height > 0 {
        let search_text = format!(" / {}", app.search_query);
        let search = Paragraph::new(search_text)
            .block(Block::default().borders(Borders::ALL).title(" Search "))
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(search, rows[0]);
    }

    // Manhwa list
    draw_list(f, app, rows[1]);

    // Bottom status bar
    draw_statusbar(f, app, rows[2]);

    // Right sidebar: keybinds
    draw_keybinds(f, cols[1]);
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible_manhwa();
    let is_empty = visible.is_empty();

    let items: Vec<ListItem> = if is_empty {
        vec![ListItem::new(Line::from(Span::styled(
            "  No manhwa yet. Add one with 'a'.",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        visible
            .iter()
            .map(|m| {
                let status_display = m.status_display();

                // Color-code by status
                let status_color = match &m.status {
                    Status::LookedInto => Color::Gray,
                    Status::Reading    => Color::Green,
                    Status::UpToDate   => Color::Cyan,
                    Status::Paused     => Color::Yellow,
                    Status::Completed  => Color::Blue,
                    Status::Dropped    => Color::DarkGray,
                };

                // Show unread badge in orange if there are unread chapters
                let unread_span = if m.unread > 0 {
                    Span::styled(
                        format!(" [{}] ", m.unread),
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw("  ")
                };

                let source_span = Span::styled(
                    format!("[{}] ", m.source),
                    Style::default().fg(Color::DarkGray),
                );

                let title_span = Span::styled(
                    m.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                );

                let status_span = Span::styled(
                    format!("  {}", status_display),
                    Style::default().fg(status_color),
                );

                ListItem::new(Line::from(vec![
                    unread_span,
                    source_span,
                    title_span,
                    status_span,
                ]))
            })
            .collect()
    };

    let title = if app.search_query.is_empty() {
        format!(" mrm — {} manhwa ", visible.len())
    } else {
        format!(" mrm — {} results ", visible.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::White)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !is_empty {
        state.select(Some(app.library_sel));
    }

    f.render_stateful_widget(list, area, &mut state);
}

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let msg = app
        .status_msg
        .as_deref()
        .unwrap_or("j/k move  Enter open  / search  q quit");

    let bar = Paragraph::new(msg).style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::White),
    );
    f.render_widget(bar, area);
}

fn draw_keybinds(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(" Navigation", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![
            Span::styled(" j/↓  ", Style::default().fg(Color::Yellow)),
            Span::raw("move down"),
        ]),
        Line::from(vec![
            Span::styled(" k/↑  ", Style::default().fg(Color::Yellow)),
            Span::raw("move up"),
        ]),
        Line::from(vec![
            Span::styled(" g    ", Style::default().fg(Color::Yellow)),
            Span::raw("top"),
        ]),
        Line::from(vec![
            Span::styled(" G    ", Style::default().fg(Color::Yellow)),
            Span::raw("bottom"),
        ]),
        Line::from(vec![
            Span::styled(" Enter", Style::default().fg(Color::Yellow)),
            Span::raw("open"),
        ]),
        Line::from(""),
        Line::from(Span::styled(" Library", Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(vec![
            Span::styled(" /    ", Style::default().fg(Color::Yellow)),
            Span::raw("search"),
        ]),
        Line::from(vec![
            Span::styled(" a    ", Style::default().fg(Color::Yellow)),
            Span::raw("add manhwa"),
        ]),
        Line::from(vec![
            Span::styled(" q    ", Style::default().fg(Color::Yellow)),
            Span::raw("quit"),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Keys ");

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}
