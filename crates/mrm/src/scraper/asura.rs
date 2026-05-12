//! AsuraScans scraper — pure Rust via rquest (Chrome TLS impersonation).
//!
//! AsuraScans is fronted by Cloudflare, which rejects requests whose TLS
//! fingerprint doesn't look like a real browser. The `rquest` crate forks
//! reqwest and patches it with BoringSSL profiles that emulate Chrome/Firefox
//! at the JA3/JA4/HTTP-2 level, so we can scrape without a headless browser
//! and without a Python `curl_cffi` subprocess.
//!
//! Confirmed live-site structure (kept in sync with the old asura.py):
//!   Series URL  : {BASE}/comics/{slug}-{hash8}/
//!   Chapter URL : {BASE}/comics/{slug}-{hash8}/chapter/{n}
//!   Search URL  : {BASE}/browse?search={query}
//!
//! HTML pages are Astro-SSR'd; chapter image URLs are embedded inside
//! HTML-escaped serialized state on the chapter page.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use std::sync::LazyLock;
use regex::Regex;
use scraper::{Html, Selector};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use super::{ChapterData, DiscoveryEntry, Scraper, SearchResult, SeriesData};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BASE: &str = "https://asurascans.com";
const DELAY: Duration = Duration::from_millis(500);

// Status text from <span> on the series page → our canonical pub_status.
fn map_status(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "ongoing"     => Some("ongoing"),
        "hiatus"      => Some("hiatus"),
        "completed"   => Some("completed"),
        "dropped"     => Some("ongoing"),
        "coming soon" => Some("ongoing"),
        _ => None,
    }
}

// Skip these as chapter "titles" — they're nav widgets, not real chapters.
fn is_nav_label(label: &str) -> bool {
    matches!(
        label.trim().to_lowercase().as_str(),
        "first chapter" | "latest chapter" | "first" | "latest"
    )
}

// ---------------------------------------------------------------------------
// Regex / Selector singletons
// ---------------------------------------------------------------------------

static SEL_COMICS_LINK:   LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href*='/comics/']").unwrap());
static SEL_H3:            LazyLock<Selector> = LazyLock::new(|| Selector::parse("h3").unwrap());
static SEL_IMG_COVERS:    LazyLock<Selector> = LazyLock::new(|| Selector::parse("img[src*='covers']").unwrap());
static SEL_H1:            LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").unwrap());
static SEL_IMG:           LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());
static SEL_SPAN:          LazyLock<Selector> = LazyLock::new(|| Selector::parse("span").unwrap());
static SEL_TRUNCATE_SPAN: LazyLock<Selector> = LazyLock::new(|| Selector::parse("span.truncate").unwrap());
static SEL_DATE_DIV:      LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.flex-shrink-0").unwrap());

static HASH_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-[0-9a-f]{8}$").unwrap());
static CHAPTER_PATH_RE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"^(/comics/[^/]+)/chapter/(\d+(?:\.\d+)?)").unwrap()
);
static CHAPTER_NUM_RE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"/chapter/([\d]+(?:\.[\d]+)?)").unwrap()
);
static IMAGE_URL_RE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r#"https://cdn\.asurascans\.com/asura-images/chapters/[^"\s<>]+\.(?:webp|jpg|jpeg|png)"#).unwrap()
);

// Two known serialized chapter-block formats embedded in Astro's HTML-encoded
// state. Both expose `is_premium` + `early_access_until`; one names the
// series via `comic_slug`, the other via `series_slug`. We match either.
static LOCK_RE_A: LazyLock<Regex> = LazyLock::new(||
    Regex::new(
        r#"number&quot;:\[0,(?P<num>\d+(?:\.\d+)?)\][^{}]*?comic_slug&quot;:\[0,&quot;(?P<slug>[^&]+)&quot;\][^{}]*?is_premium&quot;:\[0,(?P<prem>true|false)\][^{}]*?early_access_until&quot;:\[0,(?:&quot;(?P<eau>[^&]+)&quot;|[^\]]+)\]"#
    ).unwrap()
);
static LOCK_RE_B: LazyLock<Regex> = LazyLock::new(||
    Regex::new(
        r#"number&quot;:\[0,(?P<num>\d+(?:\.\d+)?)\][^{}]*?is_premium&quot;:\[0,(?P<prem>true|false)\][^{}]*?early_access_until&quot;:\[0,(?:&quot;(?P<eau>[^&]+)&quot;|[^\]]+)\][^{}]*?series_slug&quot;:\[0,&quot;(?P<slug>[^&]+)&quot;\]"#
    ).unwrap()
);

