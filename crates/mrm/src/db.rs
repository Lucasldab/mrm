use anyhow::Result;
use sqlx::{postgres::{PgPool, PgPoolOptions}, PgConnection, Row};

use crate::types::{Chapter, Discovery, Manhwa, Status};

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

pub async fn open_db(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await?;
    // Older binaries must keep working after a newer one migrated (additive
    // migrations only), so unknown applied migrations are not an error.
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(&pool).await?;
    Ok(pool)
}

// ---------------------------------------------------------------------------
// Alias queries — cross-source linking for a single canonical series.
// (Schema is wired up; coordinator/UI integration lands in a follow-up.)
// ---------------------------------------------------------------------------

/// All (source, source_url) pairs for a given canonical manhwa, including the
/// primary row's own (source, source_url) so callers can iterate uniformly.
#[allow(dead_code)]
pub async fn fetch_aliases(pool: &PgPool, manhwa_id: i64) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query(
        r#"
        SELECT source, source_url FROM manhwa WHERE id = $1
        UNION ALL
        SELECT source, source_url FROM manhwa_alias WHERE manhwa_id = $1
        "#,
    )
    .bind(manhwa_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter()
        .map(|r| (r.get::<String, _>("source"), r.get::<String, _>("source_url")))
        .collect())
}

/// Link an additional source to an existing manhwa. Idempotent — re-adding
/// the same (manhwa_id, source) pair updates the URL via ON CONFLICT.
#[allow(dead_code)]
pub async fn add_alias(
    pool: &PgPool,
    manhwa_id: i64,
    source: &str,
    source_url: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO manhwa_alias (manhwa_id, source, source_url)
        VALUES ($1, $2, $3)
        ON CONFLICT(manhwa_id, source) DO UPDATE SET
            source_url = excluded.source_url
        "#,
    )
    .bind(manhwa_id)
    .bind(source)
    .bind(source_url)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a single alias by id. The canonical manhwa row itself is never
