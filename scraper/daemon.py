"""
mrm - scraper daemon

Polls MangaDex and mangack for new chapters on all tracked manhwa,
updates the SQLite database, recomputes statuses, and fires
desktop notifications via notify-send (mako).

Usage:
    python -m scraper.daemon          # run forever
    python -m scraper.daemon --once   # single poll then exit (useful for cron)
"""

from __future__ import annotations

import asyncio
import logging
import subprocess
import sys
from datetime import datetime

import aiosqlite

from .config import load_config
from .db import (
    get_db,
    get_all_manhwa,
    upsert_chapters,
    apply_computed_status,
    get_unread_count,
)
from .scrapers.mangadex import MangaDexScraper
from .scrapers.mangack import MangackScraper
from .scrapers.asura import AsuraScraper
from .scrapers.base import ScraperError

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s  %(levelname)-8s  %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("mrm.daemon")

# Map source name → scraper class
SCRAPERS = {
    "mangadex": MangaDexScraper,
    "mangack":  MangackScraper,
    "asura":    AsuraScraper,
}


# ---------------------------------------------------------------------------
# Notifications
# ---------------------------------------------------------------------------

def notify(title: str, body: str, urgency: str = "normal") -> None:
    """
    Fire a desktop notification via notify-send (works with mako).
    Fails silently if notify-send is not available.
    """
    try:
        subprocess.run(
            [
                "notify-send",
                "--app-name", "mrm",
                "--urgency", urgency,
                "--icon", "dialog-information",
                title,
                body,
            ],
            check=False,
            timeout=5,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        log.debug("notify-send not available, skipping notification")


# ---------------------------------------------------------------------------
# Core poll logic
# ---------------------------------------------------------------------------

async def poll_manhwa(
    db: aiosqlite.Connection,
    manhwa_row,
    scrapers: dict,
) -> int:
    """
    Refresh chapters for a single manhwa.
    Returns the number of genuinely new chapters found.
    """
    source  = manhwa_row["source"]
    scraper = scrapers.get(source)

    if scraper is None:
        log.warning("No scraper for source '%s', skipping %s", source, manhwa_row["title"])
        return 0

    try:
        series = await scraper.get_series(manhwa_row["source_url"])
    except ScraperError as e:
        log.warning("Failed to fetch %s: %s", manhwa_row["title"], e)
        return 0

    new_chapter_ids = await upsert_chapters(
        db,
        manhwa_row["id"],
        series["chapters"],
    )

    new_count = len(new_chapter_ids)

    if new_count > 0:
        log.info(
            "%-40s  +%d new chapter%s",
            manhwa_row["title"][:40],
            new_count,
            "s" if new_count != 1 else "",
        )

    # Always recompute status after a refresh
    new_status = await apply_computed_status(db, manhwa_row["id"])
    log.debug("%-40s  status → %s", manhwa_row["title"][:40], new_status)

    return new_count


async def poll_all(cfg: dict) -> None:
    """Poll every tracked manhwa once."""
    log.info("── Poll started at %s ──", datetime.now().strftime("%Y-%m-%d %H:%M"))

    sources = cfg.get("sources", {})
    notifications_enabled = cfg.get("notifications", {}).get("enabled", True)

    # Build one scraper instance per enabled source
    scraper_instances: dict = {}
    for name, source_cfg in sources.items():
        if not source_cfg.get("enabled", True):
            continue
        cls = SCRAPERS.get(name)
        if cls is None:
            log.warning("Unknown source '%s' in config, skipping", name)
            continue
        scraper_instances[name] = cls(source_cfg["base_url"])

    if not scraper_instances:
        log.warning("No enabled sources found in config")
        return

    # Open all scrapers as async context managers
    active_scrapers = {}
    for name, scraper in scraper_instances.items():
        await scraper.__aenter__()
        active_scrapers[name] = scraper

    db = await get_db()
    try:
        manhwa_list = await get_all_manhwa(db)
        log.info("Checking %d manhwa across %d source(s)", len(manhwa_list), len(active_scrapers))

        newly_updated: list[tuple[str, int]] = []  # (title, unread_count)

        for manhwa in manhwa_list:
            new_count = await poll_manhwa(db, manhwa, active_scrapers)
            if new_count > 0:
                unread = await get_unread_count(db, manhwa["id"])
                newly_updated.append((manhwa["title"], unread))

        # Send a single grouped notification if anything updated
        if notifications_enabled and newly_updated:
            if len(newly_updated) == 1:
                title, unread = newly_updated[0]
                notify(
                    "New chapters available",
                    f"{title} — {unread} unread",
                    urgency="normal",
                )
            else:
                summary = "\n".join(
                    f"• {t} ({u} unread)" for t, u in newly_updated[:8]
                )
                if len(newly_updated) > 8:
                    summary += f"\n…and {len(newly_updated) - 8} more"
                notify(
                    f"{len(newly_updated)} manhwa updated",
                    summary,
                    urgency="normal",
                )

    finally:
        await db.close()
        for scraper in active_scrapers.values():
            await scraper.__aexit__(None, None, None)

    log.info("── Poll complete ──")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

async def run_once() -> None:
    cfg = load_config()
    await poll_all(cfg)


async def run_forever() -> None:
    cfg = load_config()
    interval = cfg.get("notifications", {}).get("poll_interval_minutes", 30)
    log.info("mrm daemon starting — polling every %d minutes", interval)

    while True:
        try:
            await poll_all(cfg)
        except Exception as e:
            log.error("Unexpected error during poll: %s", e, exc_info=True)

        log.info("Next poll in %d minutes", interval)
        await asyncio.sleep(interval * 60)


if __name__ == "__main__":
    once = "--once" in sys.argv
    if once:
        asyncio.run(run_once())
    else:
        asyncio.run(run_forever())
