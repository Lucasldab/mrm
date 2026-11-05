# Codebase Scan

**Date:** 2026-04-30  
**Scope:** `crates/mrm/src/` (17 `.rs` files)

---

## (a) Modules and Responsibilities

### Entry point

| File | Responsibility |
|------|---------------|
| `src/main.rs` | CLI arg parsing (`--daemon`, `--once`, default TUI); database URL resolution (`MRM_DATABASE_URL`, else `[db] url_file` from config.toml; 0600 enforced); terminal setup / teardown; `run_tui` / `run_daemon` / `run_once` dispatch; 250ms event loop via `tokio::select!`; startup `tmp/mrm_*` cleanup. |

### Core application

| File | Responsibility |
|------|---------------|
| `src/app.rs` | `App` struct — owns all TUI state (library selection, chapter list, viewer process, image paths, cover caches, config). Dispatches `AppEvent` variants: `Key`, `Tick`, `DataRefreshed`, `ScraperMsg`. Manages the chapter image streaming pipeline (semaphore-limited parallel download → ordered re-assembly → viewer feed). Launches and kills `imv`/`rv` viewer processes via `ManagedChild` RAII wrapper. Hyprland fullscreen-state snapshot/restore around viewer launches. Handles all screen-specific key handlers (Library, Detail, Reader, StatusPicker, Search, Discover). |
| `src/types.rs` | Domain types: `Status` (enum + sort rank + display labels), `SortMode`, `Manhwa`, `Chapter`, `Discovery`, `Screen`, `AppEvent`. No I/O. |
| `src/config.rs` | TOML config loader (CWD → `~/.config/mrm/config.toml`). Structs: `Config`, `SourceConfig`, `NotificationsConfig`, `DbConfig`, `KeysConfig`, `ThemeConfig`, `ImvConfig`, `RvConfig`, `ViewerKind`. Key-string → `KeyCode` parser; hex / named color parser; imv config-file serialiser; rv CLI-args builder. |

### Persistence

| File | Responsibility |
|------|---------------|
| `src/db.rs` | All Postgres access via `sqlx` (rustls). `open_db(url)` runs the embedded migrations in `migrations/` (additive only; `set_ignore_missing(true)` so older binaries keep working). Core queries: `apply_poll_result`, `claim_unnotified`, fetch/insert/delete manhwa; upsert chapters; progress (start/scroll/complete); status recompute; discovery upsert/dismiss/delete; meta key-value store. |

### Scraper subsystem

| File | Responsibility |
|------|---------------|
| `src/scraper/mod.rs` | `Scraper` trait (`search`, `get_series`, `get_chapter_image_urls`, `latest_chapters`). Shared data types: `SeriesData`, `ChapterData`, `SearchResult`, `DiscoveryEntry`. Retry helper: up to 3 attempts with 1s / 2s / 4s exponential backoff. |
| `src/scraper/mangadex.rs` | MangaDex REST API (no auth). Endpoints: `/manga` (search + latest), `/manga/{id}` (series), `/manga/{id}/feed` (paginated chapters, 250ms inter-page delay), `/at-home/server/{id}` (image CDN redirect). Deduplicates chapters by f64 bit representation. |
| `src/scraper/mangack.rs` | MangaCK HTML scraper (WordPress). CSS selectors via `scraper` crate for series/chapter/image parsing. Regex chapter-number extraction with URL fallback. Relative-date parser ("3 days ago" → YYYY-MM-DD). 500ms polite delay after each HTTP call. Discovery via `/updates/` feed. |
| `src/scraper/asura.rs` | AsuraScans HTML scraper — pure Rust. Cloudflare bypass via `rquest` Chrome TLS impersonation (BoringSSL JA3/JA4 fingerprint), no Python subprocess. `scraper` crate selectors for series/chapter/cover parsing. Astro-serialized state grepped for image URLs and locked-chapter detection. Slug-scoped chapter filtering to avoid cross-series leak from the sidebar. 500ms polite delay. |
| `src/scraper/coordinator.rs` | Background polling task (tokio). Builds scraper registry once at startup; reuses `reqwest::Client` across polls. Runs chapter-update poll every `poll_interval_minutes`. Discovery pass gated to ≤ once per 23 h via `discovery_meta` key-value row. Sends `ScraperEvent::NewChapters` / `ScraperEvent::NewDiscoveries` to TUI via `mpsc`. Shuts down cleanly on `CancellationToken`. Uses `MissedTickBehavior::Skip` to prevent poll-stacking. |

### Infrastructure

| File | Responsibility |
|------|---------------|
| `src/notifier.rs` | Desktop notification dispatch via `notify-rust` (D-Bus). Groups up to 8 titles in one notification body ("…and N more"). Errors are swallowed so a broken notification daemon never propagates. |
| `src/cover_cache.rs` | Disk-backed cover image cache at `~/.cache/mrm/covers/{subdir}`. In-memory `HashMap<i64, Option<DynamicImage>>`. Background `preload_covers` / `refetch_covers` (force-overwrite) using 4-concurrent `Semaphore`. Validates image bytes and rejects images wider/taller than 4000px before writing to disk. Supports namespace subdirs (library / discover / search) to prevent id collisions across caches. |

### UI modules

| File | Responsibility |
|------|---------------|
| `src/ui/mod.rs` | Top-level `draw()` dispatcher; reader screen (download progress + viewer status); status-picker overlay; `centered_rect` layout helper. |
| `src/ui/library.rs` | Library grid view: cover thumbnails + title/status badges; sort mode label; delete-confirmation overlay; inline search bar. |
| `src/ui/detail.rs` | Series detail view: cover image, description, publication status, chapter list with read/progress icons, status override indicator. |
| `src/ui/search.rs` | Add-series search screen: text input → concurrent fan-out to all scrapers → cover grid of results. |
| `src/ui/discover.rs` | Discover screen: cover grid of undismissed `discovered_manhwa` rows; add (`a`/Enter) and dismiss (`x`) actions. |

---

## (b) TODO / FIXME Comments

**None found.** `grep` of the entire `src/` tree for `TODO`, `FIXME`, `HACK`, `XXX`, `todo!()`, and `fixme!()` returned zero matches.

---

## (c) Risky Area: Non-Transactional Chapter Upserts in the Coordinator (RESOLVED)

**Resolved by the Postgres move.** `db::apply_poll_result` now writes each series' poll (chapters, status, publication state) in one transaction, and `db::claim_unnotified` marks unannounced chapters (`chapter.notified_at`) in a single UPDATE, so each chapter is announced exactly once across machines.

Historical context: the old `upsert_chapters` ran one statement per chapter outside a transaction. A crash mid-loop left a partial batch (lost notifications), and two concurrent pollers could both count the same chapter as new (double notification). Both are closed by the atomic write plus the claim.
