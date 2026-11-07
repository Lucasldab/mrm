//! Handler tests. DB-backed ones use the throwaway Postgres of `make test`
//! and skip without it; auth, search and token tests need no database.
//! Scrapers and the image fetcher are fakes, so nothing touches the network.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Method, Request as HttpRequest};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::db::tests::{series, test_pool};
use crate::scraper::{Scraper, SeriesData};

const TOKEN: &str = "s3cret-token";

/// A PNG signature is all `image::guess_format` needs.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-fake-png-body";

/// Scraper fake: search answers one hit or fails; get_series returns
/// `series(chapters, url)`; every chapter has two pages.
struct Fake {
    name: &'static str,
    fail: bool,
    chapters: usize,
    page_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Scraper for Fake {
    fn source_name(&self) -> &'static str {
        self.name
    }
    async fn search(&self, q: &str) -> Result<Vec<SearchResult>> {
        if self.fail {
            anyhow::bail!("down");
        }
        Ok(vec![SearchResult {
            title: format!("{} {q}", self.name),
            cover_url: Some(format!("https://{}/c.jpg", self.name)),
            source_url: format!("https://{}/{q}", self.name),
            pub_status: "ongoing".into(),
            source: self.name.into(),
        }])
    }
    async fn get_series(&self, url: &str) -> Result<SeriesData> {
        if self.fail {
            anyhow::bail!("down");
        }
        let mut s = series(self.chapters, url);
        s.cover_url = Some("https://img/cover.jpg".into());
        s.description = Some("about".into());
        Ok(s)
    }
    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>> {
        self.page_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            anyhow::bail!("down");
        }
        Ok(vec![
            format!("{chapter_url}/p0.png"),
            format!("{chapter_url}/p1.html"),
        ])
    }
}

/// Fetcher fake: `.html` URLs answer an HTML page, anything else a PNG.
#[derive(Default)]
struct FakeFetcher {
    calls: Mutex<Vec<(String, String, Option<String>)>>,
}

#[async_trait]
impl Fetcher for FakeFetcher {
    async fn fetch(&self, source: &str, url: &str, referer: Option<&str>) -> Result<Fetched> {
        self.calls
            .lock()
            .unwrap()
            .push((source.into(), url.into(), referer.map(str::to_string)));
        if url.ends_with(".html") {
            return Ok(Fetched {
                bytes: Bytes::from_static(b"<html>challenge</html>"),
                content_type: Some("text/html".into()),
            });
        }
        Ok(Fetched {
            bytes: Bytes::from_static(PNG),
            content_type: Some("image/png".into()),
        })
    }
}

struct Harness {
    app: Router,
    pool: PgPool,
    fetcher: Arc<FakeFetcher>,
    page_calls: Arc<AtomicUsize>,
    cache: tempfile::TempDir,
}

