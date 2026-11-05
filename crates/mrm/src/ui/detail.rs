use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::app::App;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let manhwa = match &app.current_manhwa {
        Some(m) => m,
        None    => return,
    };

    let has_desc = manhwa.description.as_deref().map(|s| !s.is_empty()).unwrap_or(false);

    // Reserve a description panel only when the source provided one. Some
    // series simply have no synopsis and we hide the block entirely so the
    // chapter list keeps the full screen.
    let desc_height: u16 = if has_desc { 7 } else { 0 };
    let failing = manhwa.failing_since.is_some();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if failing { 7 } else { 6 }),
            Constraint::Length(desc_height),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, app, rows[0]);
    if desc_height > 0 {
        draw_description(f, app, rows[1]);
    }
    draw_chapters(f, app, rows[2]);
    draw_statusbar(f, app, rows[3]);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let manhwa = app.current_manhwa.as_ref().unwrap();
    let theme = &app.theme;

    let status_color = theme.status_color(&manhwa.status);

    let read_count = app.chapter_list.iter().filter(|c| c.completed).count();
    let total      = app.chapter_list.len();

    let mut lines = vec![
        Line::from(Span::styled(
            format!(" {}", manhwa.title),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(theme.text_bold()),
        )),
        Line::from(vec![
            Span::styled("  Source:  ", Style::default().fg(theme.text_secondary())),
            Span::raw(&manhwa.source),
            Span::styled("  |  ", Style::default().fg(theme.text_secondary())),
            Span::styled("Pub: ", Style::default().fg(theme.text_secondary())),
            Span::raw(&manhwa.pub_status),
        ]),
        Line::from(vec![
            Span::styled("  Status:  ", Style::default().fg(theme.text_secondary())),
            Span::styled(manhwa.status_display(), Style::default().fg(status_color)),
            if manhwa.status_override {
                Span::styled("  (manual)", Style::default().fg(theme.text_secondary()))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            Span::styled("  Progress:", Style::default().fg(theme.text_secondary())),
            Span::raw(format!("  {}/{} chapters read", read_count, total)),
            if manhwa.unread > 0 {
                Span::styled(
                    format!("  ({} unread)", manhwa.unread),
                    Style::default().fg(theme.unread_badge()),
                )
            } else {
                Span::raw("")
            },
        ]),
    ];
    if let Some(health) = health_line(manhwa.failing_since.as_deref(), manhwa.last_error.as_deref()) {
        lines.push(Line::from(Span::styled(format!("  {health}"), Style::default().fg(theme.error()))));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", manhwa.title))
        .border_style(Style::default().fg(theme.border()));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

/// "⚠ failing since <date>: <error>" while polls of the series fail.
/// `failing_since` is Postgres timestamptz text; its first 10 bytes are the
/// date (`get` keeps an unexpected value from panicking the draw).
pub(crate) fn health_line(failing_since: Option<&str>, last_error: Option<&str>) -> Option<String> {
    let since = failing_since?;
    let date = since.get(..10).unwrap_or(since);
    let err = last_error.unwrap_or("unknown error");
    Some(format!("⚠ failing since {date}: {err}"))
}

fn draw_description(f: &mut Frame, app: &App, area: Rect) {
    let manhwa = app.current_manhwa.as_ref().unwrap();
    let theme = &app.theme;
    let text = manhwa.description.as_deref().unwrap_or("");

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Description ")
        .border_style(Style::default().fg(theme.border()));

    let para = Paragraph::new(text)
        .block(block)
        .style(Style::default().fg(theme.text()))
        .wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

fn draw_chapters(f: &mut Frame, app: &App, area: Rect) {
    let chapters = &app.chapter_list;
    let theme = &app.theme;

    let items: Vec<ListItem> = chapters
        .iter()
        .map(|ch| {
            let icon = ch.status_icon();
            let icon_style = if ch.completed {
                Style::default().fg(theme.success())
            } else if ch.scroll_pct > 0.0 {
                Style::default().fg(theme.warning())
            } else {
                Style::default().fg(theme.text_secondary())
            };

            let title = ch.display_title();

            let date_span = match &ch.released_at {
                Some(d) => Span::styled(
                    format!("  {}", d.get(..10).unwrap_or(d)),
                    Style::default().fg(theme.text_secondary()),
                ),
                None => Span::raw(""),
            };

            let progress_span = if ch.scroll_pct > 0.0 && !ch.completed {
                Span::styled(
                    format!("  {:.0}%", ch.scroll_pct * 100.0),
                    Style::default().fg(theme.warning()),
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

    let title = format!(" Chapters ({}) ", chapters.len());

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(theme.border())),
        )
        .highlight_style(
            Style::default()
                .bg(theme.highlight_bg())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !chapters.is_empty() {
        state.select(Some(app.chapter_sel));
    }

    f.render_stateful_widget(list, area, &mut state);
}

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let keys = &app.keys;
    let override_hint = if app.current_manhwa.as_ref().map(|m| m.status_override).unwrap_or(false) {
        format!("  {} clear override", keys.clear_override)
    } else {
        String::new()
    };
    let default_hint = format!(
        "{}/{} move  {} read  {} status  {} unread  {} mark read  R refresh{}  Esc back",
        keys.down, keys.up, keys.open, keys.set_status, keys.mark_unread, keys.mark_read, override_hint
    );
    let refreshing = app.refreshing.is_some()
        && app.refreshing == app.current_manhwa.as_ref().map(|m| m.id);
    let msg = match app.status_msg.as_deref() {
        Some(m) => m,
        None if refreshing => "Refreshing from source...",
        None => default_hint.as_str(),
    };

    let bar = Paragraph::new(msg.to_owned()).style(
        Style::default().fg(app.theme.bar_fg()).bg(app.theme.bar_bg()),
    );
    f.render_widget(bar, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_line_shows_date_and_error_only_while_failing() {
        assert_eq!(health_line(None, None), None);
        assert_eq!(
            health_line(Some("2026-11-05 10:11:12.5+00"), Some("HTTP 404")),
            Some("⚠ failing since 2026-11-05: HTTP 404".to_string()),
        );
        // Short or odd timestamp text is shown as is, never sliced mid-char.
        assert_eq!(
            health_line(Some("한국"), None),
            Some("⚠ failing since 한국: unknown error".to_string()),
        );
    }
}
