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
    pub title: String,
    pub cover_url: Option<String>,
    pub source_url: String,
    pub pub_status: String, // "ongoing" | "hiatus" | "completed"
    pub description: Option<String>,
    pub chapters: Vec<ChapterData>,
}

/// Single chapter entry returned as part of SeriesData.chapters.
#[derive(Debug, Clone)]
pub struct ChapterData {
    pub number: f64,
    pub title: Option<String>,
    pub url: String,
    pub released_at: Option<String>, // "YYYY-MM-DD" or None
}

/// Several different chapters with one number (a season or volume restarting
/// at 1). Keep one per number — the URL already stored for it, else the first
/// listed — so a reset never overwrites a stored chapter and its read mark.
/// Exact duplicate URLs are not a conflict. Returns the kept chapters (page
/// order) and a note for the series' ⚠ line, or None when nothing collided.
pub fn resolve_collisions(
    chapters: Vec<ChapterData>,
    stored: &HashMap<u64, String>,
) -> (Vec<ChapterData>, Option<String>) {
    let mut urls: HashMap<u64, Vec<String>> = HashMap::new();
    let mut order: Vec<u64> = Vec::new();
    for c in &chapters {
        let key = c.number.to_bits();
        let list = urls.entry(key).or_insert_with(|| {
            order.push(key);
            Vec::new()
        });
        if !list.contains(&c.url) {
            list.push(c.url.clone());
        }
    }
    let mut keep: HashMap<u64, String> = HashMap::new();
    let mut conflicts: Vec<(f64, usize, String)> = Vec::new();
    for key in &order {
        let list = &urls[key];
        let chosen = stored
            .get(key)
            .filter(|s| list.contains(s))
            .cloned()
            .unwrap_or_else(|| list[0].clone());
        if list.len() > 1 {
            conflicts.push((f64::from_bits(*key), list.len(), chosen.clone()));
        }
        keep.insert(*key, chosen);
    }
    let mut out = Vec::with_capacity(keep.len());
    for c in chapters {
        let key = c.number.to_bits();
        if keep.get(&key) == Some(&c.url) {
            keep.remove(&key);
            out.push(c);
        }
    }
    let note = conflicts.first().map(|(n, count, url)| {
        let path = url
            .split_once("://")
            .and_then(|(_, r)| r.find('/').map(|i| &r[i..]))
            .unwrap_or(url);
        let mut s = format!("{count} chapters share no. {n} (season reset?): keeping {path}");
        if conflicts.len() > 1 {
            s.push_str(&format!(" (+{} more numbers)", conflicts.len() - 1));
        }
        s
    });
    (out, note)
}

/// One result from a search() call.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub cover_url: Option<String>,
    pub source_url: String,
    pub pub_status: String,
    pub source: String, // "mangadex" | "mangack"
}

/// One entry from a latest_chapters() feed — a candidate for the Discover
/// screen. We don't fetch the full series metadata here; just enough to show
/// the cover grid and, on add, hand the source_url to get_series().
#[derive(Debug, Clone)]
pub struct DiscoveryEntry {
    pub title: String,
    pub cover_url: Option<String>,
    pub source_url: String,
    pub chapter_number: Option<f64>,
    pub released_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Scraper trait
// ---------------------------------------------------------------------------

// async_trait marks its generated futures #[must_use], which clippy reports
// as doubled on the code the macro writes, not on anything written here.
#[allow(clippy::double_must_use)]
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
            "mangadex" => {
                registry.insert("mangadex", Box::new(MangaDexScraper::new()));
            }
            "mangack" => {
                registry.insert("mangack", Box::new(MangackScraper::new()));
            }
            "asura" => {
                registry.insert("asura", Box::new(AsuraScraper::new()));
            }
            _ => {}
        }
    }
    registry
}

/// Enabled source names in the config that no scraper implements (for a
/// startup warning in the non-TUI modes).
pub fn unknown_sources(config: &Config) -> Vec<String> {
    let mut v: Vec<String> = config
        .sources
        .iter()
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
        "mangack" => 1,
        "asura" => 2,
        _ => 3,
    }
}

/// Human-readable source name for error messages.
pub fn source_label(name: &str) -> &str {
    match name {
        "mangadex" => "MangaDex",
        "mangack" => "MangaCK",
        "asura" => "Asura",
        other => other,
    }
}

