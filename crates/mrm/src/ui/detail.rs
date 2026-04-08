use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::App;
use crate::types::{Chapter, Status};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.size();

    let manhwa = match &app.current_manhwa {
        Some(m) => m,
        None    => return,
    };

    // Layout: header | chapter list | statusbar
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),   // header (title + meta)
            Constraint::Min(0),      // chapter list
            Constraint::Length(1),   // statusbar
        ])
        .split(area);

    draw_header(f, app, rows[0]);
    draw_chapters(f, app, rows[1]);
    draw_statusbar(f, app, rows[2]);
}

// ---------------------------------------------------------------------------
// Header — title, source, status, chapter count
// ---------------------------------------------------------------------------

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let manhwa = app.current_manhwa.as_ref().unwrap();

    let status_color = match &manhwa.status {
        Status::LookedInto => Color::Gray,
        Status::Reading    => Color::Green,
        Status::UpToDate   => Color::Cyan,
        Status::Paused     => Color::Yellow,
        Status::Completed  => Color::Blue,
        Status::Dropped    => Color::DarkGray,
    };

    let read_count = app.chapter_list.iter().filter(|c| c.completed).count();
    let total      = app.chapter_list.len();

    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", manhwa.title),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::White),
        )),
        Line::from(vec![
            Span::styled("  Source:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(&manhwa.source),
            Span::styled("  |  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Pub: ", Style::default().fg(Color::DarkGray)),
            Span::raw(&manhwa.pub_status),
        ]),
        Line::from(vec![
            Span::styled("  Status:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(manhwa.status_display(), Style::default().fg(status_color)),
            if manhwa.status_override {
                Span::styled("  (manual)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            Span::styled("  Progress:", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("  {}/{} chapters read", read_count, total)),
            if manhwa.unread > 0 {
                Span::styled(
                    format!("  ({} unread)", manhwa.unread),
                    Style::default().fg(Color::Red),
                )
            } else {
                Span::raw("")
            },
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", manhwa.title))
        .border_style(Style::default().fg(Color::White));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Chapter list
// ---------------------------------------------------------------------------

fn draw_chapters(f: &mut Frame, app: &App, area: Rect) {
    let chapters = &app.chapter_list;

    let items: Vec<ListItem> = chapters
        .iter()
        .map(|ch| {
            let icon = ch.status_icon();
            let icon_style = if ch.completed {
                Style::default().fg(Color::Green)
            } else if ch.scroll_pct > 0.0 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let title = ch.display_title();

            let date_span = match &ch.released_at {
                Some(d) => Span::styled(
                    format!("  {}", &d[..10.min(d.len())]),
                    Style::default().fg(Color::DarkGray),
                ),
                None => Span::raw(""),
            };

            let progress_span = if ch.scroll_pct > 0.0 && !ch.completed {
                Span::styled(
                    format!("  {:.0}%", ch.scroll_pct * 100.0),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::raw("")
            };

            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", icon), icon_style),
                Span::raw(title),
                date_span,
                progress_span,
            ]))
        })
        .collect();

    let title = format!(
        " Chapters ({}) ",
        chapters.len()
    );

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
    if !chapters.is_empty() {
        state.select(Some(app.chapter_sel));
    }

    f.render_stateful_widget(list, area, &mut state);
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let override_hint = if app.current_manhwa.as_ref().map(|m| m.status_override).unwrap_or(false) {
        "  c clear override"
    } else {
        ""
    };
    let default_hint = format!("j/k move  Enter read  s status  u unread{override_hint}  Esc back");
    let msg = app.status_msg.as_deref().unwrap_or(default_hint.as_str());

    let bar = Paragraph::new(msg.to_owned()).style(
        Style::default().fg(Color::Black).bg(Color::White),
    );
    f.render_widget(bar, area);
}