fn build(pool: PgPool, sources: &[(&'static str, bool)]) -> Harness {
    let page_calls = Arc::new(AtomicUsize::new(0));
    let mut reg = Registry::new();
    for &(name, fail) in sources {
        reg.insert(
            name,
            Box::new(Fake {
                name,
                fail,
                chapters: 3,
                page_calls: page_calls.clone(),
            }),
        );
    }
    let fetcher = Arc::new(FakeFetcher::default());
    let cache = tempfile::tempdir().unwrap();
    let state = AppState::new(
        pool.clone(),
        Arc::new(reg),
        fetcher.clone(),
        TOKEN.into(),
        cache.path(),
    );
    Harness {
        app: router(Arc::new(state)),
        pool,
        fetcher,
        page_calls,
        cache,
    }
}

/// Harness without a database (the pool never connects).
fn no_db(sources: &[(&'static str, bool)]) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .unwrap();
    build(pool, sources)
}

async fn with_db(sources: &[(&'static str, bool)]) -> Option<Harness> {
    Some(build(test_pool().await?, sources))
}

struct Resp {
    status: StatusCode,
    ctype: Option<String>,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.body))
    }
}

impl Harness {
    async fn call(
        &self,
        method: Method,
        uri: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        let mut req = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = auth {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        Resp {
            status,
            ctype,
            body,
        }
    }
    async fn get(&self, uri: &str) -> Resp {
        self.call(Method::GET, uri, Some(TOKEN), None).await
    }
    async fn post(&self, uri: &str, body: Value) -> Resp {
        self.call(Method::POST, uri, Some(TOKEN), Some(body)).await
    }
    async fn add(&self, n: usize, url: &str) -> i64 {
        db::insert_manhwa_with_chapters(&self.pool, &series(n, url), "mangadex")
            .await
            .unwrap()
    }
    async fn chapter_ids(&self, id: i64) -> Vec<i64> {
        db::fetch_chapters(&self.pool, id)
            .await
            .unwrap()
            .iter()
            .map(|c| c.id)
            .collect()
    }
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

const SERIES_KEYS: [&str; 11] = [
    "cover",
    "failing",
    "id",
    "latest_release",
    "note",
    "pub_status",
    "source",
    "status",
    "title",
    "total",
    "unread",
];

fn sorted(mut v: Vec<&str>) -> Vec<String> {
    v.sort();
    v.into_iter().map(String::from).collect()
}

// --- auth -------------------------------------------------------------------

#[tokio::test]
async fn health_needs_no_token() {
    let h = no_db(&[]);
    let r = h.call(Method::GET, "/api/health", None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })
    );
}

#[tokio::test]
async fn api_needs_the_right_bearer_token() {
    let h = no_db(&[("mangadex", false)]);
    for auth in [None, Some("wrong"), Some("s3cret-tokeN"), Some("")] {
        let r = h.call(Method::GET, "/api/search?q=x", auth, None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "auth {auth:?}");
        assert_eq!(r.json(), json!({ "error": "unauthorized" }));
    }
    // Not a Bearer header at all.
    let req = HttpRequest::get("/api/search?q=x")
        .header(header::AUTHORIZATION, TOKEN)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        h.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // Unknown /api routes are not probeable without the token either.
    assert_eq!(
        h.call(Method::GET, "/api/nope", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );

    assert_eq!(h.get("/api/search?q=x").await.status, StatusCode::OK);
    let r = h.get("/api/nope").await;
    assert_eq!(
        (r.status, r.json()),
        (StatusCode::NOT_FOUND, json!({ "error": "not found" }))
    );
}

#[test]
fn token_comparison() {
    assert!(token_eq(b"abc", b"abc"));
    assert!(!token_eq(b"abd", b"abc"));
    assert!(!token_eq(b"ab", b"abc"));
    assert!(!token_eq(b"", b"abc"));
}

#[test]
fn token_file_must_be_private_and_non_empty() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("serve_token");
    assert!(read_token(&p).is_err(), "missing file");

    std::fs::write(&p, "  tok123\n").unwrap();
    for mode in [0o644, 0o640, 0o604] {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        let e = read_token(&p).unwrap_err().to_string();
        assert!(
            e.contains("group/other") && !e.contains("tok123"),
            "mode {mode:o}: {e}"
        );
    }
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(read_token(&p).unwrap(), "tok123");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(read_token(&p).unwrap(), "tok123");

    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&p, " \n").unwrap();
    assert!(read_token(&p).unwrap_err().to_string().contains("empty"));
}

// --- search -----------------------------------------------------------------

#[tokio::test]
async fn search_is_an_object_of_hits_and_errors() {
    let h = no_db(&[("asura", false), ("mangack", true), ("mangadex", false)]);
    let r = h.get("/api/search?q=solo%20leveling").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({
            "hits": [
                { "title": "mangadex solo leveling", "source": "mangadex", "source_url": "https://mangadex/solo leveling",
                  "pub_status": "ongoing", "cover_url": "https://mangadex/c.jpg" },
                { "title": "asura solo leveling", "source": "asura", "source_url": "https://asura/solo leveling",
                  "pub_status": "ongoing", "cover_url": "https://asura/c.jpg" },
            ],
            "errors": ["MangaCK: down"],
        })
    );
    for uri in ["/api/search", "/api/search?q=", "/api/search?q=%20"] {
        let r = h.get(uri).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(r.json()["error"].is_string());
    }
}

// --- library / detail -------------------------------------------------------

#[tokio::test]
async fn library_shape_and_order() {
    let Some(h) = with_db(&[]).await else { return };
    let a = h.add(3, "https://x/a").await;
    let b = h.add(2, "https://x/b").await;
    db::record_poll_failure(&h.pool, a, "HTTP 503")
        .await
        .unwrap();
    // Touch b last so it sorts first (updated_at desc, like the TUI).
    db::apply_picker_status(&h.pool, b, &Status::Paused)
        .await
        .unwrap();

    let r = h.get("/api/library").await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    let list = v.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["id"], json!(b));
    for s in list {
        assert_eq!(keys(s), sorted(SERIES_KEYS.to_vec()));
    }
    assert_eq!(
        list[0],
        json!({
            "id": b, "title": "Series https://x/b", "source": "mangadex", "status": "paused",
            "pub_status": "ongoing", "unread": 2, "total": 2, "latest_release": "2026-11-05",
            "cover": format!("/api/series/{b}/cover"), "failing": false, "note": null,
        })
    );
    assert_eq!(
        (&list[1]["failing"], &list[1]["note"]),
        (&json!(true), &json!("HTTP 503"))
    );
}

