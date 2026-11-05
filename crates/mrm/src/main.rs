mod app;
mod config;
mod cover_cache;
mod db;
mod notifier;
#[cfg(feature = "ntfy")]
mod ntfy;
mod scraper;
mod types;
mod ui;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    execute,
    cursor::Show,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use app::App;
use scraper::ScraperEvent;
use types::AppEvent;

// ---------------------------------------------------------------------------
// Startup cleanup
// ---------------------------------------------------------------------------

fn startup_cleanup_tmp() {
    let tmp = std::env::temp_dir();
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(86_400))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let entries = match std::fs::read_dir(&tmp) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("mrm_") { continue; }

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
// CLI args
// ---------------------------------------------------------------------------

enum Mode {
    Tui,
    Daemon,
    Once,
}

fn parse_args() -> Mode {
    let args: Vec<String> = std::env::args().collect();
    for arg in &args[1..] {
        match arg.as_str() {
            "--daemon" | "-d" => return Mode::Daemon,
            "--once"          => return Mode::Once,
            "--help" | "-h"   => {
                eprintln!("Usage: mrm [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --daemon, -d  Run as background poller (no TUI)");
                eprintln!("  --once        Poll once and exit");
                eprintln!("  --help, -h    Show this help");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Mode::Tui
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let mode = parse_args();

    let config = config::load_config()
        .context("Config required — create config.toml (see README)")?;
    let url = config::resolve_database_url(
        std::env::var("MRM_DATABASE_URL").ok(), &config.db, &config::config_dir())?;
    let pool = match db::open_db(&url).await {
        Ok(p) => p,
        Err(e) => { eprintln!("mrm: database error: {e:#}"); return Err(e); }
    };

    match mode {
        Mode::Daemon => run_daemon(pool, config).await,
        Mode::Once   => run_once(pool, config).await,
        Mode::Tui    => run_tui(pool, config).await,
    }
}

// ---------------------------------------------------------------------------
// Daemon mode: poll forever, send notifications, no TUI
// ---------------------------------------------------------------------------

async fn run_daemon(pool: sqlx::PgPool, config: config::Config) -> Result<()> {
    eprintln!("mrm: daemon mode — polling every {} minutes", config.notifications.poll_interval_minutes);
    eprintln!("mrm: press Ctrl-C to stop");
    warn_unknown_sources(&config);

    let shutdown = CancellationToken::new();
    let registry = Arc::new(scraper::build_registry(&config));
    // Nobody reads coordinator events in daemon mode: drop the receiver so
    // sends fail fast instead of blocking once the buffer fills.
    let (tx, _) = mpsc::channel::<ScraperEvent>(1);

    let handle = tokio::spawn(scraper::coordinator_task(
        pool, config, registry, shutdown.clone(), Arc::new(Notify::new()), tx, false,
    ));

    let _ = tokio::signal::ctrl_c().await;
    eprintln!("\nmrm: shutting down...");
    scraper::stop_coordinator(&shutdown, handle, COORDINATOR_STOP_GRACE).await;

    eprintln!("mrm: daemon stopped");
    Ok(())
}

/// How long quitting waits for an in-flight poll before aborting it.
const COORDINATOR_STOP_GRACE: Duration = Duration::from_secs(2);

fn warn_unknown_sources(config: &config::Config) {
    for name in scraper::unknown_sources(config) {
        eprintln!("mrm: unknown source '{name}' in config, skipping");
    }
}

// ---------------------------------------------------------------------------
// Once mode: single poll, then exit
// ---------------------------------------------------------------------------

async fn run_once(pool: sqlx::PgPool, config: config::Config) -> Result<()> {
    eprintln!("mrm: single poll...");

    warn_unknown_sources(&config);
    let registry = scraper::build_registry(&config);

    let manhwa_list = db::fetch_all_manhwa(&pool).await?;
    eprintln!("mrm: checking {} manhwa", manhwa_list.len());

    for manhwa in &manhwa_list {
        let scraper = match registry.get(manhwa.source.as_str()) {
            Some(s) => s,
            None => continue,
        };

        let series = match scraper.get_series(&manhwa.source_url).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  ✗ {}: {e}", manhwa.title);
                continue;
            }
        };

        let new_count = match db::apply_poll_result(&pool, manhwa.id, &series, &manhwa.pub_status).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("  ✗ {}: {e}", manhwa.title);
                continue;
            }
        };

        if new_count > 0 {
            eprintln!("  + {} — {} new chapter(s)", manhwa.title, new_count);
        } else {
            eprintln!("  ✓ {} — up to date", manhwa.title);
        }
    }

    if !notifier::can_push(&config.notifications) {
        eprintln!("\nmrm: no ntfy sink in this build/config — leaving chapters for the notifying host");
    } else {
        let claimed = db::claim_unnotified(&pool).await?;
        if !claimed.is_empty() && config.notifications.enabled {
            let titles: Vec<String> = claimed.into_iter().map(|(t, _)| t).collect();
            notifier::send_grouped(&titles);
            notifier::send_grouped_remote(&config.notifications, &titles).await;
            eprintln!("\nmrm: notified for {} title(s)", titles.len());
        } else {
            eprintln!("\nmrm: nothing new to notify");
        }
    }

    // Discovery pass (once-mode always runs it to make manual triggering easy;
    // the TUI/daemon paths gate it to once per 23h via discovery_meta).
    eprintln!("mrm: discovery pass...");
    let mut new_disc = 0usize;
    for (name, scraper) in &registry {
        let entries = match scraper.latest_chapters().await {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  discovery '{name}' failed: {e}");
                continue;
            }
        };
        for entry in entries {
            if let Ok(true) = db::upsert_discovery(
                &pool,
                name,
                &entry.source_url,
                &entry.title,
                entry.cover_url.as_deref(),
                entry.chapter_number,
                entry.released_at.as_deref(),
            ).await {
                new_disc += 1;
            }
        }
    }
    eprintln!("mrm: {new_disc} new discoveries");

    Ok(())
}

