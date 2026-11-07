pub mod detail;
pub mod discover;
pub mod library;
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
        Screen::Library => library::draw(f, app),
        Screen::Detail { .. } => detail::draw(f, app),
        Screen::Reader { .. } => draw_reader(f, app),
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

/// Help text for a screen, generated from the configured keys. Lists only
/// actions the screen's key handler implements (see `App::handle_*_key`).
pub fn help_entries(
    screen: &Screen,
    keys: &crate::config::KeysConfig,
) -> (&'static str, Vec<(String, String)>) {
    let e = |k: String, d: &str| (k, d.to_string());
    let pair = |a: &str, b: &str| format!("{a} / {b}");
    let nav = |v: &mut Vec<(String, String)>, grid: bool| {
        v.push(e(
            pair(&keys.down, "↓"),
            if grid { "Move down a row" } else { "Move down" },
        ));
        v.push(e(
            pair(&keys.up, "↑"),
            if grid { "Move up a row" } else { "Move up" },
        ));
        if grid {
            v.push(e(pair(&keys.left, "←"), "Move left"));
            v.push(e(pair(&keys.right, "→"), "Move right"));
        }
    };
    let top_label = |chord: bool| {
        if chord && keys.top == "g" {
            "gg".to_string()
        } else {
            keys.top.clone()
        }
    };
    let mut v: Vec<(String, String)> = Vec::new();
    let title = match screen {
        Screen::Library => {
            nav(&mut v, true);
            v.push(e(
                format!("{} / {}", top_label(true), keys.bottom),
                "Jump to first / last",
            ));
            v.push(e(keys.open.clone(), "Open series"));
            v.push(e(
                keys.search.clone(),
                "Filter library (Enter/Esc to finish)",
            ));
            v.push(e(keys.back.clone(), "Clear filter"));
            v.push(e(keys.sort.clone(), "Cycle sort order"));
            v.push(e(keys.add.clone(), "Add series (search sources)"));
            v.push(e("D".into(), "Discover feed"));
            v.push(e(
                keys.poll_now.clone(),
                "Check sources for new chapters now",
            ));
            v.push(e(
                format!("{} {}", keys.delete, keys.delete),
                "Delete series (press twice; Esc cancels)",
            ));
            v.push(e("?".into(), "This help"));
            v.push(e("q".into(), "Quit"));
            "Library"
        }
        Screen::Detail { .. } => {
            nav(&mut v, false);
            v.push(e(
                format!("{} / {}", top_label(true), keys.bottom),
                "Jump to first / last chapter",
            ));
            v.push(e(keys.open.clone(), "Read chapter"));
            v.push(e(keys.mark_read.clone(), "Mark chapter read"));
            v.push(e(keys.mark_unread.clone(), "Mark chapter unread"));
            v.push(e(keys.set_status.clone(), "Set series status"));
            v.push(e(keys.clear_override.clone(), "Clear status override"));
            v.push(e(
                "R".into(),
                "Refresh from source (metadata + new chapters)",
            ));
            v.push(e(keys.back.clone(), "Back to library"));
            v.push(e("?".into(), "This help"));
            "Series Detail"
        }
        Screen::Search => {
            v.push(e("(typing)".into(), "Type the query; other keys are text"));
            v.push(e("Enter".into(), "Search all sources"));
            v.push(e("Esc".into(), "Cancel a running search / back to library"));
            nav(&mut v, true);
            v.push(e(
                format!("{} / {}", keys.top, keys.bottom),
                "Jump to first / last result",
            ));
            v.push(e(keys.open.clone(), "Add selected result"));
            v.push(e(
                format!("{} / {}", keys.input_mode, keys.back),
                "Back to the query",
            ));
            v.push(e("?".into(), "This help (results only)"));
            "Add Series"
        }
        Screen::Discover => {
            nav(&mut v, true);
            v.push(e(
                format!("{} / {}", keys.top, keys.bottom),
                "Jump to first / last",
            ));
            v.push(e(format!("a / {}", keys.open), "Add to library"));
            v.push(e("x".into(), "Dismiss"));
            v.push(e(keys.back.clone(), "Back to library"));
            v.push(e("?".into(), "This help"));
            "Discover"
        }
        Screen::StatusPicker { .. } => {
            nav(&mut v, false);
            v.push(e(
                keys.open.clone(),
                "Apply (Up to date marks all chapters read)",
            ));
            v.push(e(keys.back.clone(), "Cancel"));
            v.push(e("?".into(), "This help"));
            "Status Picker"
        }
        Screen::Reader { .. } => {
            v.push(e(keys.next_chapter.clone(), "Next chapter"));
            v.push(e(keys.prev_chapter.clone(), "Previous chapter"));
            v.push(e(keys.back.clone(), "Close viewer, back to series"));
            v.push(e("Ctrl-C".into(), "Save progress and quit"));
            "Reader"
        }
    };
    (title, v)
}

