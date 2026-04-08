use anyhow::Result;
use chrono::Utc;
use sqlx::{sqlite::SqlitePool, Row};

use crate::types::{Chapter, Manhwa, Status};

// ---------------------------------------------------------------------------
// Pool
// ---------------------------------------------------------------------------

pub async fn open_db(path: &str) -> Result<SqlitePool> {
    let url = format!("sqlite:{path}");
    let pool = SqlitePool::connect(&url).await?;

    // Make sure WAL mode is on and foreign keys are enabled
    sqlx::query("PRAGMA journal_mode=WAL").execute(&pool).await?;
    sqlx::query("PRAGMA foreign_keys=ON").execute(&pool).await?;
    sqlx::query("PRAGMA busy_timeout=5000").execute(&pool).await?;

    Ok(pool)
}

// ---------------------------------------------------------------------------
// Manhwa queries
// ---------------------------------------------------------------------------

pub async fn fetch_all_manhwa(pool: &SqlitePool) -> Result<Vec<Manhwa>> {
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
            status_override: row.try_get::<i64, _>("status_override")? != 0,
            unread,
        });
    }

    Ok(manhwa_list)
}

pub async fn fetch_manhwa(pool: &SqlitePool, manhwa_id: i64) -> Result<Manhwa> {
    let row = sqlx::query(
        r#"
        SELECT
            m.id, m.title, m.cover_url, m.source, m.source_url,
            m.pub_status, m.status, m.status_override,
            COUNT(c.id)           AS total_chapters,
            COUNT(p.completed_at) AS read_chapters
        FROM manhwa m
        LEFT JOIN chapter  c ON c.manhwa_id = m.id
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE m.id = ?
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
        status_override: row.try_get::<i64, _>("status_override")? != 0,
        unread,
    })
}

