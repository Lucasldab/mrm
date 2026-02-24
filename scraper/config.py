"""
mrm - config loader
Reads config.toml from the project root.
"""

import tomllib
from pathlib import Path
from typing import Any

CONFIG_PATH = Path(__file__).parent.parent / "config.toml"


def load_config() -> dict[str, Any]:
    with open(CONFIG_PATH, "rb") as f:
        return tomllib.load(f)


def get_source_config(source: str) -> dict[str, Any]:
    cfg = load_config()
    sources = cfg.get("sources", {})
    if source not in sources:
        raise KeyError(f"No config found for source '{source}'")
    return sources[source]
