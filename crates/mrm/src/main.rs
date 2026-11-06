mod app;
mod config;
mod cover_cache;
mod db;
mod import;
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
    Import { path: String, dry_run: bool },
}

fn parse_args() -> Mode {
    let args: Vec<String> = std::env::args().collect();
    for arg in &args[1..] {
        match arg.as_str() {
            "--daemon" | "-d" => return Mode::Daemon,
            "--once"          => return Mode::Once,
            "--import"        => {
                let rest = &args[2..];
                let Some(path) = rest.iter().find(|a| !a.starts_with("--")) else {
                    eprintln!("mrm: --import needs a file");
                    std::process::exit(2);
                };
                let dry_run = rest.iter().any(|a| a == "--dry-run");
                return Mode::Import { path: path.clone(), dry_run };
            }
            "--help" | "-h"   => {
                eprintln!("Usage: mrm [OPTIONS]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --daemon, -d  Run as background poller (no TUI)");
                eprintln!("  --once        Poll once and exit");
                eprintln!("  --import FILE [--dry-run]");
                eprintln!("                Add series by title, one per line: `title<TAB>read through chapter`");
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
        Mode::Import { path, dry_run } => {
            let registry = scraper::build_registry(&config);
            import::run(&pool, &registry, &path, dry_run).await
        }
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

    let mut streaks = scraper::coordinator::SourceStreaks::default();
    for manhwa in &manhwa_list {
        let scraper = match registry.get(manhwa.source.as_str()) {
            Some(s) => s,
            None => continue,
        };
        if streaks.skip(&manhwa.source) {
            eprintln!("  - {}: skipped, {} keeps failing", manhwa.title, manhwa.source);
            continue;
        }

        let series = match scraper.get_series(&manhwa.source_url).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  ✗ {}: {e:#}", manhwa.title);
                streaks.failed(&manhwa.source);
                if let Err(db_err) = db::record_poll_failure(&pool, manhwa.id, &format!("{e:#}")).await {
                    eprintln!("    (could not record the failure: {db_err})");
                }
                continue;
            }
        };
        streaks.succeeded(&manhwa.source);

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

/// Install a panic hook for the process. The hook is process-wide and runs on
/// whichever thread panicked, before tokio catches the unwind, so only a panic
/// on the installing (UI) thread may touch the terminal: it restores it and
/// chains to the previous hook so the message is readable on the main screen.
/// A panic on any other thread (a spawned task, `spawn_blocking`) is caught by
/// tokio and the UI keeps running, so it is appended to `panic.log` instead.
fn install_panic_hook() {
    install_panic_hook_with(restore_terminal, app::append_panic_log);
}

fn install_panic_hook_with(restore: fn(), log: fn(&str)) {
    let ui_thread = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            restore();
            previous(info);
        } else {
            let thread = std::thread::current();
            log(&format!(
                "thread '{}' {info}",
                thread.name().unwrap_or("<unnamed>"),
            ));
        }
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

    // The panic hook is process-global; tests that swap it must not overlap.
    static HOOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static RESTORES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static LOGGED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn count_restore() { RESTORES.fetch_add(1, Ordering::SeqCst); }
    fn record_log(s: &str) { LOGGED.lock().unwrap_or_else(|e| e.into_inner()).push(s.to_string()); }
    /// Entries mentioning `needle` (other tests' panics may be logged too).
    fn logged_with(needle: &str) -> Vec<String> {
        LOGGED.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|l| l.contains(needle)).cloned().collect()
    }

    #[test]
    fn panic_hook_restores_and_chains_on_the_ui_thread_only() {
        let _g = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        RESTORES.store(0, Ordering::SeqCst);
        LOGGED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        std::panic::set_hook(Box::new(move |_| c.store(true, Ordering::SeqCst)));
        install_panic_hook_with(count_restore, record_log);

        // UI thread: restore + chain, nothing logged.
        let _ = std::panic::catch_unwind(|| panic!("ui boom"));
        assert_eq!(RESTORES.load(Ordering::SeqCst), 1);
        assert!(called.load(Ordering::SeqCst), "previous hook was not called");
        assert!(logged_with("ui boom").is_empty());

        // Another thread: logged, no restore, no chaining.
        called.store(false, Ordering::SeqCst);
        std::thread::Builder::new().name("worker-x".into())
            .spawn(|| panic!("task boom")).unwrap().join().unwrap_err();
        assert_eq!(RESTORES.load(Ordering::SeqCst), 1, "off-thread panic restored the terminal");
        assert!(!called.load(Ordering::SeqCst), "off-thread panic reached the stderr hook");
        let logged = logged_with("task boom");
        assert_eq!(logged.len(), 1);
        assert!(logged[0].contains("task boom") && logged[0].contains("worker-x"), "{logged:?}");
        assert!(logged[0].contains("main.rs"), "location missing: {logged:?}");
        let _ = std::panic::take_hook();
    }

    #[test]
    fn panic_in_a_tokio_task_does_not_restore_the_terminal() {
        let _g = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        RESTORES.store(0, Ordering::SeqCst);
        LOGGED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_hook_with(count_restore, record_log);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let r = rt.block_on(async { tokio::spawn(async { panic!("page task") }).await });
        assert!(r.unwrap_err().is_panic());
        let r = rt.block_on(async { tokio::task::spawn_blocking(|| panic!("image work")).await });
        assert!(r.unwrap_err().is_panic());
        let _ = std::panic::take_hook();
        assert_eq!(RESTORES.load(Ordering::SeqCst), 0, "task panic restored the terminal");
        assert_eq!(logged_with("page task").len() + logged_with("image work").len(), 2);
    }

    #[test]
    fn restore_terminal_is_idempotent_without_a_tty() {
        restore_terminal();
        restore_terminal();
    }
}
