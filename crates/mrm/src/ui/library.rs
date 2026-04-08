use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use ratatui_image::StatefulImage;

use crate::app::App;
use crate::types::Status;

// Each grid cell: cover image + title + status line
const CELL_WIDTH: u16 = 22;  // columns per cell
const CELL_HEIGHT: u16 = 16; // rows per cell (image area + 2 text lines)
const IMG_WIDTH: u16 = 18;   // image width within cell (centered)
const IMG_HEIGHT: u16 = 12;  // fixed image height

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.size();

    // Split: main grid | right sidebar (keybinds)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(22)])
        .split(area);

    // Split left: search bar (if active) | grid | status bar
    let search_height = if app.search_active || !app.search_query.is_empty() { 3 } else { 0 };
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
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(search, rows[0]);
    }

    // Title bar
    let visible = app.visible_manhwa();
    let title = if app.search_query.is_empty() {
        format!(" mrm — {} manhwa ", visible.len())
    } else {
        format!(" mrm — {} results ", visible.len())
    };
    let title_bar = Paragraph::new(title)
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(title_bar, rows[1]);

    // Grid area
    draw_grid(f, app, rows[2]);

    // Bottom status bar
    draw_statusbar(f, app, rows[3]);

    // Right sidebar: keybinds
    draw_keybinds(f, cols[1]);

    // Delete confirmation overlay
    if app.confirm_delete_id.is_some() {
        draw_delete_confirm(f, app);
    }
}

fn draw_grid(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < CELL_WIDTH || area.height < CELL_HEIGHT {
        let msg = Paragraph::new("Terminal too small")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        f.render_widget(msg, area);
        return;
    }

    let total = app.visible_manhwa().len();
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
    let total_rows = (total + grid_cols - 1) / grid_cols;

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

            draw_cell(f, app, cell_area, idx, is_selected);
        }
    }
}

fn draw_cell(f: &mut Frame, app: &mut App, area: Rect, idx: usize, is_selected: bool) {
    // Collect needed data before mutably borrowing app for cover rendering
    let manhwa_data = {
        let visible = app.visible_manhwa();
        visible.get(idx).map(|m| (m.id, m.title.clone(), m.cover_url.clone(), m.status.clone(), m.unread, m.status_display()))
    };
    let (manhwa_id, title, cover_url, status, unread, status_display) = match manhwa_data {
        Some(d) => d,
        None => return,
    };

    if area.width < 4 || area.height < 4 {
        return;
    }

    // Selected cell gets a yellow border
    if is_selected {
        let border = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        f.render_widget(border, area);
    }

    // Content area (inside border for selected, full area for unselected)
    let content = if is_selected {
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2))
    } else {
        area
    };
    if content.width < 3 || content.height < 4 { return; }

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
    render_title_text(f, txt_area, &title, is_selected);
    if sts_area.width > 0 {
        render_status_line(f, sts_area, &status, unread, &status_display);
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
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    f.render_widget(placeholder, area);
}

/// Truncate a string to fit within `max_chars` display columns, appending "…" if needed.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if max_chars == 0 { return String::new(); }
    let char_count: usize = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn render_title_text(f: &mut Frame, area: Rect, title: &str, selected: bool) {
    let display = truncate_str(title, area.width as usize);

    let style = if selected {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let para = Paragraph::new(display)
        .style(style)
        .alignment(Alignment::Center);
    f.render_widget(para, area);
}

fn render_status_line(f: &mut Frame, area: Rect, status: &Status, unread: u32, status_display: &str) {
    let status_color = match status {
        Status::LookedInto => Color::Gray,
        Status::Reading    => Color::Green,
        Status::UpToDate   => Color::Cyan,
        Status::Paused     => Color::Yellow,
        Status::Completed  => Color::Blue,
        Status::Dropped    => Color::DarkGray,
    };

    let text = if unread > 0 {
        format!("[{}] {}", unread, status_display)
    } else {
        status_display.to_string()
    };

    let text = truncate_str(&text, area.width as usize);

    let para = Paragraph::new(text)
        .style(Style::default().fg(status_color))
        .alignment(Alignment::Center);
    f.render_widget(para, area);
}

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let msg = app
        .status_msg
        .as_deref()
        .unwrap_or("h/l move  j/k row  Enter open  / search  a add  d delete  q quit");

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
            Span::styled(" h/←  ", Style::default().fg(Color::Yellow)),
            Span::raw("left"),
        ]),
        Line::from(vec![
            Span::styled(" l/→  ", Style::default().fg(Color::Yellow)),
            Span::raw("right"),
        ]),
        Line::from(vec![
            Span::styled(" j/↓  ", Style::default().fg(Color::Yellow)),
            Span::raw("down"),
        ]),
        Line::from(vec![
            Span::styled(" k/↑  ", Style::default().fg(Color::Yellow)),
            Span::raw("up"),
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
            Span::raw(" open"),
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
            Span::raw("add"),
        ]),
        Line::from(vec![
            Span::styled(" d    ", Style::default().fg(Color::Yellow)),
            Span::raw("delete"),
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

fn draw_delete_confirm(f: &mut Frame, app: &App) {
    let area = f.size();
    let popup = centered_rect(50, 25, area);

    let title = app.confirm_delete_id
        .and_then(|id| app.manhwa_list.iter().find(|m| m.id == id))
        .map(|m| m.title.as_str())
        .unwrap_or("this manhwa");

    let text = format!(
        "\n  Delete \"{}\"?\n\n  This removes all chapters and reading progress.\n\n  Press d to confirm  |  Esc to cancel",
        title
    );

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm Delete ")
                    .border_style(Style::default().fg(Color::Red)),
            )
            .style(Style::default().fg(Color::White)),
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
