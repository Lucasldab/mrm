"""
mrm - MangaDex scraper (api.mangadex.org)

MangaDex has a clean public REST API — no scraping needed.

Key endpoints used:
  Search    : GET /manga?title=...&availableTranslatedLanguage[]=en
  Series    : GET /manga/{id}?includes[]=cover_art
  Chapters  : GET /manga/{id}/feed?translatedLanguage[]=en&order[chapter]=asc&limit=500
  Images    : GET /at-home/server/{chapter_id}
                -> baseUrl + /data/ + hash + / + filename

Cover image URL pattern:
  https://uploads.mangadex.org/covers/{manga_id}/{filename}
"""

from __future__ import annotations

import asyncio
from typing import Optional

from .base import BaseScraper, ScraperError

BASE = "https://api.mangadex.org"
COVER_BASE = "https://uploads.mangadex.org/covers"

_STATUS_MAP = {
    "ongoing":   "ongoing",
    "hiatus":    "hiatus",
    "completed": "completed",
    "cancelled": "completed",
}


def _status(raw: Optional[str]) -> str:
    return _STATUS_MAP.get((raw or "").lower(), "ongoing")


def _title(attributes: dict) -> str:
    """Extract best English title from MangaDex title dict."""
    t = attributes.get("title", {})
    return (
        t.get("en")
        or t.get("ja-ro")
        or next(iter(t.values()), "")
    )


class MangaDexScraper(BaseScraper):
    SOURCE = "mangadex"

    # MangaDex asks clients to rate-limit to ~5 req/s.
    # We add a small delay between paginated requests to be polite.
    _DELAY = 0.25

    # -----------------------------------------------------------------------
    # Search
    # -----------------------------------------------------------------------

    async def search(self, query: str) -> list[dict]:
        resp = await self.get(
            f"{BASE}/manga",
            params={
                "title": query,
                "limit": 20,
                "availableTranslatedLanguage[]": "en",
                "includes[]": "cover_art",
                "contentRating[]": ["safe", "suggestive", "erotica"],
                "order[relevance]": "desc",
            },
        )
        data = resp.json()

        results = []
        for item in data.get("data", []):
            manga_id = item["id"]
            attr = item.get("attributes", {})
            cover_url = self._extract_cover(manga_id, item.get("relationships", []))
            results.append({
                "title":      _title(attr),
                "cover_url":  cover_url,
                "source_url": f"{BASE}/manga/{manga_id}",
                "pub_status": _status(attr.get("status")),
            })

        return results

    # -----------------------------------------------------------------------
    # Series metadata + full chapter list
    # -----------------------------------------------------------------------

    async def get_series(self, source_url: str) -> dict:
        manga_id = self._id_from_url(source_url)

        # Fetch manga metadata
        resp = await self.get(
            f"{BASE}/manga/{manga_id}",
            params={"includes[]": "cover_art"},
        )
        item = resp.json().get("data", {})
        attr = item.get("attributes", {})
        cover_url = self._extract_cover(manga_id, item.get("relationships", []))

        # Fetch ALL chapters (paginated, 500 per page)
        chapters = await self._fetch_all_chapters(manga_id)

        return {
            "title":      _title(attr),
            "cover_url":  cover_url,
            "source_url": source_url,
            "pub_status": _status(attr.get("status")),
            "chapters":   chapters,
        }

    # -----------------------------------------------------------------------
    # Chapter images
    # -----------------------------------------------------------------------

    async def get_chapter_image_urls(self, chapter_url: str) -> list[str]:
        """
        chapter_url is the MangaDex chapter UUID.
        We hit /at-home/server/{id} to get the image server and filenames.
        """
        chapter_id = self._id_from_url(chapter_url)
        resp = await self.get(f"{BASE}/at-home/server/{chapter_id}")
        data = resp.json()

        base_url     = data["baseUrl"]
        chapter_hash = data["chapter"]["hash"]
        filenames    = data["chapter"]["data"]  # full quality

        return [
            f"{base_url}/data/{chapter_hash}/{filename}"
            for filename in filenames
        ]

    # -----------------------------------------------------------------------
    # Helpers
    # -----------------------------------------------------------------------

    async def _fetch_all_chapters(self, manga_id: str) -> list[dict]:
        """Paginate through /manga/{id}/feed to get every English chapter."""
        chapters = []
        offset   = 0
        limit    = 500
        seen_numbers: set[float] = set()

        while True:
            await asyncio.sleep(self._DELAY)
            resp = await self.get(
                f"{BASE}/manga/{manga_id}/feed",
                params={
                    "translatedLanguage[]": "en",
                    "order[chapter]":       "asc",
                    "limit":                limit,
                    "offset":               offset,
                    "contentRating[]":      ["safe", "suggestive", "erotica"],
                },
            )
            data  = resp.json()
            items = data.get("data", [])
            total = data.get("total", 0)

            for item in items:
                attr       = item.get("attributes", {})
                ch_str     = attr.get("chapter")        # e.g. "42", "42.5", null
                if ch_str is None:
                    continue
                try:
                    number = float(ch_str)
                except ValueError:
                    continue

                # MangaDex often has duplicate chapter numbers from different
                # scanlation groups — keep only the first (earliest uploaded).
                if number in seen_numbers:
                    continue
                seen_numbers.add(number)

                chapters.append({
                    "number":      number,
                    "title":       attr.get("title") or f"Chapter {ch_str}",
                    "url":         f"{BASE}/chapter/{item['id']}",
                    "released_at": (attr.get("publishAt") or "")[:10] or None,
                })

            offset += len(items)
            if offset >= total or not items:
                break

        return chapters

    def _extract_cover(self, manga_id: str, relationships: list) -> Optional[str]:
        """Build cover URL from the included cover_art relationship."""
        for rel in relationships:
            if rel.get("type") == "cover_art":
                filename = rel.get("attributes", {}).get("fileName")
                if filename:
                    return f"{COVER_BASE}/{manga_id}/{filename}"
        return None

    def _id_from_url(self, url: str) -> str:
        """
        Extract UUID from URLs like:
          https://api.mangadex.org/manga/abc-123
          https://api.mangadex.org/chapter/xyz-456
        """
        return url.rstrip("/").split("/")[-1]