#[tokio::test]
async fn series_detail_lists_chapters_ascending() {
    let Some(h) = with_db(&[]).await else { return };
    let id = h.add(3, "https://x/d").await;
    let v = h.get(&format!("/api/series/{id}")).await.json();
    let mut want = SERIES_KEYS.to_vec();
    want.extend(["description", "chapters"]);
    assert_eq!(keys(&v), sorted(want));
    let chs = v["chapters"].as_array().unwrap();
    assert_eq!(
        chs.iter()
            .map(|c| c["number"].as_f64().unwrap())
            .collect::<Vec<_>>(),
        vec![1.0, 2.0, 3.0]
    );
    assert_eq!(
        keys(&chs[0]),
        sorted(vec!["id", "number", "title", "released_at", "read"])
    );
    assert_eq!(
        (&chs[0]["read"], &chs[0]["title"], &chs[0]["released_at"]),
        (&json!(false), &json!(null), &json!("2026-11-05"))
    );

    let r = h.get("/api/series/999999").await;
    assert_eq!(
        (r.status, r.json()),
        (
            StatusCode::NOT_FOUND,
            json!({ "error": "series not found" })
        )
    );
}

// --- read / unread / status -------------------------------------------------

#[tokio::test]
async fn read_unread_round_trip_changes_completed_and_status() {
    let Some(h) = with_db(&[]).await else { return };
    let id = h.add(6, "https://x/r").await;
    let chs = h.chapter_ids(id).await;
    for c in &chs[..5] {
        let r = h
            .post(&format!("/api/chapters/{c}/read"), json!({ "read": true }))
            .await;
        assert_eq!(r.status, StatusCode::OK);
        let v = r.json();
        assert_eq!(keys(&v), sorted(vec!["chapter_id", "read", "series"]));
        assert_eq!((&v["chapter_id"], &v["read"]), (&json!(c), &json!(true)));
    }
    let v = h
        .post(
            &format!("/api/chapters/{}/read", chs[4]),
            json!({ "read": true }),
        )
        .await
        .json();
    assert_eq!(
        (&v["series"]["status"], &v["series"]["unread"]),
        (&json!("reading"), &json!(1)),
        "idempotent"
    );
    let detail = h.get(&format!("/api/series/{id}")).await.json();
    let read: Vec<bool> = detail["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["read"].as_bool().unwrap())
        .collect();
    assert_eq!(read, vec![true, true, true, true, true, false]);

    let v = h
        .post(
            &format!("/api/chapters/{}/read", chs[0]),
            json!({ "read": false }),
        )
        .await
        .json();
    assert_eq!(
        (&v["read"], &v["series"]["status"], &v["series"]["unread"]),
        (&json!(false), &json!("looked_into"), &json!(2))
    );
    assert!(!db::fetch_chapters(&h.pool, id).await.unwrap()[0].completed);

    let r = h
        .post("/api/chapters/999999/read", json!({ "read": true }))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    for bad in [json!({}), json!({ "read": "yes" })] {
        let r = h.post(&format!("/api/chapters/{}/read", chs[0]), bad).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.json()["error"].is_string());
    }
}

