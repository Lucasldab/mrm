"""
mrm - asura scraper (asurascans.com)

AsuraScans uses an Astro-based frontend behind Cloudflare.
Requires curl_cffi to bypass TLS fingerprinting.

Confirmed structure from live site inspection:
  Series URL  : {base}/comics/{slug}-{hash}/
  Chapter URL : {base}/comics/{slug}-{hash}/chapter/{n}
  Search URL  : {base}/browse?search={query}

  Series page HTML (Astro SSR):
    Title    : first <h1>
    Cover    : first <img> with 'covers' in src
    Status   : <span> with text in (ongoing, completed, hiatus, dropped)
    Chapters : <a href="/comics/.../chapter/{n}"> links
      Number : extracted from URL path /chapter/{n}
      Title  : <span class="...truncate..."> inside <a>
      Date   : <div class="flex-shrink-0..."> inside <a>

  Chapter page:
    Images   : CDN URLs matching cdn.asurascans.com/asura-images/chapters/
               embedded in Astro serialized data (HTML-encoded)
"""

import asyncio
import html
import re
from datetime import datetime, timezone
from typing import Optional

from curl_cffi import requests as cffi_requests

from .base import BaseScraper, ScraperError


_STATUS_MAP = {
    "ongoing":   "ongoing",
    "hiatus":    "hiatus",
    "completed": "completed",
    "dropped":   "ongoing",      # treat dropped as ongoing for our purposes
    "coming soon": "ongoing",
}

_NAV_LABELS = {"first chapter", "latest chapter", "first", "latest"}

_DATE_FORMATS = ["%b %d, %Y", "%B %d, %Y", "%Y-%m-%d"]


_CHAPTER_META_RE = re.compile(
    r'number&quot;:\[0,(?P<num>\d+(?:\.\d+)?)\][^}]*?'
    r'is_premium&quot;:\[0,(?P<prem>true|false)\][^}]*?'
    r'early_access_until&quot;:\[0,(?:&quot;(?P<eau>[^&]+)&quot;|[^\]]+)\]'
)


def _parse_locked_chapters(raw_html: str) -> set[float]:
    """Return set of chapter numbers still behind the early-access paywall.

    AsuraScans marks fresh chapters with is_premium=true and an
    early_access_until timestamp; once that timestamp passes the chapter
    becomes free to read. We skip chapters still in the early-access window.
    """
    locked: set[float] = set()
    now = datetime.now(timezone.utc)
    for m in _CHAPTER_META_RE.finditer(raw_html):
        if m.group("prem") != "true":
            continue
        eau = m.group("eau")
        if not eau:
            continue
        try:
            ts = datetime.fromisoformat(eau.replace("Z", "+00:00"))
        except ValueError:
            continue
        if ts > now:
            locked.add(float(m.group("num")))
    return locked


def _parse_date(raw: str) -> Optional[str]:
    """Parse dates like 'Jan 7, 2026' or 'December 3, 2025'."""
    raw = raw.strip()
    for fmt in _DATE_FORMATS:
        try:
            return datetime.strptime(raw, fmt).date().isoformat()
        except ValueError:
            continue
    return None


