use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use ratatui_image::StatefulImage;

use crate::app::App;

// Each grid cell: cover image + title + status line
const CELL_WIDTH: u16 = 22; // columns per cell
const CELL_HEIGHT: u16 = 16; // rows per cell (image area + 2 text lines)
const IMG_WIDTH: u16 = 18; // image width within cell (centered)
const IMG_HEIGHT: u16 = 12; // fixed image height

/// Everything a grid cell needs, copied out once per frame so drawing never
/// re-runs the filter + sort and doesn't hold a borrow on `app`.
struct CellData {
    id: i64,
    title: String,
    cover_url: Option<String>,
    status: crate::types::Status,
    unread: u32,
    status_display: String,
    /// Polls of this series are failing (shown as a ⚠ marker).
    failing: bool,
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let cells: Vec<CellData> = app
        .visible_manhwa()
        .into_iter()
        .map(|m| CellData {
            id: m.id,
            title: m.title.clone(),
            cover_url: m.cover_url.clone(),
            status: m.status.clone(),
            unread: m.unread,
            status_display: m.status_display(),
            failing: m.failing_since.is_some() || m.chapter_conflict.is_some(),
        })
        .collect();

    // Split: main grid | right sidebar (keybinds)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(22)])
        .split(area);

    // Split left: search bar (if active) | grid | status bar
    let search_height = if app.search_active || !app.search_query.is_empty() {
        3
    } else {
        0
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(search_height),
            Constraint::Length(1), // title bar
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(cols[0]);

    // Search bar
    if search_height > 0 {
        let search_text = format!(" / {}", app.search_query);
        let search = Paragraph::new(search_text)
            .block(Block::default().borders(Borders::ALL).title(" Search "))
            .style(Style::default().fg(app.theme.accent()));
        f.render_widget(search, rows[0]);
    }

    // Title bar
    let title = if app.search_query.is_empty() {
        format!(" mrm — {} manhwa ", cells.len())
    } else {
        format!(" mrm — {} results ", cells.len())
    };
    let title_bar = Paragraph::new(title).style(
        Style::default()
            .fg(app.theme.text_bold())
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(title_bar, rows[1]);

    // Grid area
    draw_grid(f, app, &cells, rows[2]);

    // Bottom status bar
    draw_statusbar(f, app, rows[3]);

    // Right sidebar: keybinds
    draw_keybinds(f, app, cols[1]);

    // Delete confirmation overlay
    if app.confirm_delete_id.is_some() {
        draw_delete_confirm(f, app);
    }
}

fn draw_grid(f: &mut Frame, app: &mut App, cells: &[CellData], area: Rect) {
    if area.width < CELL_WIDTH || area.height < CELL_HEIGHT {
        let msg = Paragraph::new("Terminal too small")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        f.render_widget(msg, area);
        return;
    }

    let total = cells.len();
    if total == 0 {
        let msg = Paragraph::new("No manhwa yet. Press 'a' to add one.")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        f.render_widget(msg, area);
        return;
    }

    let grid_cols = (area.width / CELL_WIDTH).max(1) as usize;
    let grid_rows = (area.height / CELL_HEIGHT).max(1) as usize;
    app.grid_cols = grid_cols;

    let sel = app.library_sel.min(total.saturating_sub(1));

    // Compute scroll offset so the selected item is visible
    let sel_row = sel / grid_cols;
    let total_rows = total.div_ceil(grid_cols);

    // We store scroll state as a simple row offset
    let scroll_row = if sel_row < grid_rows / 2 {
        0
    } else if sel_row + grid_rows / 2 >= total_rows {
        total_rows.saturating_sub(grid_rows)
    } else {
        sel_row.saturating_sub(grid_rows / 2)
    };

    // Render each visible cell
    for vis_row in 0..grid_rows {
        let data_row = scroll_row + vis_row;
        for col in 0..grid_cols {
            let idx = data_row * grid_cols + col;
            if idx >= total {
                break;
            }

            let cell_x = area.x + (col as u16) * CELL_WIDTH;
            let cell_y = area.y + (vis_row as u16) * CELL_HEIGHT;

            // Clamp to area bounds
            if cell_x + CELL_WIDTH > area.x + area.width
                || cell_y + CELL_HEIGHT > area.y + area.height
            {
                continue;
            }

            let cell_area = Rect::new(cell_x, cell_y, CELL_WIDTH, CELL_HEIGHT);
            let is_selected = idx == sel;

            draw_cell(f, app, &cells[idx], cell_area, is_selected);
        }
    }
}

fn draw_cell(f: &mut Frame, app: &mut App, cell: &CellData, area: Rect, is_selected: bool) {
    let CellData {
        id: manhwa_id,
        title,
        cover_url,
        status,
        unread,
        status_display,
        failing,
    } = cell;
    let (manhwa_id, unread, failing) = (*manhwa_id, *unread, *failing);

    if area.width < 4 || area.height < 4 {
        return;
    }

    // Selected cell gets an accent border
    if is_selected {
        let border = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.accent()));
        f.render_widget(border, area);
    }

