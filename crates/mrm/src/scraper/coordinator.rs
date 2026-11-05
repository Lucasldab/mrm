//! Background scraper coordinator — Rust replacement for scraper/daemon.py.
//!
//! Spawned from main() as a tokio task. Shares the PgPool with the TUI.
//! Communicates new-chapter events to the TUI via an mpsc Sender<ScraperEvent>.
//! Shuts down cleanly when the provided CancellationToken is cancelled.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db;
use crate::notifier;
use crate::scraper::Registry;

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

/// Delay before the first poll after launch: soon enough that a fresh session
/// shows new chapters, late enough not to compete with startup work.
/// No spurious pushes result: only unclaimed chapters are ever announced.
pub const FIRST_POLL_DELAY: Duration = Duration::from_secs(10);

/// Run the scraper coordinator until the CancellationToken is cancelled.
///
/// Arguments:
///   pool        — shared PgPool
///   config      — loaded at startup; poll_interval_minutes controls cadence
///   registry    — the process's scraper registry (`scraper::build_registry`)
///   shutdown    — cancelled by main() on quit / Ctrl-C; an in-flight poll
///                 stops at its next network call
///   poll_now    — `notify_one()` starts a poll immediately (TUI "poll now" key)
///   scraper_tx  — sends ScraperEvent to the TUI run_loop
///   quiet       — when true, suppress stderr progress logs (TUI alternate-screen
///                 would otherwise get corrupted by mid-render writes)
pub async fn coordinator_task(
    pool: PgPool,
    config: Config,
    registry: Arc<Registry>,
    shutdown: CancellationToken,
    poll_now: Arc<Notify>,
    scraper_tx: mpsc::Sender<ScraperEvent>,
    quiet: bool,
) {
    let period = Duration::from_secs(config.notifications.poll_interval_minutes.max(1) * 60);
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + FIRST_POLL_DELAY,
        period,
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
            _ = poll_now.notified() => {
                // A manual poll restarts the cadence.
                interval.reset();
            }
        }
        // poll_all races every network call against `shutdown` itself and
        // returns at once when it fires. It is not dropped from out here, so a
        // claim already made still gets its push (within stop_coordinator's
        // grace period) instead of being cut off mid-request.
        let _ = poll_all(&pool, &config, &registry, &shutdown, &scraper_tx, quiet).await;
    }
}