// ---------------------------------------------------------------------------
// Struct
// ---------------------------------------------------------------------------

pub struct AsuraScraper {
    /// rquest::Client with Chrome TLS emulation — defeats Cloudflare's JA3/JA4
    /// fingerprint check.
    client: Arc<rquest::Client>,
}

impl AsuraScraper {
    pub fn new() -> Self {
        let client = rquest::Client::builder()
            .emulation(rquest_util::Emulation::Chrome131)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("rquest Client::build should never fail at startup");
        Self { client: Arc::new(client) }
    }
}

impl Default for AsuraScraper {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HTTP helper
// ---------------------------------------------------------------------------

/// GET with retry + status validation. Returns `(final_url, body)` so callers
/// can see post-redirect URLs (AsuraScans rotates series-slug hashes and
/// 302's old → new).
async fn fetch(client: &rquest::Client, url: &str) -> Result<(String, String)> {
    super::retry(|| async {
        let resp = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("Asura GET {url} failed"))?
            .error_for_status()
            .with_context(|| format!("Asura GET {url} returned HTTP error"))?;
        let final_url = resp.url().to_string();
        let body = resp
            .text()
            .await
            .with_context(|| format!("Asura GET {url} body read failed"))?;
        Ok((final_url, body))
    })
    .await
}

// ---------------------------------------------------------------------------
// Pure-function helpers (parsing — ported 1:1 from asura.py)
// ---------------------------------------------------------------------------

/// Return the `/comics/{slug}` path component of a series URL.
fn series_path(source_url: &str) -> String {
    let after_scheme = source_url.split_once("://").map(|(_, r)| r).unwrap_or(source_url);
    let path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None    => "/",
    };
    path.trim_end_matches('/').to_string()
}

/// Strip the rotating 8-hex-char hash suffix from a series slug.
fn series_slug(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or("");
    HASH_SUFFIX_RE.replace(last, "").into_owned()
}

/// Decode HTML entities used by Astro's serialized state (just the subset we
/// actually encounter on chapter pages; full HTML entity decoding is overkill).
fn html_unescape(s: &str) -> String {
    s.replace("&amp;",  "&")
     .replace("&quot;", "\"")
     .replace("&#x27;", "'")
     .replace("&#39;",  "'")
     .replace("&lt;",   "<")
     .replace("&gt;",   ">")
}

