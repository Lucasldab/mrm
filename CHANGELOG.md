# Changelog

All notable changes to this project are documented in this file.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-05-12

### Changed
- **AsuraScans scraper rewritten in pure Rust.** Cloudflare TLS-fingerprint bypass now uses [`rquest`](https://crates.io/crates/rquest) (BoringSSL Chrome impersonation) directly in-process instead of shelling out to a `curl_cffi` Python subprocess. The `scraper/` Python directory and its venv are gone.
- The `[sources.asura].scraper_dir` config field is parsed-but-ignored for backward compatibility — remove it from your `config.toml` at your leisure.

### Added
- **`?` help overlay.** Toggles on every screen (Library, Detail, Search, Discover, StatusPicker); shows the keybinds relevant to the current view. Any key dismisses.
- **Cross-source aliases schema.** New `manhwa_alias` table linking one canonical series to multiple `(source, source_url)` pairs. Helpers `fetch_aliases`, `add_alias`, `remove_alias`. Coordinator/UI wire-up lands in a follow-up release.
- **`[theme]` palette section in `config.toml`** is now documented and surfaced as a customization point.

### Removed
- `scraper/` Python directory (1,705 lines deleted), `requirements.txt`, `asura_bridge.py`. Python is no longer a runtime or build dependency.

## [0.1.0] - 2026-05-12

First tagged release. Audit-driven reliability sweep across scrapers, DB layer, and cover cache.

### Fixed
- **MangaCK**: every `send()` chain now checks `.error_for_status()` before reading the body. Previously a 404/5xx HTML page was parsed as if it were valid scraped content (same cache-poisoning class of bug that bit `noguisteam`'s image cache).
- **MangaDex**: `fetch_all_chapters` gained a 50-page safety cap and exits cleanly on empty pages before advancing `offset` — protects the coordinator from malformed `total` fields that could otherwise wedge the loop.
- **Asura**: bridge validates `scraper/.venv/bin/python3` exists before spawning the subprocess and returns an actionable error if the venv is missing.
- **DB**: `upsert_chapters` now runs inside a single transaction. Avoids partial writes on coordinator crash and removes N round-trips per poll.
- **DB**: `fetch_chapters` propagates row parse errors instead of silently substituting `id = 0` (which would collide with real chapter ids and corrupt progress joins).
- **DB**: `upsert_discovery` novelty is decided via an `EXISTS` pre-check. SQLite's `rows_affected` returns 1 for both `INSERT` and `ON CONFLICT UPDATE`, so the previous heuristic counted every update as a new discovery.
- **Cover cache**: `preload_covers_inner` checks `error_for_status` before reading bytes. Files are now written atomically (tmp + rename) so partial downloads can't leave a corrupt file behind.

[Unreleased]: https://github.com/Lucasldab/mrm/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Lucasldab/mrm/releases/tag/v0.2.0
[0.1.0]: https://github.com/Lucasldab/mrm/releases/tag/v0.1.0
