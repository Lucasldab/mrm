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

use super::{ChapterData, CheckStatus, DiscoveryEntry, Scraper, SearchResult, SeriesData};

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
// Any host: the CDN has been renamed before (L13).
static IMAGE_URL_RE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r#"https://[^"\s<>]+/chapters/[^"\s<>]+\.(?:webp|jpe?g|png|avif)"#).unwrap()
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
        // AsuraScans rotates the 8-hex slug suffix and 302's old → current.
        // rquest (unlike reqwest) does NOT follow redirects by default, so
        // without this every series URL returns the redirect stub: no <h1>,
        // no chapter anchors, `final_url` stuck on the dead hash → 0 chapters
        // parsed, silently reported "up to date". Must follow redirects.
        let client = rquest::Client::builder()
            .emulation(rquest_util::Emulation::Chrome131)
            .timeout(Duration::from_secs(20))
            .redirect(rquest::redirect::Policy::limited(10))
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
            .check_status()
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

/// Return the `/comics/{slug}` path component of a series URL, without any
/// `?query` or `#fragment`.
fn series_path(source_url: &str) -> String {
    let no_query = source_url.split(['?', '#']).next().unwrap_or(source_url);
    let after_scheme = no_query.split_once("://").map(|(_, r)| r).unwrap_or(no_query);
    let path = match after_scheme.find('/') {
        Some(i) => &after_scheme[i..],
        None    => "/",
    };
    path.trim_end_matches('/').to_string()
}

/// Strip the rotating 8-hex-char hash suffix from a series slug.
pub(crate) fn series_slug(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or("");
    HASH_SUFFIX_RE.replace(last, "").into_owned()
}

