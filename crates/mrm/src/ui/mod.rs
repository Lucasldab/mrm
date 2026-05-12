pub mod library;
pub mod detail;
pub mod search;
pub mod discover;

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
        Screen::Discover => discover::draw(f, app),
    }
    if app.show_help {
        draw_help_overlay(f, app);
    }
}

// ---------------------------------------------------------------------------
// Help overlay — `?` toggles, any key dismisses.
// ---------------------------------------------------------------------------

fn draw_help_overlay(f: &mut Frame, app: &App) {
    use ratatui::text::Span;

    let theme = &app.theme;
    let area = f.area();

    // Pick keybind set based on current screen
    let (title, entries): (&str, Vec<(&str, &str)>) = match &app.screen {
        Screen::Library => ("Library", vec![
            ("j / k / ↑ / ↓", "Move selection"),
            ("h / l / ← / →", "Grid navigation"),
            ("g g / G",       "Jump to top / bottom"),
            ("Enter",         "Open series detail"),
            ("/",             "Search library"),
            ("a",             "Add series (cross-source search)"),
            ("d",             "Discover feed"),
            ("s",             "Set status"),
            ("r",             "Refresh from sources"),
            ("x",             "Delete series"),
            ("?",             "This help"),
            ("q",             "Quit"),
        ]),
        Screen::Detail { .. } => ("Series Detail", vec![
            ("j / k / ↑ / ↓", "Select chapter"),
            ("g g / G",       "Jump to first / last chapter"),
            ("Enter",         "Read chapter"),
            ("m",             "Mark chapter unread"),
            ("s",             "Set series status"),
            ("c",             "Clear status override"),
            ("Esc",           "Back to library"),
            ("?",             "This help"),
        ]),
        Screen::Search => ("Add Series", vec![
            ("i",     "Enter input mode (type query)"),
            ("Enter", "Submit / select result"),
            ("Esc",   "Exit input mode / back"),
            ("Tab",   "Switch source"),
            ("?",     "This help"),
        ]),
        Screen::Discover => ("Discover", vec![
            ("j / k / ↑ / ↓", "Select discovery"),
            ("h / l / ← / →", "Grid navigation"),
            ("Enter / a",     "Add to library"),
            ("x",             "Dismiss"),
            ("Esc",           "Back to library"),
            ("?",             "This help"),
        ]),
        Screen::StatusPicker { .. } => ("Status Picker", vec![
            ("j / k / ↑ / ↓", "Select status"),
            ("Enter",         "Apply"),
            ("Esc",           "Cancel"),
        ]),
        Screen::Reader { .. } => ("Reader", vec![
            ("(handled by external viewer)", ""),
        ]),
    };

    // Compute popup size: 60% wide, ~4 rows of chrome + 1 row per entry, max 80% tall
    let popup_w = (area.width as f32 * 0.6).clamp(40.0, 100.0) as u16;
    let popup_h = ((entries.len() + 4) as u16).min((area.height as f32 * 0.8) as u16);
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(popup_x, popup_y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Help — {} ", title))
        .border_style(Style::default().fg(crate::config::ThemeConfig::parse_color(&theme.accent)))
        .style(Style::default().fg(crate::config::ThemeConfig::parse_color(&theme.text)));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let key_color  = crate::config::ThemeConfig::parse_color(&theme.accent);
    let desc_color = crate::config::ThemeConfig::parse_color(&theme.text);
    let lines: Vec<Line> = entries
        .iter()
        .map(|(k, d)| Line::from(vec![
            Span::styled(format!("  {:<16}", k), Style::default().fg(key_color).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {}", d),      Style::default().fg(desc_color)),
        ]))
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "  Press any key to close",
            Style::default()
                .fg(crate::config::ThemeConfig::parse_color(&theme.text_secondary))
                .add_modifier(Modifier::ITALIC),
        ))))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
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
    let viewer_name = match app.viewer_kind {
        crate::config::ViewerKind::Imv => "imv",
        crate::config::ViewerKind::Rv  => "rv",
    };
    let (msg, color) = if app.viewer_process.is_some() {
        let extra = if app.images_loading {
            format!(" ({n} loaded, more coming...)")
        } else {
            format!(" ({n} pages)")
        };
        (
            format!("\n  {viewer_name} open{extra}.\n\n  arrows/scroll  navigate\n  +/-  zoom\n  f  fullscreen\n  q  quit {viewer_name}"),
            theme.success(),
        )
    } else if app.images_loading {
        (format!("\n  Downloading pages... ({n} ready)"), theme.warning())
    } else if app.image_paths.is_empty() {
        ("\n  No images found.".into(), theme.error())
    } else {
        (format!("\n  Opening {viewer_name}..."), theme.warning())
    };

    f.render_widget(
        Paragraph::new(msg).style(Style::default().fg(color)),
        rows[1],
    );

    f.render_widget(
        Paragraph::new(format!(
            " {} prev chapter  {} next chapter  Esc back  |  {viewer_name}: arrows pan  scroll/+/- zoom  f fullscreen  q quit {viewer_name}",
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