/// Stop a spawned coordinator: cancel it, wait at most `grace`, then abort.
/// Aborting mid-poll is safe: each series' write is one transaction, and a
/// claim whose push is cut off counts as a failed push (at-most-once).
pub async fn stop_coordinator(
    shutdown: &CancellationToken,
    mut handle: tokio::task::JoinHandle<()>,
    grace: Duration,
) {
    shutdown.cancel();
    if tokio::time::timeout(grace, &mut handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}

// ---------------------------------------------------------------------------
// Poll cycle
// ---------------------------------------------------------------------------

async fn poll_all(
    pool: &PgPool,
    config: &Config,
    registry: &Registry,
    shutdown: &CancellationToken,
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

        // Fetch latest series data (scraper has built-in retry). Shutdown
        // wins over a slow source.
        let fetched = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                log!("mrm: poll interrupted by shutdown");
                return Ok(());
            }
            r = scraper.get_series(&manhwa.source_url) => r,
        };
        let series = match fetched {
            Ok(s) => s,
            Err(e) => {
                log!("mrm: failed to fetch '{}': {e}", manhwa.title);
                continue;
            }
        };

        let new_count = match db::apply_poll_result(pool, manhwa.id, &series, &manhwa.pub_status).await {
            Ok(n) => n,
            Err(e) => {
                log!("mrm: poll write error for '{}': {e}", manhwa.title);
                continue;
            }
        };
        if new_count > 0 {
            log!("mrm: '{}' +{} new chapter(s)", manhwa.title, new_count);
            updated_titles.push(manhwa.title.clone());
        }
    }

    if shutdown.is_cancelled() {
        return Ok(());
    }

    // Notifying pollers (daemon) announce whatever no poller has announced yet,
    // including chapters a laptop's quiet poller inserted first. The quiet TUI
    // poller never claims, so it cannot swallow the phone push.
    if !quiet && !notifier::can_push(&config.notifications) {
        log!("mrm: no ntfy sink in this build/config — leaving chapters for the notifying host");
    } else if !quiet {
        match db::claim_unnotified(pool).await {
            Ok(claimed) if !claimed.is_empty() && config.notifications.enabled => {
                let titles: Vec<String> = claimed.into_iter().map(|(t, _)| t).collect();
                notifier::send_grouped(&titles);
                notifier::send_grouped_remote(&config.notifications, &titles).await;
            }
            Ok(_) => {}
            Err(e) => log!("mrm: claim error: {e}"),
        }
    }
    if !updated_titles.is_empty() {
        // Signal TUI to refresh library list; ignore send error (TUI may be exiting)
        let _ = scraper_tx.send(ScraperEvent::NewChapters { titles: updated_titles }).await;
    }

    // Piggyback the discovery pass on the same tick, but gated so it runs at
    // most once a day regardless of how often the coordinator wakes up.
    if should_run_discovery(pool).await {
        let discovered = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            r = run_discovery(pool, registry, quiet) => r,
        };
        match discovered {
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

async fn should_run_discovery(pool: &PgPool) -> bool {
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
    pool: &PgPool,
    registry: &Registry,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{series, test_pool};
    use crate::scraper::{DiscoveryEntry, Scraper, SearchResult, SeriesData};
    use async_trait::async_trait;

    /// A source that never answers: `get_series` signals that it was called
    /// and then hangs, like a Cloudflare-blocked host mid-retry.
    struct HangingScraper(Arc<Notify>);

    #[async_trait]
    impl Scraper for HangingScraper {
        fn source_name(&self) -> &'static str { "hang" }
        async fn search(&self, _q: &str) -> Result<Vec<SearchResult>> { Ok(vec![]) }
        async fn get_series(&self, _url: &str) -> Result<SeriesData> {
            self.0.notify_one();
            std::future::pending().await
        }
        async fn get_chapter_image_urls(&self, _url: &str) -> Result<Vec<String>> { Ok(vec![]) }
        async fn latest_chapters(&self) -> Result<Vec<DiscoveryEntry>> { Ok(vec![]) }
    }

    fn test_config() -> Config {
        crate::config::parse_config(
            "[notifications]\nenabled = false\npoll_interval_minutes = 30\n[sources]\n",
        ).unwrap()
    }

    #[tokio::test]
    async fn poll_now_polls_immediately_and_shutdown_interrupts_the_poll() {
        let Some(pool) = test_pool().await else {
            eprintln!("skipped: MRM_TEST_DATABASE_URL unset");
            return;
        };
        db::insert_manhwa_with_chapters(&pool, &series(1, "https://x/hang"), "hang").await.unwrap();

        let entered = Arc::new(Notify::new());
        let mut reg: Registry = Registry::new();
        reg.insert("hang", Box::new(HangingScraper(entered.clone())));

        let shutdown = CancellationToken::new();
        let poll_now = Arc::new(Notify::new());
        let (tx, _rx) = mpsc::channel(4);
        let handle = tokio::spawn(coordinator_task(
            pool, test_config(), Arc::new(reg), shutdown.clone(), poll_now.clone(), tx, true,
        ));

        // Neither the 30-minute interval nor the first-poll delay has elapsed:
        // only the poll_now wake can start this poll.
        poll_now.notify_one();
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("poll_now did not start a poll");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("coordinator kept polling after shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn stop_coordinator_aborts_a_task_that_ignores_shutdown() {
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(std::future::pending::<()>());
        let t0 = std::time::Instant::now();
        stop_coordinator(&shutdown, handle, Duration::from_millis(100)).await;
        assert!(shutdown.is_cancelled());
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
