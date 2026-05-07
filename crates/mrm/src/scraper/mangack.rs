//! MangaCK scraper — full implementation of the Scraper trait.
//!
//! Ports the Python `scraper/scrapers/mangack.py` to Rust faithfully,
//! replicating CSS selector logic, relative date parsing, chapter number
//! extraction with regex fallback, nav button filtering, and image URL filtering.
//!
//! Site structure (WordPress theme):
//!   Series URL  : {base}/manga/{slug}/
//!   Chapter URL : {base}/chapter/{slug}-chapter-{n}/
//!   Search URL  : {base}/?s={query}

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::Datelike;
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::time::sleep;

use super::{ChapterData, DiscoveryEntry, Scraper, SearchResult, SeriesData};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BASE: &str = "https://mangack.com";
const DELAY: Duration = Duration::from_millis(500);

// Nav button labels to skip (not real chapters)
static NAV_LABELS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s = HashSet::new();
    s.insert("first chapter");
    s.insert("last chapter");
    s.insert("first");
    s.insert("last");
    s
});

// Compiled selectors (lazy, compiled once)
static SEL_A_MANGA: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"a[href*="/manga/"]"#).unwrap());
static SEL_IMG: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("img").unwrap());
static SEL_H1: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("h1").unwrap());
static SEL_IMG_COVER: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"img[src*="wp-content/uploads"]"#).unwrap());
static SEL_TD_TH: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td, th").unwrap());
static SEL_A_CHAPTER: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"a[href*="/chapter/"]"#).unwrap());
static SEL_DESCRIPTION: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(
        r#".summary__content, .description-summary, .summary, .post-content_item .summary-content, [itemprop="description"]"#,
    ).unwrap());

// Compiled regex patterns (lazy, compiled once)
static RE_NEW_BADGE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\s*NEW\s*$").unwrap());
static RE_CHAPTER_NUM: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)chapter[-\s]*([\d]+(?:[._][\d]+)?)").unwrap());
static RE_MINUTE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\d+)\s+minute").unwrap());
static RE_HOUR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\d+)\s+hour").unwrap());
static RE_DAY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\d+)\s+day").unwrap());
static RE_WEEK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\d+)\s+week").unwrap());
static RE_MONTH: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\d+)\s+month").unwrap());
static RE_YEAR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(\d+)\s+year").unwrap());

// ---------------------------------------------------------------------------
// Status normalisation
// ---------------------------------------------------------------------------

fn normalize_status(raw: &str) -> String {
    match raw.to_lowercase().trim() {
        "ongoing"              => "ongoing",
        "hiatus"               => "hiatus",
        "completed" | "complete" => "completed",
        _                      => "ongoing",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Struct + constructor
// ---------------------------------------------------------------------------

pub struct MangackScraper {
    client: reqwest::Client,
}

impl MangackScraper {
    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "Accept-Language",
            "en-US,en;q=0.9".parse().unwrap(),
        );

        Self {
            client: reqwest::Client::builder()
                .user_agent(
                    "Mozilla/5.0 (X11; Linux x86_64) \
                     AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/124.0.0.0 Safari/537.36",
                )
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::limited(10))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest client build failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Scraper trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Scraper for MangackScraper {
    fn source_name(&self) -> &'static str {
        "mangack"
    }

    /// Search via GET /?s=query — returns /manga/ links from results page.
    async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/");
        let client = &self.client;

        let text = super::retry(|| async {
            let r = client
                .get(&url)
                .query(&[("s", query)])
                .send()
                .await
                .with_context(|| format!("MangaCK GET {url}?s={query} failed"))?;
            r.text()
                .await
                .with_context(|| format!("MangaCK GET {url} body read failed"))
        })
        .await?;

        let results = parse_search_results(&text);

        sleep(DELAY).await;

        Ok(results)
    }

    /// Fetch series metadata and full chapter list from source_url.
    async fn get_series(&self, source_url: &str) -> Result<SeriesData> {
        let client = &self.client;
        let url = source_url.to_string();

        let text = super::retry(|| async {
            let r = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("MangaCK GET {url} failed"))?;
            r.text()
                .await
                .with_context(|| format!("MangaCK GET {url} body read failed"))
        })
        .await?;

        let source_url_owned = source_url.to_string();
        let series = parse_series_page(&text, &source_url_owned)?;

        sleep(DELAY).await;

        Ok(series)
    }

    /// Fetch the /updates/ feed and surface recently updated series as
    /// discovery candidates. MangaCK's homepage no longer lists current
    /// updates — only /updates/ does — so we harvest /manga/ links from
    /// there and pair each with the highest chapter number in its block.
    async fn latest_chapters(&self) -> Result<Vec<DiscoveryEntry>> {
        let url = format!("{BASE}/updates/");
        let client = &self.client;

        let text = super::retry(|| async {
            let r = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("MangaCK GET {url} failed"))?;
            r.text()
                .await
                .with_context(|| format!("MangaCK GET {url} body read failed"))
        })
        .await?;

        let entries = parse_homepage_latest(&text);

        sleep(DELAY).await;

        Ok(entries)
    }

    /// Fetch ordered image URLs for a chapter from chapter_url.
    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>> {
        let client = &self.client;
        let url = chapter_url.to_string();

        let text = super::retry(|| async {
            let r = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("MangaCK GET {url} failed"))?;
            r.text()
                .await
                .with_context(|| format!("MangaCK GET {url} body read failed"))
        })
        .await?;

        let urls = parse_chapter_images(&text);

        sleep(DELAY).await;

        Ok(urls)
    }
}