    // Content area (inside border for selected, full area for unselected)
    let content = if is_selected {
        Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        )
    } else {
        area
    };
    if content.width < 3 || content.height < 4 {
        return;
    }

    // Center image horizontally within content
    let img_w = IMG_WIDTH.min(content.width);
    let img_h = IMG_HEIGHT.min(content.height.saturating_sub(2));
    let img_x = content.x + (content.width.saturating_sub(img_w)) / 2;
    let img_area = Rect::new(img_x, content.y, img_w, img_h);

    // Text lines placed directly below image, using full content width
    let txt_y = content.y + img_h;
    let txt_area = Rect::new(content.x, txt_y, content.width, 1);
    let sts_y = txt_y + 1;
    let sts_area = if sts_y < content.y + content.height {
        Rect::new(content.x, sts_y, content.width, 1)
    } else {
        Rect::new(content.x, txt_y, 0, 0) // no room
    };

    render_cover(f, app, img_area, manhwa_id, cover_url.as_deref());
    render_title_text(f, txt_area, title, is_selected, &app.theme);
    if sts_area.width > 0 {
        render_status_line(
            f,
            sts_area,
            status,
            unread,
            status_display,
            failing,
            &app.theme,
        );
    }
}

fn render_cover(f: &mut Frame, app: &mut App, area: Rect, manhwa_id: i64, cover_url: Option<&str>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    app.cover_cache.ensure_loaded(manhwa_id, cover_url);

    if let Some(protocol) = app.get_cover_protocol(manhwa_id) {
        let image_widget = StatefulImage::default();
        f.render_stateful_widget(image_widget, area, protocol);
        return;
    }

    let placeholder = Paragraph::new("No\nCover")
        .style(Style::default().fg(app.theme.text_secondary()))
        .alignment(Alignment::Center);
    f.render_widget(placeholder, area);
}

/// Truncate a string to fit within `max_chars` display columns, appending "…" if needed.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count: usize = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn render_title_text(
    f: &mut Frame,
    area: Rect,
    title: &str,
    selected: bool,
    theme: &crate::config::ThemeConfig,
) {
    let display = truncate_str(title, area.width as usize);

    let style = if selected {
        Style::default()
            .fg(theme.text_bold())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text())
    };

    let para = Paragraph::new(display)
        .style(style)
        .alignment(Alignment::Center);
    f.render_widget(para, area);
}

/// Status line text: unread badge, status, and a leading ⚠ while the
/// series' polls are failing.
fn status_text(unread: u32, status_display: &str, failing: bool) -> String {
    let base = if unread > 0 {
        format!("[{}] {}", unread, status_display)
    } else {
        status_display.to_string()
    };
    if failing {
        format!("⚠ {base}")
    } else {
        base
    }
}

fn render_status_line(
    f: &mut Frame,
    area: Rect,
    status: &crate::types::Status,
    unread: u32,
    status_display: &str,
    failing: bool,
    theme: &crate::config::ThemeConfig,
) {
    let status_color = theme.status_color(status);
    let text = truncate_str(
        &status_text(unread, status_display, failing),
        area.width as usize,
    );

    // The marker in the warning colour, the rest in the status colour.
    let line = match text.strip_prefix("⚠ ") {
        Some(rest) if failing => Line::from(vec![
            Span::styled("⚠ ", Style::default().fg(theme.error())),
            Span::styled(rest.to_string(), Style::default().fg(status_color)),
        ]),
        _ => Line::from(Span::styled(text, Style::default().fg(status_color))),
    };

    let para = Paragraph::new(line).alignment(Alignment::Center);
    f.render_widget(para, area);
}

