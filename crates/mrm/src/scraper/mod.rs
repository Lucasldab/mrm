//! Scraper trait and shared types for all source implementations.
//!
//! Build order:
//!   scraper/mod.rs (this file — trait + types + retry)
//!   → scraper/mangadex.rs   (Plan 02)
//!   → scraper/mangack.rs    (Phase 3)

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::Duration;

use crate::config::Config;

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
    pub description: Option<String>,
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
    #[allow(dead_code)]
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
// Registry
// ---------------------------------------------------------------------------

/// Enabled scrapers keyed by source name. Built once per process by
/// [`build_registry`] and shared (behind an `Arc`) by the coordinator, the
/// TUI's search/add/refresh tasks and the image fetcher.
pub type Registry = HashMap<&'static str, Box<dyn Scraper>>;

/// The one place scrapers are constructed. Sources with `enabled = false` and
/// unknown names are left out, so they are never contacted.
pub fn build_registry(config: &Config) -> Registry {
    let mut registry: Registry = HashMap::new();
    for (name, source_cfg) in &config.sources {
        if !source_cfg.enabled {
            continue;
        }
        match name.as_str() {
            "mangadex" => { registry.insert("mangadex", Box::new(MangaDexScraper::new())); }
            "mangack"  => { registry.insert("mangack",  Box::new(MangackScraper::new())); }
            "asura"    => { registry.insert("asura",    Box::new(AsuraScraper::new())); }
            _ => {}
        }
    }
    registry
}

/// Enabled source names in the config that no scraper implements (for a
/// startup warning in the non-TUI modes).
pub fn unknown_sources(config: &Config) -> Vec<String> {
    let mut v: Vec<String> = config.sources.iter()
        .filter(|(n, c)| c.enabled && !matches!(n.as_str(), "mangadex" | "mangack" | "asura"))
        .map(|(n, _)| n.clone())
        .collect();
    v.sort();
    v
}

/// Fixed display order for merged search results (HashMap order is random).
pub fn source_rank(name: &str) -> u8 {
    match name {
        "mangadex" => 0,
        "mangack"  => 1,
        "asura"    => 2,
        _          => 3,
    }
}

/// Human-readable source name for error messages.
pub fn source_label(name: &str) -> &str {
    match name {
        "mangadex" => "MangaDex",
        "mangack"  => "MangaCK",
        "asura"    => "Asura",
        other      => other,
    }
}

// ---------------------------------------------------------------------------
// HTML image helper
// ---------------------------------------------------------------------------

/// The real URL of an `<img>`: lazy-load attributes (`data-src`,
/// `data-lazy-src`, first `srcset` candidate) win over `src`, which lazy-load
/// themes fill with a `data:` placeholder. `data:` URIs and blanks are skipped.
pub(crate) fn img_url(img: &scraper::ElementRef<'_>) -> Option<String> {
    let el = img.value();
    let srcset_first = el.attr("srcset")
        .and_then(|s| s.split(',').next())
        .and_then(|c| c.split_whitespace().next());
    [el.attr("data-src"), el.attr("data-lazy-src"), srcset_first, el.attr("src")]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|u| !u.is_empty() && !u.starts_with("data:"))
        .map(str::to_string)
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
pub use coordinator::{coordinator_task, stop_coordinator, ScraperEvent};

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

    fn config_with(sources: &str) -> crate::config::Config {
        crate::config::parse_config(&format!(
            "[notifications]\nenabled = false\npoll_interval_minutes = 30\n[sources]\n{sources}"
        )).unwrap()
    }

    #[test]
    fn registry_excludes_a_disabled_source() {
        let cfg = config_with(
            "[sources.mangadex]\nbase_url = \"x\"\nenabled = true\n\
             [sources.mangack]\nbase_url = \"x\"\nenabled = false\n",
        );
        let reg = build_registry(&cfg);
        assert!(reg.contains_key("mangadex"));
        assert!(!reg.contains_key("mangack"), "disabled source must not be in the registry");
        assert!(!reg.contains_key("asura"), "unconfigured source must not be in the registry");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn registry_skips_unknown_sources() {
        let cfg = config_with("[sources.nope]\nbase_url = \"x\"\nenabled = true\n");
        assert!(build_registry(&cfg).is_empty());
        assert_eq!(unknown_sources(&cfg), vec!["nope".to_string()]);
    }

    #[test]
    fn search_order_is_stable() {
        let mut names = vec!["asura", "zzz", "mangack", "mangadex"];
        names.sort_by_key(|n| source_rank(n));
        assert_eq!(names, vec!["mangadex", "mangack", "asura", "zzz"]);
    }

    fn first_img(html: &str) -> Option<String> {
        let doc = scraper::Html::parse_fragment(html);
        let sel = scraper::Selector::parse("img").unwrap();
        let el = doc.select(&sel).next().unwrap();
        img_url(&el)
    }

    #[test]
    fn img_url_prefers_lazy_attributes_over_placeholder_src() {
        assert_eq!(
            first_img(r#"<img src="data:image/svg+xml;base64,AAA" data-src="https://c/1.jpg">"#),
            Some("https://c/1.jpg".into()),
        );
        assert_eq!(
            first_img(r#"<img src="https://c/placeholder.gif" data-lazy-src="https://c/2.jpg">"#),
            Some("https://c/2.jpg".into()),
        );
        assert_eq!(
            first_img(r#"<img src="data:," srcset="https://c/3.webp 1x, https://c/3@2x.webp 2x">"#),
            Some("https://c/3.webp".into()),
        );
        assert_eq!(first_img(r#"<img src="https://c/4.png">"#), Some("https://c/4.png".into()));
        assert_eq!(first_img(r#"<img src="data:image/gif;base64,R0" data-src=" ">"#), None);
    }

    #[test]
    fn mangack_scraper_instantiates() {
        let scraper = MangackScraper::new();
        assert_eq!(scraper.source_name(), "mangack");
    }
}
