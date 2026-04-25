"""
Bridge script for Rust → Python AsuraScans scraper.

Called as:
    python -m scraper.asura_bridge search <query>
    python -m scraper.asura_bridge get_series <source_url>
    python -m scraper.asura_bridge get_chapter_image_urls <chapter_url>

Outputs JSON to stdout. Errors go to stderr with exit code 1.
"""

from __future__ import annotations

import asyncio
import json
import sys

from .config import load_config
from .scrapers.asura import AsuraScraper
from .scrapers.base import ScraperError


def _get_base_url() -> str:
    cfg = load_config()
    return cfg.get("sources", {}).get("asura", {}).get("base_url", "https://asurascans.com")


async def run(command: str, arg: str) -> str:
    base_url = _get_base_url()

    async with AsuraScraper(base_url) as scraper:
        if command == "search":
            results = await scraper.search(arg)
            return json.dumps(results)

        elif command == "get_series":
            series = await scraper.get_series(arg)
            # Description is optional — older scraper versions don't populate
            # it, so default to None on the wire so the Rust side can store
            # it as NULL without a special case.
            series.setdefault("description", None)
            return json.dumps(series)

        elif command == "get_chapter_image_urls":
            urls = await scraper.get_chapter_image_urls(arg)
            return json.dumps(urls)

        elif command == "latest_chapters":
            # Optional — only available if the underlying scraper exposes it.
            fn = getattr(scraper, "latest_chapters", None)
            if fn is None:
                return json.dumps([])
            results = await fn()
            return json.dumps(results)

        else:
            raise ValueError(f"Unknown command: {command}")


def main() -> None:
    if len(sys.argv) != 3:
        print(
            "Usage: python -m scraper.asura_bridge <command> <arg>",
            file=sys.stderr,
        )
        sys.exit(1)

    command = sys.argv[1]
    arg = sys.argv[2]

    try:
        output = asyncio.run(run(command, arg))
        print(output)
    except (ScraperError, ValueError) as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
