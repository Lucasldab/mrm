//! Background scraper coordinator — Rust replacement for scraper/daemon.py.
//!
//! Spawned from main() as a tokio task. Shares the SqlitePool with the TUI.
//! Communicates new-chapter events to the TUI via an mpsc Sender<ScraperEvent>.
//! Shuts down cleanly when the provided CancellationToken is cancelled.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db;
use crate::notifier;
use crate::scraper::{MangaDexScraper, MangackScraper, Scraper};

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// Events the coordinator sends to the TUI event loop.
#[derive(Debug)]
pub enum ScraperEvent {
    /// One or more manhwa have new chapters. TUI should refresh the library list.
    NewChapters { titles: Vec<String> },
}

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
pub async fn coordinator_task(
    pool: SqlitePool,
    config: Config,
    shutdown: CancellationToken,
    scraper_tx: mpsc::Sender<ScraperEvent>,
) {
    let interval_secs = config.notifications.poll_interval_minutes * 60;
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consume the immediate first tick so the first real poll happens after the interval.
    // This prevents spurious notifications on every app startup.
    interval.tick().await;

    // Build the scraper registry once — reuse reqwest::Client across all polls.
    let registry = build_registry(&config);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break;
            }
            _ = interval.tick() => {
                let _ = poll_all(&pool, &config, &registry, &scraper_tx).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

fn build_registry(config: &Config) -> HashMap<&'static str, Box<dyn Scraper>> {
    let mut registry: HashMap<&'static str, Box<dyn Scraper>> = HashMap::new();

    for (name, source_cfg) in &config.sources {
        if !source_cfg.enabled {
            continue;
        }
        match name.as_str() {
            "mangadex" => { registry.insert("mangadex", Box::new(MangaDexScraper::new())); }
            "mangack"  => { registry.insert("mangack",  Box::new(MangackScraper::new())); }
            other => {
                eprintln!("mrm: unknown source '{other}' in config, skipping");
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
) -> Result<()> {
    eprintln!("mrm: poll started");

    let manhwa_list = db::fetch_all_manhwa(pool).await?;
    eprintln!("mrm: checking {} manhwa", manhwa_list.len());

    let mut updated_titles: Vec<String> = Vec::new();

    for manhwa in &manhwa_list {
        let scraper = match registry.get(manhwa.source.as_str()) {
            Some(s) => s,
            None => {
                eprintln!("mrm: no scraper for source '{}', skipping '{}'",
                          manhwa.source, manhwa.title);
                continue;
            }
        };

        // Fetch latest series data (scraper has built-in retry)
        let series = match scraper.get_series(&manhwa.source_url).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mrm: failed to fetch '{}': {e}", manhwa.title);
                continue;
            }
        };

        // Upsert chapters; returns count of chapters new to the user
        let new_count = match db::upsert_chapters(pool, manhwa.id, &series.chapters).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("mrm: upsert error for '{}': {e}", manhwa.title);
                continue;
            }
        };

        // Recompute status regardless (pub_status may have changed)
        if let Err(e) = db::recompute_status(pool, manhwa.id).await {
            eprintln!("mrm: recompute_status error for '{}': {e}", manhwa.title);
        }

        if new_count > 0 {
            eprintln!("mrm: '{}' +{} new chapter(s)", manhwa.title, new_count);
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

    eprintln!("mrm: poll complete");
    Ok(())
}
