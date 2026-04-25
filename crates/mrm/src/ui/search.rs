use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use ratatui_image::StatefulImage;

use crate::app::{search_result_id, App};

// Same cell geometry as library/discover so covers render identically.
const CELL_WIDTH:  u16 = 22;
const CELL_HEIGHT: u16 = 16;
const IMG_WIDTH:   u16 = 18;
const IMG_HEIGHT:  u16 = 12;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme_accent = app.theme.accent();
    let theme_text   = app.theme.text();
    let theme_bar_fg = app.theme.bar_fg();
    let theme_bar_bg = app.theme.bar_bg();
    let theme_error  = app.theme.error();

    let error_height = if app.add_search_error.is_some() { 1 } else { 0 };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),           // input box
            Constraint::Length(1),           // results-count title
            Constraint::Min(0),              // grid
            Constraint::Length(error_height),
            Constraint::Length(1),           // status bar
        ])
        .split(area);

    // --- Input box ---
    let input_style = if app.add_search_input_active {
        Style::default().fg(theme_accent)
    } else {
        Style::default().fg(theme_text)
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
            .style(Style::default().fg(theme_text)),
        rows[0],
    );

    // --- Results count line ---
    let count_text = format!(" Results ({})", app.add_search_results.len());
    f.render_widget(
        Paragraph::new(count_text)
            .style(Style::default().fg(app.theme.text_bold()).add_modifier(Modifier::BOLD)),
        rows[1],
    );

    // --- Grid ---
    draw_grid(f, app, rows[2]);

    // --- Error line ---
    if let Some(err) = app.add_search_error.clone() {
        f.render_widget(
            Paragraph::new(format!(" {}", err))
                .style(Style::default().fg(theme_error)),
            rows[3],
        );
    }

    // --- Status bar ---
    let keys = &app.keys;
    let hint = if app.add_search_loading {
        " Fetching... please wait".to_string()
    } else if app.add_search_input_active {
        " Enter search  Esc back".to_string()
    } else {
        format!(
            " {}/{}/{}/{} move  Enter add  {} edit query  Esc back",
            keys.left, keys.down, keys.up, keys.right, keys.input_mode,
        )
    };
    f.render_widget(
        Paragraph::new(hint)
            .style(Style::default().fg(theme_bar_fg).bg(theme_bar_bg)),
        rows[4],
    );
}

fn draw_grid(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < CELL_WIDTH || area.height < CELL_HEIGHT {
        f.render_widget(
            Paragraph::new("Terminal too small")
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let total = app.add_search_results.len();
    if total == 0 {
        let hint = if app.add_search_loading {
            "  Searching..."
        } else if app.add_search_query.is_empty() {
            "  Type a title and press Enter to search"
        } else {
            "  No results found"
        };
        f.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(app.theme.text_secondary()))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let grid_cols = (area.width / CELL_WIDTH).max(1) as usize;
    let grid_rows = (area.height / CELL_HEIGHT).max(1) as usize;
    app.add_search_grid_cols = grid_cols;

    let sel = app.add_search_sel.min(total.saturating_sub(1));
    let sel_row = sel / grid_cols;
    let total_rows = (total + grid_cols - 1) / grid_cols;

    let scroll_row = if sel_row < grid_rows / 2 {
        0
    } else if sel_row + grid_rows / 2 >= total_rows {
        total_rows.saturating_sub(grid_rows)
    } else {
        sel_row.saturating_sub(grid_rows / 2)
    };

    let highlight = !app.add_search_input_active;

    for vis_row in 0..grid_rows {
        let data_row = scroll_row + vis_row;
        for col in 0..grid_cols {
            let idx = data_row * grid_cols + col;
            if idx >= total { break; }

            let cell_x = area.x + (col as u16) * CELL_WIDTH;
            let cell_y = area.y + (vis_row as u16) * CELL_HEIGHT;
            if cell_x + CELL_WIDTH > area.x + area.width
                || cell_y + CELL_HEIGHT > area.y + area.height
            {
                continue;
            }
            let cell_area = Rect::new(cell_x, cell_y, CELL_WIDTH, CELL_HEIGHT);
            draw_cell(f, app, cell_area, idx, highlight && idx == sel);
        }
    }
}

fn draw_cell(f: &mut Frame, app: &mut App, area: Rect, idx: usize, is_selected: bool) {
    let entry = match app.add_search_results.get(idx) {
        Some(r) => (
            search_result_id(&r.source_url),
            r.title.clone(),
            r.cover_url.clone(),
            r.source.clone(),
            r.pub_status.clone(),
        ),
        None => return,
    };
    let (cache_id, title, cover_url, source, pub_status) = entry;

    if area.width < 4 || area.height < 4 { return; }

    if is_selected {
        let border = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.accent()));
        f.render_widget(border, area);
    }

    let content = if is_selected {
        Rect::new(
            area.x + 1, area.y + 1,
            area.width.saturating_sub(2), area.height.saturating_sub(2),
        )
    } else {
        area
    };
    if content.width < 3 || content.height < 4 { return; }

    let img_w = IMG_WIDTH.min(content.width);
    let img_h = IMG_HEIGHT.min(content.height.saturating_sub(2));
    let img_x = content.x + (content.width.saturating_sub(img_w)) / 2;
    let img_area = Rect::new(img_x, content.y, img_w, img_h);

    let txt_y = content.y + img_h;
    let txt_area = Rect::new(content.x, txt_y, content.width, 1);
    let sts_y = txt_y + 1;
    let sts_area = if sts_y < content.y + content.height {
        Rect::new(content.x, sts_y, content.width, 1)
    } else {
        Rect::new(content.x, txt_y, 0, 0)
    };

    render_cover(f, app, img_area, cache_id, cover_url.as_deref());
    render_title(f, txt_area, &title, is_selected, &app.theme);
    if sts_area.width > 0 {
        render_meta(f, sts_area, &source, &pub_status, &app.theme);
    }
}

fn render_cover(f: &mut Frame, app: &mut App, area: Rect, cache_id: i64, cover_url: Option<&str>) {
    if area.width == 0 || area.height == 0 { return; }
    app.search_cover_cache.ensure_loaded(cache_id, cover_url);
    if let Some(protocol) = app.get_search_cover_protocol(cache_id) {
        let image_widget = StatefulImage::default();
        f.render_stateful_widget(image_widget, area, protocol);
        return;
    }
    let placeholder = Paragraph::new("No\nCover")
        .style(Style::default().fg(app.theme.text_secondary()))
        .alignment(Alignment::Center);
    f.render_widget(placeholder, area);
}

fn render_title(f: &mut Frame, area: Rect, title: &str, selected: bool, theme: &crate::config::ThemeConfig) {
    let display = truncate(title, area.width as usize);
    let style = if selected {
        Style::default().fg(theme.text_bold()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text())
    };
    f.render_widget(
        Paragraph::new(display).style(style).alignment(Alignment::Center),
        area,
    );
}

fn render_meta(f: &mut Frame, area: Rect, source: &str, pub_status: &str, theme: &crate::config::ThemeConfig) {
    let text = if pub_status.is_empty() {
        format!("[{source}]")
    } else {
        format!("[{source}] {pub_status}")
    };
    let text = truncate(&text, area.width as usize);
    f.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.text_secondary()))
            .alignment(Alignment::Center),
        area,
    );
}

fn truncate(s: &str, max_chars: usize) -> String {
    if max_chars == 0 { return String::new(); }
    let n = s.chars().count();
    if n <= max_chars { return s.to_string(); }
    let t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{t}…")
}
