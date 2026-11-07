//! `mrm --serve`: the JSON API mrm-phone reads the shared library through.
//!
//! Runs on hearth beside `mrm --daemon` and shares its Postgres library,
//! `db.rs` and the scraper registry. It never polls and never claims
//! chapters for notification; the daemon stays the only notifier. Contract:
//! homelab/docs/superpowers/specs/2026-11-07-mrm-phone-design.md (dotfiles).
//!
//! Every `/api/*` route except `/api/health` needs `Authorization: Bearer
//! <token>`, the token being `~/.config/mrm/serve_token`. Path ids are
//! parsed as integers only, and the disk caches build their paths from those
//! integers alone, so no request can name a file.

mod cache;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{FromRequest, FromRequestParts, Path, Query, Request, State},
    http::{header, request::Parts, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sqlx::PgPool;

use crate::db;
use crate::scraper::{search_registry, source_unavailable, Registry, SearchResult};
use crate::types::{Chapter, Manhwa, Status};

pub use cache::DiskCache;

/// Default listen address (tailnet exposure is `tailscale serve` on hearth).
pub const DEFAULT_ADDR: &str = "127.0.0.1:8102";
/// How long a chapter's resolved image URL list is reused.
const PAGE_LIST_TTL: Duration = Duration::from_secs(15 * 60);
/// Size cap of the page disk cache (LRU by mtime).
const PAGE_CACHE_CAP: u64 = 2 * 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Startup: token, addresses, cache roots
// ---------------------------------------------------------------------------

/// `~/.config/mrm/serve_token`.
pub fn token_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/mrm/serve_token"))
}

/// Read the bearer token: whitespace trimmed, refused when missing, empty or
/// readable by group/other. Errors name the file, never its contents.
pub fn read_token(path: &FsPath) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("token file {} is missing", path.display()))?;
    if meta.permissions().mode() & 0o044 != 0 {
        anyhow::bail!(
            "token file {} is readable by group/other — chmod 600 it",
            path.display()
        );
    }
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("could not read token file {}", path.display()))?
        .trim()
        .to_string();
    if token.is_empty() {
        anyhow::bail!("token file {} is empty", path.display());
    }
    Ok(token)
}

/// `~/.cache/mrm`, parent of `serve-pages/` and `serve-covers/`.
pub fn cache_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".cache/mrm"))
}

/// Constant-time token comparison (the length is not secret).
fn token_eq(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    let diff = given
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(diff) == 0
}

// ---------------------------------------------------------------------------
// Image fetching (a trait so tests run without the network)
// ---------------------------------------------------------------------------

/// An upstream image response.
pub struct Fetched {
    pub bytes: Bytes,
    pub content_type: Option<String>,
}

// async_trait's generated `#[must_use]` trips this lint (as on `Scraper`).
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Fetcher: Send + Sync {
    /// GET `url` with `source`'s image rules (UA) and an optional Referer.
    async fn fetch(&self, source: &str, url: &str, referer: Option<&str>) -> Result<Fetched>;
}