// ---------------------------------------------------------------------------
// Sync parse functions (extracted so Html doesn't live in async fn bodies)
// ---------------------------------------------------------------------------

fn parse_search_results(html: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for element in document.select(&SEL_A_MANGA) {
        let href = match element.value().attr("href") {
            Some(h) => h.to_string(),
            None => continue,
        };

        let title: String = element.text().collect::<Vec<_>>().join("").trim().to_string();

        if title.is_empty() || title.len() < 2 || seen.contains(&href) {
            continue;
        }
        if href.matches('/').count() < 4 {
            continue;
        }

        seen.insert(href.clone());

        let cover_url = find_cover_in_ancestors(&element);

        results.push(SearchResult {
            title,
            cover_url,
            source_url: href,
            pub_status: "ongoing".into(),
            source: "mangack".into(),
        });
    }

    results
}

fn parse_series_page(html: &str, source_url: &str) -> Result<SeriesData> {
    let document = Html::parse_document(html);

    let title = document
        .select(&SEL_H1)
        .next()
        .map(|el| el.text().collect::<Vec<_>>().join("").trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("MangaCK: no h1 title found at {source_url}"))?;

    let cover_url = document
        .select(&SEL_IMG_COVER)
        .find_map(|el| {
            el.value()
                .attr("src")
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });

    let pub_status = find_status(&document);
    let description = find_description(&document);

    let now = chrono::Utc::now();

    let series_slug = extract_series_slug(source_url);
    let mut chapters = parse_chapter_links(&document, now, series_slug.as_deref());
    chapters.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));

    Ok(SeriesData {
        title,
        cover_url,
        source_url: source_url.to_string(),
        pub_status,
        description,
        chapters,
    })
}

fn find_description(document: &Html) -> Option<String> {
    let node = document.select(&SEL_DESCRIPTION).next()?;
    let text = node.text().collect::<Vec<_>>().join(" ");
    let trimmed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() { None } else { Some(trimmed) }
}

fn parse_homepage_latest(html: &str) -> Vec<DiscoveryEntry> {
    let document = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for element in document.select(&SEL_A_MANGA) {
        let href = match element.value().attr("href") {
            Some(h) => h.to_string(),
            None => continue,
        };
        let title: String = element.text().collect::<Vec<_>>().join("").trim().to_string();
        if title.is_empty() || title.len() < 2 || seen.contains(&href) {
            continue;
        }
        if href.matches('/').count() < 4 {
            continue;
        }
        seen.insert(href.clone());

        let cover_url = find_cover_in_ancestors(&element);

        // Look at the parent block for a sibling /chapter/ link to extract
        // the most recent chapter number for this series.
        let chapter_number = element
            .parent()
            .and_then(scraper::ElementRef::wrap)
            .and_then(|p| p.parent().and_then(scraper::ElementRef::wrap).or(Some(p)))
            .and_then(|ancestor| {
                ancestor.select(&SEL_A_CHAPTER).find_map(|a| {
                    let label = a.text().collect::<Vec<_>>().join("");
                    let href = a.value().attr("href").unwrap_or("");
                    extract_chapter_number(&label, href)
                })
            });

        out.push(DiscoveryEntry {
            title,
            cover_url,
            source_url: href,
            chapter_number,
            released_at: None,
        });
    }

    out
}

