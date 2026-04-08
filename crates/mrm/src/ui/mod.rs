pub mod library;
pub mod detail;
pub mod search;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::App;
use crate::types::{Screen, Status};

pub fn draw(f: &mut Frame, app: &mut App) {
    match &app.screen.clone() {
        Screen::Library                    => library::draw(f, app),
        Screen::Detail { .. }              => detail::draw(f, app),
        Screen::Reader { .. }              => draw_reader(f, app),
        Screen::StatusPicker { .. } => {
            detail::draw(f, app);
            draw_status_picker(f, app);
        }
        Screen::Search => search::draw(f, app),
    }
}

// ---------------------------------------------------------------------------
// Reader screen
// ---------------------------------------------------------------------------

fn draw_reader(f: &mut Frame, app: &App) {
    let area = f.area();
    let theme = &app.theme;
    let keys = &app.keys;

    let chapter = match &app.current_chapter {
        Some(ch) => ch,
        None => return,
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let header = format!(
        " {} — {}{}  |  {} prev ch  {} next ch  Esc back",
        app.current_manhwa.as_ref().map(|m| m.title.as_str()).unwrap_or(""),
        chapter.display_title(),
        if app.images_loading { "  ⏳" } else { "" },
        keys.prev_chapter,
        keys.next_chapter,
    );
    f.render_widget(
        Paragraph::new(header).style(Style::default().fg(theme.bar_fg()).bg(theme.bar_bg())),
        rows[0],
    );

    f.render_widget(Clear, rows[1]);
    let n = app.image_paths.len();
    let (msg, color) = if app.imv_process.is_some() {
        let extra = if app.images_loading {
            format!(" ({n} loaded, more coming...)")
        } else {
            format!(" ({n} pages)")
        };
        (
            format!("\n  imv open{extra}.\n\n  arrows/scroll  navigate\n  +/-  zoom\n  f  fullscreen\n  q  quit imv"),
            theme.success(),
        )
    } else if app.images_loading {
        (format!("\n  Downloading pages... ({n} ready)"), theme.warning())
    } else if app.image_paths.is_empty() {
        ("\n  No images found.".into(), theme.error())
    } else {
        ("\n  Opening imv...".into(), theme.warning())
    };

    f.render_widget(
        Paragraph::new(msg).style(Style::default().fg(color)),
        rows[1],
    );

    f.render_widget(
        Paragraph::new(format!(
            " {} prev chapter  {} next chapter  Esc back  |  imv: arrows pan  scroll/+/- zoom  f fullscreen  q quit imv",
            keys.prev_chapter, keys.next_chapter
        ))
            .style(Style::default().fg(theme.bar_fg()).bg(theme.bar_bg())),
        rows[2],
    );
}

// ---------------------------------------------------------------------------
// Status picker overlay
// ---------------------------------------------------------------------------

fn draw_status_picker(f: &mut Frame, app: &App) {
    let area    = f.area();
    let popup   = centered_rect(40, 60, area);
    let options = Status::all();
    let theme = &app.theme;

    let items: Vec<ListItem> = options.iter()
        .map(|s| ListItem::new(Line::from(format!("  {}", s.label(0)))))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Set Status ")
            .border_style(Style::default().fg(theme.accent())))
        .highlight_style(Style::default().bg(theme.accent()).fg(theme.bar_fg()).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.status_sel));

    f.render_widget(Clear, popup);
    f.render_stateful_widget(list, popup, &mut state);
}

// ---------------------------------------------------------------------------
// Helper: centered popup rect
// ---------------------------------------------------------------------------

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
