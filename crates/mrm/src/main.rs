mod app;
mod config;
mod db;
mod notifier;
mod scraper;
mod types;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;
use types::AppEvent;

// ---------------------------------------------------------------------------
// DB path resolution
// ---------------------------------------------------------------------------

fn db_path() -> String {
    // Look for mrm.db relative to the binary's location, then CWD, then home
    let candidates = [
        PathBuf::from("mrm.db"),
        PathBuf::from("../../mrm.db"),   // when running from crates/mrm/
        dirs_next(),
    ];
    for p in &candidates {
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    // Default: create in CWD
    "mrm.db".into()
}

fn dirs_next() -> PathBuf {
    // XDG data dir fallback
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/mrm/mrm.db")
    } else {
        PathBuf::from("mrm.db")
    }
}

// ---------------------------------------------------------------------------
// Startup cleanup
// ---------------------------------------------------------------------------

/// Remove stale mrm_* directories in /tmp left by crashed sessions.
/// Called once at startup before the TUI starts. Silently ignores errors.
fn startup_cleanup_tmp() {
    let tmp = std::env::temp_dir();
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(86_400))   // 1 day
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let entries = match std::fs::read_dir(&tmp) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("mrm_") { continue; }

        // Only delete if older than 1 day (avoids touching a live session
        // that happens to share the same prefix in a multi-user setup).
        let age_ok = entry.metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);

        if age_ok {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let db_path = db_path();
    eprintln!("mrm: opening DB at {}", db_path);
    let pool = match db::open_db(&db_path).await {
        Ok(p) => p,
        Err(e) => { eprintln!("mrm: DB error: {e}"); return Err(e); }
    };
    startup_cleanup_tmp();
    let mut app = match App::new(pool).await {
        Ok(a) => a,
        Err(e) => { eprintln!("mrm: app init error: {e}"); return Err(e); }
    };

    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_loop(&mut terminal, &mut app).await;

    // Always restore terminal, even on error
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut msg_timer: u8 = 0;

    loop {
        // Draw first so the screen is always up-to-date before waiting
        terminal.draw(|f| ui::draw(f, app))?;

        tokio::select! {
            // Keyboard / terminal event
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.handle_event(AppEvent::Key(key)).await?;
                    }
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                }
            }

            // Tick: UI refresh + status message auto-clear
            _ = tick.tick() => {
                app.handle_event(AppEvent::Tick).await?;

                if app.status_msg.is_some() {
                    msg_timer += 1;
                    if msg_timer >= 8 {
                        app.clear_msg();
                        msg_timer = 0;
                    }
                } else {
                    msg_timer = 0;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}