fn draw_help_overlay(f: &mut Frame, app: &App) {
    use ratatui::text::Span;

    let theme = &app.theme;
    let area = f.area();

    let (title, entries) = help_entries(&app.screen, &app.keys);
    let entries: Vec<(&str, &str)> = entries
        .iter()
        .map(|(k, d)| (k.as_str(), d.as_str()))
        .collect();

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

    let key_color = crate::config::ThemeConfig::parse_color(&theme.accent);
    let desc_color = crate::config::ThemeConfig::parse_color(&theme.text);
    let lines: Vec<Line> = entries
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:<16}", k),
                    Style::default().fg(key_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {}", d), Style::default().fg(desc_color)),
            ])
        })
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "  Press any key to close",
            Style::default()
                .fg(crate::config::ThemeConfig::parse_color(
                    &theme.text_secondary,
                ))
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
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let header = format!(
        " {} — {}{}  |  {} prev ch  {} next ch  Esc back",
        app.current_manhwa
            .as_ref()
            .map(|m| m.title.as_str())
            .unwrap_or(""),
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
        crate::config::ViewerKind::Rv => "rv",
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
        (
            format!("\n  Downloading pages... ({n} ready)"),
            theme.warning(),
        )
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
    let area = f.area();
    let popup = centered_rect(40, 60, area);
    let options = Status::all();
    let theme = &app.theme;

    let items: Vec<ListItem> = options
        .iter()
        .map(|s| ListItem::new(Line::from(format!("  {}", s.label(0)))))
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Set Status ")
                .border_style(Style::default().fg(theme.accent())),
        )
        .highlight_style(
            Style::default()
                .bg(theme.accent())
                .fg(theme.bar_fg())
                .add_modifier(Modifier::BOLD),
        )
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

#[cfg(test)]
mod help_tests {
    use super::*;
    use crate::config::KeysConfig;

    fn text(screen: &Screen, keys: &KeysConfig) -> String {
        help_entries(screen, keys)
            .1
            .iter()
            .map(|(k, d)| format!("{k} {d}\n"))
            .collect()
    }

    #[test]
    fn library_help_uses_the_configured_delete_key() {
        let keys = KeysConfig {
            delete: "X".into(),
            poll_now: "P".into(),
            ..Default::default()
        };
        let t = text(&Screen::Library, &keys);
        assert!(t.contains("X X"), "{t}");
        assert!(t.contains("P Check sources"), "{t}");
        assert!(t.contains("D Discover"), "{t}");
    }

    #[test]
    fn detail_help_lists_mark_read_unread_and_refresh() {
        let keys = KeysConfig {
            mark_read: "m".into(),
            mark_unread: "n".into(),
            ..Default::default()
        };
        let t = text(&Screen::Detail { manhwa_id: 1 }, &keys);
        assert!(t.contains("m Mark chapter read"), "{t}");
        assert!(t.contains("n Mark chapter unread"), "{t}");
        assert!(t.contains("R Refresh"), "{t}");
    }

    #[test]
    fn no_unimplemented_entries() {
        let keys = KeysConfig::default();
        let all: String = [
            Screen::Library,
            Screen::Detail { manhwa_id: 1 },
            Screen::Search,
            Screen::Discover,
            Screen::StatusPicker { manhwa_id: 1 },
        ]
        .iter()
        .map(|s| text(s, &keys))
        .collect();
        assert!(!all.contains("Tab"), "{all}");
        assert!(!all.contains("Switch source"));
        // Library has no `s`/`r`/`x` bindings; Discover alone has `x`.
        let lib = text(&Screen::Library, &keys);
        assert!(
            !lib.contains("Set status") && !lib.contains("Mark") && !lib.contains("x "),
            "{lib}"
        );
        // Library `d` is delete, not Discover.
        assert!(lib.contains("d d Delete"), "{lib}");
        assert!(!lib.contains("d Discover"), "{lib}");
    }
}