fn parse_chapter_images(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let mut urls = Vec::new();

    for img in document.select(&SEL_IMG) {
        let src = img
            .value()
            .attr("src")
            .or_else(|| img.value().attr("data-src"))
            .unwrap_or("")
            .to_string();

        if src.is_empty() || src.starts_with("data:") {
            continue;
        }

        let low = src.to_lowercase();

        if !low.contains(".webp")
            && !low.contains(".jpg")
            && !low.contains(".jpeg")
            && !low.contains(".png")
        {
            continue;
        }

        if low.contains("logo")
            || low.contains("icon")
            || low.contains("avatar")
            || low.contains("banner")
            || low.contains("thumb")
        {
            continue;
        }

        urls.push(src);
    }

    urls
}

// ---------------------------------------------------------------------------
// Private helper: find_status
// ---------------------------------------------------------------------------

fn find_status(document: &Html) -> String {
    for element in document.select(&SEL_TD_TH) {
        let text: String = element
            .text()
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_lowercase();

        if text == "status" {
            // Walk next siblings to find the first Element node
            for sibling in element.next_siblings() {
                if let scraper::node::Node::Element(_) = sibling.value() {
                    let sibling_ref = scraper::ElementRef::wrap(sibling).unwrap();
                    let raw = sibling_ref
                        .text()
                        .collect::<Vec<_>>()
                        .join("")
                        .trim()
                        .to_string();
                    return normalize_status(&raw);
                }
            }
            break;
        }
    }
    "ongoing".to_string()
}

// ---------------------------------------------------------------------------
// Private helper: find_cover_in_ancestors
// ---------------------------------------------------------------------------