/// A series' identity across host moves and hash rotations: the last path
/// segment without its `-hash8` suffix. Kept in step with
/// `db::ASURA_SLUG_SQL`.
pub(crate) fn slug_key(url: &str) -> String {
    series_slug(&series_path(url))
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
        let data = parse_series(source_url, &final_url, &body)?;
        sleep(DELAY).await;
        Ok(data)
    }

    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>> {
        let (_, body) = fetch(&self.client, chapter_url).await?;
        let out = parse_chapter_images(&body)
            .with_context(|| format!("Asura chapter {chapter_url}"))?;
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

/// Image URLs embedded in Astro's HTML-encoded serialized state, in page
/// order, deduplicated. No match means the page did not parse (interstitial,
/// layout or CDN change): an error, not an empty chapter.
fn parse_chapter_images(body: &str) -> Result<Vec<String>> {
    let decoded = html_unescape(body);
    let mut seen: HashSet<&str> = HashSet::new();
    let out: Vec<String> = IMAGE_URL_RE.find_iter(&decoded)
        .map(|m| m.as_str())
        .filter(|u| seen.insert(u))
        .map(str::to_string)
        .collect();
    if out.is_empty() {
        anyhow::bail!("no chapter image URLs found on the page");
    }
    Ok(out)
}

/// Parse a series page fetched from `source_url` that ended at `final_url`
/// after redirects. A page without an `<h1>` title or without any chapter
/// anchors for this series (Cloudflare interstitial served with 200, soft
/// 404, layout change) is an error, so it can't blank the stored series.
fn parse_series(source_url: &str, final_url: &str, body: &str) -> Result<SeriesData> {
    let tree = Html::parse_document(body);

    let h1 = tree.select(&SEL_H1)
        .find(|h1| !h1.text().collect::<String>().trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Asura: no <h1> title at {source_url}"))?;
    let title = h1.text().collect::<String>().trim().to_string();

    let cover_url = cover_img(&tree.root_element()).map(|(_, url)| url);

    // The status nearest the title: the smallest ancestor of the <h1> holding
    // a status word. Filter menus and sidebar cards elsewhere don't count.
    let status = h1.ancestors()
        .filter_map(scraper::ElementRef::wrap)
        .take_while(|el| !matches!(el.value().name(), "body" | "html"))
        .find_map(|block| block.select(&SEL_SPAN).find_map(|span| {
            map_status(&span.text().collect::<String>())
        }))
        .unwrap_or("ongoing")
        .to_string();

    let path = series_path(final_url);
    let locked = parse_locked_chapters(body, &path);
    let mut chapters = parse_chapter_links(&tree, &path);
    if chapters.is_empty() {
        anyhow::bail!("Asura: no chapter links for {path} at {source_url}");
    }
    chapters.retain(|c| !locked.contains(&c.number.to_bits()));
    chapters.sort_by(|a, b| a.number.partial_cmp(&b.number).unwrap_or(std::cmp::Ordering::Equal));

    Ok(SeriesData {
        title,
        cover_url,
        // Post-redirect: AsuraScans rotates the -hash8 suffix and moves hosts;
        // the poll path stores this so the old redirect chain can die (L12).
        source_url: final_url.to_string(),
        pub_status: status,
        description: None,
        chapters,
    })
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

        let (img, cover_url) = match cover_img(&a) {
            Some(c) => c,
            None    => continue,
        };
        let title = img.value().attr("alt").unwrap_or("").trim().to_string();
        if title.is_empty() || title.len() < 2 { continue; }
        cards.insert(path, (title, Some(cover_url)));
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
/// The first `img` under `el` whose real (lazy-aware) URL is a cover.
fn cover_img<'a>(el: &scraper::ElementRef<'a>) -> Option<(scraper::ElementRef<'a>, String)> {
    el.select(&SEL_IMG)
        .filter_map(|img| super::img_url(&img).map(|u| (img, u)))
        .find(|(_, u)| u.contains("covers"))
}

fn find_cover_near(a: &scraper::ElementRef<'_>) -> Option<String> {
    // The anchor itself
    if let Some((_, src)) = cover_img(a) {
        return Some(src);
    }
    // Walk up two levels via NodeRef::parent() + ElementRef::wrap (same
    // pattern as mangack::find_cover_in_ancestors).
    let parent     = a.parent().and_then(scraper::ElementRef::wrap);
    let grandparent = a.parent()
        .and_then(|p| p.parent())
        .and_then(scraper::ElementRef::wrap);
    for ancestor in [parent, grandparent].into_iter().flatten() {
        if let Some((_, src)) = cover_img(&ancestor) {
            return Some(src);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_links_keep_equal_numbers_with_different_paths() {
        let html = r#"<html><body>
            <a href="/comics/x-1a2b3c4d/chapter/1"><span class="truncate">Chapter 1</span></a>
            <a href="/comics/x-1a2b3c4d/chapter/1?season=2"><span class="truncate">Chapter 1</span></a>
            <a href="/comics/x-1a2b3c4d/chapter/1"><span class="truncate">Chapter 1</span></a>
        </body></html>"#;
        let tree = Html::parse_document(html);
        let chapters = parse_chapter_links(&tree, "/comics/x-1a2b3c4d");
        assert_eq!(chapters.len(), 2);
    }

    #[test]
    fn series_path_and_slug_ignore_query_and_fragment() {
        let u = "https://asurascans.com/comics/solo-leveling-1a2b3c4d/?ref=home#top";
        assert_eq!(series_path(u), "/comics/solo-leveling-1a2b3c4d");
        assert_eq!(slug_key(u), "solo-leveling");
        assert_eq!(slug_key("https://asurascans.com/comics/solo-leveling-9f8e7d6c"), "solo-leveling");
    }

    // --- M8: lazy-loaded covers ---

    #[test]
    fn search_cover_uses_lazy_src() {
        let html = r#"<html><body><div class="card">
            <a href="/comics/solo-leveling-1a2b3c4d">
              <img src="data:image/svg+xml;base64,AAA" data-src="https://cdn.x/covers/solo.webp">
              <h3>Solo Leveling</h3>
            </a>
        </div></body></html>"#;
        let r = parse_search(html);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].cover_url.as_deref(), Some("https://cdn.x/covers/solo.webp"));
    }

    #[test]
    fn search_cover_found_in_ancestor_with_lazy_src() {
        let html = r#"<html><body><div class="card">
            <img src="data:," data-lazy-src="https://cdn.x/covers/tog.webp">
            <a href="/comics/tower-of-god-1a2b3c4d"><h3>Tower of God</h3></a>
        </div></body></html>"#;
        let r = parse_search(html);
        assert_eq!(r[0].cover_url.as_deref(), Some("https://cdn.x/covers/tog.webp"));
    }

    // --- M7: unparsable pages are errors, status is scoped ---

    const OLD: &str = "https://asurascans.com/comics/solo-leveling-1a2b3c4d";
    const NEW: &str = "https://asurascans.com/comics/solo-leveling-9f8e7d6c";

    fn series_page(status_sidebar: &str, status_info: &str) -> String {
        format!(r#"<html><body>
            <aside><span>{status_sidebar}</span><a href="/comics/other-00000000/chapter/9">Chapter 9</a></aside>
            <main><div class="info">
              <img src="https://cdn.x/covers/solo.webp">
              <h1>Solo Leveling</h1>
              <div><span>Status</span><span>{status_info}</span></div>
            </div>
            <div class="chapters">
              <a href="/comics/solo-leveling-9f8e7d6c/chapter/1"><span class="truncate">Chapter 1</span></a>
              <a href="/comics/solo-leveling-9f8e7d6c/chapter/2"><span class="truncate">Chapter 2</span></a>
            </div></main>
        </body></html>"#)
    }

    #[test]
    fn cloudflare_interstitial_is_an_error() {
        let html = r#"<html><head><title>Just a moment...</title></head>
            <body><div id="challenge-running">Checking your browser</div></body></html>"#;
        assert!(parse_series(OLD, OLD, html).is_err());
    }

    #[test]
    fn page_without_chapter_anchors_is_an_error() {
        let html = r#"<html><body><h1>Solo Leveling</h1><span>Completed</span></body></html>"#;
        assert!(parse_series(OLD, OLD, html).is_err());
    }

    #[test]
    fn status_comes_from_the_info_block_not_the_sidebar() {
        let s = parse_series(OLD, NEW, &series_page("Completed", "Ongoing")).unwrap();
        assert_eq!(s.title, "Solo Leveling");
        assert_eq!(s.pub_status, "ongoing");
        assert_eq!(s.chapters.len(), 2);
        let s = parse_series(OLD, NEW, &series_page("Ongoing", "Completed")).unwrap();
        assert_eq!(s.pub_status, "completed");
    }

    // --- L12: redirects are persisted, slugs compare without the hash ---

    #[test]
    fn series_reports_the_post_redirect_url() {
        let s = parse_series(OLD, NEW, &series_page("", "Ongoing")).unwrap();
        assert_eq!(s.source_url, NEW);
    }

    #[test]
    fn slug_key_ignores_host_and_hash_suffix() {
        assert_eq!(slug_key(OLD), "solo-leveling");
        assert_eq!(slug_key(&format!("{NEW}/")), "solo-leveling");
        assert_eq!(slug_key("https://asuracomic.net/series/solo-leveling-abcdef12/"), "solo-leveling");
        assert_ne!(slug_key("https://asurascans.com/comics/solo-leveling-2-1a2b3c4d"), "solo-leveling");
    }

    // --- L13: image URLs on any CDN host, empty is an error ---

    #[test]
    fn chapter_images_match_any_cdn_host() {
        let body = r#"<script>{&quot;pages&quot;:[&quot;https://img.newcdn.example/asura/chapters/abc/001.webp&quot;,
            &quot;https://img.newcdn.example/asura/chapters/abc/002.avif&quot;,
            &quot;https://img.newcdn.example/asura/chapters/abc/001.webp&quot;,
            &quot;https://cdn.asurascans.com/asura-images/chapters/abc/003.jpeg&quot;]}</script>"#;
        assert_eq!(
            parse_chapter_images(body).unwrap(),
            vec![
                "https://img.newcdn.example/asura/chapters/abc/001.webp".to_string(),
                "https://img.newcdn.example/asura/chapters/abc/002.avif".to_string(),
                "https://cdn.asurascans.com/asura-images/chapters/abc/003.jpeg".to_string(),
            ],
        );
    }

    #[test]
    fn chapter_page_without_images_is_an_error() {
        assert!(parse_chapter_images("<html><body>Just a moment...</body></html>").is_err());
    }
}
