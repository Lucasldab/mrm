# Codebase Scan

**Date:** 2026-04-30  
**Scope:** `crates/mrm/src/` (17 `.rs` files)

---

## (a) Modules and Responsibilities

### Entry point

| File | Responsibility |
|------|---------------|
| `src/main.rs` | CLI arg parsing (`--daemon`, `--once`, default TUI); DB path resolution (config.toml → XDG fallback chain); terminal setup / teardown; `run_tui` / `run_daemon` / `run_once` dispatch; 250ms event loop via `tokio::select!`; startup `tmp/mrm_*` cleanup. |

### Core application

| File | Responsibility |
|------|---------------|
| `src/app.rs` | `App` struct — owns all TUI state (library selection, chapter list, viewer process, image paths, cover caches, config). Dispatches `AppEvent` variants: `Key`, `Tick`, `DataRefreshed`, `ScraperMsg`. Manages the chapter image streaming pipeline (semaphore-limited parallel download → ordered re-assembly → viewer feed). Launches and kills `imv`/`rv` viewer processes via `ManagedChild` RAII wrapper. Hyprland fullscreen-state snapshot/restore around viewer launches. Handles all screen-specific key handlers (Library, Detail, Reader, StatusPicker, Search, Discover). |
| `src/types.rs` | Domain types: `Status` (enum + sort rank + display labels), `SortMode`, `Manhwa`, `Chapter`, `Discovery`, `Screen`, `AppEvent`. No I/O. |
| `src/config.rs` | TOML config loader (CWD → `~/.config/mrm/config.toml`). Structs: `Config`, `SourceConfig`, `NotificationsConfig`, `DbConfig`, `KeysConfig`, `ThemeConfig`, `ImvConfig`, `RvConfig`, `ViewerKind`. Key-string → `KeyCode` parser; hex / named color parser; imv config-file serialiser; rv CLI-args builder. |

### Persistence

| File | Responsibility |
|------|---------------|
| `src/db.rs` | All SQLite access via `sqlx`. Opens pool with WAL mode, `foreign_keys=ON`, `busy_timeout=5000`. Inline schema migrations at startup (adds `description` column; creates `discovered_manhwa` and `discovery_meta` tables if absent). Core queries: fetch/insert/delete manhwa; upsert chapters; progress (start/scroll/complete); status recompute; discovery upsert/dismiss/delete; meta key-value store. |

### Scraper subsystem

| File | Responsibility |
|------|---------------|
| `src/scraper/mod.rs` | `Scraper` trait (`search`, `get_series`, `get_chapter_image_urls`, `latest_chapters`). Shared data types: `SeriesData`, `ChapterData`, `SearchResult`, `DiscoveryEntry`. Retry helper: up to 3 attempts with 1s / 2s / 4s exponential backoff. |
| `src/scraper/mangadex.rs` | MangaDex REST API (no auth). Endpoints: `/manga` (search + latest), `/manga/{id}` (series), `/manga/{id}/feed` (paginated chapters, 250ms inter-page delay), `/at-home/server/{id}` (image CDN redirect). Deduplicates chapters by f64 bit representation. |
| `src/scraper/mangack.rs` | MangaCK HTML scraper (WordPress). CSS selectors via `scraper` crate for series/chapter/image parsing. Regex chapter-number extraction with URL fallback. Relative-date parser ("3 days ago" → YYYY-MM-DD). 500ms polite delay after each HTTP call. Discovery via `/updates/` feed. |
| `src/scraper/asura.rs` | AsuraScans bridge: Cloudflare bypass via Python subprocess (`python -m scraper.asura_bridge <cmd>`). 30s subprocess timeout. Parses JSON stdout. Any failure from `latest_chapters` is silently suppressed (returns empty vec) to avoid degrading discovery for other sources. |
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

## (c) Risky Area: Non-Transactional Chapter Upserts in the Coordinator

### What the code does