fn find_cover_in_ancestors(element: &scraper::ElementRef<'_>) -> Option<String> {
    let parent = element.parent()?;
    let parent_ref = scraper::ElementRef::wrap(parent);

    let grandparent = parent.parent();
    let grandparent_ref = grandparent.and_then(scraper::ElementRef::wrap);

    for ancestor in [parent_ref, grandparent_ref].into_iter().flatten() {
        if let Some(img) = ancestor.select(&SEL_IMG).next() {
            let src = img
                .value()
                .attr("src")
                .or_else(|| img.value().attr("data-src"));
            if let Some(s) = src {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Private helper: parse_chapter_links
// ---------------------------------------------------------------------------

fn parse_chapter_links(
    document: &Html,
    now: chrono::DateTime<chrono::Utc>,
    series_slug: Option<&str>,
) -> Vec<ChapterData> {
    let mut chapters = Vec::new();
    let mut seen_numbers: HashSet<u64> = HashSet::new();

    for element in document.select(&SEL_A_CHAPTER) {
        let href = match element.value().attr("href") {
            Some(h) => h.to_string(),
            None => continue,
        };

        // Drop chapter links that don't belong to this series — sidebars,
        // "popular chapters", and related-manga widgets all bleed `/chapter/`
        // links from other series into the same DOM. Without this filter we
        // pick up unrelated chapters and assign them wrong numbers.
        if let Some(slug) = series_slug {
            if !chapter_href_matches_slug(&href, slug) {
                continue;
            }
        }

        let raw_label: String = element
            .text()
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();

        // Skip quick-nav buttons ("First Chapter" / "Last Chapter" etc.)
        if NAV_LABELS.contains(raw_label.to_lowercase().as_str()) {
            continue;
        }

        // Strip "NEW" badge
        let label = RE_NEW_BADGE
            .replace(&raw_label, "")
            .trim()
            .to_string();

        // Extract chapter number — skip if cannot determine.
        // For the URL pass, use the post-slug portion so digits inside the
        // series slug can never be mistaken for the chapter number.
        let url_for_extract = series_slug
            .and_then(|slug| chapter_url_after_slug(&href, slug))
            .unwrap_or_else(|| href.clone());
        let number = match extract_chapter_number(&label, &url_for_extract) {
            Some(n) => n,
            None => continue,
        };

        // Deduplicate by number using bit representation
        let key = number.to_bits();
        if seen_numbers.contains(&key) {
            continue;
        }
        seen_numbers.insert(key);

        // Date: get all text from parent, subtract the chapter label and "NEW"
        let released_at = extract_chapter_date(&element, &raw_label, now);

        let title = if label.is_empty() { None } else { Some(label) };

        chapters.push(ChapterData {
            number,
            title,
            url: href,
            released_at,
        });
    }

    chapters
}

// ---------------------------------------------------------------------------
// Private helper: extract_chapter_date
// ---------------------------------------------------------------------------

fn extract_chapter_date(
    element: &scraper::ElementRef<'_>,
    raw_label: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let parent = element.parent()?;
    let parent_ref = scraper::ElementRef::wrap(parent)?;

    let full_text: String = parent_ref
        .text()
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();

    let date_text = full_text
        .replace(raw_label, "")
        .replace("NEW", "")
        .trim()
        .to_string();

    if date_text.is_empty() {
        return None;
    }

    parse_relative_date(&date_text, now)
}

// ---------------------------------------------------------------------------
// Private helper: series-slug derivation + chapter-href slug matching
// ---------------------------------------------------------------------------

/// Extract the series slug from a MangaCK series URL.
///
/// Series URLs are `{base}/manga/{slug}/` (possibly with query/fragment).
/// Returns `None` if the URL doesn't look like a series page.
pub(crate) fn extract_series_slug(source_url: &str) -> Option<String> {
    let path = source_url
        .split('?').next().unwrap_or(source_url)
        .split('#').next().unwrap_or(source_url);

    let mut segments = path.split('/').filter(|s| !s.is_empty());
    while let Some(seg) = segments.next() {
        if seg.eq_ignore_ascii_case("manga") {
            let slug = segments.next()?.trim();
            if slug.is_empty() {
                return None;
            }
            return Some(slug.to_string());
        }
    }
    None
}

/// Does this `/chapter/...` href belong to the series with the given slug?
///
/// MangaCK chapter URLs are `{base}/chapter/{slug}-chapter-{n}/`. We isolate
/// the path component after `/chapter/` and require it to start with
/// `{slug}-` so unrelated `/chapter/` links elsewhere on the page (sidebars,
/// "popular updates" widgets, related-manga blocks) get filtered out.
pub(crate) fn chapter_href_matches_slug(href: &str, slug: &str) -> bool {
    let path = href
        .split('?').next().unwrap_or(href)
        .split('#').next().unwrap_or(href);

    let after = match path.split("/chapter/").nth(1) {
        Some(s) => s,
        None => return false,
    };

    // Compare case-insensitively to be tolerant of host-side casing quirks,
    // but require the slug to be followed by `-chapter-` (the canonical
    // separator) — `solo-leveling-2` must not match `solo-leveling`.
    let needle = format!("{slug}-chapter-");
    after.len() >= needle.len()
        && after[..needle.len()].eq_ignore_ascii_case(&needle)
}

/// Return the portion of a chapter href after `{slug}-`, so chapter-number
/// extraction can run on a string that contains only the chapter component
/// and never the series slug. Returns `None` when the href doesn't match.
pub(crate) fn chapter_url_after_slug(href: &str, slug: &str) -> Option<String> {
    let path = href
        .split('?').next().unwrap_or(href)
        .split('#').next().unwrap_or(href);

    let after = path.split("/chapter/").nth(1)?;
    let prefix = format!("{slug}-");
    if after.len() < prefix.len()
        || !after[..prefix.len()].eq_ignore_ascii_case(&prefix)
    {
        return None;
    }
    Some(after[prefix.len()..].to_string())
}

// ---------------------------------------------------------------------------
// Private helper: extract_chapter_number
// ---------------------------------------------------------------------------

pub(crate) fn extract_chapter_number(label: &str, url: &str) -> Option<f64> {
    let label_match = RE_CHAPTER_NUM.captures(label).and_then(|cap| {
        let s = cap[1].replace('_', ".");
        s.parse::<f64>().ok()
    });

    let url_match = RE_CHAPTER_NUM.captures(url).and_then(|cap| {
        let s = cap[1].replace('_', ".");
        s.parse::<f64>().ok()
    });

    match (label_match, url_match) {
        (Some(ln), Some(un)) => {
            if (ln - un).abs() > 1.0 {
                tracing::warn!(
                    "MangaCK: label/URL chapter number mismatch for '{}' at {}",
                    label,
                    url
                );
            }
            Some(ln)
        }
        (Some(ln), None) => Some(ln),
        (None, Some(un)) => {
            tracing::warn!(
                "MangaCK: falling back to URL regex for chapter number — label='{}' url='{}'",
                label,
                url
            );
            Some(un)
        }
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Private helper: parse_relative_date
// ---------------------------------------------------------------------------

pub(crate) fn parse_relative_date(raw: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let s = raw.to_lowercase();

    // Relative date patterns — try each in order
    if let Some(cap) = RE_MINUTE.captures(&s) {
        let n: i64 = cap[1].parse().ok()?;
        let dt = now - chrono::Duration::minutes(n);
        return Some(dt.format("%Y-%m-%d").to_string());
    }
    if let Some(cap) = RE_HOUR.captures(&s) {
        let n: i64 = cap[1].parse().ok()?;
        let dt = now - chrono::Duration::hours(n);
        return Some(dt.format("%Y-%m-%d").to_string());
    }
    if let Some(cap) = RE_DAY.captures(&s) {
        let n: i64 = cap[1].parse().ok()?;
        let dt = now - chrono::Duration::days(n);
        return Some(dt.format("%Y-%m-%d").to_string());
    }
    if let Some(cap) = RE_WEEK.captures(&s) {
        let n: i64 = cap[1].parse().ok()?;
        let dt = now - chrono::Duration::weeks(n);
        return Some(dt.format("%Y-%m-%d").to_string());
    }
    if let Some(cap) = RE_MONTH.captures(&s) {
        let n: i64 = cap[1].parse().ok()?;
        let dt = now - chrono::Duration::days(n * 30);
        return Some(dt.format("%Y-%m-%d").to_string());
    }
    if let Some(cap) = RE_YEAR.captures(&s) {
        let n: i32 = cap[1].parse().ok()?;
        let date = now.date_naive();
        // Use calendar-aware year subtraction so leap years don't shift the date
        let past = date.with_year(date.year() - n)?;
        return Some(past.format("%Y-%m-%d").to_string());
    }

    // Absolute date fallbacks — try common formats
    for fmt in ["%B %d, %Y", "%Y-%m-%d", "%b %d, %Y"] {
        if let Ok(dt) = chrono::NaiveDate::parse_from_str(raw.trim(), fmt) {
            return Some(dt.format("%Y-%m-%d").to_string());
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn fixed_now() -> chrono::DateTime<Utc> {
        // 2024-06-15 12:00:00 UTC — fixed reference for deterministic assertions
        Utc.with_ymd_and_hms(2024, 6, 15, 12, 0, 0).unwrap()
    }

    // --- parse_relative_date ---

    #[test]
    fn relative_date_minutes() {
        let result = parse_relative_date("5 minutes ago", fixed_now());
        assert_eq!(result, Some("2024-06-15".to_string()));
    }

    #[test]
    fn relative_date_hours() {
        // 2 hours before 2024-06-15 12:00 is still 2024-06-15
        let result = parse_relative_date("2 hours ago", fixed_now());
        assert_eq!(result, Some("2024-06-15".to_string()));
    }

    #[test]
    fn relative_date_days() {
        // 3 days before 2024-06-15 is 2024-06-12
        let result = parse_relative_date("3 days ago", fixed_now());
        assert_eq!(result, Some("2024-06-12".to_string()));
    }

    #[test]
    fn relative_date_weeks() {
        // 1 week = 7 days before 2024-06-15 is 2024-06-08
        let result = parse_relative_date("1 week ago", fixed_now());
        assert_eq!(result, Some("2024-06-08".to_string()));
    }

    #[test]
    fn relative_date_months() {
        // 2 months = 60 days before 2024-06-15 is 2024-04-16
        let result = parse_relative_date("2 months ago", fixed_now());
        assert_eq!(result, Some("2024-04-16".to_string()));
    }

    #[test]
    fn relative_date_years() {
        // 1 year = 365 days before 2024-06-15 is 2023-06-15
        let result = parse_relative_date("1 year ago", fixed_now());
        assert_eq!(result, Some("2023-06-15".to_string()));
    }

    #[test]
    fn relative_date_absolute_long() {
        let result = parse_relative_date("January 15, 2024", fixed_now());
        assert_eq!(result, Some("2024-01-15".to_string()));
    }

    #[test]
    fn relative_date_absolute_iso() {
        let result = parse_relative_date("2024-03-10", fixed_now());
        assert_eq!(result, Some("2024-03-10".to_string()));
    }

    #[test]
    fn relative_date_gibberish_returns_none() {
        let result = parse_relative_date("gibberish text here", fixed_now());
        assert_eq!(result, None);
    }

    // --- extract_chapter_number ---

    #[test]
    fn chapter_number_from_label() {
        assert_eq!(extract_chapter_number("Chapter 42", ""), Some(42.0));
    }

    #[test]
    fn chapter_number_decimal() {
        assert_eq!(extract_chapter_number("Chapter 42.5", ""), Some(42.5));
    }

    #[test]
    fn chapter_number_case_insensitive() {
        assert_eq!(extract_chapter_number("CHAPTER 10", ""), Some(10.0));
    }

    #[test]
    fn chapter_number_url_fallback() {
        assert_eq!(
            extract_chapter_number("", "https://mangack.com/chapter/chapter-7/"),
            Some(7.0)
        );
    }

    #[test]
    fn chapter_number_underscore_normalized() {
        assert_eq!(extract_chapter_number("Chapter 1_5", ""), Some(1.5));
    }

    #[test]
    fn chapter_number_nav_button_returns_none() {
        // Nav buttons contain no chapter number
        assert_eq!(extract_chapter_number("First Chapter", "/chapter/first/"), None);
    }

    // --- extract_series_slug ---

    #[test]
    fn series_slug_basic() {
        assert_eq!(
            extract_series_slug("https://mangack.com/manga/solo-leveling/"),
            Some("solo-leveling".to_string())
        );
    }

    #[test]
    fn series_slug_with_query_fragment() {
        assert_eq!(
            extract_series_slug("https://mangack.com/manga/the-100/?ref=home#top"),
            Some("the-100".to_string())
        );
    }

    #[test]
    fn series_slug_missing_returns_none() {
        assert_eq!(extract_series_slug("https://mangack.com/"), None);
    }

    // --- chapter_href_matches_slug ---

    #[test]
    fn chapter_href_matches_correct_series() {
        assert!(chapter_href_matches_slug(
            "https://mangack.com/chapter/solo-leveling-chapter-200/",
            "solo-leveling",
        ));
    }

    #[test]
    fn chapter_href_rejects_other_series() {
        // Sidebar bleed: a different manga's chapter must not be accepted.
        assert!(!chapter_href_matches_slug(
            "https://mangack.com/chapter/tower-of-god-chapter-590/",
            "solo-leveling",
        ));
    }

    #[test]
    fn chapter_href_rejects_slug_prefix_collision() {
        // `solo-leveling-2` must not match the `solo-leveling` series — the
        // `-chapter-` separator guards against partial-prefix collisions.
        assert!(!chapter_href_matches_slug(
            "https://mangack.com/chapter/solo-leveling-2-chapter-1/",
            "solo-leveling",
        ));
    }

    #[test]
    fn chapter_href_rejects_non_chapter_url() {
        assert!(!chapter_href_matches_slug(
            "https://mangack.com/manga/solo-leveling/",
            "solo-leveling",
        ));
    }

    // --- chapter_url_after_slug ---

    #[test]
    fn url_after_slug_returns_chapter_segment() {
        assert_eq!(
            chapter_url_after_slug(
                "https://mangack.com/chapter/the-100-chapter-42/",
                "the-100",
            ),
            Some("chapter-42/".to_string()),
        );
    }

    #[test]
    fn url_after_slug_with_digit_slug_isolates_chapter_number() {
        // Slug contains digits ("the-100"); regex on the full href could
        // mis-pick "100" as the chapter. Slicing after the slug keeps only
        // the chapter component, so number extraction sees just "chapter-7/".
        let post = chapter_url_after_slug(
            "https://mangack.com/chapter/the-100-chapter-7/",
            "the-100",
        )
        .unwrap();
        assert_eq!(extract_chapter_number("", &post), Some(7.0));
    }

    #[test]
    fn url_after_slug_returns_none_for_other_series() {
        assert!(chapter_url_after_slug(
            "https://mangack.com/chapter/another-manga-chapter-5/",
            "the-100",
        )
        .is_none());
    }
}