/// touched — use `delete_manhwa` for that.
#[allow(dead_code)]
pub async fn remove_alias(pool: &PgPool, alias_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM manhwa_alias WHERE id = $1")
        .bind(alias_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Manhwa queries
// ---------------------------------------------------------------------------

pub async fn fetch_all_manhwa(pool: &PgPool) -> Result<Vec<Manhwa>> {
    let rows = sqlx::query(
        r#"
        SELECT
            m.id,
            m.title,
            m.cover_url,
            m.source,
            m.source_url,
            m.pub_status,
            m.status,
            m.status_override,
            m.description,
            m.last_error,
            m.failing_since::text AS failing_since,
            COUNT(c.id)           AS total_chapters,
            COUNT(p.completed_at) AS read_chapters
        FROM manhwa m
        LEFT JOIN chapter  c ON c.manhwa_id = m.id
        LEFT JOIN progress p ON p.chapter_id = c.id
        GROUP BY m.id
        ORDER BY m.updated_at DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut manhwa_list = Vec::with_capacity(rows.len());
    for row in rows {
        let total: i64 = row.try_get("total_chapters")?;
        let read:  i64 = row.try_get("read_chapters")?;
        let unread = (total - read).max(0) as u32;

        manhwa_list.push(Manhwa {
            id:              row.try_get("id")?,
            title:           row.try_get("title")?,
            cover_url:       row.try_get("cover_url")?,
            source:          row.try_get("source")?,
            source_url:      row.try_get("source_url")?,
            pub_status:      row.try_get("pub_status")?,
            status:          Status::from_str(row.try_get("status")?),
            status_override: row.try_get::<bool, _>("status_override")?,
            description:     row.try_get("description").unwrap_or(None),
            unread,
            total:           total.max(0) as u32,
            last_error:      row.try_get("last_error")?,
            failing_since:   row.try_get("failing_since")?,
        });
    }

    Ok(manhwa_list)
}

pub async fn fetch_manhwa(pool: &PgPool, manhwa_id: i64) -> Result<Manhwa> {
    let row = sqlx::query(
        r#"
        SELECT
            m.id, m.title, m.cover_url, m.source, m.source_url,
            m.pub_status, m.status, m.status_override, m.description,
            m.last_error, m.failing_since::text AS failing_since,
            COUNT(c.id)           AS total_chapters,
            COUNT(p.completed_at) AS read_chapters
        FROM manhwa m
        LEFT JOIN chapter  c ON c.manhwa_id = m.id
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE m.id = $1
        GROUP BY m.id
        "#,
    )
    .bind(manhwa_id)
    .fetch_one(pool)
    .await?;

    let total: i64 = row.try_get("total_chapters")?;
    let read:  i64 = row.try_get("read_chapters")?;
    let unread = (total - read).max(0) as u32;

    Ok(Manhwa {
        id:              row.try_get("id")?,
        title:           row.try_get("title")?,
        cover_url:       row.try_get("cover_url")?,
        source:          row.try_get("source")?,
        source_url:      row.try_get("source_url")?,
        pub_status:      row.try_get("pub_status")?,
        status:          Status::from_str(row.try_get("status")?),
        status_override: row.try_get::<bool, _>("status_override")?,
        description:     row.try_get("description").unwrap_or(None),
        unread,
        total:           total.max(0) as u32,
        last_error:      row.try_get("last_error")?,
        failing_since:   row.try_get("failing_since")?,
    })
}

pub async fn set_manhwa_status(
    pool: &PgPool,
    manhwa_id: i64,
    status: &Status,
    is_override: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE manhwa SET status = $1, status_override = $2, updated_at = now()
         WHERE id = $3",
    )
    .bind(status.as_str())
    .bind(is_override)
    .bind(manhwa_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Insert a new manhwa and all its chapters in a single transaction.
///
/// Returns the new manhwa row id.
/// Returns Err if a manhwa with the same source_url already exists.
pub async fn insert_manhwa_with_chapters(
    pool: &PgPool,
    series: &crate::scraper::SeriesData,
    source: &str,
) -> Result<i64> {
    let mut tx = pool.begin().await?;

    // Check for duplicate source_url
    let exists: bool = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM manhwa WHERE source_url = $1)",
    )
    .bind(&series.source_url)
    .fetch_one(&mut *tx)
    .await?
    .try_get::<bool, _>(0)
    .unwrap_or(false);

    if exists {
        return Err(anyhow::anyhow!("Already in library: {}", series.title));
    }

    let row = sqlx::query(
        r#"
        INSERT INTO manhwa (title, source, source_url, cover_url, pub_status,
                            description, status, status_override, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, 'looked_into', FALSE, now())
        RETURNING id
        "#,
    )
    .bind(&series.title)
    .bind(source)
    .bind(&series.source_url)
    .bind(&series.cover_url)
    .bind(&series.pub_status)
    .bind(&series.description)
    .fetch_one(&mut *tx)
    .await?;

    let manhwa_id: i64 = row.try_get("id")?;

    // Upsert all chapters within the same transaction
    for ch in &series.chapters {
        sqlx::query(
            r#"
            INSERT INTO chapter (manhwa_id, number, title, url, released_at, notified_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT(manhwa_id, number) DO UPDATE SET
                title       = excluded.title,
                url         = excluded.url,
                released_at = excluded.released_at
            "#,
        )
        .bind(manhwa_id)
        .bind(ch.number)
        .bind(&ch.title)
        .bind(&ch.url)
        .bind(&ch.released_at)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(manhwa_id)
}

/// Id of the library row with this exact source URL, if any.
pub async fn manhwa_id_by_source_url(pool: &PgPool, source_url: &str) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM manhwa WHERE source_url = $1")
        .bind(source_url)
        .fetch_optional(pool)
        .await?)
}

/// Delete a manhwa and all associated chapters and progress records.
/// Relies on ON DELETE CASCADE (confirmed in schema).
pub async fn delete_manhwa(pool: &PgPool, manhwa_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM manhwa WHERE id = $1")
        .bind(manhwa_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clear the status override for a manhwa, re-enabling auto-computation.
pub async fn clear_status_override(pool: &PgPool, manhwa_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE manhwa SET status_override = FALSE, updated_at = now() WHERE id = $1",
    )
    .bind(manhwa_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Chapter queries
// ---------------------------------------------------------------------------

pub async fn fetch_chapters(pool: &PgPool, manhwa_id: i64) -> Result<Vec<Chapter>> {
    let rows = sqlx::query(
        r#"
        SELECT
            c.id, c.manhwa_id, c.number, c.title, c.url, c.released_at,
            COALESCE(p.scrolled_pct, 0.0)         AS scroll_pct,
            (p.completed_at IS NOT NULL)           AS completed
        FROM chapter c
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE c.manhwa_id = $1
        ORDER BY c.number ASC
        "#,
    )
    .bind(manhwa_id)
    .fetch_all(pool)
    .await?;

    // Surface schema/type drift instead of silently substituting `id = 0`
    // (which would collide with every other unparseable row and corrupt
    // progress tracking).
    let chapters: Result<Vec<Chapter>> = rows
        .iter()
        .map(|row| -> Result<Chapter> {
            Ok(Chapter {
                id:          row.try_get("id")?,
                manhwa_id:   row.try_get("manhwa_id")?,
                number:      row.try_get("number")?,
                title:       row.try_get("title").unwrap_or(None),
                url:         row.try_get("url")?,
                released_at: row.try_get("released_at").unwrap_or(None),
                scroll_pct:  row.try_get("scroll_pct").unwrap_or(0.0),
                completed:   row.try_get::<bool, _>("completed").unwrap_or(false),
            })
        })
        .collect();

    chapters
}

/// The series a chapter belongs to, or None for an unknown chapter id.
pub async fn chapter_manhwa_id(pool: &PgPool, chapter_id: i64) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar("SELECT manhwa_id FROM chapter WHERE id = $1")
        .bind(chapter_id)
        .fetch_optional(pool)
        .await?)
}

/// (series source, chapter URL) for a chapter id: what the image fetch needs.
pub async fn chapter_source(pool: &PgPool, chapter_id: i64) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT m.source, c.url FROM chapter c JOIN manhwa m ON m.id = c.manhwa_id WHERE c.id = $1",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| Ok((r.try_get("source")?, r.try_get("url")?))).transpose()
}

// ---------------------------------------------------------------------------
// Chapter upsert (coordinator writes)
// ---------------------------------------------------------------------------

/// Upsert a batch inside the caller's transaction. Returns how many rows were
/// newly inserted (`xmax = 0` is true only for the INSERT path of an upsert).
/// New rows start with notified_at NULL; updates never touch notified_at.
async fn upsert_chapters_conn(
    conn: &mut PgConnection,
    manhwa_id: i64,
    chapters: &[crate::scraper::ChapterData],
) -> Result<usize> {
    let mut inserted = 0usize;
    for ch in chapters {
        let new: bool = sqlx::query_scalar(
            r#"
            INSERT INTO chapter (manhwa_id, number, title, url, released_at)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (manhwa_id, number) DO UPDATE SET
                title       = excluded.title,
                url         = excluded.url,
                released_at = excluded.released_at
            RETURNING (xmax = 0)
            "#,
        )
        .bind(manhwa_id)
        .bind(ch.number)
        .bind(&ch.title)
        .bind(&ch.url)
        .bind(&ch.released_at)
        .fetch_one(&mut *conn)
        .await?;
        if new { inserted += 1; }
    }
    Ok(inserted)
}

/// One poll result for one series, atomically: chapters, pub_status, and the
/// status recompute (only when something it depends on changed). A crash
/// mid-way leaves nothing half-written (SCAN.md risks 1 and 3).
pub async fn apply_poll_result(
    pool: &PgPool,
    manhwa_id: i64,
    series: &crate::scraper::SeriesData,
    old_pub_status: &str,
) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let inserted = upsert_chapters_conn(&mut tx, manhwa_id, &series.chapters).await?;
    // Follow a source-side move (Asura reports its post-redirect URL), so the
    // stored URL doesn't depend on the redirect chain staying alive. Skipped
    // when another library row already holds the new URL (UNIQUE).
    sqlx::query(
        r#"
        UPDATE manhwa SET source_url = $1
        WHERE id = $2 AND source_url <> $1
          AND NOT EXISTS (SELECT 1 FROM manhwa WHERE source_url = $1)
        "#,
    )
    .bind(&series.source_url)
    .bind(manhwa_id)
    .execute(&mut *tx)
    .await?;
    let pub_changed = series.pub_status != old_pub_status;
    if pub_changed {
        update_pub_status_conn(&mut tx, manhwa_id, &series.pub_status).await?;
    }
    if inserted > 0 || pub_changed {
        recompute_status_conn(&mut tx, manhwa_id).await?;
    }
    // The poll worked: clear any failure streak.
    sqlx::query(
        "UPDATE manhwa SET last_ok_at = now(), last_error = NULL, failing_since = NULL WHERE id = $1",
    )
    .bind(manhwa_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inserted)
}

/// Longest stored poll error, in characters.
const MAX_POLL_ERROR_CHARS: usize = 300;

/// A poll of this series failed: keep the error (truncated) and the start
/// of the failure streak, so Detail/Library can show "failing since".
pub async fn record_poll_failure(pool: &PgPool, manhwa_id: i64, err: &str) -> Result<()> {
    let err: String = err.chars().take(MAX_POLL_ERROR_CHARS).collect();
    sqlx::query(
        "UPDATE manhwa SET last_error = $1, failing_since = COALESCE(failing_since, now()) WHERE id = $2",
    )
    .bind(err)
    .bind(manhwa_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Claim every chapter no notifying poller has announced yet, in one statement
/// (row locks make concurrent claims disjoint). Returns (series id, title, count) for the
/// claimed chapters the user has not started; started/read ones are claimed
/// silently. Only notifying pollers call this - never the TUI's quiet poller.
/// Claim precedes the push: a failed push is not retried (at-most-once).
/// Rows are locked in id order and locked ones skipped, so a concurrent poll
/// transaction cannot deadlock with the claim.
pub async fn claim_unnotified(pool: &PgPool) -> Result<Vec<Claimed>> {
    let rows = sqlx::query(
        r#"
        WITH c AS (
            SELECT id FROM chapter WHERE notified_at IS NULL ORDER BY id FOR UPDATE SKIP LOCKED
        ),
        claimed AS (
            UPDATE chapter SET notified_at = now()
            FROM c WHERE chapter.id = c.id
            RETURNING chapter.id, chapter.manhwa_id
        )
        SELECT m.id, m.title, COUNT(*) AS n
        FROM claimed c
        JOIN manhwa m ON m.id = c.manhwa_id
        WHERE NOT EXISTS (SELECT 1 FROM progress p WHERE p.chapter_id = c.id)
        GROUP BY m.id, m.title
        ORDER BY m.title
        "#,
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| Ok(Claimed { manhwa_id: r.try_get("id")?, title: r.try_get("title")?, chapters: r.try_get("n")? }))
        .collect()
}

/// One series with freshly claimed, unstarted chapters (see [`claim_unnotified`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub manhwa_id: i64,
    pub title:     String,
    pub chapters:  i64,
}

// ---------------------------------------------------------------------------
// Progress queries
// ---------------------------------------------------------------------------

pub async fn start_chapter(pool: &PgPool, chapter_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO progress (chapter_id, started_at, scrolled_pct)
        VALUES ($1, now(), 0.0)
        ON CONFLICT(chapter_id) DO NOTHING
        "#,
    )
    .bind(chapter_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Mark all chapters for a manhwa as read (insert progress rows with completed_at).
/// Used when the user sets status to "Up to Date".
pub async fn mark_all_chapters_read(pool: &PgPool, manhwa_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO progress (chapter_id, started_at, scrolled_pct, completed_at)
        SELECT c.id, now(), 1.0, now()
        FROM chapter c
        WHERE c.manhwa_id = $1
          AND c.id NOT IN (SELECT chapter_id FROM progress WHERE completed_at IS NOT NULL)
        ON CONFLICT(chapter_id) DO UPDATE SET
            scrolled_pct = 1.0,
            completed_at = COALESCE(progress.completed_at, now())
        "#,
    )
    .bind(manhwa_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Mark every chapter numbered `<= through` read (bulk import of reading
/// history). Returns how many chapters became read.
pub async fn mark_read_through(pool: &PgPool, manhwa_id: i64, through: f64) -> Result<u64> {
    let done = sqlx::query(
        r#"
        INSERT INTO progress (chapter_id, started_at, scrolled_pct, completed_at)
        SELECT c.id, now(), 1.0, now()
        FROM chapter c
        WHERE c.manhwa_id = $1 AND c.number <= $2
        ON CONFLICT(chapter_id) DO UPDATE SET
            scrolled_pct = 1.0,
            completed_at = COALESCE(progress.completed_at, now())
        WHERE progress.completed_at IS NULL
        "#,
    )
    .bind(manhwa_id)
    .bind(through)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Update scroll progress. Returns true if this call newly completed the chapter.
pub async fn update_scroll(
    pool: &PgPool,
    chapter_id: i64,
    pct: f64,
) -> Result<bool> {
    let pct = pct.clamp(0.0, 1.0);

    // Check if it was already completed before this update
    let was_completed: bool = sqlx::query(
        "SELECT completed_at IS NOT NULL FROM progress WHERE chapter_id = $1",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?
    .map(|r| r.try_get::<bool, _>(0).unwrap_or(false))
    .unwrap_or(false);

    sqlx::query(
        r#"
        UPDATE progress
        SET scrolled_pct = GREATEST(scrolled_pct, $1),
            completed_at = COALESCE(completed_at, CASE WHEN $1 >= 1.0 THEN now() END)
        WHERE chapter_id = $2
        "#,
    )
    .bind(pct)
    .bind(chapter_id)
    .execute(pool)
    .await?;

    // Newly completed = pct hit 1.0 AND wasn't completed before
    Ok(pct >= 1.0 && !was_completed)
}

/// Undo a chapter's completion (the Detail screen's mark-unread); the
/// progress row itself stays.
pub async fn mark_chapter_unread(pool: &PgPool, chapter_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE progress SET completed_at = NULL, scrolled_pct = 0.0 WHERE chapter_id = $1",
    )
    .bind(chapter_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Refresh stored title/cover/pub_status/description from a freshly scraped
/// SeriesData. Used by the "refresh metadata" action on the detail screen.
pub async fn update_manhwa_metadata(
    pool: &PgPool,
    manhwa_id: i64,
    series: &crate::scraper::SeriesData,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE manhwa
        SET title       = COALESCE(NULLIF($1, ''), title),
            cover_url   = COALESCE($2, cover_url),
            pub_status  = $3,
            description = COALESCE($4, description),
            updated_at  = now()
        WHERE id = $5
        "#,
    )
    .bind(&series.title)
    .bind(&series.cover_url)
    .bind(&series.pub_status)
    .bind(&series.description)
    .bind(manhwa_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn update_pub_status_conn(
    conn: &mut PgConnection,
    manhwa_id: i64,
    pub_status: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE manhwa SET pub_status = $1, updated_at = now() WHERE id = $2",
    )
    .bind(pub_status)
    .bind(manhwa_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// "Looked into" means the user has not yet read a meaningful sample: fewer
/// than five chapters read, or fewer than all of them when the series has
/// fewer than five. An empty series has nothing to be up to date with.
pub(crate) fn looked_into(read: i64, total: i64) -> bool {
    total == 0 || read < total.min(5)
}

/// Apply a status chosen in the picker. Up to date marks every chapter read
/// and writes that status explicitly (no override: later chapters resume
/// auto-tracking); any other status is a manual override.
pub async fn apply_picker_status(pool: &PgPool, manhwa_id: i64, chosen: &Status) -> Result<()> {
    if *chosen == Status::UpToDate {
        mark_all_chapters_read(pool, manhwa_id).await?;
        set_manhwa_status(pool, manhwa_id, chosen, false).await
    } else {
        set_manhwa_status(pool, manhwa_id, chosen, true).await
    }
}

/// Recompute and persist status for a manhwa (mirrors Python's apply_computed_status).
pub async fn recompute_status(pool: &PgPool, manhwa_id: i64) -> Result<Status> {
    let mut conn = pool.acquire().await?;
    recompute_status_conn(&mut conn, manhwa_id).await
}

pub(crate) async fn recompute_status_conn(conn: &mut PgConnection, manhwa_id: i64) -> Result<Status> {
    let row = sqlx::query(
        r#"
        SELECT
            COUNT(c.id)                    AS total,
            COUNT(p.completed_at)          AS read,
            COUNT(c.id) - COUNT(p.completed_at) AS unread,
            m.pub_status,
            m.status_override
        FROM manhwa m
        LEFT JOIN chapter  c ON c.manhwa_id = m.id
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE m.id = $1
        GROUP BY m.id
        "#,
    )
    .bind(manhwa_id)
    .fetch_one(&mut *conn)
    .await?;

    let status_override: bool = row.try_get("status_override")?;
    if status_override {
        // User has manually set status — don't touch it
        let current = sqlx::query("SELECT status FROM manhwa WHERE id = $1")
            .bind(manhwa_id)
            .fetch_one(&mut *conn)
            .await?;
        return Ok(Status::from_str(current.try_get("status")?));
    }

    let read:   i64  = row.try_get("read")?;
    let unread: i64  = row.try_get("unread")?;
    let pub_st: &str = row.try_get("pub_status")?;

    let total:  i64  = row.try_get("total")?;
    let status = if looked_into(read, total) {
        Status::LookedInto
    } else if unread > 0 {
        Status::Reading
    } else if pub_st == "completed" {
        Status::Completed
    } else if pub_st == "hiatus" {
        Status::Paused
    } else {
        Status::UpToDate
    };

    sqlx::query(
        "UPDATE manhwa SET status = $1, updated_at = now() WHERE id = $2",
    )
    .bind(status.as_str())
    .bind(manhwa_id)
    .execute(&mut *conn)
    .await?;

    Ok(status)
}

// ---------------------------------------------------------------------------
// Discovery queries
// ---------------------------------------------------------------------------

/// SQL twin of `scraper::asura::slug_key` over a `source_url` column: last
/// path segment, trailing `/` ignored, `-hash8` suffix stripped.
const ASURA_SLUG_SQL: &str =
    r"regexp_replace(regexp_replace(rtrim(regexp_replace(source_url, '[?#].*$', ''), '/'), '^.*/', ''), '-[0-9a-f]{8}$', '')";

/// Insert a discovered entry if not already known in either library or
/// discoveries. Returns true if a new row was inserted.
pub async fn upsert_discovery(
    pool: &PgPool,
    source: &str,
    source_url: &str,
    title: &str,
    cover_url: Option<&str>,
    chapter_number: Option<f64>,
    released_at: Option<&str>,
) -> Result<bool> {
    // Asura rotates the -hash8 suffix (and has moved hosts), so its series
    // are matched by slug; other sources by exact URL.
    let slug: Option<String> = (source == "asura")
        .then(|| crate::scraper::asura::slug_key(source_url))
        .filter(|k| !k.is_empty());

    // Skip if already in library
    let in_lib: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM manhwa WHERE source_url = $1 \
           OR ($2::text IS NOT NULL AND source = 'asura' AND {ASURA_SLUG_SQL} = $2))"
    ))
    .bind(source_url)
    .bind(&slug)
    .fetch_one(pool)
    .await?;
    if in_lib {
        return Ok(false);
    }

    // Known under another hash: move that row to the current URL, keeping
    // its dismissed flag, instead of surfacing the series again.
    if let Some(slug) = &slug {
        let moved = sqlx::query(&format!(
            r#"
            UPDATE discovered_manhwa SET
                source_url     = $1,
                title          = $3,
                cover_url      = COALESCE($4, cover_url),
                chapter_number = COALESCE($5, chapter_number),
                released_at    = COALESCE($6, released_at)
            WHERE id = (
                SELECT id FROM discovered_manhwa
                WHERE source = 'asura' AND {ASURA_SLUG_SQL} = $2 AND source_url <> $1
                ORDER BY id LIMIT 1
            )
              AND NOT EXISTS (SELECT 1 FROM discovered_manhwa WHERE source_url = $1)
            "#
        ))
        .bind(source_url)
        .bind(slug)
        .bind(title)
        .bind(cover_url)
        .bind(chapter_number)
        .bind(released_at)
        .execute(pool)
        .await?;
        if moved.rows_affected() > 0 {
            return Ok(false);
        }
    }

    // xmax = 0 only on a freshly inserted row, so RETURNING tells INSERT from
    // the ON CONFLICT UPDATE path in a single round trip.
    let inserted: bool = sqlx::query_scalar(
        r#"
        INSERT INTO discovered_manhwa
            (source, source_url, title, cover_url, chapter_number, released_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT(source_url) DO UPDATE SET
            title          = excluded.title,
            cover_url      = COALESCE(excluded.cover_url, discovered_manhwa.cover_url),
            chapter_number = COALESCE(excluded.chapter_number, discovered_manhwa.chapter_number),
            released_at    = COALESCE(excluded.released_at, discovered_manhwa.released_at)
        RETURNING (xmax = 0) AS inserted
        "#,
    )
    .bind(source)
    .bind(source_url)
    .bind(title)
    .bind(cover_url)
    .bind(chapter_number)
    .bind(released_at)
    .fetch_one(pool)
    .await?;

    Ok(inserted)
}

/// Fetch all undismissed discoveries, newest first.
pub async fn fetch_discoveries(pool: &PgPool) -> Result<Vec<Discovery>> {
    let rows = sqlx::query(
        r#"
        SELECT id, source, source_url, title, cover_url,
               chapter_number, released_at, first_seen_at::text AS first_seen_at
        FROM discovered_manhwa
        WHERE dismissed = FALSE
        ORDER BY discovered_manhwa.first_seen_at DESC, id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(Discovery {
            id:             row.try_get("id")?,
            source:         row.try_get("source")?,
            source_url:     row.try_get("source_url")?,
            title:          row.try_get("title")?,
            cover_url:      row.try_get("cover_url")?,
            chapter_number: row.try_get("chapter_number")?,
            released_at:    row.try_get("released_at")?,
            first_seen_at:  row.try_get("first_seen_at")?,
        });
    }
    Ok(out)
}

/// Mark a discovery as dismissed (soft delete — kept so we don't re-surface it).
pub async fn dismiss_discovery(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("UPDATE discovered_manhwa SET dismissed = TRUE WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove a discovery outright (called after the user adds it to the library
/// so the row in `manhwa` becomes the canonical entry).
pub async fn delete_discovery(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM discovered_manhwa WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Read a value from the discovery_meta key/value table.
pub async fn get_discovery_meta(pool: &PgPool, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM discovery_meta WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.try_get("value").ok()))
}

/// Write a value to the discovery_meta key/value table.
pub async fn set_discovery_meta(pool: &PgPool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO discovery_meta (key, value) VALUES ($1, $2)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        "#,
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::scraper::{ChapterData, SeriesData};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A migrated pool on a fresh database, or None (test skipped) when
    /// MRM_TEST_DATABASE_URL is unset. Run via scripts/with-test-pg.
    pub(crate) async fn test_pool() -> Option<PgPool> {
        let base = std::env::var("MRM_TEST_DATABASE_URL").ok()?;
        let admin = PgPool::connect(&base).await.expect("connect test cluster");
        let name = format!("mrm_t_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst));
        sqlx::query(&format!("CREATE DATABASE {name}")).execute(&admin).await.unwrap();
        let url = base.replacen("/mrm_test", &format!("/{name}"), 1);
        Some(open_db(&url).await.expect("open_db"))
    }

    pub(crate) fn series(n: usize, url: &str) -> SeriesData {
        SeriesData {
            title: format!("Series {url}"),
            cover_url: None,
            source_url: url.to_string(),
            pub_status: "ongoing".into(),
            description: None,
            chapters: (1..=n).map(|i| ch(i as f64, url)).collect(),
        }
    }

    pub(crate) fn ch(num: f64, url: &str) -> ChapterData {
        ChapterData { number: num, title: None, url: format!("{url}/ch/{num}"), released_at: Some("2026-11-05".into()) }
    }

    macro_rules! pool_or_skip {
        () => { match test_pool().await { Some(p) => p, None => { eprintln!("skipped: MRM_TEST_DATABASE_URL unset"); return; } } };
    }

    #[tokio::test]
    async fn open_db_is_idempotent() {
        let pool = pool_or_skip!();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(n, 6);
    }

    #[tokio::test]
    async fn insert_and_fetch_manhwa() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(3, "https://x/a"), "mangadex").await.unwrap();
        let all = fetch_all_manhwa(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].unread, 3);
        assert!(!all[0].status_override);
        let chs = fetch_chapters(&pool, id).await.unwrap();
        assert_eq!(chs.iter().map(|c| c.number).collect::<Vec<_>>(), vec![1.0, 2.0, 3.0]);
        assert!(chs.iter().all(|c| !c.completed));
        assert!(insert_manhwa_with_chapters(&pool, &series(3, "https://x/a"), "mangadex").await.is_err());
    }

    #[tokio::test]
    async fn progress_and_status() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(6, "https://x/b"), "mangadex").await.unwrap();
        let chs = fetch_chapters(&pool, id).await.unwrap();
        for c in &chs[..5] {
            start_chapter(&pool, c.id).await.unwrap();
            assert!(update_scroll(&pool, c.id, 1.0).await.unwrap());
        }
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::Reading);
        start_chapter(&pool, chs[5].id).await.unwrap();
        update_scroll(&pool, chs[5].id, 0.8).await.unwrap();
        update_scroll(&pool, chs[5].id, 0.5).await.unwrap();
        let pct: f64 = sqlx::query_scalar("SELECT scrolled_pct FROM progress WHERE chapter_id = $1")
            .bind(chs[5].id).fetch_one(&pool).await.unwrap();
        assert_eq!(pct, 0.8);
        assert!(update_scroll(&pool, chs[5].id, 1.0).await.unwrap());
        assert!(!update_scroll(&pool, chs[5].id, 1.0).await.unwrap());
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::UpToDate);
        let mut done = series(6, "https://x/b");
        done.pub_status = "completed".into();
        apply_poll_result(&pool, id, &done, "ongoing").await.unwrap();
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::Completed);
        set_manhwa_status(&pool, id, &Status::Dropped, true).await.unwrap();
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::Dropped);
    }

    #[test]
    fn looked_into_rule() {
        assert!(looked_into(0, 0));
        assert!(looked_into(0, 3));
        assert!(!looked_into(3, 3));
        assert!(looked_into(3, 10));
        assert!(!looked_into(5, 10));
        assert!(!looked_into(4, 4));
    }

    #[tokio::test]
    async fn short_series_fully_read_is_up_to_date() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(3, "https://x/short"), "mangadex").await.unwrap();
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::LookedInto);
        mark_all_chapters_read(&pool, id).await.unwrap();
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::UpToDate);
    }

    #[tokio::test]
    async fn long_series_with_three_read_is_looked_into() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(10, "https://x/long"), "mangadex").await.unwrap();
        let chs = fetch_chapters(&pool, id).await.unwrap();
        for c in &chs[..3] {
            start_chapter(&pool, c.id).await.unwrap();
            update_scroll(&pool, c.id, 1.0).await.unwrap();
        }
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::LookedInto);
    }

    #[tokio::test]
    async fn picker_up_to_date_sticks_even_for_completed_series() {
        let pool = pool_or_skip!();
        let mut s = series(3, "https://x/pick");
        s.pub_status = "completed".into();
        let id = insert_manhwa_with_chapters(&pool, &s, "mangadex").await.unwrap();
        apply_picker_status(&pool, id, &Status::UpToDate).await.unwrap();
        let m = fetch_manhwa(&pool, id).await.unwrap();
        assert_eq!(m.status, Status::UpToDate);
        assert_eq!(m.unread, 0);
        assert!(!m.status_override);
        apply_picker_status(&pool, id, &Status::Dropped).await.unwrap();
        let m = fetch_manhwa(&pool, id).await.unwrap();
        assert_eq!(m.status, Status::Dropped);
        assert!(m.status_override);
    }

    #[tokio::test]
    async fn mark_all_read_and_delete_cascades() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(4, "https://x/c"), "mangadex").await.unwrap();
        mark_all_chapters_read(&pool, id).await.unwrap();
        assert_eq!(fetch_manhwa(&pool, id).await.unwrap().unread, 0);
        delete_manhwa(&pool, id).await.unwrap();
        let left: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM chapter) + (SELECT COUNT(*) FROM progress)")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn discovery_novelty_and_dismiss() {
        let pool = pool_or_skip!();
        assert!(upsert_discovery(&pool, "asura", "https://x/d", "D", None, Some(3.0), None).await.unwrap());
        assert!(!upsert_discovery(&pool, "asura", "https://x/d", "D", Some("c.jpg"), None, None).await.unwrap());
        let d = fetch_discoveries(&pool).await.unwrap();
        assert_eq!((d.len(), d[0].cover_url.as_deref(), d[0].chapter_number), (1, Some("c.jpg"), Some(3.0)));
        insert_manhwa_with_chapters(&pool, &series(1, "https://x/e"), "asura").await.unwrap();
        assert!(!upsert_discovery(&pool, "asura", "https://x/e", "E", None, None, None).await.unwrap());
        dismiss_discovery(&pool, d[0].id).await.unwrap();
        assert!(fetch_discoveries(&pool).await.unwrap().is_empty());
        set_discovery_meta(&pool, "k", "v1").await.unwrap();
        set_discovery_meta(&pool, "k", "v2").await.unwrap();
        assert_eq!(get_discovery_meta(&pool, "k").await.unwrap().as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn poll_counts_only_inserts() {
        let pool = pool_or_skip!();
        let mut s = series(3, "https://x/p");
        let id = insert_manhwa_with_chapters(&pool, &s, "mangadex").await.unwrap();
        s.chapters.push(ch(4.0, "https://x/p"));
        assert_eq!(apply_poll_result(&pool, id, &s, "ongoing").await.unwrap(), 1);
        assert_eq!(apply_poll_result(&pool, id, &s, "ongoing").await.unwrap(), 0);
        s.pub_status = "completed".into();
        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        assert_eq!(fetch_manhwa(&pool, id).await.unwrap().pub_status, "completed");
    }

    #[tokio::test]
    async fn claim_notifies_new_chapters_once() {
        let pool = pool_or_skip!();
        let mut s = series(3, "https://x/q");
        let id = insert_manhwa_with_chapters(&pool, &s, "mangadex").await.unwrap();
        assert!(claim_unnotified(&pool).await.unwrap().is_empty(), "back catalogue is never announced");
        s.chapters.push(ch(4.0, "https://x/q"));
        s.chapters.push(ch(4.5, "https://x/q"));
        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        assert_eq!(claim_unnotified(&pool).await.unwrap(), vec![Claimed { manhwa_id: id, title: s.title.clone(), chapters: 2 }]);
        assert!(claim_unnotified(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_claims_never_double_notify() {
        let pool = pool_or_skip!();
        let mut s = series(1, "https://x/r");
        let id = insert_manhwa_with_chapters(&pool, &s, "mangadex").await.unwrap();
        for n in 2..=21 { s.chapters.push(ch(n as f64, "https://x/r")); }
        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        let (a, b) = tokio::join!(claim_unnotified(&pool), claim_unnotified(&pool));
        let total: i64 = a.unwrap().iter().chain(b.unwrap().iter()).map(|c| c.chapters).sum();
        assert_eq!(total, 20);
    }

    #[tokio::test]
    async fn claim_skips_rows_locked_by_a_poll() {
        let pool = pool_or_skip!();
        let mut s = series(1, "https://x/lk");
        let id = insert_manhwa_with_chapters(&pool, &s, "mangadex").await.unwrap();
        s.chapters.push(ch(2.0, "https://x/lk"));
        s.chapters.push(ch(3.0, "https://x/lk"));
        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        // A concurrent poll transaction holds a lock on one unnotified chapter.
        let mut tx = pool.begin().await.unwrap();
        let locked: i64 = sqlx::query_scalar(
            "SELECT id FROM chapter WHERE manhwa_id = $1 AND number = 2 FOR UPDATE")
            .bind(id).fetch_one(&mut *tx).await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), claim_unnotified(&pool))
            .await.expect("claim must not block on locked rows").unwrap();
        assert_eq!(got, vec![Claimed { manhwa_id: id, title: s.title.clone(), chapters: 1 }]);
        tx.rollback().await.unwrap();
        let again = claim_unnotified(&pool).await.unwrap();
        assert_eq!(again, vec![Claimed { manhwa_id: id, title: s.title.clone(), chapters: 1 }], "skipped row {locked} is claimed next time");
    }

    #[tokio::test]
    async fn read_chapters_are_claimed_silently() {
        let pool = pool_or_skip!();
        let mut s = series(1, "https://x/s");
        let id = insert_manhwa_with_chapters(&pool, &s, "mangadex").await.unwrap();
        s.chapters.push(ch(2.0, "https://x/s"));
        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        let c2 = fetch_chapters(&pool, id).await.unwrap().into_iter().find(|c| c.number == 2.0).unwrap();
        start_chapter(&pool, c2.id).await.unwrap();
        update_scroll(&pool, c2.id, 1.0).await.unwrap();
        assert!(claim_unnotified(&pool).await.unwrap().is_empty());
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chapter WHERE notified_at IS NULL")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(pending, 0);
    }

    // --- M7: an empty scraped title never blanks the stored one ---

    #[tokio::test]
    async fn metadata_refresh_with_empty_title_keeps_the_old_one() {
        let pool = pool_or_skip!();
        let mut s = series(1, "https://x/t");
        let id = insert_manhwa_with_chapters(&pool, &s, "asura").await.unwrap();
        s.title = String::new();
        update_manhwa_metadata(&pool, id, &s).await.unwrap();
        assert_eq!(fetch_manhwa(&pool, id).await.unwrap().title, "Series https://x/t");
        s.title = "Renamed".into();
        update_manhwa_metadata(&pool, id, &s).await.unwrap();
        assert_eq!(fetch_manhwa(&pool, id).await.unwrap().title, "Renamed");
    }

    // --- L12: redirects persisted; Asura discoveries compare by slug ---

    const ASURA_OLD: &str = "https://asurascans.com/comics/solo-leveling-1a2b3c4d";
    const ASURA_NEW: &str = "https://asurascans.com/comics/solo-leveling-9f8e7d6c";

    #[tokio::test]
    async fn poll_stores_the_post_redirect_source_url() {
        let pool = pool_or_skip!();
        let mut s = series(1, ASURA_OLD);
        let id = insert_manhwa_with_chapters(&pool, &s, "asura").await.unwrap();
        s.source_url = ASURA_NEW.into();
        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        assert_eq!(fetch_manhwa(&pool, id).await.unwrap().source_url, ASURA_NEW);
    }

    #[tokio::test]
    async fn poll_redirect_onto_an_existing_row_keeps_both_and_succeeds() {
        let pool = pool_or_skip!();
        let mut s = series(1, ASURA_OLD);
        let id = insert_manhwa_with_chapters(&pool, &s, "asura").await.unwrap();
        insert_manhwa_with_chapters(&pool, &series(1, ASURA_NEW), "asura").await.unwrap();
        s.source_url = ASURA_NEW.into();
        s.chapters = vec![ch(2.0, ASURA_OLD)];
        assert_eq!(apply_poll_result(&pool, id, &s, "ongoing").await.unwrap(), 1);
        assert_eq!(fetch_manhwa(&pool, id).await.unwrap().source_url, ASURA_OLD);
    }

    #[tokio::test]
    async fn mark_read_through_marks_only_earlier_chapters() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(10, "https://x/import"), "asura").await.unwrap();
        assert_eq!(mark_read_through(&pool, id, 6.0).await.unwrap(), 6);
        assert_eq!(mark_read_through(&pool, id, 6.0).await.unwrap(), 0, "idempotent");
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::Reading);
        assert_eq!(mark_read_through(&pool, id, 99.0).await.unwrap(), 4);
        assert_eq!(recompute_status(&pool, id).await.unwrap(), Status::UpToDate);
    }

    #[tokio::test]
    async fn asura_discovery_ignores_hash_rotation() {
        let pool = pool_or_skip!();
        // In the library under the old hash: the rotated URL is not new.
        insert_manhwa_with_chapters(&pool, &series(1, ASURA_OLD), "asura").await.unwrap();
        assert!(!upsert_discovery(&pool, "asura", ASURA_NEW, "SL", None, None, None).await.unwrap());
        assert!(fetch_discoveries(&pool).await.unwrap().is_empty());
        // A tracking query on the discovered URL changes nothing.
        let with_query = format!("{ASURA_NEW}?ref=home");
        assert!(!upsert_discovery(&pool, "asura", &with_query, "SL", None, None, None).await.unwrap());
        assert!(fetch_discoveries(&pool).await.unwrap().is_empty());

        // Dismissed under one hash, re-surfaced under another: stays dismissed.
        let a = "https://asurascans.com/comics/tower-of-god-00aa11bb";
        let b = "https://asuracomic.net/series/tower-of-god-22cc33dd/";
        assert!(upsert_discovery(&pool, "asura", a, "ToG", None, Some(1.0), None).await.unwrap());
        let d = fetch_discoveries(&pool).await.unwrap();
        dismiss_discovery(&pool, d[0].id).await.unwrap();
        assert!(!upsert_discovery(&pool, "asura", b, "ToG", None, Some(2.0), None).await.unwrap());
        assert!(fetch_discoveries(&pool).await.unwrap().is_empty());

        // Other sources still compare exact URLs.
        assert!(upsert_discovery(&pool, "mangack", "https://m/manga/solo-leveling-1a2b3c4d/", "SL", None, None, None).await.unwrap());
    }

    // --- M9: per-series poll health ---

    #[tokio::test]
    async fn poll_failure_is_recorded_and_cleared_by_the_next_success() {
        let pool = pool_or_skip!();
        let s = series(1, "https://x/h");
        let id = insert_manhwa_with_chapters(&pool, &s, "mangack").await.unwrap();
        let m = fetch_manhwa(&pool, id).await.unwrap();
        assert_eq!((m.last_error, m.failing_since), (None, None));

        record_poll_failure(&pool, id, "HTTP 404").await.unwrap();
        let m = fetch_manhwa(&pool, id).await.unwrap();
        assert_eq!(m.last_error.as_deref(), Some("HTTP 404"));
        let since = m.failing_since.clone().expect("failing_since set");
        assert!(since.starts_with("20"), "timestamp text: {since}");

        // A later failure updates the error but keeps the first failure time.
        sqlx::query("UPDATE manhwa SET failing_since = '2026-01-02 03:04:05+00' WHERE id = $1")
            .bind(id).execute(&pool).await.unwrap();
        record_poll_failure(&pool, id, "HTTP 503").await.unwrap();
        let all = fetch_all_manhwa(&pool).await.unwrap();
        assert_eq!(all[0].last_error.as_deref(), Some("HTTP 503"));
        assert!(all[0].failing_since.as_deref().unwrap().starts_with("2026-01-02"));

        apply_poll_result(&pool, id, &s, "ongoing").await.unwrap();
        let m = fetch_manhwa(&pool, id).await.unwrap();
        assert_eq!((m.last_error, m.failing_since), (None, None));
        let ok: bool = sqlx::query_scalar("SELECT last_ok_at IS NOT NULL FROM manhwa WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert!(ok, "success stamps last_ok_at");
    }

    #[tokio::test]
    async fn poll_failure_text_is_truncated_on_a_char_boundary() {
        let pool = pool_or_skip!();
        let id = insert_manhwa_with_chapters(&pool, &series(1, "https://x/u"), "mangack").await.unwrap();
        let long = "한".repeat(400);
        record_poll_failure(&pool, id, &long).await.unwrap();
        let e = fetch_manhwa(&pool, id).await.unwrap().last_error.unwrap();
        assert_eq!(e.chars().count(), 300);
    }
}