`db::upsert_chapters` (`src/db.rs:308–356`) executes one `INSERT … ON CONFLICT DO UPDATE` statement **per chapter**, all running against the bare connection pool (no wrapping transaction):

```rust
for ch in chapters {
    let already_exists: bool = sqlx::query(...)   // SELECT EXISTS
        .fetch_one(pool).await?;

    sqlx::query("INSERT INTO chapter … ON CONFLICT … DO UPDATE …")
        .execute(pool).await?;

    if !already_exists {
        new_count += 1;
    }
}
```

The coordinator calls this inside `poll_all` (`src/scraper/coordinator.rs:152`) for every manhwa in the library — also without a surrounding transaction. Likewise `db::recompute_status` and `db::update_pub_status` are called after the upsert loop, not within it.

### Why it's risky

1. **Partial update on crash.** If the process is killed (SIGKILL, OOM, power loss) while `upsert_chapters` is mid-loop, some chapters are written and some are not. SQLite WAL protects individual statements from corruption, but the logical batch is not atomic. On the next startup:
   - The `already_exists` check will find the partially-written chapters and count them as pre-existing, so `new_count` for those chapters is 0.
   - `recompute_status` may have already been called (or not), leaving the manhwa's status inconsistent with its chapter/progress counts.
   - The user gets no notification for chapters that arrived but were not counted.

2. **`check-then-act` race with concurrent instances.** Nothing prevents running `mrm` (TUI) and `mrm --daemon` simultaneously. Both call `upsert_chapters` concurrently against the same DB. The SELECT-then-INSERT pair is not atomic: two processes can both observe `already_exists = false` for the same chapter and both increment `new_count`, leading to a double notification.

3. **`recompute_status` called on stale snapshot.** Between `upsert_chapters` returning and `recompute_status` executing, another concurrent writer could have updated the progress table, so the status recompute may silently stomp on a status that was correct.

### Suggested test

```rust
/// Verify that chapter upsert correctly reports new_count even after a
/// simulated partial write (process restart mid-loop).
///
/// Desired behaviour: wrapping the loop in a transaction should make
/// the batch atomic — either all chapters are counted as new, or none.
#[tokio::test]
async fn upsert_chapters_is_atomic() {
    // 1. Open an in-memory SQLite pool and create the schema.
    let pool = SqlitePool::connect(":memory:").await.unwrap();
    setup_schema(&pool).await;

    let manhwa_id = insert_test_manhwa(&pool).await;

    // 2. Build a batch of 5 chapters.
    let batch: Vec<ChapterData> = (1..=5).map(|n| ChapterData {
        number: n as f64,
        title: None,
        url: format!("https://example.com/ch/{n}"),
        released_at: None,
    }).collect();

    // 3. Simulate a partial write: insert only the first 3 manually.
    for ch in &batch[..3] {
        sqlx::query("INSERT INTO chapter (manhwa_id, number, title, url) VALUES (?,?,?,?)")
            .bind(manhwa_id).bind(ch.number).bind(&ch.title).bind(&ch.url)
            .execute(&pool).await.unwrap();
    }

    // 4. Now call upsert_chapters with the full 5-chapter batch.
    let new_count = upsert_chapters(&pool, manhwa_id, &batch).await.unwrap();

    // 5. Only chapters 4 and 5 should be "new" — chapters 1–3 already existed.
    assert_eq!(new_count, 2,
        "expected 2 new chapters; got {new_count} — \
         partial-write simulation revealed double-counting or missed chapters");

    // 6. Verify total chapter count is exactly 5 (no duplicates).
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chapter WHERE manhwa_id = ?")
        .bind(manhwa_id).fetch_one(&pool).await.unwrap();
    assert_eq!(total, 5);
}
```

This test currently passes because `already_exists` handles the idempotent-insert case correctly. The latent risk surfaces only in the TOCTOU/concurrent-writer scenario. To make it provably safe, `upsert_chapters` should wrap its loop in a single `pool.begin()` / `tx.commit()` transaction — identical to how `insert_manhwa_with_chapters` already handles its batch insert.