/// Library status-bar hint, built from the configured keys (`D` and `q` are
/// fixed bindings).
pub(crate) fn library_hint(keys: &crate::config::KeysConfig) -> String {
    format!(
        "{}/{} move  {}/{} row  {} open  {} search  {} add  D discover  {} delete  {} poll  q quit",
        keys.left,
        keys.right,
        keys.down,
        keys.up,
        keys.open,
        keys.search,
        keys.add,
        keys.delete,
        keys.poll_now,
    )
}

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let default_hint = library_hint(&app.keys);
    let msg = app.status_msg.as_deref().unwrap_or(&default_hint);

    let bar = Paragraph::new(msg).style(
        Style::default()
            .fg(app.theme.bar_fg())
            .bg(app.theme.bar_bg()),
    );
    f.render_widget(bar, area);
}

fn draw_keybinds(f: &mut Frame, app: &App, area: Rect) {
    let accent = app.theme.accent();
    let secondary = app.theme.text_secondary();
    let keys = &app.keys;

    let key_style = Style::default().fg(accent);
    let lines = vec![
        Line::from(Span::styled(
            " Navigation",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {}/←  ", keys.left), key_style),
            Span::raw("left"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}/→  ", keys.right), key_style),
            Span::raw("right"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}/↓  ", keys.down), key_style),
            Span::raw("down"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}/↑  ", keys.up), key_style),
            Span::raw("up"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.top), key_style),
            Span::raw("top"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.bottom), key_style),
            Span::raw("bottom"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}", keys.open), key_style),
            Span::raw(" open"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Library",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.search), key_style),
            Span::raw("search"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.add), key_style),
            Span::raw("add"),
        ]),
        Line::from(vec![
            Span::styled(" D    ", key_style),
            Span::raw("discover"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.delete), key_style),
            Span::raw("delete"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.sort), key_style),
            Span::raw("sort"),
        ]),
        Line::from(vec![
            Span::styled(format!(" {}    ", keys.poll_now), key_style),
            Span::raw("poll sources now"),
        ]),
        Line::from(vec![Span::styled(" q    ", key_style), Span::raw("quit")]),
    ];

    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(secondary))
        .title(" Keys ");

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup = centered_rect(50, 25, area);

    let title = app
        .confirm_delete_id
        .and_then(|id| app.manhwa_list.iter().find(|m| m.id == id))
        .map(|m| m.title.as_str())
        .unwrap_or("this manhwa");

    let text = format!(
        "\n  Delete \"{}\"?\n\n  This removes all chapters and reading progress.\n\n  Press {} to confirm  |  Esc to cancel",
        title, app.keys.delete
    );

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm Delete ")
                    .border_style(Style::default().fg(app.theme.error())),
            )
            .style(Style::default().fg(app.theme.text())),
        popup,
    );
}

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
mod tests {
    use super::*;

    #[test]
    fn failing_series_get_a_warning_marker() {
        assert_eq!(status_text(0, "Reading", false), "Reading");
        assert_eq!(status_text(2, "2 new", false), "[2] 2 new");
        assert_eq!(status_text(0, "Reading", true), "⚠ Reading");
        assert_eq!(status_text(2, "2 new", true), "⚠ [2] 2 new");
    }

    #[test]
    fn library_hint_follows_rebound_keys() {
        let keys = crate::config::KeysConfig::default();
        assert_eq!(
            library_hint(&keys),
            "h/l move  j/k row  Enter open  / search  a add  D discover  d delete  p poll  q quit",
        );
        let rebound = crate::config::KeysConfig {
            poll_now: "P".into(),
            left: "n".into(),
            ..keys
        };
        let hint = library_hint(&rebound);
        assert!(
            hint.starts_with("n/l move") && hint.contains("P poll"),
            "{hint}"
        );
    }
}
