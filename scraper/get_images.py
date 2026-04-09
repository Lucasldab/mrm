"""
CLI helper called by the Rust TUI to fetch chapter image URLs.

Usage:
    python -m scraper.get_images <source> <chapter_url>

Outputs a JSON array of image URLs to stdout, one per line.
Exit code 0 on success, 1 on failure.

Example:
    python -m scraper.get_images mangack https://mangack.com/chapter/xxx/
    python -m scraper.get_images mangadex https://mangadex.org/chapter/uuid/
"""

from __future__ import annotations

import asyncio
import json
import sys

from .config import load_config
from .scrapers.mangadex import MangaDexScraper
from .scrapers.mangack import MangackScraper
from .scrapers.asura import AsuraScraper
from .scrapers.base import ScraperError

SCRAPERS = {
    "mangadex": MangaDexScraper,
    "mangack":  MangackScraper,
    "asura":    AsuraScraper,
}


async def fetch(source: str, chapter_url: str) -> list[str]:
    cfg = load_config()
    source_cfg = cfg.get("sources", {}).get(source, {})
    base_url = source_cfg.get("base_url", "")

    cls = SCRAPERS.get(source)
    if cls is None:
        raise ValueError(f"Unknown source: {source}")

    async with cls(base_url) as scraper:
        urls = await scraper.get_chapter_image_urls(chapter_url)

    return urls


def main() -> None:
    if len(sys.argv) != 3:
        print(
            f"Usage: python -m scraper.get_images <source> <chapter_url>",
            file=sys.stderr,
        )
        sys.exit(1)

    source      = sys.argv[1]
    chapter_url = sys.argv[2]

    try:
        urls = asyncio.run(fetch(source, chapter_url))
        print(json.dumps(urls))
    except (ScraperError, ValueError) as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
