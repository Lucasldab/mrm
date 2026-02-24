"""
mrm - Asura scraper (asuracomic.net)

Asura is a Next.js site. We extract the __NEXT_DATA__ JSON blob embedded
in every page rather than parsing HTML — this is far more stable than CSS
selectors and survives most redesigns.

Series URL pattern : {base}/series/{slug}
Chapter URL pattern: {base}/series/{slug}/{chapter-slug}
"""

import json
import re
from datetime import datetime
from typing import Optional
from selectolax.parser import HTMLParser

from .base import BaseScraper, ScraperError


# Matches the __NEXT_DATA__ script tag
_NEXT_DATA_RE = re.compile(r'<script id="__NEXT_DATA__"[^>]*>(.*?)</script>', re.DOTALL)

# Maps Asura's status strings to our pub_status values
_STATUS_MAP = {
    "ongoing":   "ongoing",
    "hiatus":    "hiatus",
    "completed": "completed",
    "dropped":   "completed",  # treat dropped as completed for our purposes
    "seasonal":  "ongoing",
}


def _parse_next_data(html: str) -> dict:
    m = _NEXT_DATA_RE.search(html)
    if not m:
        raise ScraperError("Could not find __NEXT_DATA__ in page — site may have changed")
    return json.loads(m.group(1))


def _normalize_status(raw: str) -> str:
    return _STATUS_MAP.get(raw.lower().strip(), "ongoing")


def _parse_date(raw: Optional[str]) -> Optional[str]:
    """Try to parse various date formats Asura uses into ISO format."""
    if not raw:
        return None
    for fmt in ("%B %d, %Y", "%Y-%m-%d", "%d %B %Y", "%b %d, %Y"):
        try:
            return datetime.strptime(raw.strip(), fmt).isoformat()
        except ValueError:
            continue
    return None


class AsuraScraper(BaseScraper):
    SOURCE = "asura"

    # -----------------------------------------------------------------------
    # Search
    # -----------------------------------------------------------------------

    async def search(self, query: str) -> list[dict]:
        """
        Asura doesn't have a public search API, so we hit their /series page
        with a search param and parse the results from __NEXT_DATA__.
        """
        url = f"{self.base_url}/series"
        resp = await self.get(url, params={"name": query})
        data = _parse_next_data(resp.text)

        # The series list lives at: props.pageProps.series (list of series objects)
        try:
            series_list = data["props"]["pageProps"]["series"]
        except KeyError:
            # Fallback: try to find it anywhere in the tree
            series_list = self._find_series_list(data)

        results = []
        for s in series_list:
            results.append({
                "title":      s.get("title") or s.get("name", ""),
                "cover_url":  s.get("image") or s.get("thumb") or s.get("cover"),
                "source_url": f"{self.base_url}/series/{s['slug']}",
                "pub_status": _normalize_status(s.get("status", "ongoing")),
            })
        return results

    # -----------------------------------------------------------------------
    # Series metadata + chapter list
    # -----------------------------------------------------------------------

    async def get_series(self, source_url: str) -> dict:
        resp = await self.get(source_url)
        data = _parse_next_data(resp.text)

        try:
            props = data["props"]["pageProps"]
        except KeyError:
            raise ScraperError(f"Unexpected __NEXT_DATA__ shape at {source_url}")

        # Series metadata — Asura uses different key names across versions
        series = props.get("series") or props.get("comicData") or props

        title     = series.get("title") or series.get("name", "")
        cover_url = series.get("image") or series.get("thumb") or series.get("cover")
        status    = _normalize_status(series.get("status", "ongoing"))

        # Chapters — may be at props.chapters or series.chapters
        raw_chapters = (
            props.get("chapters")
            or series.get("chapters")
            or []
        )

        chapters = []
        for ch in raw_chapters:
            number = self._extract_chapter_number(ch)
            if number is None:
                continue
            chapters.append({
                "number":      number,
                "title":       ch.get("title") or ch.get("name"),
                "url":         self._build_chapter_url(source_url, ch),
                "released_at": _parse_date(ch.get("createdAt") or ch.get("date")),
            })

        # Sort ascending by chapter number
        chapters.sort(key=lambda c: c["number"])

        return {
            "title":      title,
            "cover_url":  cover_url,
            "source_url": source_url,
            "pub_status": status,
            "chapters":   chapters,
        }

    # -----------------------------------------------------------------------
    # Chapter images
    # -----------------------------------------------------------------------

    async def get_chapter_image_urls(self, chapter_url: str) -> list[str]:
        resp = await self.get(chapter_url)
        data = _parse_next_data(resp.text)

        try:
            props = data["props"]["pageProps"]
        except KeyError:
            raise ScraperError(f"Unexpected __NEXT_DATA__ shape at {chapter_url}")

        # Images may be at props.images, props.pages, or props.chapterData.images
        images = (
            props.get("images")
            or props.get("pages")
            or (props.get("chapterData") or {}).get("images")
            or []
        )

        if not images:
            # Last resort: scrape <img> tags inside the chapter reader container
            images = self._fallback_image_scrape(resp.text)

        return [self._normalize_image_url(img) for img in images if img]

    # -----------------------------------------------------------------------
    # Helpers
    # -----------------------------------------------------------------------

    def _find_series_list(self, data: dict) -> list:
        """Recursively search __NEXT_DATA__ for a list of series objects."""
        if isinstance(data, list) and data and isinstance(data[0], dict) and "slug" in data[0]:
            return data
        if isinstance(data, dict):
            for v in data.values():
                result = self._find_series_list(v)
                if result:
                    return result
        return []

    def _extract_chapter_number(self, ch: dict) -> Optional[float]:
        """Extract chapter number as float from various field names."""
        for key in ("chapter", "chapterNumber", "number", "chap"):
            val = ch.get(key)
            if val is not None:
                try:
                    return float(val)
                except (ValueError, TypeError):
                    pass
        # Try to parse from the name/title string e.g. "Chapter 42.5"
        name = ch.get("name") or ch.get("title") or ""
        m = re.search(r"chapter\s*([\d.]+)", name, re.IGNORECASE)
        if m:
            return float(m.group(1))
        return None

    def _build_chapter_url(self, series_url: str, ch: dict) -> str:
        """Build a full chapter URL from a chapter dict."""
        # If chapter has a full URL already
        if "url" in ch and ch["url"].startswith("http"):
            return ch["url"]
        # If chapter has a slug
        slug = ch.get("slug") or ch.get("id")
        if slug:
            return f"{series_url}/{slug}"
        # Fallback: append chapter number
        number = self._extract_chapter_number(ch)
        return f"{series_url}/chapter-{number}"

    def _normalize_image_url(self, img) -> str:
        """Handle both plain URL strings and dicts like {'url': '...'}."""
        if isinstance(img, str):
            url = img
        elif isinstance(img, dict):
            url = img.get("url") or img.get("src") or img.get("image") or ""
        else:
            return ""
        # Make absolute if relative
        if url.startswith("/"):
            return self.base_url + url
        return url

    def _fallback_image_scrape(self, html: str) -> list[str]:
        """Parse img tags from the chapter reader as a last resort."""
        tree = HTMLParser(html)
        imgs = []
        # Asura wraps chapter images in a div with class containing "reader" or "chapter"
        for selector in (".chapter-content img", ".reader img", "main img"):
            nodes = tree.css(selector)
            if nodes:
                imgs = [n.attrs.get("src") or n.attrs.get("data-src", "") for n in nodes]
                break
        return [i for i in imgs if i and not i.endswith(".gif")]  # skip UI gifs
