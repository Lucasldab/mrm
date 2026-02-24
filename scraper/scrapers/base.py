"""
mrm - base scraper
All scrapers inherit from BaseScraper.
"""

import httpx
import asyncio
from abc import ABC, abstractmethod
from typing import Optional


class ScraperError(Exception):
    pass


class BaseScraper(ABC):
    # Subclasses set this to their source name, e.g. "asura"
    SOURCE: str = ""

    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")
        self._client: Optional[httpx.AsyncClient] = None

    async def __aenter__(self):
        self._client = httpx.AsyncClient(
            headers={
                "User-Agent": (
                    "Mozilla/5.0 (X11; Linux x86_64) "
                    "AppleWebKit/537.36 (KHTML, like Gecko) "
                    "Chrome/124.0.0.0 Safari/537.36"
                ),
                "Accept-Language": "en-US,en;q=0.9",
            },
            follow_redirects=True,
            timeout=20.0,
        )
        return self

    async def __aexit__(self, *_):
        if self._client:
            await self._client.aclose()

    async def get(self, url: str, **kwargs) -> httpx.Response:
        """GET with simple retry logic."""
        assert self._client, "Use scraper as async context manager"
        for attempt in range(3):
            try:
                resp = await self._client.get(url, **kwargs)
                resp.raise_for_status()
                return resp
            except httpx.HTTPStatusError as e:
                if e.response.status_code == 403:
                    raise ScraperError(
                        f"Blocked by {self.SOURCE} (403). "
                        "Site may require Cloudflare bypass."
                    ) from e
                if attempt == 2:
                    raise ScraperError(f"HTTP error after 3 attempts: {e}") from e
            except httpx.RequestError as e:
                if attempt == 2:
                    raise ScraperError(f"Request failed after 3 attempts: {e}") from e
            await asyncio.sleep(2 ** attempt)  # 1s, 2s, 4s backoff

    @abstractmethod
    async def search(self, query: str) -> list[dict]:
        """
        Search for manhwa by title.
        Returns list of dicts: {title, cover_url, source_url, pub_status}
        """

    @abstractmethod
    async def get_series(self, source_url: str) -> dict:
        """
        Fetch metadata for a single series.
        Returns dict: {title, cover_url, source_url, pub_status, chapters: [...]}
        Each chapter: {number, title, url, released_at}
        """

    @abstractmethod
    async def get_chapter_image_urls(self, chapter_url: str) -> list[str]:
        """
        Return ordered list of image URLs for a chapter page.
        """
