mod app;
mod db;
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