/// The real fetcher: the TUI reader's client and Referer rules
/// (`scraper::image_client` / `fetch_image`), one client per source.
#[derive(Default)]
pub struct HttpFetcher {
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl HttpFetcher {
    fn client(&self, source: &str) -> Result<reqwest::Client> {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = clients.get(source) {
            return Ok(c.clone());
        }
        let c = crate::scraper::image_client(source)?;
        clients.insert(source.to_string(), c.clone());
        Ok(c)
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, source: &str, url: &str, referer: Option<&str>) -> Result<Fetched> {
        let client = self.client(source)?;
        let resp = crate::scraper::fetch_image(&client, url, referer)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
        Ok(Fetched {
            bytes,
            content_type,
        })
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A chapter's resolved image URLs plus what fetching them needs.
struct PageList {
    at: Instant,
    source: String,
    chapter_url: String,
    urls: Vec<String>,
}

pub struct AppState {
    pool: PgPool,
    registry: Arc<Registry>,
    fetcher: Arc<dyn Fetcher>,
    token: String,
    pages: DiskCache,
    covers: DiskCache,
    page_lists: Mutex<HashMap<i64, Arc<PageList>>>,
}

impl AppState {
    /// `cache_root` holds `serve-pages/` (capped) and `serve-covers/`.
    pub fn new(
        pool: PgPool,
        registry: Arc<Registry>,
        fetcher: Arc<dyn Fetcher>,
        token: String,
        cache_root: &FsPath,
    ) -> Self {
        Self {
            pool,
            registry,
            fetcher,
            token,
            pages: DiskCache::new(cache_root.join("serve-pages"), Some(PAGE_CACHE_CAP)),
            covers: DiskCache::new(cache_root.join("serve-covers"), None),
            page_lists: Mutex::new(HashMap::new()),
        }
    }
}

type St = State<Arc<AppState>>;

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/library", get(library))
        .route("/series", post(add_series))
        .route("/series/{id}", get(series_detail))
        .route("/series/{id}/status", post(set_status))
        .route("/series/{id}/refresh", post(refresh))
        .route("/series/{id}/cover", get(cover))
        .route("/chapters/{id}/read", post(set_read))
        .route("/chapters/{id}/pages", get(pages))
        .route("/chapters/{id}/pages/{n}", get(page))
        .route("/search", get(search))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/api/health", get(health))
        .nest("/api", api)
        .fallback(not_found)
        .layer(middleware::from_fn(log_request))
        .with_state(state)
}

/// Bind `addr` and serve until Ctrl-C / SIGTERM.
pub async fn run(state: AppState, addr: &str) -> Result<()> {
    let sweep = state.pages.clone();
    tokio::task::spawn_blocking(move || sweep.sweep());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("could not bind {addr}"))?;
    eprintln!(
        "mrm: serve mode — listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, router(Arc::new(state)))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    eprintln!("mrm: serve stopped");
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let term = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}

/// One stderr line per request (path only: no query, no headers). Successful
/// image GETs are skipped, they would drown the journal while reading.
async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let resp = next.run(req).await;
    let image = path.contains("/pages/") || path.ends_with("/cover");
    if !(image && resp.status().is_success()) {
        eprintln!(
            "mrm serve: {method} {path} {} {}ms",
            resp.status().as_u16(),
            started.elapsed().as_millis()
        );
    }
    resp
}

async fn require_token(State(st): St, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| token_eq(t.trim().as_bytes(), st.token.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

// ---------------------------------------------------------------------------
// Errors and extractors (every failure is `{"error": "…"}`)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            msg: msg.into(),
        }
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
    /// The source failed us (scrape, image fetch) or is not enabled here.
    fn upstream(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.msg }))).into_response()
    }
}

/// Internal errors: a vanished row is a 404, anything else is logged and 500.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if matches!(
            e.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::RowNotFound)
        ) {
            return Self::not_found("not found");
        }
        eprintln!("mrm serve: internal error: {e:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("not found")
}

/// `Path` whose rejection (a non-integer id, say) is a JSON 400.
struct ApiPath<T>(T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(t)| ApiPath(t))
            .map_err(|e| ApiError::bad_request(e.body_text()))
    }
}

/// `Json` whose rejection is a JSON 400.
struct ApiJson<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(t)| ApiJson(t))
            .map_err(|e| ApiError::bad_request(e.body_text()))
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Series {
    id: i64,
    title: String,
    source: String,
    status: &'static str,
    pub_status: String,
    unread: u32,
    total: u32,
    /// Release date (YYYY-MM-DD) of the newest dated chapter, for the phone's
    /// "new" sort. Older app builds ignore it.
    latest_release: Option<String>,
    cover: String,
    failing: bool,
    note: Option<String>,
}