pub async fn set_manhwa_status(
    pool: &SqlitePool,
    manhwa_id: i64,
    status: &Status,
    is_override: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE manhwa SET status = ?, status_override = ?, updated_at = CURRENT_TIMESTAMP
         WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(is_override as i64)
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
    pool: &SqlitePool,
    series: &crate::scraper::SeriesData,
    source: &str,
) -> Result<i64> {
    let mut tx = pool.begin().await?;

    // Check for duplicate source_url
    let exists: bool = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM manhwa WHERE source_url = ?)",
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
                            status, status_override, updated_at)
        VALUES (?, ?, ?, ?, ?, 'looked_into', 0, CURRENT_TIMESTAMP)
        RETURNING id
        "#,
    )
    .bind(&series.title)
    .bind(source)
    .bind(&series.source_url)
    .bind(&series.cover_url)
    .bind(&series.pub_status)
    .fetch_one(&mut *tx)
    .await?;

    let manhwa_id: i64 = row.try_get("id")?;

    // Upsert all chapters within the same transaction
    for ch in &series.chapters {
        sqlx::query(
            r#"
            INSERT INTO chapter (manhwa_id, number, title, url, released_at)
            VALUES (?, ?, ?, ?, ?)
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

/// Delete a manhwa and all associated chapters and progress records.
/// Relies on ON DELETE CASCADE (confirmed in schema).
pub async fn delete_manhwa(pool: &SqlitePool, manhwa_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM manhwa WHERE id = ?")
        .bind(manhwa_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Clear the status override for a manhwa, re-enabling auto-computation.
pub async fn clear_status_override(pool: &SqlitePool, manhwa_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE manhwa SET status_override = 0, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(manhwa_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Chapter queries
// ---------------------------------------------------------------------------

pub async fn fetch_chapters(pool: &SqlitePool, manhwa_id: i64) -> Result<Vec<Chapter>> {
    let rows = sqlx::query(
        r#"
        SELECT
            c.id, c.manhwa_id, c.number, c.title, c.url, c.released_at,
            COALESCE(p.scrolled_pct, 0.0)         AS scroll_pct,
            (p.completed_at IS NOT NULL)           AS completed
        FROM chapter c
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE c.manhwa_id = ?
        ORDER BY c.number ASC
        "#,
    )
    .bind(manhwa_id)
    .fetch_all(pool)
    .await?;

    let chapters = rows
        .iter()
        .map(|row| Chapter {
            id:          row.try_get("id").unwrap_or(0),
            manhwa_id:   row.try_get("manhwa_id").unwrap_or(0),
            number:      row.try_get("number").unwrap_or(0.0),
            title:       row.try_get("title").unwrap_or(None),
            url:         row.try_get("url").unwrap_or_default(),
            released_at: row.try_get("released_at").unwrap_or(None),
            scroll_pct:  row.try_get("scroll_pct").unwrap_or(0.0),
            completed:   row.try_get::<i64, _>("completed").unwrap_or(0) != 0,
        })
        .collect();

    Ok(chapters)
}

// ---------------------------------------------------------------------------
// Chapter upsert (coordinator writes)
// ---------------------------------------------------------------------------

/// Upsert a batch of chapters for a manhwa.
///
/// Each chapter is inserted with ON CONFLICT(manhwa_id, number) DO UPDATE
/// so title/url/released_at are refreshed even for known chapters.
///
/// Returns the count of chapters that are genuinely new — defined as having
/// no entry in the progress table (i.e., the user has never started them).
/// This count is used to decide whether to send a desktop notification.
pub async fn upsert_chapters(
    pool: &SqlitePool,
    manhwa_id: i64,
    chapters: &[crate::scraper::ChapterData],
) -> Result<usize> {
    if chapters.is_empty() {
        return Ok(0);
    }

    let mut new_count = 0usize;

    for ch in chapters {
        // Check if this chapter already exists before upserting.
        let already_exists: bool = sqlx::query(
            "SELECT EXISTS(SELECT 1 FROM chapter WHERE manhwa_id = ? AND number = ?)",
        )
        .bind(manhwa_id)
        .bind(ch.number)
        .fetch_one(pool)
        .await?
        .try_get::<bool, _>(0)
        .unwrap_or(false);

        // Upsert the chapter row.
        sqlx::query(
            r#"
            INSERT INTO chapter (manhwa_id, number, title, url, released_at)
            VALUES (?, ?, ?, ?, ?)
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
        .execute(pool)
        .await?;

        if !already_exists {
            new_count += 1;
        }
    }

    Ok(new_count)
}

// ---------------------------------------------------------------------------
// Progress queries
// ---------------------------------------------------------------------------

pub async fn start_chapter(pool: &SqlitePool, chapter_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO progress (chapter_id, started_at, scrolled_pct)
        VALUES (?, CURRENT_TIMESTAMP, 0.0)
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
pub async fn mark_all_chapters_read(pool: &SqlitePool, manhwa_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO progress (chapter_id, started_at, scrolled_pct, completed_at)
        SELECT c.id, CURRENT_TIMESTAMP, 1.0, CURRENT_TIMESTAMP
        FROM chapter c
        WHERE c.manhwa_id = ?
          AND c.id NOT IN (SELECT chapter_id FROM progress WHERE completed_at IS NOT NULL)
        ON CONFLICT(chapter_id) DO UPDATE SET
            scrolled_pct = 1.0,
            completed_at = COALESCE(progress.completed_at, CURRENT_TIMESTAMP)
        "#,
    )
    .bind(manhwa_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Update scroll progress. Returns true if this call newly completed the chapter.
pub async fn update_scroll(
    pool: &SqlitePool,
    chapter_id: i64,
    pct: f64,
) -> Result<bool> {
    let pct = pct.clamp(0.0, 1.0);
    let completed_at: Option<String> = if pct >= 1.0 {
        Some(Utc::now().format("%Y-%m-%d %H:%M:%S").to_string())
    } else {
        None
    };

    // Check if it was already completed before this update
    let was_completed: bool = sqlx::query(
        "SELECT completed_at IS NOT NULL FROM progress WHERE chapter_id = ?",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?
    .map(|r| r.try_get::<bool, _>(0).unwrap_or(false))
    .unwrap_or(false);

    sqlx::query(
        r#"
        UPDATE progress
        SET scrolled_pct = MAX(scrolled_pct, ?),
            completed_at = COALESCE(completed_at, ?)
        WHERE chapter_id = ?
        "#,
    )
    .bind(pct)
    .bind(&completed_at)
    .bind(chapter_id)
    .execute(pool)
    .await?;

    // Newly completed = pct hit 1.0 AND wasn't completed before
    Ok(pct >= 1.0 && !was_completed)
}

/// Recompute and persist status for a manhwa (mirrors Python's apply_computed_status).
pub async fn recompute_status(pool: &SqlitePool, manhwa_id: i64) -> Result<Status> {
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
        WHERE m.id = ?
        GROUP BY m.id
        "#,
    )
    .bind(manhwa_id)
    .fetch_one(pool)
    .await?;

    let status_override: i64 = row.try_get("status_override")?;
    if status_override != 0 {
        // User has manually set status — don't touch it
        let current = sqlx::query("SELECT status FROM manhwa WHERE id = ?")
            .bind(manhwa_id)
            .fetch_one(pool)
            .await?;
        return Ok(Status::from_str(current.try_get("status")?));
    }

    let read:   i64  = row.try_get("read")?;
    let unread: i64  = row.try_get("unread")?;
    let pub_st: &str = row.try_get("pub_status")?;

    let status = if read < 5 {
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
        "UPDATE manhwa SET status = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(manhwa_id)
    .execute(pool)
    .await?;

    Ok(status)
}

