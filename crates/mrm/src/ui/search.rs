use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::App;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.size();

    let error_height = if app.add_search_error.is_some() { 1 } else { 0 };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),            // input box
            Constraint::Min(0),               // results list
            Constraint::Length(error_height), // error line
            Constraint::Length(1),            // status bar
        ])
        .split(area);

    // --- Input box ---
    let input_style = if app.add_search_input_active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::White)
    };
    let cursor = if app.add_search_input_active { "█" } else { "" };
    let input_text = format!(" {}{}", app.add_search_query, cursor);
    let input_title = if app.add_search_loading {
        " Search (loading...) "
    } else {
        " Search — type and press Enter "
    };
    f.render_widget(
        Paragraph::new(input_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(input_title)
                    .border_style(input_style),
            )
            .style(Style::default().fg(Color::White)),
        rows[0],
    );

    // --- Results list ---
    let results = &app.add_search_results;
    let items: Vec<ListItem> = if results.is_empty() && !app.add_search_loading {
        let hint = if app.add_search_query.is_empty() {
            "  Type a title and press Enter to search"
        } else {
            "  No results found"
        };
        vec![ListItem::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        results
            .iter()
            .map(|r| {
                let source_badge = Span::styled(
                    format!("[{}] ", r.source),
                    Style::default().fg(Color::Cyan),
                );
                let title_span = Span::styled(
                    r.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                );
                let pub_span = Span::styled(
                    format!("  {}", r.pub_status),
                    Style::default().fg(Color::DarkGray),
                );
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    source_badge,
                    title_span,
                    pub_span,
                ]))
            })
            .collect()
    };

    let list_title = format!(" Results ({}) ", results.len());
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(list_title)
                .border_style(Style::default().fg(Color::White)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut list_state = ListState::default();
    if !results.is_empty() && !app.add_search_input_active {
        list_state.select(Some(app.add_search_sel));
    }
    f.render_stateful_widget(list, rows[1], &mut list_state);

    // --- Error line ---
    if let Some(err) = &app.add_search_error {
        f.render_widget(
            Paragraph::new(format!(" {}", err))
                .style(Style::default().fg(Color::Red)),
            rows[2],
        );
    }

    // --- Status bar ---
    let hint = if app.add_search_loading {
        " Fetching... please wait"
    } else if app.add_search_input_active {
        " Enter search  Esc back"
    } else {
        " j/k move  Enter add  i edit query  Esc back"
    };
    f.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(Color::Black).bg(Color::White)),
        rows[3],
    );
}
