# mrm - Manhwa Reader Manager

A terminal-based manhwa/manga reader and library manager built in Rust. Tracks reading progress, auto-classifies status based on reading behavior, scrapes sources for new chapters, and sends desktop notifications when new releases drop.

## Features

- **TUI library** with grid/list views and cover image thumbnails
- **Chapter reader** via external image viewer (imv)
- **Auto-status tracking** — status updates automatically based on your reading progress
- **Background polling** — checks sources for new chapters on a configurable interval
- **Desktop notifications** via notify-send (mako)
- **Search and add** new series from supported sources
- **Configurable keybinds and theme colors** via TOML

### Supported Sources

- [MangaDex](https://mangadex.org) (REST API)
- [MangaCK](https://mangack.com) (HTML scraping)

## Requirements

- Rust 1.70+
- SQLite3
- Linux with a terminal that supports images (kitty, iTerm2, etc.)
- [imv](https://sr.ht/~exec64/imv/) — image viewer
- notify-send (optional, for desktop notifications)

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
[sources.mangadex]
base_url = "https://api.mangadex.org"
enabled = true

[sources.mangack]
base_url = "https://mangack.com"
enabled = true

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
| `g`/`G` | Jump to top/bottom |
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

## Disclaimer

This project does not host, store, or distribute any copyrighted content. It simply interacts with publicly available third-party websites and APIs. The developers are not responsible for any misuse of this software or for any content accessed through it.

This software is provided for **personal and educational purposes only**. Users are solely responsible for ensuring their use complies with applicable laws and the terms of service of any third-party sources. The developers do not condone or encourage piracy in any form.

If you are a copyright holder and believe this project infringes on your rights, please open an issue on this repository.

## License

MIT - see [LICENSE](LICENSE)