/// Parse a free-form date string into ISO-8601 (YYYY-MM-DD).
fn parse_date(raw: &str) -> Option<String> {
    let raw = raw.trim();
    for fmt in ["%b %d, %Y", "%B %d, %Y", "%Y-%m-%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(raw, fmt) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }
    None
}

/// Return the set of chapter numbers behind the early-access paywall for the
/// given series. Cross-references the Astro-serialized blob against the
/// series' slug so we don't leak other series' lock state into this one.
fn parse_locked_chapters(raw_html: &str, path: &str) -> HashSet<u64> {
    let mut locked: HashSet<u64> = HashSet::new();
    let target_slug = series_slug(path);
    let now: DateTime<Utc> = Utc::now();

    for re in [&*LOCK_RE_A, &*LOCK_RE_B] {
        for cap in re.captures_iter(raw_html) {
            let slug = cap.name("slug").map(|m| m.as_str()).unwrap_or("");
            if slug != target_slug { continue; }
            let prem = cap.name("prem").map(|m| m.as_str()).unwrap_or("");
            if prem != "true" { continue; }
            let eau  = cap.name("eau").map(|m| m.as_str()).unwrap_or("");
            if eau.is_empty() { continue; }
            // ISO-8601 → DateTime<Utc>. Tolerate trailing `Z` or `+00:00`.
            let normalized = eau.replace('Z', "+00:00");
            let ts: DateTime<Utc> = match DateTime::parse_from_rfc3339(&normalized) {
                Ok(t)  => t.with_timezone(&Utc),
                Err(_) => continue,
            };
            if ts > now {
                if let Some(num_str) = cap.name("num") {
                    if let Ok(n) = num_str.as_str().parse::<f64>() {
                        // Use bit-repr to make f64 hashable (matches python's
                        // chapter-number identity scheme elsewhere in mrm).
                        locked.insert(n.to_bits());
                    }
                }
            }
        }
    }
    locked
}

// ---------------------------------------------------------------------------
// Scraper trait
// ---------------------------------------------------------------------------

#[async_trait]
impl Scraper for AsuraScraper {
    fn source_name(&self) -> &'static str { "asura" }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/browse?search={}", urlencoding_minimal(query));
        let (_, body) = fetch(&self.client, &url).await?;
        // Parse + collect owned data in a sync scope — `scraper::Html` is
        // !Send, so it must not live across an `await`.
        let results = parse_search(&body);
        sleep(DELAY).await;
        Ok(results)
    }

    async fn get_series(&self, source_url: &str) -> Result<SeriesData> {
        let (final_url, body) = fetch(&self.client, source_url).await?;
        let data = parse_series(source_url, &final_url, &body);
        sleep(DELAY).await;
        Ok(data)
    }

    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>> {
        let (_, body) = fetch(&self.client, chapter_url).await?;

        // Image URLs are embedded inside Astro's HTML-encoded serialized state.
        let decoded = html_unescape(&body);
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for m in IMAGE_URL_RE.find_iter(&decoded) {
            let u = m.as_str().to_string();
            if seen.insert(u.clone()) {
                out.push(u);
            }
        }
        sleep(DELAY).await;
        Ok(out)
    }

    async fn latest_chapters(&self) -> Result<Vec<DiscoveryEntry>> {
        let (_, body) = fetch(&self.client, &format!("{BASE}/")).await?;
        let out = parse_latest_chapters(&body);
        sleep(DELAY).await;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Sync parse functions
// ---------------------------------------------------------------------------
// scraper::Html is !Send, so all HTML parsing happens in synchronous blocks
// that return owned data — never held across an `.await`.

fn parse_search(body: &str) -> Vec<SearchResult> {
    let tree = Html::parse_document(body);
    let mut results = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for a in tree.select(&SEL_COMICS_LINK) {
        let href = match a.value().attr("href") {
            Some(h) => h.to_string(),
            None    => continue,
        };
        if href.is_empty() || seen.contains(&href) { continue; }

        let title = match a.select(&SEL_H3).next() {
            Some(h3) => h3.text().collect::<String>().trim().to_string(),
            None     => continue,
        };
        if title.is_empty() || title.len() < 2 { continue; }
        seen.insert(href.clone());

        let cover_url = find_cover_near(&a);

        let full = if href.starts_with('/') { format!("{BASE}{href}") } else { href };

        results.push(SearchResult {
            title,
            cover_url,
            source_url: full,
            pub_status: "ongoing".into(),
            source:     "asura".into(),
        });
    }
    results
}

fn parse_series(source_url: &str, final_url: &str, body: &str) -> SeriesData {
    let tree = Html::parse_document(body);

    let title = tree.select(&SEL_H1).next()
        .map(|h1| h1.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let cover_url = tree.select(&SEL_IMG)
        .filter_map(|img| img.value().attr("src"))
        .find(|src| src.contains("covers"))
        .map(|s| s.to_string());

    let status = tree.select(&SEL_SPAN)
        .filter_map(|span| {
            let t = span.text().collect::<String>().trim().to_lowercase();
            map_status(&t).map(|s| s.to_string())
        })
        .next()
        .unwrap_or_else(|| "ongoing".to_string());

    let path = series_path(final_url);
    let locked = parse_locked_chapters(body, &path);
    let mut chapters = parse_chapter_links(&tree, &path);
    chapters.retain(|c| !locked.contains(&c.number.to_bits()));
    chapters.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));

    SeriesData {
        title,
        cover_url,
        source_url: source_url.to_string(),
        pub_status: status,
        description: None,
        chapters,
    }
}

fn parse_latest_chapters(body: &str) -> Vec<DiscoveryEntry> {
    let tree = Html::parse_document(body);

    let mut cards: HashMap<String, (String, Option<String>)> = HashMap::new();
    for a in tree.select(&SEL_COMICS_LINK) {
        let href = match a.value().attr("href") {
            Some(h) => h,
            None    => continue,
        };
        if href.contains("/chapter/") { continue; }
        let path = href.split('?').next().unwrap_or("").trim_end_matches('/').to_string();
        if !path.starts_with("/comics/") || cards.contains_key(&path) { continue; }

        let img = match a.select(&SEL_IMG_COVERS).next() {
            Some(i) => i,
            None    => continue,
        };
        let title = img.value().attr("alt").unwrap_or("").trim().to_string();
        if title.is_empty() || title.len() < 2 { continue; }
        let cover_url = img.value().attr("src")
            .or_else(|| img.value().attr("data-src"))
            .map(|s| s.to_string());
        cards.insert(path, (title, cover_url));
    }

    let mut latest: HashMap<String, f64> = HashMap::new();
    for a in tree.select(&SEL_COMICS_LINK) {
        let href = match a.value().attr("href") {
            Some(h) => h,
            None    => continue,
        };
        let cap = match CHAPTER_PATH_RE.captures(href) {
            Some(c) => c,
            None    => continue,
        };
        let path = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
        let num: f64 = match cap.get(2).map(|m| m.as_str().parse()) {
            Some(Ok(n)) => n,
            _ => continue,
        };
        let entry = latest.entry(path).or_insert(-1.0);
        if num > *entry { *entry = num; }
    }

    let mut out = Vec::new();
    for (path, num) in latest {
        let (title, cover) = match cards.get(&path) {
            Some(v) => v.clone(),
            None    => continue,
        };
        out.push(DiscoveryEntry {
            title,
            cover_url:      cover,
            source_url:     format!("{BASE}{path}"),
            chapter_number: Some(num),
            released_at:    None,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk an anchor and up to two of its ancestors looking for an `img` tagged
/// as a cover. Mirrors asura.py's two-step ancestor walk.
fn find_cover_near(a: &scraper::ElementRef<'_>) -> Option<String> {
    // The anchor itself
    if let Some(img) = a.select(&SEL_IMG_COVERS).next() {
        if let Some(src) = img.value().attr("src").or_else(|| img.value().attr("data-src")) {
            return Some(src.to_string());
        }
    }
    // Walk up two levels via NodeRef::parent() + ElementRef::wrap (same
    // pattern as mangack::find_cover_in_ancestors).
    let parent     = a.parent().and_then(scraper::ElementRef::wrap);
    let grandparent = a.parent()
        .and_then(|p| p.parent())
        .and_then(scraper::ElementRef::wrap);
    for ancestor in [parent, grandparent].into_iter().flatten() {
        if let Some(img) = ancestor.select(&SEL_IMG_COVERS).next() {
            if let Some(src) = img.value().attr("src").or_else(|| img.value().attr("data-src")) {
                return Some(src.to_string());
            }
        }
    }
    None
}

/// Parse the chapter list under a series page. `series_path_prefix` is
/// `/comics/{slug-hash}` — only anchors whose href starts with
/// `{prefix}/chapter/` are considered, so the cross-series sidebar can't leak.
fn parse_chapter_links(tree: &Html, series_path_prefix: &str) -> Vec<ChapterData> {
    let prefix = format!("{series_path_prefix}/chapter/");
    let sel = match Selector::parse(&format!("a[href^='{prefix}']")) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut chapters = Vec::new();
    let mut seen_numbers: HashSet<u64> = HashSet::new();
    let mut seen_urls:    HashSet<String> = HashSet::new();

    for a in tree.select(&sel) {
        let href = match a.value().attr("href") {
            Some(h) => h.to_string(),
            None    => continue,
        };
        if href.is_empty() || seen_urls.contains(&href) { continue; }

        let label_text = a.text().collect::<String>();
        if is_nav_label(&label_text) { continue; }
        seen_urls.insert(href.clone());

        let cap = match CHAPTER_NUM_RE.captures(&href) {
            Some(c) => c,
            None    => continue,
        };
        let number: f64 = match cap.get(1).and_then(|m| m.as_str().parse().ok()) {
            Some(n) => n,
            None    => continue,
        };
        let bits = number.to_bits();
        if !seen_numbers.insert(bits) { continue; }

        let title = a
            .select(&SEL_TRUNCATE_SPAN)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty());

        let released_at = a
            .select(&SEL_DATE_DIV)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .and_then(|s| parse_date(&s));

        let full_url = if href.starts_with('/') { format!("{BASE}{href}") } else { href.clone() };

        chapters.push(ChapterData {
            number,
            title,
            url: full_url,
            released_at,
        });
    }
    chapters
}

/// Minimal percent-encoding for query strings — sufficient for search queries.
/// (Avoids pulling in a full urlencoding crate just for one call site.)
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
