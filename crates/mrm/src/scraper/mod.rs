//! Scraper trait and shared types for all source implementations.
//!
//! Build order:
//!   scraper/mod.rs (this file — trait + types + retry)
//!   → scraper/mangadex.rs   (Plan 02)
//!   → scraper/mangack.rs    (Phase 3)

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Shared data types
// ---------------------------------------------------------------------------

/// Full series metadata + chapter list returned by get_series().
#[derive(Debug, Clone)]
pub struct SeriesData {
    pub title:      String,
    pub cover_url:  Option<String>,
    pub source_url: String,
    pub pub_status: String,   // "ongoing" | "hiatus" | "completed"
    pub chapters:   Vec<ChapterData>,
}

/// Single chapter entry returned as part of SeriesData.chapters.
#[derive(Debug, Clone)]
pub struct ChapterData {
    pub number:      f64,
    pub title:       Option<String>,
    pub url:         String,
    pub released_at: Option<String>,  // "YYYY-MM-DD" or None
}

/// One result from a search() call.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title:      String,
    pub cover_url:  Option<String>,
    pub source_url: String,
    pub pub_status: String,
    pub source:     String,   // "mangadex" | "mangack"
}

/// One entry from a latest_chapters() feed — a candidate for the Discover
/// screen. We don't fetch the full series metadata here; just enough to show
/// the cover grid and, on add, hand the source_url to get_series().
#[derive(Debug, Clone)]
pub struct DiscoveryEntry {
    pub title:          String,
    pub cover_url:      Option<String>,
    pub source_url:     String,
    pub chapter_number: Option<f64>,
    pub released_at:    Option<String>,
}

// ---------------------------------------------------------------------------
// Scraper trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Scraper: Send + Sync {
    /// Unique source identifier (e.g. "mangadex").
    fn source_name(&self) -> &'static str;

    /// Search for a manhwa title. Returns up to 20 results.
    async fn search(&self, query: &str) -> Result<Vec<SearchResult>>;

    /// Fetch series metadata and full chapter list from source_url.
    async fn get_series(&self, source_url: &str) -> Result<SeriesData>;

    /// Fetch ordered image URLs for a chapter from chapter_url.
    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>>;

    /// Return the source's most recently updated series (latest-chapters feed).
    /// Used by the discovery coordinator to surface unknown manhwa to the user.
    /// Default is an empty vec for sources that don't implement it yet.
    async fn latest_chapters(&self) -> Result<Vec<DiscoveryEntry>> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Retry helper
// ---------------------------------------------------------------------------

/// Retry an async closure up to 3 times with exponential backoff: 1s, 2s, 4s.
///
/// Returns the first Ok result, or the last Err if all attempts fail.
///
/// # Example
/// ```rust,ignore
/// let resp = retry(|| client.get(url).send()).await?;
/// ```
pub async fn retry<F, Fut, T>(mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let delays = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)];
    let mut last_err = None;

    for (attempt, delay) in delays.iter().enumerate() {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                last_err = Some(e);
                if attempt < delays.len() - 1 {
                    tokio::time::sleep(*delay).await;
                }
            }
        }
    }

    Err(last_err.expect("delays is non-empty"))
}

// ---------------------------------------------------------------------------
// Submodule declarations (populated in later plans/phases)
// ---------------------------------------------------------------------------

pub mod mangadex;   // Plan 02
pub use mangadex::MangaDexScraper;

pub mod mangack;    // Phase 3 Plan 01
pub use mangack::MangackScraper;

pub mod asura;
pub use asura::AsuraScraper;

pub mod coordinator;  // Phase 5 Plan 02
pub use coordinator::{coordinator_task, ScraperEvent};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangadex_scraper_instantiates() {
        let scraper = MangaDexScraper::new();
        assert_eq!(scraper.source_name(), "mangadex");
    }

    #[test]
    fn mangack_scraper_instantiates() {
        let scraper = MangackScraper::new();
        assert_eq!(scraper.source_name(), "mangack");
    }
}