// ---------------------------------------------------------------------------
// TUI mode (default)
// ---------------------------------------------------------------------------

/// Put the terminal back into cooked mode on the main screen with a visible
/// cursor. Idempotent and infallible: safe to call from a panic hook, a
/// `Drop`, or twice in a row.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
}

/// Restores the terminal when dropped, so early returns (`?`) and unwinding
/// panics never strand the shell in raw mode on the alternate screen.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Install a panic hook that restores the terminal first (so the message is
/// readable on the main screen) and then runs the previously installed hook.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

async fn run_tui(pool: sqlx::PgPool, config: config::Config) -> Result<()> {
    startup_cleanup_tmp();

    let (scraper_tx, scraper_rx) = mpsc::channel::<ScraperEvent>(32);
    let (app_tx, app_rx) = mpsc::unbounded_channel::<AppEvent>();
    let shutdown = CancellationToken::new();

    let picker = {
        let p = ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
        use std::io::Write;
        let _ = std::io::stdout().flush();
        Some(p)
    };

    let coordinator_config = config.clone();
    let mut app = match App::new(pool.clone(), picker, config, app_tx).await {
        Ok(a) => a,
        Err(e) => { eprintln!("mrm: app init error: {e}"); return Err(e); }
    };

    // The coordinator shares the App's registry and poll-now handle.
    let coordinator_handle = tokio::spawn(scraper::coordinator_task(
        pool,
        coordinator_config,
        app.registry.clone(),
        shutdown.clone(),
        app.poll_now.clone(),
        scraper_tx,
        true,
    ));

    // Spawn background cover image preload
    {
        let preload_list: Vec<(i64, Option<String>)> = app.manhwa_list.iter()
            .map(|m| (m.id, m.cover_url.clone()))
            .collect();
        tokio::spawn(cover_cache::preload_covers(
            app.cover_cache.cache_dir().clone(),
            preload_list,
        ));
    }

    // Load any pending discoveries from the previous session so the Discover
    // screen is populated on first open without waiting for a fresh poll.
    if let Err(e) = app.refresh_discoveries().await {
        eprintln!("mrm: initial discovery load failed: {e}");
    }

    // Set up terminal
    install_panic_hook();
    enable_raw_mode()?;
    let guard = TerminalGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_loop(&mut terminal, &mut app, scraper_rx, app_rx).await;

    // Restore the terminal before anything else, even on error.
    drop(guard);

    scraper::stop_coordinator(&shutdown, coordinator_handle, COORDINATOR_STOP_GRACE).await;

    result
}

/// Run one event through the app. A handler error (DB blip, scraper failure)
/// is shown in the status bar instead of ending the session.
async fn dispatch(app: &mut App, event: AppEvent) {
    if let Err(e) = app.handle_event(event).await {
        app.set_msg(format!("error: {e:#}"));
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    mut scraper_rx: mpsc::Receiver<ScraperEvent>,
    mut app_rx: mpsc::UnboundedReceiver<AppEvent>,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    loop {
        terminal.draw(|f| ui::draw(f, &mut *app))?;

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        dispatch(app, AppEvent::Key(key)).await;
                    }
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                }
            }

            msg = scraper_rx.recv() => {
                if let Some(ev) = msg {
                    dispatch(app, AppEvent::ScraperMsg(ev)).await;
                }
            }

            msg = app_rx.recv() => {
                if let Some(ev) = msg {
                    dispatch(app, ev).await;
                }
            }

            _ = tick.tick() => {
                dispatch(app, AppEvent::Tick).await;
                app.expire_msg(std::time::Instant::now());
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn panic_hook_chains_to_previous_hook() {
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        std::panic::set_hook(Box::new(move |_| c.store(true, Ordering::SeqCst)));
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("boom"));
        let _ = std::panic::take_hook(); // back to default
        assert!(called.load(Ordering::SeqCst), "previous hook was not called");
    }

    #[test]
    fn restore_terminal_is_idempotent_without_a_tty() {
        restore_terminal();
        restore_terminal();
    }
}