class AsuraScraper(BaseScraper):
    SOURCE = "asura"

    # -------------------------------------------------------------------
    # HTTP override — use curl_cffi instead of httpx for Cloudflare bypass
    # -------------------------------------------------------------------

    async def __aenter__(self):
        # We don't use the httpx client; curl_cffi handles sessions
        self._session = cffi_requests.Session(impersonate="chrome")
        return self

    async def __aexit__(self, *_):
        if self._session:
            self._session.close()

    async def get(self, url: str, **kwargs):
        """GET with retry, using curl_cffi for Cloudflare bypass."""
        for attempt in range(3):
            try:
                resp = self._session.get(url, **kwargs)
                resp.raise_for_status()
                return resp
            except Exception as e:
                status = getattr(getattr(e, "response", None), "status_code", None)
                if status == 403:
                    raise ScraperError(
                        f"Blocked by {self.SOURCE} (403). "
                        "Cloudflare may have changed fingerprinting."
                    ) from e
                if attempt == 2:
                    raise ScraperError(
                        f"Request failed after 3 attempts: {e}"
                    ) from e
            await asyncio.sleep(2 ** attempt)

    # -------------------------------------------------------------------
    # Search
    # -------------------------------------------------------------------

    async def search(self, query: str) -> list[dict]:
        """Search via /browse?search=query."""
        from selectolax.parser import HTMLParser

        resp = await self.get(
            f"{self.base_url}/browse",
            params={"search": query},
        )
        tree = HTMLParser(resp.text)

        results = []
        seen = set()

        for a in tree.css("a[href*='/comics/']"):
            href = a.attrs.get("href", "")
            if not href or href in seen:
                continue

            # Title lives in an <h3> inside the link; skip cards without one
            h3 = a.css_first("h3")
            if not h3:
                continue
            title = h3.text(strip=True)
            if not title or len(title) < 2:
                continue

            seen.add(href)

            # Cover image — the <h3> link is separate from the image card,
            # but they share a common parent container
            cover_url = None
            parent = a.parent
            for ancestor in (parent, parent.parent if parent else None):
                if ancestor:
                    img = ancestor.css_first("img[src*='covers']")
                    if img:
                        cover_url = img.attrs.get("src") or img.attrs.get("data-src")
                        break

            # Ensure full URL
            if href.startswith("/"):
                href = self.base_url + href

            results.append({
                "title":      title,
                "cover_url":  cover_url,
                "source_url": href,
                "pub_status": "ongoing",
            })

        return results

    # -------------------------------------------------------------------
    # Series metadata + chapter list
    # -------------------------------------------------------------------

    async def get_series(self, source_url: str) -> dict:
        from selectolax.parser import HTMLParser

        resp = await self.get(source_url)
        tree = HTMLParser(resp.text)

        # Title — first h1
        title_node = tree.css_first("h1")
        title = title_node.text(strip=True) if title_node else ""

        # Cover — first img with 'covers' in src
        cover_url = None
        for img in tree.css("img"):
            src = img.attrs.get("src", "") or ""
            if "covers" in src:
                cover_url = src
                break

        # Status — span with known status text
        status = "ongoing"
        for span in tree.css("span"):
            t = (span.text(strip=True) or "").lower()
            if t in _STATUS_MAP:
                status = _STATUS_MAP[t]
                break

        # Chapters
        locked = _parse_locked_chapters(resp.text)
        chapters = self._parse_chapter_links(tree)
        chapters = [c for c in chapters if c["number"] not in locked]
        chapters.sort(key=lambda c: c["number"])

        return {
            "title":      title,
            "cover_url":  cover_url,
            "source_url": source_url,
            "pub_status": status,
            "chapters":   chapters,
        }

    # -------------------------------------------------------------------
    # Chapter images
    # -------------------------------------------------------------------

    async def get_chapter_image_urls(self, chapter_url: str) -> list[str]:
        resp = await self.get(chapter_url)

        # Images are embedded in Astro serialized data (HTML-encoded)
        decoded = html.unescape(resp.text)
        urls = re.findall(
            r"https://cdn\.asurascans\.com/asura-images/chapters/[^\"\s<>]+\.(?:webp|jpg|jpeg|png)",
            decoded,
        )
        # Deduplicate while preserving order
        return list(dict.fromkeys(urls))

    # -------------------------------------------------------------------
    # Helpers
    # -------------------------------------------------------------------

    def _parse_chapter_links(self, tree) -> list[dict]:
        """Extract chapter list from <a href=".../chapter/{n}"> links."""
        chapters = []
        seen_numbers: set[float] = set()
        seen_urls: set[str] = set()

        for a in tree.css("a[href*='/chapter/']"):
            href = a.attrs.get("href", "") or ""
            if not href or href in seen_urls:
                continue

            label = a.text(strip=True)
            if label.lower() in _NAV_LABELS:
                continue

            seen_urls.add(href)

            # Chapter number from URL: /chapter/42 or /chapter/42.5
            m = re.search(r"/chapter/([\d]+(?:\.[\d]+)?)", href)
            if not m:
                continue
            number = float(m.group(1))
            if number in seen_numbers:
                continue
            seen_numbers.add(number)

            # Title (subtitle span with truncate class)
            title = None
            title_el = a.css_first("span.truncate")
            if title_el:
                title = title_el.text(strip=True) or None

            # Date (right-side div)
            released_at = None
            date_el = a.css_first("div.flex-shrink-0")
            if date_el:
                date_text = date_el.text(strip=True)
                if date_text:
                    released_at = _parse_date(date_text)

            # Ensure full URL
            if href.startswith("/"):
                href = self.base_url + href

            chapters.append({
                "number":      number,
                "title":       title,
                "url":         href,
                "released_at": released_at,
            })

        return chapters
