use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use ratatui_image::StatefulImage;

use crate::app::App;

// Same cell geometry as library grid so covers render identically.
const CELL_WIDTH:  u16 = 22;
const CELL_HEIGHT: u16 = 16;
const IMG_WIDTH:   u16 = 18;
const IMG_HEIGHT:  u16 = 12;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme_accent = app.theme.accent();
    let theme_bar_fg = app.theme.bar_fg();
    let theme_bar_bg = app.theme.bar_bg();
    let theme_text_bold = app.theme.text_bold();
    let theme_error = app.theme.error();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(if app.discover_error.is_some() { 1 } else { 0 }),
            Constraint::Length(1),
        ])
        .split(area);

    let title = format!(
        " Discover — {} new manhwa {}",
        app.discoveries.len(),
        if app.discover_adding { "(adding...)" } else { "" },
    );
    f.render_widget(
        Paragraph::new(title)
            .style(Style::default().fg(theme_text_bold).add_modifier(Modifier::BOLD)),
        rows[0],
    );

    draw_grid(f, app, rows[1]);

    if let Some(err) = app.discover_error.clone() {
        f.render_widget(
            Paragraph::new(format!(" {err}"))
                .style(Style::default().fg(theme_error)),
            rows[2],
        );
    }

    let hint = " h/l move  j/k row  a/Enter add  x dismiss  Esc back";
    f.render_widget(
        Paragraph::new(hint).style(
            Style::default().fg(theme_bar_fg).bg(theme_bar_bg),
        ),
        rows[3],
    );

    // Keep accent used so optimiser doesn't drop the lookup.
    let _ = theme_accent;
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

    let total = app.discoveries.len();
    if total == 0 {
        f.render_widget(
            Paragraph::new(
                "\n  No discoveries yet.\n\n  The coordinator polls sources once a day.\n  New manhwa will appear here as they're detected.",
            )
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let grid_cols = (area.width / CELL_WIDTH).max(1) as usize;
    let grid_rows = (area.height / CELL_HEIGHT).max(1) as usize;
    app.discover_grid_cols = grid_cols;

    let sel = app.discover_sel.min(total.saturating_sub(1));
    let sel_row = sel / grid_cols;
    let total_rows = total.div_ceil(grid_cols);

    let scroll_row = if sel_row < grid_rows / 2 {
        0
    } else if sel_row + grid_rows / 2 >= total_rows {
        total_rows.saturating_sub(grid_rows)
    } else {
        sel_row.saturating_sub(grid_rows / 2)
    };

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
            draw_cell(f, app, cell_area, idx, idx == sel);
        }
    }
}

fn draw_cell(f: &mut Frame, app: &mut App, area: Rect, idx: usize, is_selected: bool) {
    let entry = match app.discoveries.get(idx) {
        Some(d) => (
            d.id,
            d.title.clone(),
            d.cover_url.clone(),
            d.chapter_number,
            d.source.clone(),
        ),
        None => return,
    };
    let (disc_id, title, cover_url, chapter_number, source) = entry;

    if area.width < 4 || area.height < 4 { return; }

    if is_selected {
        let border = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.accent()));
        f.render_widget(border, area);
    }

    let content = if is_selected {
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2))
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

    render_cover(f, app, img_area, disc_id, cover_url.as_deref());
    render_title(f, txt_area, &title, is_selected, &app.theme);
    if sts_area.width > 0 {
        render_meta(f, sts_area, &source, chapter_number, &app.theme);
    }
}

fn render_cover(f: &mut Frame, app: &mut App, area: Rect, disc_id: i64, cover_url: Option<&str>) {
    if area.width == 0 || area.height == 0 { return; }
    app.discover_cover_cache.ensure_loaded(disc_id, cover_url);
    if let Some(protocol) = app.get_discover_cover_protocol(disc_id) {
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

fn render_meta(f: &mut Frame, area: Rect, source: &str, chapter_number: Option<f64>, theme: &crate::config::ThemeConfig) {
    let ch = chapter_number
        .map(|n| if n.fract() == 0.0 { format!("ch.{n:.0}") } else { format!("ch.{n}") })
        .unwrap_or_default();
    let text = if ch.is_empty() {
        format!("[{source}]")
    } else {
        format!("[{source}] {ch}")
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
