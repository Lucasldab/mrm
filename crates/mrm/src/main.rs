mod app;
mod config;
mod cover_cache;
mod db;
mod notifier;
mod scraper;
mod types;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use app::App;
use scraper::ScraperEvent;
use types::AppEvent;

// ---------------------------------------------------------------------------
// DB path resolution
// ---------------------------------------------------------------------------

fn db_path() -> String {
    // Check config.toml first for an explicit path
    let config_path = config_file_path();
    let config_dir = config_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if let Ok(contents) = std::fs::read_to_string(&config_path) {
        if let Ok(config) = toml::from_str::<toml::Value>(&contents) {
            if let Some(path) = config.get("db").and_then(|d| d.get("path")).and_then(|p| p.as_str()) {
                if !path.is_empty() {
                    let db = PathBuf::from(path);
                    // Resolve relative paths against the config file's directory
                    let resolved = if db.is_absolute() { db } else { config_dir.join(&db) };
                    if resolved.exists() {
                        return resolved.to_string_lossy().into_owned();
                    }
                }
            }
        }
    }

    // Search common locations
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        PathBuf::from("mrm.db"),                                          // CWD
        PathBuf::from(&home).join(".config/mrm/mrm.db"),                  // XDG config
        PathBuf::from(&home).join(".local/share/mrm/mrm.db"),             // XDG data
        PathBuf::from("../../mrm.db"),                                    // dev: from crates/mrm/
    ];
    for p in &candidates {
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }

    // Default: create in XDG config dir
    let default_dir = PathBuf::from(&home).join(".config/mrm");
    let _ = std::fs::create_dir_all(&default_dir);
    default_dir.join("mrm.db").to_string_lossy().into_owned()
}

/// Find config.toml: CWD first, then ~/.config/mrm/
fn config_file_path() -> PathBuf {
    let local = PathBuf::from("config.toml");
    if local.exists() {
        return local;
    }
    if let Ok(home) = std::env::var("HOME") {
        let xdg = PathBuf::from(home).join(".config/mrm/config.toml");
        if xdg.exists() {
            return xdg;
        }
    }
    local // fallback to CWD (will fail gracefully)
}

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

    let db_path = db_path();
    let pool = match db::open_db(&db_path).await {
        Ok(p) => p,
        Err(e) => { eprintln!("mrm: DB error: {e}"); return Err(e); }
    };

    let config = config::load_config()
        .context("Config required — create config.toml (see README)");

    match mode {
        Mode::Daemon => run_daemon(pool, config?).await,
        Mode::Once   => run_once(pool, config?).await,
        Mode::Tui    => run_tui(pool, config.ok()).await,
    }
}

// ---------------------------------------------------------------------------
// Daemon mode: poll forever, send notifications, no TUI
// ---------------------------------------------------------------------------

async fn run_daemon(pool: sqlx::SqlitePool, config: config::Config) -> Result<()> {
    eprintln!("mrm: daemon mode — polling every {} minutes", config.notifications.poll_interval_minutes);
    eprintln!("mrm: press Ctrl-C to stop");

    let shutdown = CancellationToken::new();
    let shutdown_c = shutdown.clone();

    // Handle Ctrl-C gracefully
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("\nmrm: shutting down...");
        shutdown_c.cancel();
    });

    let (_tx, mut _rx) = mpsc::channel::<ScraperEvent>(32);

    // Run coordinator directly (not as a spawned task — this IS the main task)
    scraper::coordinator_task(pool, config, shutdown, _tx, false).await;

    eprintln!("mrm: daemon stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Once mode: single poll, then exit
// ---------------------------------------------------------------------------

async fn run_once(pool: sqlx::SqlitePool, config: config::Config) -> Result<()> {
    eprintln!("mrm: single poll...");

    let (_tx, mut _rx) = mpsc::channel::<ScraperEvent>(32);
    let _shutdown = CancellationToken::new();

    // Build scraper registry
    let registry = build_registry_for_once(&config);

    let manhwa_list = db::fetch_all_manhwa(&pool).await?;
    eprintln!("mrm: checking {} manhwa", manhwa_list.len());

    let mut updated: Vec<String> = Vec::new();

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

        let new_count = db::upsert_chapters(&pool, manhwa.id, &series.chapters).await?;
        let _ = db::recompute_status(&pool, manhwa.id).await;

        if new_count > 0 {
            eprintln!("  + {} — {} new chapter(s)", manhwa.title, new_count);
            updated.push(manhwa.title.clone());
        } else {
            eprintln!("  ✓ {} — up to date", manhwa.title);
        }
    }

    if !updated.is_empty() && config.notifications.enabled {
        notifier::send_grouped(&updated);
        eprintln!("\nmrm: notified for {} title(s)", updated.len());
    } else {
        eprintln!("\nmrm: no new chapters");
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

fn build_registry_for_once(config: &config::Config) -> std::collections::HashMap<&'static str, Box<dyn scraper::Scraper>> {
    use scraper::{AsuraScraper, MangaDexScraper, MangackScraper};
    let mut registry: std::collections::HashMap<&'static str, Box<dyn scraper::Scraper>> = std::collections::HashMap::new();

    for (name, source_cfg) in &config.sources {
        if !source_cfg.enabled { continue; }
        match name.as_str() {
            "mangadex" => { registry.insert("mangadex", Box::new(MangaDexScraper::new())); }
            "mangack"  => { registry.insert("mangack",  Box::new(MangackScraper::new())); }
            "asura"    => {
                let dir = source_cfg.scraper_dir.as_deref().unwrap_or(".").into();
                registry.insert("asura", Box::new(AsuraScraper::new(dir)));
            }
            _ => {}
        }
    }

    registry
}

// ---------------------------------------------------------------------------
// TUI mode (default)
// ---------------------------------------------------------------------------

async fn run_tui(pool: sqlx::SqlitePool, config_opt: Option<config::Config>) -> Result<()> {
    startup_cleanup_tmp();

    let (scraper_tx, scraper_rx) = mpsc::channel::<ScraperEvent>(32);
    let shutdown = CancellationToken::new();

    let config_for_app = config_opt.clone().expect("config.toml required");
    let coordinator_handle = config_opt.map(|cfg| {
        let pool_c     = pool.clone();
        let shutdown_c = shutdown.clone();
        let tx_c       = scraper_tx.clone();
        tokio::spawn(scraper::coordinator_task(pool_c, cfg, shutdown_c, tx_c, true))
    });

    let picker = {
        let p = ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
        use std::io::Write;
        let _ = std::io::stdout().flush();
        Some(p)
    };

    let mut app = match App::new(pool, picker, config_for_app).await {
        Ok(a) => a,
        Err(e) => { eprintln!("mrm: app init error: {e}"); return Err(e); }
    };

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
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_loop(&mut terminal, &mut app, scraper_rx).await;

    // Always restore terminal, even on error
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    shutdown.cancel();
    if let Some(handle) = coordinator_handle {
        let _ = handle.await;
    }

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    mut scraper_rx: mpsc::Receiver<ScraperEvent>,
) -> Result<()> {
    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut msg_timer: u8 = 0;

    loop {
        terminal.draw(|f| ui::draw(f, &mut *app))?;

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.handle_event(AppEvent::Key(key)).await?;
                    }
                    Some(Err(e)) => return Err(e.into()),
                    _ => {}
                }
            }

            msg = scraper_rx.recv() => {
                if let Some(ev) = msg {
                    app.handle_event(AppEvent::ScraperMsg(ev)).await?;
                }
            }

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