#[tokio::test]
async fn status_endpoint_is_the_picker() {
    let Some(h) = with_db(&[]).await else { return };
    let id = h.add(3, "https://x/s").await;
    let r = h
        .post(
            &format!("/api/series/{id}/status"),
            json!({ "status": "dropped" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(keys(&v), sorted(SERIES_KEYS.to_vec()));
    assert_eq!(v["status"], "dropped");
    assert!(db::fetch_manhwa(&h.pool, id).await.unwrap().status_override);

    let v = h
        .post(
            &format!("/api/series/{id}/status"),
            json!({ "status": "up_to_date" }),
        )
        .await
        .json();
    assert_eq!(
        (&v["status"], &v["unread"]),
        (&json!("up_to_date"), &json!(0)),
        "up_to_date marks all read"
    );
    assert!(!db::fetch_manhwa(&h.pool, id).await.unwrap().status_override);

    let r = h
        .post(
            &format!("/api/series/{id}/status"),
            json!({ "status": "Reading" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h
        .post("/api/series/999999/status", json!({ "status": "reading" }))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

// --- refresh / add ----------------------------------------------------------

#[tokio::test]
async fn refresh_polls_the_series_without_claiming() {
    let Some(h) = with_db(&[("mangadex", false)]).await else {
        return;
    };
    let id = h.add(1, "https://x/f").await;
    let r = h
        .post(&format!("/api/series/{id}/refresh"), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["chapters"].as_array().unwrap().len(), 3);
    assert_eq!(v["description"], "about");
    let unclaimed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chapter WHERE manhwa_id = $1 AND notified_at IS NULL",
    )
    .bind(id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(unclaimed, 2, "new chapters stay for the daemon's push");
}

#[tokio::test]
async fn failed_refresh_is_502_and_marks_the_series_failing() {
    let Some(h) = with_db(&[("mangadex", true)]).await else {
        return;
    };
    let id = h.add(1, "https://x/ff").await;
    let r = h
        .call(
            Method::POST,
            &format!("/api/series/{id}/refresh"),
            Some(TOKEN),
            None,
        )
        .await;
    assert_eq!(
        (r.status, r.json()),
        (StatusCode::BAD_GATEWAY, json!({ "error": "down" }))
    );
    let v = h.get(&format!("/api/series/{id}")).await.json();
    assert_eq!((&v["failing"], &v["note"]), (&json!(true), &json!("down")));
}

#[tokio::test]
async fn add_series_then_409_on_duplicate() {
    let Some(h) = with_db(&[("mangadex", false)]).await else {
        return;
    };
    let body = json!({ "source": "mangadex", "source_url": "https://x/new" });
    let r = h.post("/api/series", body.clone()).await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(
        (&v["title"], &v["status"], &v["total"]),
        (
            &json!("Series https://x/new"),
            &json!("looked_into"),
            &json!(3)
        )
    );
    assert_eq!(v["chapters"].as_array().unwrap().len(), 3);

    let r = h.post("/api/series", body).await;
    assert_eq!(
        (r.status, r.json()),
        (
            StatusCode::CONFLICT,
            json!({ "error": "already in library" })
        )
    );

    let r = h
        .post(
            "/api/series",
            json!({ "source": "asura", "source_url": "https://x/other" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "source not enabled here");
}

// --- pages / covers ---------------------------------------------------------

#[tokio::test]
async fn pages_resolve_proxy_and_cache() {
    let Some(h) = with_db(&[("mangadex", false)]).await else {
        return;
    };
    let id = h.add(2, "https://x/p").await;
    let c = h.chapter_ids(id).await[0];

    let r = h.get(&format!("/api/chapters/{c}/pages")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({
            "chapter_id": c,
            "pages": [format!("/api/chapters/{c}/pages/0"), format!("/api/chapters/{c}/pages/1")],
        })
    );

    let r = h.get(&format!("/api/chapters/{c}/pages/0")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!((r.ctype.as_deref(), &r.body[..]), (Some("image/png"), PNG));
    let calls = h.fetcher.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(
            "mangadex".to_string(),
            "https://x/p/ch/1/p0.png".to_string(),
            Some("https://x/p/ch/1".to_string()),
        )],
        "chapter URL goes out as Referer"
    );
    assert_eq!(
        h.page_calls.load(Ordering::SeqCst),
        1,
        "image list is reused within the TTL"
    );
    assert!(h.cache.path().join(format!("serve-pages/{c}/0")).is_file());

    // Served from disk: no new upstream fetch.
    let r = h.get(&format!("/api/chapters/{c}/pages/0")).await;
    assert_eq!(
        (r.status, r.ctype.as_deref(), &r.body[..]),
        (StatusCode::OK, Some("image/png"), PNG)
    );
    assert_eq!(h.fetcher.calls.lock().unwrap().len(), 1);

    // Upstream answered HTML: 502, not cached.
    let r = h.get(&format!("/api/chapters/{c}/pages/1")).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert!(r.json()["error"]
        .as_str()
        .unwrap()
        .contains("not return an image"));
    assert!(!h.cache.path().join(format!("serve-pages/{c}/1")).exists());

    assert_eq!(
        h.get(&format!("/api/chapters/{c}/pages/2")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.get("/api/chapters/999999/pages").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn pages_of_a_failing_source_are_502() {
    let Some(h) = with_db(&[("mangadex", true)]).await else {
        return;
    };
    let id = h.add(1, "https://x/pf").await;
    let c = h.chapter_ids(id).await[0];
    let r = h.get(&format!("/api/chapters/{c}/pages")).await;
    assert_eq!(
        (r.status, r.json()),
        (StatusCode::BAD_GATEWAY, json!({ "error": "down" }))
    );
}

#[tokio::test]
async fn cover_is_proxied_and_cached() {
    let Some(h) = with_db(&[("mangadex", false)]).await else {
        return;
    };
    let mut s = series(1, "https://x/cv");
    s.cover_url = Some("https://img/cv.png".into());
    let id = db::insert_manhwa_with_chapters(&h.pool, &s, "mangadex")
        .await
        .unwrap();
    let bare = h.add(1, "https://x/bare").await;

    let r = h.get(&format!("/api/series/{id}/cover")).await;
    assert_eq!(
        (r.status, r.ctype.as_deref(), &r.body[..]),
        (StatusCode::OK, Some("image/png"), PNG)
    );
    assert_eq!(
        h.fetcher.calls.lock().unwrap()[0].2,
        None,
        "covers go without Referer"
    );
    assert!(h.cache.path().join(format!("serve-covers/{id}")).is_file());
    h.get(&format!("/api/series/{id}/cover")).await;
    assert_eq!(
        h.fetcher.calls.lock().unwrap().len(),
        1,
        "second hit served from disk"
    );

    assert_eq!(
        h.get(&format!("/api/series/{bare}/cover")).await.status,
        StatusCode::NOT_FOUND
    );
}

// --- path safety ------------------------------------------------------------

#[tokio::test]
async fn ids_are_integers_only() {
    // Rejected at extraction, before any database access or file path.
    let h = no_db(&[("mangadex", false)]);
    for uri in [
        "/api/series/abc",
        "/api/series/1.5/cover",
        "/api/series/..%2F..%2Fetc%2Fpasswd/cover",
        "/api/chapters/..%2F1/pages",
        "/api/chapters/1/pages/..%2F..%2Fx",
        "/api/chapters/1/pages/-1",
        "/api/chapters/1/pages/0x1",
        "/api/chapters/%2Fetc%2Fpasswd/pages/0",
        "/api/chapters/99999999999999999999/pages/0",
    ] {
        let r = h.get(uri).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(r.json()["error"].is_string(), "{uri}");
    }
    // Literal traversal never routes anywhere.
    for uri in [
        "/api/chapters/1/pages/../../../etc/passwd",
        "/api/series/1/../../x",
    ] {
        assert_ne!(h.get(uri).await.status, StatusCode::OK, "{uri}");
    }
    assert!(
        std::fs::read_dir(h.cache.path()).unwrap().next().is_none(),
        "nothing written"
    );
}