/// Status text for a source that is not in the registry.
pub fn source_unavailable(source: &str) -> String {
    format!("Source '{source}' is disabled or unknown (see [sources] in config.toml)")
}

/// Search every source in the registry concurrently. Results are merged in a
/// fixed source order; failures become "Source: error" strings.
pub async fn search_registry(registry: &Registry, query: &str) -> (Vec<SearchResult>, Vec<String>) {
    let mut names: Vec<&'static str> = registry.keys().copied().collect();
    if names.is_empty() {
        return (Vec::new(), vec!["No sources enabled in config.toml".into()]);
    }
    names.sort_by_key(|n| (source_rank(n), *n));
    let outcomes =
        futures::future::join_all(names.iter().map(|n| registry[*n].search(query))).await;

    let mut merged = Vec::new();
    let mut errors = Vec::new();
    for (name, outcome) in names.iter().zip(outcomes) {
        match outcome {
            Ok(r) => merged.extend(r),
            Err(e) => errors.push(format!("{}: {e}", source_label(name))),
        }
    }
    (merged, errors)
}

// ---------------------------------------------------------------------------
// Page image fetching (shared by the TUI reader and `mrm --serve`)
// ---------------------------------------------------------------------------

/// HTTP client for page/cover images of `source`. AsuraScans' CDN rejects
/// non-browser requests, so Asura gets a realistic Chrome UA; everyone else
/// a short browser-ish one.
pub fn image_client(source: &str) -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    let builder = if source == "asura" {
        builder.user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        )
    } else {
        builder.user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
    };
    builder.build()
}

/// GET one image with `client` from [`image_client`]. Pages pass the chapter
/// URL as Referer (many CDNs hotlink-check); a non-success status is an error.
pub async fn fetch_image(
    client: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
) -> reqwest::Result<reqwest::Response> {
    let mut req = client.get(url);
    if let Some(r) = referer {
        req = req.header(reqwest::header::REFERER, r);
    }
    req.send().await?.error_for_status()
}

// ---------------------------------------------------------------------------
// HTML image helper
// ---------------------------------------------------------------------------

/// The real URL of an `<img>`: lazy-load attributes (`data-src`,
/// `data-lazy-src`, first `srcset` candidate) win over `src`, which lazy-load
/// themes fill with a `data:` placeholder. `data:` URIs and blanks are skipped.
pub(crate) fn img_url(img: &scraper::ElementRef<'_>) -> Option<String> {
    let el = img.value();
    let srcset_first = el
        .attr("srcset")
        .and_then(|s| s.split(',').next())
        .and_then(|c| c.split_whitespace().next());
    [
        el.attr("data-src"),
        el.attr("data-lazy-src"),
        srcset_first,
        el.attr("src"),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|u| !u.is_empty() && !u.starts_with("data:"))
    .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Retry helper
// ---------------------------------------------------------------------------

/// Longest `Retry-After` we wait inside one fetch; anything longer fails this
/// attempt and the next poll tries again.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A non-success HTTP status, with the server's `Retry-After` when it sent
/// one. Scrapers produce it via [`check_reqwest`] / [`check_rquest`] so the
/// retry classifier sees the header, which `error_for_status` drops.
#[derive(Debug)]
pub(crate) struct HttpStatus {
    pub status: u16,
    pub retry_after: Option<Duration>,
}

impl HttpStatus {
    pub(crate) fn new(status: u16, retry_after: Option<&str>) -> Self {
        Self {
            status,
            retry_after: retry_after.and_then(|v| parse_retry_after(v, chrono::Utc::now())),
        }
    }
}

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.status)
    }
}

impl std::error::Error for HttpStatus {}

/// `Retry-After` is either delta-seconds or an HTTP-date.
pub(crate) fn parse_retry_after(v: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?.to_utc();
    Some((at - now).to_std().unwrap_or(Duration::ZERO))
}

/// `error_for_status` for reqwest that keeps `Retry-After`.
pub(crate) fn check_reqwest(
    resp: reqwest::Response,
) -> std::result::Result<reqwest::Response, HttpStatus> {
    let st = resp.status();
    if st.is_client_error() || st.is_server_error() {
        let ra = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok());
        return Err(HttpStatus::new(st.as_u16(), ra));
    }
    Ok(resp)
}