impl From<&Manhwa> for Series {
    fn from(m: &Manhwa) -> Self {
        let failing = m.failing_since.is_some();
        Self {
            id: m.id,
            title: m.title.clone(),
            source: m.source.clone(),
            status: m.status.as_str(),
            pub_status: m.pub_status.clone(),
            unread: m.unread,
            total: m.total,
            latest_release: m.latest_release.clone(),
            cover: format!("/api/series/{}/cover", m.id),
            failing,
            note: if failing { m.last_error.clone() } else { None },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SeriesDetail {
    #[serde(flatten)]
    series: Series,
    description: Option<String>,
    chapters: Vec<ChapterOut>,
}

#[derive(Debug, Serialize)]
pub struct ChapterOut {
    id: i64,
    number: f64,
    title: Option<String>,
    released_at: Option<String>,
    read: bool,
}

impl From<&Chapter> for ChapterOut {
    fn from(c: &Chapter) -> Self {
        Self {
            id: c.id,
            number: c.number,
            title: c.title.clone(),
            released_at: c.released_at.clone(),
            read: c.completed,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    title: String,
    source: String,
    source_url: String,
    pub_status: String,
    cover_url: Option<String>,
}

impl From<SearchResult> for SearchHit {
    fn from(r: SearchResult) -> Self {
        Self {
            title: r.title,
            source: r.source,
            source_url: r.source_url,
            pub_status: r.pub_status,
            cover_url: r.cover_url,
        }
    }
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

#[derive(Deserialize)]
struct ReadBody {
    read: bool,
}

#[derive(Deserialize)]
struct AddBody {
    source: String,
    source_url: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

async fn library(State(st): St) -> ApiResult<Json<Vec<Series>>> {
    let all = db::fetch_all_manhwa(&st.pool).await?;
    Ok(Json(all.iter().map(Series::from).collect()))
}

async fn fetch_series(st: &AppState, id: i64) -> ApiResult<Manhwa> {
    db::fetch_manhwa(&st.pool, id)
        .await
        .map_err(|e| match ApiError::from(e) {
            e if e.status == StatusCode::NOT_FOUND => ApiError::not_found("series not found"),
            e => e,
        })
}

async fn detail(st: &AppState, id: i64) -> ApiResult<SeriesDetail> {
    let m = fetch_series(st, id).await?;
    let chapters = db::fetch_chapters(&st.pool, id).await?;
    Ok(SeriesDetail {
        series: Series::from(&m),
        description: m.description.clone(),
        chapters: chapters.iter().map(ChapterOut::from).collect(),
    })
}

async fn series_detail(State(st): St, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<SeriesDetail>> {
    Ok(Json(detail(&st, id).await?))
}

/// Same write as the TUI's status picker.
async fn set_status(
    State(st): St,
    ApiPath(id): ApiPath<i64>,
    ApiJson(body): ApiJson<StatusBody>,
) -> ApiResult<Json<Series>> {
    let chosen = Status::all()
        .iter()
        .find(|s| s.as_str() == body.status)
        .ok_or_else(|| ApiError::bad_request(format!("unknown status '{}'", body.status)))?;
    fetch_series(&st, id).await?;
    db::apply_picker_status(&st.pool, id, chosen).await?;
    Ok(Json(Series::from(&fetch_series(&st, id).await?)))
}

/// One poll of the series, like the TUI's `R` on the Detail screen: new
/// chapters through the poll write path (never claimed), then metadata.
async fn refresh(State(st): St, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<SeriesDetail>> {
    let m = fetch_series(&st, id).await?;
    let scraper = st
        .registry
        .get(m.source.as_str())
        .ok_or_else(|| ApiError::upstream(source_unavailable(&m.source)))?;
    let series = match scraper.get_series(&m.source_url).await {
        Ok(s) => s,
        Err(e) => {
            let e = format!("{e:#}");
            db::record_poll_failure(&st.pool, id, &e).await?;
            return Err(ApiError::upstream(e));
        }
    };
    // Fresh pub_status: a daemon poll may have changed it meanwhile.
    let old = fetch_series(&st, id).await?;
    db::apply_poll_result(&st.pool, id, &series, &old.pub_status).await?;
    db::update_manhwa_metadata(&st.pool, id, &series).await?;
    st.covers.remove(&[id]).await;
    Ok(Json(detail(&st, id).await?))
}

#[derive(Serialize)]
struct ReadOut {
    chapter_id: i64,
    read: bool,
    series: Series,
}

/// The TUI Detail screen's mark read (`r`) / mark unread (`u`).
async fn set_read(
    State(st): St,
    ApiPath(id): ApiPath<i64>,
    ApiJson(body): ApiJson<ReadBody>,
) -> ApiResult<Json<ReadOut>> {
    let manhwa_id = db::chapter_manhwa_id(&st.pool, id)
        .await?
        .ok_or_else(|| ApiError::not_found("chapter not found"))?;
    if body.read {
        db::start_chapter(&st.pool, id).await?;
        db::update_scroll(&st.pool, id, 1.0).await?;
    } else {
        db::mark_chapter_unread(&st.pool, id).await?;
    }
    db::recompute_status(&st.pool, manhwa_id).await?;
    let series = Series::from(&fetch_series(&st, manhwa_id).await?);
    Ok(Json(ReadOut {
        chapter_id: id,
        read: body.read,
        series,
    }))
}

/// A chapter's image URLs from the series' scraper, reused for 15 minutes.
async fn page_list(st: &AppState, chapter_id: i64) -> ApiResult<Arc<PageList>> {
    {
        let mut lists = st.page_lists.lock().unwrap_or_else(|e| e.into_inner());
        lists.retain(|_, l| l.at.elapsed() < PAGE_LIST_TTL);
        if let Some(l) = lists.get(&chapter_id) {
            return Ok(l.clone());
        }
    }
    let (source, chapter_url) = db::chapter_source(&st.pool, chapter_id)
        .await?
        .ok_or_else(|| ApiError::not_found("chapter not found"))?;
    let scraper = st
        .registry
        .get(source.as_str())
        .ok_or_else(|| ApiError::upstream(source_unavailable(&source)))?;
    let urls = scraper
        .get_chapter_image_urls(&chapter_url)
        .await
        .map_err(|e| ApiError::upstream(format!("{e:#}")))?;
    if urls.is_empty() {
        return Err(ApiError::upstream("chapter has no images"));
    }
    let list = Arc::new(PageList {
        at: Instant::now(),
        source,
        chapter_url,
        urls,
    });
    st.page_lists
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(chapter_id, list.clone());
    Ok(list)
}

#[derive(Serialize)]
struct PagesOut {
    chapter_id: i64,
    pages: Vec<String>,
}

async fn pages(State(st): St, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<PagesOut>> {
    let list = page_list(&st, id).await?;
    let pages = (0..list.urls.len())
        .map(|n| format!("/api/chapters/{id}/pages/{n}"))
        .collect();
    Ok(Json(PagesOut {
        chapter_id: id,
        pages,
    }))
}

async fn page(State(st): St, ApiPath((id, n)): ApiPath<(i64, u32)>) -> ApiResult<Response> {
    let key = [id, i64::from(n)];
    if let Some(bytes) = st.pages.get(&key).await {
        return Ok(image_response(bytes, None));
    }
    let list = page_list(&st, id).await?;
    let url = list
        .urls
        .get(n as usize)
        .ok_or_else(|| ApiError::not_found("page not found"))?;
    let fetched = st
        .fetcher
        .fetch(&list.source, url, Some(&list.chapter_url))
        .await
        .map_err(|e| ApiError::upstream(format!("page {n}: {e:#}")))?;
    checked_image(&fetched)?;
    st.pages.put(&key, fetched.bytes.clone()).await;
    Ok(image_response(fetched.bytes, fetched.content_type))
}

async fn cover(State(st): St, ApiPath(id): ApiPath<i64>) -> ApiResult<Response> {
    let key = [id];
    if let Some(bytes) = st.covers.get(&key).await {
        return Ok(image_response(bytes, None));
    }
    let m = fetch_series(&st, id).await?;
    let url = m
        .cover_url
        .as_deref()
        .ok_or_else(|| ApiError::not_found("series has no cover"))?;
    let fetched = st
        .fetcher
        .fetch(&m.source, url, None)
        .await
        .map_err(|e| ApiError::upstream(format!("cover: {e:#}")))?;
    checked_image(&fetched)?;
    st.covers.put(&key, fetched.bytes.clone()).await;
    Ok(image_response(fetched.bytes, fetched.content_type))
}

/// Reject what is not an image (a Cloudflare challenge page answered with
/// 200, say) so it is neither served nor cached.
fn checked_image(f: &Fetched) -> ApiResult<()> {
    image::guess_format(&f.bytes)
        .map(|_| ())
        .map_err(|_| ApiError::upstream("upstream did not return an image"))
}

/// Image bytes with the upstream Content-Type, or one sniffed from the magic
/// bytes (cache hits, or an upstream that sent none / a non-image type).
fn image_response(bytes: Bytes, upstream_type: Option<String>) -> Response {
    let ct = upstream_type
        .filter(|t| t.starts_with("image/"))
        .or_else(|| {
            image::guess_format(&bytes)
                .ok()
                .map(|f| f.to_mime_type().to_string())
        })
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let ct = HeaderValue::from_str(&ct)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    ([(header::CONTENT_TYPE, ct)], bytes).into_response()
}

#[derive(Serialize)]
struct SearchOut {
    hits: Vec<SearchHit>,
    errors: Vec<String>,
}

async fn search(
    State(st): St,
    query: Result<Query<SearchQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<SearchOut>> {
    let Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let q = query.q.as_deref().map(str::trim).unwrap_or("");
    if q.is_empty() {
        return Err(ApiError::bad_request("missing query parameter q"));
    }
    let (results, errors) = search_registry(&st.registry, q).await;
    Ok(Json(SearchOut {
        hits: results.into_iter().map(SearchHit::from).collect(),
        errors,
    }))
}

/// Add from search, like the TUI: get_series, then insert (chapters start
/// notified, so adding never pushes).
async fn add_series(
    State(st): St,
    ApiJson(body): ApiJson<AddBody>,
) -> ApiResult<Json<SeriesDetail>> {
    let scraper = st
        .registry
        .get(body.source.as_str())
        .ok_or_else(|| ApiError::bad_request(source_unavailable(&body.source)))?;
    let duplicate = || ApiError::new(StatusCode::CONFLICT, "already in library");
    if db::manhwa_id_by_source_url(&st.pool, &body.source_url)
        .await?
        .is_some()
    {
        return Err(duplicate());
    }
    let series = scraper
        .get_series(&body.source_url)
        .await
        .map_err(|e| ApiError::upstream(format!("{e:#}")))?;
    // The source may have redirected onto a URL that is already tracked.
    if db::manhwa_id_by_source_url(&st.pool, &series.source_url)
        .await?
        .is_some()
    {
        return Err(duplicate());
    }
    let id = db::insert_manhwa_with_chapters(&st.pool, &series, &body.source)
        .await
        .map_err(|e| {
            if e.to_string().starts_with("Already in library") {
                duplicate()
            } else {
                e.into()
            }
        })?;
    Ok(Json(detail(&st, id).await?))
}
