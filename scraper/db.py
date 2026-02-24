"""
mrm - database layer
All SQLite interaction goes through this module.
Shared between the scraper daemon and (via file) the Rust TUI.
"""

import aiosqlite
import asyncio
from datetime import datetime
from pathlib import Path
from typing import Optional

DB_PATH = Path(__file__).parent.parent / "mrm.db"


# ---------------------------------------------------------------------------
# Schema
# ---------------------------------------------------------------------------

SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS manhwa (
    id          INTEGER PRIMARY KEY,
    title       TEXT NOT NULL,
    cover_url   TEXT,
    source      TEXT NOT NULL,
    source_url  TEXT NOT NULL UNIQUE,
    pub_status  TEXT CHECK(pub_status IN ('ongoing','hiatus','completed')) DEFAULT 'ongoing',
    status      TEXT CHECK(status IN (
                    'looked_into','reading','up_to_date',
                    'paused','completed','dropped'
                )) NOT NULL DEFAULT 'looked_into',
    status_override INTEGER NOT NULL DEFAULT 0,  -- 1 = user manually set, don't auto-update
    added_at    DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at  DATETIME DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS chapter (
    id          INTEGER PRIMARY KEY,
    manhwa_id   INTEGER NOT NULL REFERENCES manhwa(id) ON DELETE CASCADE,
    number      REAL NOT NULL,
    title       TEXT,
    url         TEXT NOT NULL UNIQUE,
    released_at DATETIME,
    UNIQUE(manhwa_id, number)
);

CREATE TABLE IF NOT EXISTS progress (
    id            INTEGER PRIMARY KEY,
    chapter_id    INTEGER NOT NULL UNIQUE REFERENCES chapter(id) ON DELETE CASCADE,
    started_at    DATETIME,
    completed_at  DATETIME,       -- NULL = not finished / not read
    scrolled_pct  REAL NOT NULL DEFAULT 0.0  -- 0.0–1.0; only "read" at 1.0
);

CREATE INDEX IF NOT EXISTS idx_chapter_manhwa ON chapter(manhwa_id);
CREATE INDEX IF NOT EXISTS idx_progress_chapter ON progress(chapter_id);
"""


# ---------------------------------------------------------------------------
# Connection
# ---------------------------------------------------------------------------

async def get_db() -> aiosqlite.Connection:
    db = await aiosqlite.connect(DB_PATH)
    db.row_factory = aiosqlite.Row
    await db.executescript(SCHEMA)
    return db


# ---------------------------------------------------------------------------
# Manhwa
# ---------------------------------------------------------------------------

async def upsert_manhwa(
    db: aiosqlite.Connection,
    *,
    title: str,
    source: str,
    source_url: str,
    cover_url: Optional[str] = None,
    pub_status: str = "ongoing",
) -> int:
    """Insert or update a manhwa by source_url. Returns its id."""
    await db.execute(
        """
        INSERT INTO manhwa (title, source, source_url, cover_url, pub_status, updated_at)
        VALUES (:title, :source, :source_url, :cover_url, :pub_status, CURRENT_TIMESTAMP)
        ON CONFLICT(source_url) DO UPDATE SET
            title      = excluded.title,
            cover_url  = excluded.cover_url,
            pub_status = excluded.pub_status,
            updated_at = CURRENT_TIMESTAMP
        """,
        {
            "title": title,
            "source": source,
            "source_url": source_url,
            "cover_url": cover_url,
            "pub_status": pub_status,
        },
    )
    await db.commit()
    cursor = await db.execute(
        "SELECT id FROM manhwa WHERE source_url = ?", (source_url,)
    )
    row = await cursor.fetchone()
    return row["id"]


async def get_manhwa(db: aiosqlite.Connection, manhwa_id: int) -> Optional[aiosqlite.Row]:
    cursor = await db.execute("SELECT * FROM manhwa WHERE id = ?", (manhwa_id,))
    return await cursor.fetchone()


async def get_all_manhwa(db: aiosqlite.Connection) -> list[aiosqlite.Row]:
    cursor = await db.execute("SELECT * FROM manhwa ORDER BY updated_at DESC")
    return await cursor.fetchall()


async def set_status_override(
    db: aiosqlite.Connection, manhwa_id: int, status: str
) -> None:
    """Manually set status — marks it as user-overridden so auto-compute skips it."""
    await db.execute(
        """
        UPDATE manhwa SET status = ?, status_override = 1, updated_at = CURRENT_TIMESTAMP
        WHERE id = ?
        """,
        (status, manhwa_id),
    )
    await db.commit()


async def clear_status_override(db: aiosqlite.Connection, manhwa_id: int) -> None:
    """Re-enable automatic status computation for a manhwa."""
    await db.execute(
        "UPDATE manhwa SET status_override = 0 WHERE id = ?", (manhwa_id,)
    )
    await db.commit()


# ---------------------------------------------------------------------------
# Chapters
# ---------------------------------------------------------------------------

async def upsert_chapters(
    db: aiosqlite.Connection,
    manhwa_id: int,
    chapters: list[dict],
) -> list[int]:
    """
    Upsert a list of chapter dicts. Each dict must have:
        number, url, and optionally title, released_at.
    Returns ids of chapters that are NEW (not previously known).
    """
    new_ids = []
    for ch in chapters:
        cursor = await db.execute(
            """
            INSERT INTO chapter (manhwa_id, number, title, url, released_at)
            VALUES (:manhwa_id, :number, :title, :url, :released_at)
            ON CONFLICT(manhwa_id, number) DO UPDATE SET
                title      = excluded.title,
                url        = excluded.url,
                released_at = excluded.released_at
            RETURNING id, (SELECT COUNT(*) FROM progress WHERE chapter_id = id) as has_progress
            """,
            {
                "manhwa_id": manhwa_id,
                "number": ch["number"],
                "title": ch.get("title"),
                "url": ch["url"],
                "released_at": ch.get("released_at"),
            },
        )
        row = await cursor.fetchone()
        if row and row["has_progress"] == 0:
            new_ids.append(row["id"])

    await db.commit()
    return new_ids


async def get_chapters(
    db: aiosqlite.Connection, manhwa_id: int
) -> list[aiosqlite.Row]:
    cursor = await db.execute(
        "SELECT * FROM chapter WHERE manhwa_id = ? ORDER BY number ASC",
        (manhwa_id,),
    )
    return await cursor.fetchall()


# ---------------------------------------------------------------------------
# Progress
# ---------------------------------------------------------------------------

async def start_chapter(db: aiosqlite.Connection, chapter_id: int) -> None:
    await db.execute(
        """
        INSERT INTO progress (chapter_id, started_at, scrolled_pct)
        VALUES (?, CURRENT_TIMESTAMP, 0.0)
        ON CONFLICT(chapter_id) DO NOTHING
        """,
        (chapter_id,),
    )
    await db.commit()


async def update_scroll(
    db: aiosqlite.Connection, chapter_id: int, pct: float
) -> bool:
    """
    Update scroll progress. Marks chapter complete if pct >= 1.0.
    Returns True if this call newly completed the chapter.
    """
    pct = max(0.0, min(1.0, pct))
    completed_at = datetime.utcnow().isoformat() if pct >= 1.0 else None

    cursor = await db.execute(
        """
        UPDATE progress
        SET scrolled_pct = MAX(scrolled_pct, :pct),
            completed_at = COALESCE(completed_at, :completed_at)
        WHERE chapter_id = :chapter_id
        RETURNING (completed_at IS NOT NULL AND :pct >= 1.0) as just_completed
        """,
        {"pct": pct, "completed_at": completed_at, "chapter_id": chapter_id},
    )
    row = await cursor.fetchone()
    await db.commit()
    return bool(row and row["just_completed"])


async def get_progress(
    db: aiosqlite.Connection, chapter_id: int
) -> Optional[aiosqlite.Row]:
    cursor = await db.execute(
        "SELECT * FROM progress WHERE chapter_id = ?", (chapter_id,)
    )
    return await cursor.fetchone()


# ---------------------------------------------------------------------------
# Status computation
# ---------------------------------------------------------------------------

async def compute_status(db: aiosqlite.Connection, manhwa_id: int) -> str:
    """
    Derive the correct status from reading history.
    Does NOT write to DB — call apply_computed_status for that.
    """
    cursor = await db.execute(
        """
        SELECT
            COUNT(c.id)                                         AS total_chapters,
            COUNT(p.completed_at)                               AS read_chapters,
            COUNT(c.id) - COUNT(p.completed_at)                AS unread_chapters,
            m.pub_status,
            m.status_override
        FROM manhwa m
        LEFT JOIN chapter c  ON c.manhwa_id = m.id
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE m.id = ?
        GROUP BY m.id
        """,
        (manhwa_id,),
    )
    row = await cursor.fetchone()
    if not row:
        return "looked_into"

    if row["status_override"]:
        m = await get_manhwa(db, manhwa_id)
        return m["status"]

    read     = row["read_chapters"]
    unread   = row["unread_chapters"]
    pub      = row["pub_status"]

    if read == 0:
        return "looked_into"
    if read <= 5:
        return "looked_into"
    if unread > 0:
        return "reading"
    # unread == 0 from here
    if pub == "completed":
        return "completed"
    if pub == "hiatus":
        return "paused"
    return "up_to_date"


async def apply_computed_status(db: aiosqlite.Connection, manhwa_id: int) -> str:
    """Compute and persist status. Returns the new status string."""
    status = await compute_status(db, manhwa_id)
    await db.execute(
        """
        UPDATE manhwa SET status = ?, updated_at = CURRENT_TIMESTAMP
        WHERE id = ? AND status_override = 0
        """,
        (status, manhwa_id),
    )
    await db.commit()
    return status


# ---------------------------------------------------------------------------
# Unread helpers  (used by TUI and notifications)
# ---------------------------------------------------------------------------

async def get_unread_count(db: aiosqlite.Connection, manhwa_id: int) -> int:
    cursor = await db.execute(
        """
        SELECT COUNT(c.id) AS unread
        FROM chapter c
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE c.manhwa_id = ? AND (p.completed_at IS NULL)
        """,
        (manhwa_id,),
    )
    row = await cursor.fetchone()
    return row["unread"] if row else 0


async def get_manhwa_with_unread(db: aiosqlite.Connection) -> list[aiosqlite.Row]:
    """All manhwa that have at least one unread chapter."""
    cursor = await db.execute(
        """
        SELECT m.*, COUNT(c.id) AS unread
        FROM manhwa m
        JOIN chapter c ON c.manhwa_id = m.id
        LEFT JOIN progress p ON p.chapter_id = c.id
        WHERE p.completed_at IS NULL
        GROUP BY m.id
        HAVING unread > 0
        ORDER BY m.updated_at DESC
        """
    )
    return await cursor.fetchall()
