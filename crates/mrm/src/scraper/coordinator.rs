//! Background scraper coordinator — Rust replacement for scraper/daemon.py.
//!
//! Spawned from main() as a tokio task. Shares the SqlitePool with the TUI.
//! Communicates new-chapter events to the TUI via an mpsc Sender<ScraperEvent>.
//! Shuts down cleanly when the provided CancellationToken is cancelled.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db;
use crate::notifier;
use crate::scraper::{AsuraScraper, MangaDexScraper, MangackScraper, Scraper};

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// Events the coordinator sends to the TUI event loop.
#[derive(Debug)]
pub enum ScraperEvent {
    /// One or more manhwa have new chapters. TUI should refresh the library list.
    NewChapters { titles: Vec<String> },
    /// Discovery pass found N new unknown manhwa across enabled sources.
    NewDiscoveries { count: usize },
}

/// Minimum interval between discovery polls. The chapter poll fires much more
/// often; we gate discovery to once per day so we don't hammer source homepages.
const DISCOVERY_MIN_INTERVAL_HOURS: i64 = 23;
const DISCOVERY_META_KEY: &str = "last_discovery_poll_at";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the scraper coordinator until the CancellationToken is cancelled.
///
/// Arguments:
///   pool        — shared SqlitePool (WAL mode, safe for concurrent reads)
///   config      — loaded at startup; poll_interval_minutes controls cadence
///   shutdown    — cancelled by main() on Ctrl-C / SIGTERM
///   scraper_tx  — sends ScraperEvent to the TUI run_loop
///   quiet       — when true, suppress stderr progress logs (TUI alternate-screen
///                 would otherwise get corrupted by mid-render writes)
pub async fn coordinator_task(
    pool: SqlitePool,
    config: Config,
    shutdown: CancellationToken,
    scraper_tx: mpsc::Sender<ScraperEvent>,
    quiet: bool,
) {
    let interval_secs = config.notifications.poll_interval_minutes * 60;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so the first real poll happens after the interval.
    // This prevents spurious notifications on every app startup.
    interval.tick().await;

    // Build the scraper registry once — reuse reqwest::Client across all polls.
    let registry = build_registry(&config, quiet);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            _ = interval.tick() => {
                let _ = poll_all(&pool, &config, &registry, &scraper_tx, quiet).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn build_registry(config: &Config, quiet: bool) -> HashMap<&'static str, Box<dyn Scraper>> {
    let mut registry: HashMap<&'static str, Box<dyn Scraper>> = HashMap::new();

    for (name, source_cfg) in &config.sources {
        if !source_cfg.enabled {
            continue;
        }
        match name.as_str() {
            "mangadex" => { registry.insert("mangadex", Box::new(MangaDexScraper::new())); }
            "mangack"  => { registry.insert("mangack",  Box::new(MangackScraper::new())); }
            "asura"    => {
                let dir = source_cfg.scraper_dir.as_deref().unwrap_or(".").into();
                registry.insert("asura", Box::new(AsuraScraper::new(dir)));
            }
            other => {
                if !quiet {
                    eprintln!("mrm: unknown source '{other}' in config, skipping");
                }
            }
        }
    }

    registry
}

// ---------------------------------------------------------------------------
// Poll cycle
// ---------------------------------------------------------------------------

async fn poll_all(
    pool: &SqlitePool,
    config: &Config,
    registry: &HashMap<&'static str, Box<dyn Scraper>>,
    scraper_tx: &mpsc::Sender<ScraperEvent>,
    quiet: bool,
) -> Result<()> {
    macro_rules! log {
        ($($arg:tt)*) => { if !quiet { eprintln!($($arg)*); } };
    }

    log!("mrm: poll started");

    let manhwa_list = db::fetch_all_manhwa(pool).await?;
    log!("mrm: checking {} manhwa", manhwa_list.len());

    let mut updated_titles: Vec<String> = Vec::new();

    for manhwa in &manhwa_list {
        let scraper = match registry.get(manhwa.source.as_str()) {
            Some(s) => s,
            None => {
                log!("mrm: no scraper for source '{}', skipping '{}'",
                     manhwa.source, manhwa.title);
                continue;
            }
        };

        // Fetch latest series data (scraper has built-in retry)
        let series = match scraper.get_series(&manhwa.source_url).await {
            Ok(s) => s,
            Err(e) => {
                log!("mrm: failed to fetch '{}': {e}", manhwa.title);
                continue;
            }
        };

        // Upsert chapters; returns count of chapters new to the user
        let new_count = match db::upsert_chapters(pool, manhwa.id, &series.chapters).await {
            Ok(n) => n,
            Err(e) => {
                log!("mrm: upsert error for '{}': {e}", manhwa.title);
                continue;
            }
        };

        // Only recompute status when something the formula depends on actually
        // changed: new chapters arrived or the publication status shifted.
        // Polling-driven recomputes on unchanged inputs caused status flips
        // that looked random to the user.
        let pub_changed = series.pub_status != manhwa.pub_status;
        if pub_changed {
            if let Err(e) = db::update_pub_status(pool, manhwa.id, &series.pub_status).await {
                log!("mrm: update_pub_status error for '{}': {e}", manhwa.title);
            }
        }
        if new_count > 0 || pub_changed {
            if let Err(e) = db::recompute_status(pool, manhwa.id).await {
                log!("mrm: recompute_status error for '{}': {e}", manhwa.title);
            }
        }

        if new_count > 0 {
            log!("mrm: '{}' +{} new chapter(s)", manhwa.title, new_count);
            updated_titles.push(manhwa.title.clone());
        }
    }

    // Notify and signal TUI only if there were updates
    if !updated_titles.is_empty() {
        if config.notifications.enabled {
            notifier::send_grouped(&updated_titles);
        }
        // Signal TUI to refresh library list; ignore send error (TUI may be exiting)
        let _ = scraper_tx.send(ScraperEvent::NewChapters {
            titles: updated_titles,
        }).await;
    }

    // Piggyback the discovery pass on the same tick, but gated so it runs at
    // most once a day regardless of how often the coordinator wakes up.
    if should_run_discovery(pool).await {
        match run_discovery(pool, registry, quiet).await {
            Ok(count) if count > 0 => {
                let _ = scraper_tx.send(ScraperEvent::NewDiscoveries { count }).await;
            }
            Ok(_) => {}
            Err(e) => {
                log!("mrm: discovery error: {e}");
            }
        }
    }

    log!("mrm: poll complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

async fn should_run_discovery(pool: &SqlitePool) -> bool {
    let last = match db::get_discovery_meta(pool, DISCOVERY_META_KEY).await {
        Ok(v) => v,
        Err(_) => return true,
    };
    let last: Option<DateTime<Utc>> = last
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc));
    match last {
        Some(t) => (Utc::now() - t).num_hours() >= DISCOVERY_MIN_INTERVAL_HOURS,
        None    => true,
    }
}

/// Walk every enabled scraper, call latest_chapters(), and upsert any entries
/// not already in the library into discovered_manhwa. Returns how many new
/// rows were inserted (existing + dismissed entries don't count).
async fn run_discovery(
    pool: &SqlitePool,
    registry: &HashMap<&'static str, Box<dyn Scraper>>,
    quiet: bool,
) -> Result<usize> {
    macro_rules! log {
        ($($arg:tt)*) => { if !quiet { eprintln!($($arg)*); } };
    }
    log!("mrm: discovery started");

    let mut new_count = 0usize;
    for (name, scraper) in registry {
        let entries = match scraper.latest_chapters().await {
            Ok(e) => e,
            Err(e) => {
                log!("mrm: discovery '{name}' failed: {e}");
                continue;
            }
        };
        if entries.is_empty() {
            continue;
        }
        for entry in entries {
            match db::upsert_discovery(
                pool,
                name,
                &entry.source_url,
                &entry.title,
                entry.cover_url.as_deref(),
                entry.chapter_number,
                entry.released_at.as_deref(),
            ).await {
                Ok(true)  => new_count += 1,
                Ok(false) => {}
                Err(e)    => log!("mrm: discovery upsert error: {e}"),
            }
        }
    }

    // Record the poll timestamp even if nothing new — otherwise we'd retry on
    // the very next coordinator tick.
    if let Err(e) = db::set_discovery_meta(
        pool,
        DISCOVERY_META_KEY,
        &Utc::now().to_rfc3339(),
    ).await {
        log!("mrm: discovery meta write failed: {e}");
    }

    log!("mrm: discovery complete: {new_count} new");
    Ok(new_count)
}
