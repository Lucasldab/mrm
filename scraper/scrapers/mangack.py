"""
mrm - mangack scraper (mangack.com)

mangack uses a custom WordPress theme (not MangaPress).

Confirmed structure from live site inspection:
  Series URL  : {base}/manga/{slug}/
  Chapter URL : {base}/chapter/{slug}-chapter-{n}/

  Series page HTML:
    Title    : h1 (first on page)
    Cover    : first img[src*="wp-content/uploads"]
    Status   : table row where td text == "Status" -> sibling td > a
    Chapters : a[href*="/chapter/"] links inline on the page
    Ch. date : raw text node in parent <li> after the <a>

  Search page HTML:
    Results  : a[href*="/manga/"] links
"""

import re
from datetime import datetime, timedelta
from typing import Optional
from selectolax.parser import HTMLParser

from .base import BaseScraper, ScraperError


_STATUS_MAP = {
    "ongoing":   "ongoing",
    "hiatus":    "hiatus",
    "completed": "completed",
    "complete":  "completed",
}

# Quick-nav buttons at the top of series pages -- not real chapters
_NAV_LABELS = {"first chapter", "last chapter", "first", "last"}


def _normalize_status(raw: str) -> str:
    return _STATUS_MAP.get(raw.lower().strip(), "ongoing")


def _parse_relative_date(raw: str) -> Optional[str]:
    """
    Convert strings like '5 hours ago', '2 weeks ago', '1 year ago'
    into ISO date strings. mangack uses relative dates throughout.
    """
    raw = raw.strip().lower()
    now = datetime.utcnow()

    patterns = [
        (r"(\d+)\s+minute",  lambda n: now - timedelta(minutes=int(n))),
        (r"(\d+)\s+hour",    lambda n: now - timedelta(hours=int(n))),
        (r"(\d+)\s+day",     lambda n: now - timedelta(days=int(n))),
        (r"(\d+)\s+week",    lambda n: now - timedelta(weeks=int(n))),
        (r"(\d+)\s+month",   lambda n: now - timedelta(days=int(n) * 30)),
        (r"(\d+)\s+year",    lambda n: now - timedelta(days=int(n) * 365)),
    ]
    for pattern, calc in patterns:
        m = re.search(pattern, raw)
        if m:
            return calc(m.group(1)).date().isoformat()

    # Fallback: try absolute date formats
    for fmt in ("%B %d, %Y", "%Y-%m-%d", "%b %d, %Y"):
        try:
            return datetime.strptime(raw.strip(), fmt).isoformat()
        except ValueError:
            continue

    return None


class MangackScraper(BaseScraper):
    SOURCE = "mangack"

    # -----------------------------------------------------------------------
    # Search
    # -----------------------------------------------------------------------

    async def search(self, query: str) -> list[dict]:
        """Search via /?s=query -- returns /manga/ links from results page."""
        url = f"{self.base_url}/"
        resp = await self.get(url, params={"s": query})
        tree = HTMLParser(resp.text)

        results = []
        seen = set()

        for a in tree.css("a[href*='/manga/']"):
            href  = a.attrs.get("href", "")
            title = a.text(strip=True)

            if not title or len(title) < 2 or href in seen:
                continue
            # Skip bare domain links and nav links
            if href.count("/") < 4:
                continue

            seen.add(href)

            # Look for a cover image near this link
            cover_url = None
            parent = a.parent
            grandparent = parent.parent if parent else None
            for ancestor in [parent, grandparent]:
                if ancestor:
                    img = ancestor.css_first("img")
                    if img:
                        cover_url = img.attrs.get("src") or img.attrs.get("data-src")
                        break

            results.append({
                "title":      title,
                "cover_url":  cover_url,
                "source_url": href,
                "pub_status": "ongoing",  # not available on search results page
            })

        return results

    # -----------------------------------------------------------------------
    # Series metadata + chapter list
    # -----------------------------------------------------------------------

    async def get_series(self, source_url: str) -> dict:
        resp = await self.get(source_url)
        tree = HTMLParser(resp.text)

        # Title -- first h1
        title_node = tree.css_first("h1")
        title = title_node.text(strip=True) if title_node else ""

        # Cover -- first upload image
        cover_url = None
        for img in tree.css("img[src*='wp-content/uploads']"):
            src = img.attrs.get("src", "")
            if src:
                cover_url = src
                break

        # Status -- table row: td "Status" -> sibling td
        status = "ongoing"
        for td in tree.css("td, th"):
            if td.text(strip=True).lower() == "status":
                sibling = td.next
                if sibling:
                    status = _normalize_status(sibling.text(strip=True))
                break

        # Chapters
        chapters = self._parse_chapter_links(tree)
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
        tree = HTMLParser(resp.text)

        return [
            img.attrs.get("src") or img.attrs.get("data-src", "")
            for img in tree.css("img[src*='wp-content/uploads']")
            if img.attrs.get("src") or img.attrs.get("data-src")
        ]

    # -----------------------------------------------------------------------
    # Helpers
    # -----------------------------------------------------------------------

    def _parse_chapter_links(self, tree: HTMLParser) -> list[dict]:
        """Extract chapter list from inline <a href="/chapter/..."> links."""
        chapters = []
        seen_numbers: set[float] = set()

        for a in tree.css("a[href*='/chapter/']"):
            href  = a.attrs.get("href", "")
            label = a.text(strip=True)

            # Skip quick-nav buttons ("First Chapter" / "Last Chapter")
            if label.lower() in _NAV_LABELS:
                continue

            # Keep original label for date text subtraction
            raw_label = label
            # Strip the "NEW" badge mangack appends to recent chapters
            label = re.sub(r"\s*NEW\s*$", "", label, flags=re.IGNORECASE).strip()

            number = self._extract_chapter_number(label, href)
            if number is None or number in seen_numbers:
                continue
            seen_numbers.add(number)

            # Date is a raw text node in the parent <li>, after the <a>.
            # Get all text from parent then subtract the link text.
            released_at = None
            parent = a.parent
            if parent:
                full_text = parent.text(strip=True)
                date_text = (
                    full_text
                    .replace(raw_label, "")
                    .replace("NEW", "")
                    .strip()
                )
                if date_text:
                    released_at = _parse_relative_date(date_text)

            chapters.append({
                "number":      number,
                "title":       label or None,
                "url":         href,
                "released_at": released_at,
            })

        return chapters

    def _extract_chapter_number(self, label: str, url: str = "") -> Optional[float]:
        """Extract chapter number from 'CHAPTER 42' label or URL slug."""
        for text in (label, url):
            m = re.search(r"chapter[-\s]*([\d]+(?:[._][\d]+)?)", text, re.IGNORECASE)
            if m:
                try:
                    return float(m.group(1).replace("_", "."))
                except ValueError:
                    continue
        return None