/// `error_for_status` for rquest (Asura) that keeps `Retry-After`.
pub(crate) fn check_rquest(
    resp: rquest::Response,
) -> std::result::Result<rquest::Response, HttpStatus> {
    let st = resp.status();
    if st.is_client_error() || st.is_server_error() {
        let ra = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok());
        return Err(HttpStatus::new(st.as_u16(), ra));
    }
    Ok(resp)
}

/// `.check_status()` on either client's response: [`check_reqwest`] /
/// [`check_rquest`] in method position, a drop-in for `error_for_status`.
pub(crate) trait CheckStatus: Sized {
    fn check_status(self) -> std::result::Result<Self, HttpStatus>;
}

impl CheckStatus for reqwest::Response {
    fn check_status(self) -> std::result::Result<Self, HttpStatus> {
        check_reqwest(self)
    }
}

impl CheckStatus for rquest::Response {
    fn check_status(self) -> std::result::Result<Self, HttpStatus> {
        check_rquest(self)
    }
}

/// What [`retry`] does after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// Permanent (4xx other than 429, parse/decode errors, anything unknown).
    Fail,
    /// Transient: wait the usual backoff step.
    Backoff,
    /// Transient, and the server said how long to wait (at most `MAX_RETRY_AFTER`).
    After(Duration),
}

fn classify_status(status: u16, retry_after: Option<Duration>) -> RetryDecision {
    if status == 429 || (500..600).contains(&status) {
        match retry_after {
            Some(d) if d > MAX_RETRY_AFTER => RetryDecision::Fail,
            Some(d) => RetryDecision::After(d),
            None => RetryDecision::Backoff,
        }
    } else {
        RetryDecision::Fail
    }
}

/// Retry only what can succeed on a second try: timeouts, connection
/// errors, 429 and 5xx. 4xx and parse errors fail at once.
pub(crate) fn classify(err: &anyhow::Error) -> RetryDecision {
    for cause in err.chain() {
        if let Some(h) = cause.downcast_ref::<HttpStatus>() {
            return classify_status(h.status, h.retry_after);
        }
        if let Some(e) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(st) = e.status() {
                return classify_status(st.as_u16(), None);
            }
            return if e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() {
                RetryDecision::Backoff
            } else {
                RetryDecision::Fail
            };
        }
        if let Some(e) = cause.downcast_ref::<rquest::Error>() {
            if let Some(st) = e.status() {
                return classify_status(st.as_u16(), None);
            }
            return if e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() {
                RetryDecision::Backoff
            } else {
                RetryDecision::Fail
            };
        }
    }
    RetryDecision::Fail
}

const BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Up to 3 attempts with 1 s, 2 s backoff (or the server's `Retry-After`; one
/// above 60 s fails immediately), but only for errors [`classify`] calls transient.
///
/// Returns the first Ok result, or the error that ended the attempts.
///
/// # Example
/// ```rust,ignore
/// let resp = retry(|| client.get(url).send()).await?;
/// ```
pub async fn retry<F, Fut, T>(f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    retry_with(&BACKOFF, f).await
}

async fn retry_with<F, Fut, T>(delays: &[Duration], mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        let err = match f().await {
            Ok(val) => return Ok(val),
            Err(e) => e,
        };
        attempt += 1;
        if attempt >= delays.len() {
            return Err(err);
        }
        match classify(&err) {
            RetryDecision::Fail => return Err(err),
            RetryDecision::Backoff => tokio::time::sleep(delays[attempt - 1]).await,
            RetryDecision::After(d) => tokio::time::sleep(d).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Submodule declarations (populated in later plans/phases)
// ---------------------------------------------------------------------------

pub mod mangadex; // Plan 02
pub use mangadex::MangaDexScraper;

pub mod mangack; // Phase 3 Plan 01
pub use mangack::MangackScraper;

pub mod asura;
pub use asura::AsuraScraper;

pub mod coordinator; // Phase 5 Plan 02
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
        ))
        .unwrap()
    }

    #[test]
    fn registry_excludes_a_disabled_source() {
        let cfg = config_with(
            "[sources.mangadex]\nbase_url = \"x\"\nenabled = true\n\
             [sources.mangack]\nbase_url = \"x\"\nenabled = false\n",
        );
        let reg = build_registry(&cfg);
        assert!(reg.contains_key("mangadex"));
        assert!(
            !reg.contains_key("mangack"),
            "disabled source must not be in the registry"
        );
        assert!(
            !reg.contains_key("asura"),
            "unconfigured source must not be in the registry"
        );
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
        assert_eq!(
            first_img(r#"<img src="https://c/4.png">"#),
            Some("https://c/4.png".into())
        );
        assert_eq!(
            first_img(r#"<img src="data:image/gif;base64,R0" data-src=" ">"#),
            None
        );
    }

    // --- M9: retry classification ---

    fn status(code: u16, retry_after: Option<&str>) -> anyhow::Error {
        anyhow::Error::new(HttpStatus::new(code, retry_after)).context("GET https://x failed")
    }

    #[test]
    fn client_errors_and_parse_errors_fail_immediately() {
        assert_eq!(classify(&status(404, None)), RetryDecision::Fail);
        assert_eq!(classify(&status(403, None)), RetryDecision::Fail);
        assert_eq!(classify(&status(400, Some("5"))), RetryDecision::Fail);
        assert_eq!(
            classify(&anyhow::anyhow!("Asura: no <h1> title")),
            RetryDecision::Fail
        );
    }

    #[test]
    fn server_errors_back_off() {
        assert_eq!(classify(&status(500, None)), RetryDecision::Backoff);
        assert_eq!(classify(&status(502, None)), RetryDecision::Backoff);
        assert_eq!(
            classify(&status(503, Some("7"))),
            RetryDecision::After(Duration::from_secs(7))
        );
    }

    #[test]
    fn too_many_requests_honours_retry_after_and_fails_beyond_the_cap() {
        assert_eq!(classify(&status(429, None)), RetryDecision::Backoff);
        assert_eq!(
            classify(&status(429, Some("5"))),
            RetryDecision::After(Duration::from_secs(5))
        );
        assert_eq!(
            classify(&status(429, Some("60"))),
            RetryDecision::After(MAX_RETRY_AFTER)
        );
        assert_eq!(classify(&status(429, Some("61"))), RetryDecision::Fail);
        assert_eq!(classify(&status(503, Some("3600"))), RetryDecision::Fail);
        assert_eq!(
            classify(&status(429, Some("garbage"))),
            RetryDecision::Backoff
        );
    }

    #[test]
    fn retry_after_http_date() {
        let now = chrono::DateTime::parse_from_rfc2822("Wed, 21 Oct 2015 07:28:00 GMT")
            .unwrap()
            .to_utc();
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:10 GMT", now),
            Some(Duration::from_secs(10)),
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:27:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after(" 12 ", now),
            Some(Duration::from_secs(12))
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    /// A local port with nothing listening (bind, read the port, close).
    fn closed_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// Serve one canned raw HTTP response per connection on a local port.
    async fn serve(raw: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(raw.as_bytes()).await;
            }
        });
        port
    }

    /// Accept connections and never answer.
    async fn silent() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = l.accept().await {
                held.push(sock);
            }
        });
        port
    }

    #[tokio::test]
    async fn reqwest_transport_errors_back_off_and_decode_errors_fail() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();

        let e = client
            .get(format!("http://127.0.0.1:{}/", closed_port()))
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            classify(&anyhow::Error::new(e)),
            RetryDecision::Backoff,
            "connection refused"
        );

        let e = client
            .get(format!("http://127.0.0.1:{}/", silent().await))
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            classify(&anyhow::Error::new(e)),
            RetryDecision::Backoff,
            "timeout"
        );

        let port =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").await;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        let e = resp.json::<serde_json::Value>().await.unwrap_err();
        assert_eq!(
            classify(&anyhow::Error::new(e)),
            RetryDecision::Fail,
            "JSON decode"
        );
    }

    #[tokio::test]
    async fn status_checks_carry_retry_after_for_both_clients() {
        let port = serve("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 9\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let url = format!("http://127.0.0.1:{port}/");

        let resp = reqwest::get(&url).await.unwrap();
        let e = anyhow::Error::new(check_reqwest(resp).unwrap_err());
        assert_eq!(classify(&e), RetryDecision::After(Duration::from_secs(9)));

        let resp = rquest::Client::new().get(&url).send().await.unwrap();
        let e = anyhow::Error::new(check_rquest(resp).unwrap_err());
        assert_eq!(classify(&e), RetryDecision::After(Duration::from_secs(9)));
    }

    #[tokio::test]
    async fn rquest_connection_refused_backs_off() {
        let client = rquest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let e = client
            .get(format!("http://127.0.0.1:{}/", closed_port()))
            .send()
            .await
            .unwrap_err();
        assert_eq!(classify(&anyhow::Error::new(e)), RetryDecision::Backoff);
    }

    #[tokio::test]
    async fn retry_stops_on_permanent_errors_and_retries_transient_ones() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let delays = [Duration::ZERO; 3];

        let calls = AtomicUsize::new(0);
        let r: Result<()> = retry_with(&delays, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(status(404, None))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "404 must not be retried");

        let calls = AtomicUsize::new(0);
        let r: Result<()> = retry_with(&delays, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(status(503, None))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "503 is retried up to the attempt limit"
        );

        let calls = AtomicUsize::new(0);
        let r = retry_with(&delays, || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(status(500, None))
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 7);
    }

    #[test]
    fn mangack_scraper_instantiates() {
        let scraper = MangackScraper::new();
        assert_eq!(scraper.source_name(), "mangack");
    }
}

