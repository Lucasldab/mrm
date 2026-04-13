# mrm - Manhwa Reader Manager

A terminal-based manhwa/manga reader and library manager built in Rust. Tracks reading progress, auto-classifies status based on reading behavior, scrapes sources for new chapters, and sends desktop notifications when new releases drop.

## Features

- **TUI library** with grid/list views and cover image thumbnails
- **Chapter reader** via external image viewer — supports [imv](https://sr.ht/~exec64/imv/) or [rv](https://github.com/Lucasldab/readingViewer), selectable in `config.toml`
- **Auto-status tracking** — status updates automatically based on your reading progress
- **Background polling** — checks sources for new chapters on a configurable interval
- **Desktop notifications** via notify-send (mako)
- **Search and add** new series from supported sources
- **Configurable keybinds and theme colors** via TOML

### Supported Sources

- [MangaDex](https://mangadex.org) (REST API)
- [MangaCK](https://mangack.com) (HTML scraping)
- [AsuraScans](https://asurascans.com) (Python scraper; skips early-access/paywalled chapters until they unlock)

## Requirements

- Rust 1.70+
- SQLite3
- Linux with a terminal that supports images (kitty, iTerm2, etc.)
- An image viewer — either [imv](https://sr.ht/~exec64/imv/) or [rv](https://github.com/Lucasldab/readingViewer) (with `rv-msg`) on `PATH`
- notify-send (optional, for desktop notifications)
- Python 3.11+ with `scraper/requirements.txt` installed in `scraper/.venv` (only if AsuraScans source is enabled)

## Installation

```sh
git clone https://github.com/Lucasldab/mrm.git
cd mrm
cargo build --release
```

The binary will be at `target/release/mrm`.

## Configuration

Create `~/.config/mrm/config.toml`:

```toml
# Chapter reader: "imv" (default) or "rv"
viewer = "rv"

[sources.mangadex]
base_url = "https://api.mangadex.org"
enabled = true

[sources.mangack]
base_url = "https://mangack.com"
enabled = true

[sources.asura]
base_url = "https://asurascans.com"
enabled = true
scraper_dir = "/path/to/mrm"  # repo root; Python scraper lives in ./scraper

[notifications]
enabled = true
poll_interval_minutes = 30

[db]
path = "mrm.db"
```

The database file (`mrm.db`) is auto-created at `~/.config/mrm/mrm.db` if no path is specified.

## Usage

```sh
# Launch the TUI (default)
mrm

# Run as a background polling daemon
mrm --daemon

# Poll once and exit
mrm --once
```

## Default Keybinds

| Key | Action |
|-----|--------|
| `j`/`k` | Navigate down/up |
| `h`/`l` | Navigate left/right |
| `gg`/`G` | Jump to top/bottom |
| `Enter` | Open selected |
| `Esc` | Go back |
| `/` | Search library |
| `a` | Add new series |
| `d` | Delete series |
| `s` | Set status |
| `u` | Mark chapter unread |
| `c` | Clear status override |
| `]`/`[` | Next/previous chapter |
| `o` | Sort library |
| `q` | Quit |

All keybinds can be customized in `config.toml` under `[keys]`.

## Theme

Colors are configurable under `[theme]` in `config.toml`. Supports named colors, hex (`#RRGGBB`), and 256-color indices.

```toml
[theme]
accent = "yellow"
status_reading = "#00ff88"
highlight_bg = "236"
```

## imv Viewer

The image viewer (imv) options and keybinds are customizable under `[imv]` in `config.toml`. Both subsections are optional — anything you don't specify keeps the default.

### Options

```toml
[imv.options]
initial_pan = "50 0"      # horizontal center, top of page
scaling_mode = "none"      # 1:1 pixels (alternatives: "shrink", "full", "crop")
pan_limits = "yes"         # prevent panning past image edges
```

### Keybinds

```toml
[imv.binds]
q = "quit"
"<Left>" = "prev; pan 0 0"
"<Right>" = "next; pan 0 0"
j = "pan 0 -50"
k = "pan 0 50"
"<Shift+J>" = "pan 0 -500"
"<Shift+K>" = "pan 0 500"
h = "pan 50 0"
l = "pan -50 0"
"<Up>" = "zoom 1"
"<Down>" = "zoom -1"
f = "fullscreen"
"<scroll-up>" = "pan 0 50"
"<scroll-down>" = "pan 0 -50"
"<shift-scroll-up>" = "pan 0 500"
"<shift-scroll-down>" = "pan 0 -500"
```

To customize, just override the keys you want. For example, to increase pan speed and add a reset-zoom bind:

```toml
[imv.binds]
j = "pan 0 -100"
k = "pan 0 100"
r = "scaling_mode none"
```

> **Note:** If you provide `[imv.binds]`, it **replaces** all default binds, so include every bind you want.

See the [imv man page](https://man.sr.ht/~exec64/imv/) for all available options and commands.

## rv Viewer

[`rv`](https://github.com/Lucasldab/readingViewer) is an alternative reader that streams pages into a vertical continuous-scroll view, which suits webtoon-style content better than imv's paged mode. Select it by setting `viewer = "rv"` at the top of `config.toml`. When a chapter opens, mrm launches `rv`, reads the socket path it prints on stdout, and pushes new pages to it via `rv-msg` as they finish downloading — so you can start reading before the whole chapter is ready.

### Options

```toml
[rv]
scroll_speed      = 80    # pixels per scroll tick
fast_scroll_speed = 600   # pixels per fast-scroll tick
fullscreen        = false
```

### Keybinds

Defaults roughly follow vim motion:

```toml
[rv.binds]
q       = "quit"
j       = "scroll_down"
k       = "scroll_up"
J       = "fast_scroll_down"
K       = "fast_scroll_up"
space   = "page_down"
g       = "top"
G       = "bottom"
up      = "zoom_in"
down    = "zoom_out"
equals  = "zoom_reset"
f       = "fullscreen"
h       = "pan_left"
l       = "pan_right"
```

As with imv, providing `[rv.binds]` **replaces** the default set, so include every bind you want.

Requires the `rv` and `rv-msg` binaries to be on your `PATH`.

## Disclaimer

This project does not host, store, or distribute any copyrighted content. It simply interacts with publicly available third-party websites and APIs. The developers are not responsible for any misuse of this software or for any content accessed through it.

This software is provided for **personal and educational purposes only**. Users are solely responsible for ensuring their use complies with applicable laws and the terms of service of any third-party sources. The developers do not condone or encourage piracy in any form.

If you are a copyright holder and believe this project infringes on your rights, please open an issue on this repository.

## License

MIT - see [LICENSE](LICENSE)