#[cfg(test)]
mod collision_tests {
    use super::*;

    fn ch(n: f64, url: &str) -> ChapterData {
        ChapterData {
            number: n,
            title: None,
            url: url.into(),
            released_at: None,
        }
    }

    #[test]
    fn stored_url_wins_a_season_reset_either_order() {
        let stored =
            HashMap::from([(1f64.to_bits(), "https://m/chapter/x-chapter-1/".to_string())]);
        for page in [
            vec![
                ch(1.0, "https://m/chapter/x-s2-chapter-1/"),
                ch(2.0, "https://m/chapter/x-chapter-2/"),
                ch(1.0, "https://m/chapter/x-chapter-1/"),
            ],
            vec![
                ch(1.0, "https://m/chapter/x-chapter-1/"),
                ch(2.0, "https://m/chapter/x-chapter-2/"),
                ch(1.0, "https://m/chapter/x-s2-chapter-1/"),
            ],
        ] {
            let (kept, note) = resolve_collisions(page, &stored);
            assert_eq!(kept.len(), 2);
            assert!(kept
                .iter()
                .any(|c| c.url == "https://m/chapter/x-chapter-1/"));
            assert!(!kept.iter().any(|c| c.url.contains("s2")));
            let note = note.expect("collision must be reported");
            assert!(note.contains("2 chapters share no. 1"), "{note}");
            assert!(note.contains("/chapter/x-chapter-1/"), "{note}");
        }
    }

    #[test]
    fn without_a_stored_url_the_first_listed_wins() {
        let (kept, note) = resolve_collisions(
            vec![ch(1.0, "https://m/a-1"), ch(1.0, "https://m/b-1")],
            &HashMap::new(),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url, "https://m/a-1");
        assert!(note.is_some());
    }

    #[test]
    fn same_url_twice_is_not_a_conflict() {
        let (kept, note) = resolve_collisions(
            vec![ch(3.0, "https://m/c-3"), ch(3.0, "https://m/c-3")],
            &HashMap::new(),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(note, None);
    }

    #[test]
    fn a_moved_url_alone_is_not_a_conflict() {
        // Asura hash rotation: the stored URL is gone from the page.
        let stored = HashMap::from([(
            5f64.to_bits(),
            "https://a/comics/s-aaaaaaaa/chapter/5".to_string(),
        )]);
        let (kept, note) = resolve_collisions(
            vec![ch(5.0, "https://a/comics/s-bbbbbbbb/chapter/5")],
            &stored,
        );
        assert_eq!(kept[0].url, "https://a/comics/s-bbbbbbbb/chapter/5");
        assert_eq!(note, None);
    }

    #[test]
    fn several_colliding_numbers_give_one_note() {
        let (kept, note) = resolve_collisions(
            vec![
                ch(1.0, "https://m/a-1"),
                ch(1.0, "https://m/b-1"),
                ch(2.0, "https://m/a-2"),
                ch(2.0, "https://m/b-2"),
                ch(2.0, "https://m/c-2"),
            ],
            &HashMap::new(),
        );
        assert_eq!(kept.len(), 2);
        let note = note.unwrap();
        assert!(note.starts_with("2 chapters share no. 1"), "{note}");
        assert!(note.ends_with("(+1 more numbers)"), "{note}");
    }
}
