//! TOML config loader — replaces Python scraper/config.py
//!
//! Reads config.toml from the project root (or path overridable via env).
//! Config is loaded once at startup and passed by clone into tasks that need it.

use std::collections::HashMap;
use anyhow::{Context, Result};
use crossterm::event::KeyCode;
use ratatui::style::Color;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub sources:       HashMap<String, SourceConfig>,
    pub notifications: NotificationsConfig,
    pub db:            DbConfig,
    #[serde(default)]
    pub keys:          KeysConfig,
    #[serde(default)]
    pub theme:         ThemeConfig,
    #[serde(default)]
    pub imv:           ImvConfig,
    #[serde(default)]
    pub rv:            RvConfig,
    #[serde(default = "default_viewer")]
    pub viewer:        String,
}

fn default_viewer() -> String { "imv".into() }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerKind { Imv, Rv }

impl Config {
    pub fn viewer_kind(&self) -> ViewerKind {
        match self.viewer.to_lowercase().as_str() {
            "rv" => ViewerKind::Rv,
            _    => ViewerKind::Imv,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub base_url: String,
    pub enabled:  bool,
    /// Path to the project root where the Python scraper package lives.
    /// Required for sources that delegate to Python (e.g. asura).
    pub scraper_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NotificationsConfig {
    pub enabled:               bool,
    pub poll_interval_minutes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DbConfig {
    pub path: String,
}

// ---------------------------------------------------------------------------
// Keybinds
// ---------------------------------------------------------------------------

/// All keybinds as single-char strings in TOML. Each field defaults to a sensible key.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct KeysConfig {
    // Navigation (shared across screens)
    pub down:         String,
    pub up:           String,
    pub left:         String,
    pub right:        String,
    pub top:          String,
    pub bottom:       String,
    pub open:         String,
    pub back:         String,

    // Library
    pub search:       String,
    pub add:          String,
    pub delete:       String,

    // Detail
    pub set_status:   String,
    pub mark_unread:  String,
    pub clear_override: String,

    // Reader
    pub next_chapter: String,
    pub prev_chapter: String,

    // Search results
    pub input_mode:   String,

    // Library sort
    pub sort:         String,
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            down:           "j".into(),
            up:             "k".into(),
            left:           "h".into(),
            right:          "l".into(),
            top:            "g".into(),
            bottom:         "G".into(),
            open:           "Enter".into(),
            back:           "Esc".into(),
            search:         "/".into(),
            add:            "a".into(),
            delete:         "d".into(),
            set_status:     "s".into(),
            mark_unread:    "u".into(),
            clear_override: "c".into(),
            next_chapter:   "]".into(),
            prev_chapter:   "[".into(),
            input_mode:     "i".into(),
            sort:           "o".into(),
        }
    }
}

impl KeysConfig {
    /// Parse a key string into a KeyCode.
    pub fn parse_key(s: &str) -> KeyCode {
        match s {
            "Enter" | "enter" | "Return" | "return" => KeyCode::Enter,
            "Esc" | "esc" | "Escape" | "escape"     => KeyCode::Esc,
            "Backspace" | "backspace" | "bs"         => KeyCode::Backspace,
            "Tab" | "tab"                            => KeyCode::Tab,
            "Up" | "up"                              => KeyCode::Up,
            "Down" | "down"                          => KeyCode::Down,
            "Left" | "left"                          => KeyCode::Left,
            "Right" | "right"                        => KeyCode::Right,
            s if s.len() == 1                        => KeyCode::Char(s.chars().next().unwrap()),
            _                                        => KeyCode::Null,
        }
    }

    pub fn down(&self)           -> KeyCode { Self::parse_key(&self.down) }
    pub fn up(&self)             -> KeyCode { Self::parse_key(&self.up) }
    pub fn left(&self)           -> KeyCode { Self::parse_key(&self.left) }
    pub fn right(&self)          -> KeyCode { Self::parse_key(&self.right) }
    pub fn top(&self)            -> KeyCode { Self::parse_key(&self.top) }
    pub fn bottom(&self)         -> KeyCode { Self::parse_key(&self.bottom) }
    pub fn open(&self)           -> KeyCode { Self::parse_key(&self.open) }
    pub fn back(&self)           -> KeyCode { Self::parse_key(&self.back) }
    pub fn search(&self)         -> KeyCode { Self::parse_key(&self.search) }
    pub fn add(&self)            -> KeyCode { Self::parse_key(&self.add) }
    pub fn delete(&self)         -> KeyCode { Self::parse_key(&self.delete) }
    pub fn set_status(&self)     -> KeyCode { Self::parse_key(&self.set_status) }
    pub fn mark_unread(&self)    -> KeyCode { Self::parse_key(&self.mark_unread) }
    pub fn clear_override(&self) -> KeyCode { Self::parse_key(&self.clear_override) }
    pub fn next_chapter(&self)   -> KeyCode { Self::parse_key(&self.next_chapter) }
    pub fn prev_chapter(&self)   -> KeyCode { Self::parse_key(&self.prev_chapter) }
    pub fn input_mode(&self)     -> KeyCode { Self::parse_key(&self.input_mode) }
    pub fn sort(&self)           -> KeyCode { Self::parse_key(&self.sort) }
}

// ---------------------------------------------------------------------------
// Theme / Colors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    // Status colors
    pub status_looked_into: String,
    pub status_reading:     String,
    pub status_up_to_date:  String,
    pub status_paused:      String,
    pub status_completed:   String,
    pub status_dropped:     String,

    // UI elements
    pub accent:             String, // selected borders, search bar, key labels
    pub text:               String, // primary text
    pub text_secondary:     String, // labels, hints, placeholders
    pub text_bold:          String, // titles, headers
    pub bar_fg:             String, // status bar foreground
    pub bar_bg:             String, // status bar background
    pub highlight_bg:       String, // list selection background
    pub unread_badge:       String, // unread chapter count
    pub error:              String, // error messages
    pub success:            String, // success indicators
    pub warning:            String, // warnings, partial progress
    pub border:             String, // default borders
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            status_looked_into: "gray".into(),
            status_reading:     "green".into(),
            status_up_to_date:  "cyan".into(),
            status_paused:      "yellow".into(),
            status_completed:   "blue".into(),
            status_dropped:     "darkgray".into(),

            accent:             "yellow".into(),
            text:               "white".into(),
            text_secondary:     "darkgray".into(),
            text_bold:          "white".into(),
            bar_fg:             "black".into(),
            bar_bg:             "white".into(),
            highlight_bg:       "darkgray".into(),
            unread_badge:       "red".into(),
            error:              "red".into(),
            success:            "green".into(),
            warning:            "yellow".into(),
            border:             "white".into(),
        }
    }
}

impl ThemeConfig {
    pub fn parse_color(s: &str) -> Color {
        match s.to_lowercase().as_str() {
            "black"       => Color::Black,
            "red"         => Color::Red,
            "green"       => Color::Green,
            "yellow"      => Color::Yellow,
            "blue"        => Color::Blue,
            "magenta"     => Color::Magenta,
            "cyan"        => Color::Cyan,
            "gray" | "grey" => Color::Gray,
            "darkgray" | "darkgrey" | "dark_gray" | "dark_grey" => Color::DarkGray,
            "lightred" | "light_red"       => Color::LightRed,
            "lightgreen" | "light_green"   => Color::LightGreen,
            "lightyellow" | "light_yellow" => Color::LightYellow,
            "lightblue" | "light_blue"     => Color::LightBlue,
            "lightmagenta" | "light_magenta" => Color::LightMagenta,
            "lightcyan" | "light_cyan"     => Color::LightCyan,
            "white"       => Color::White,
            "reset"       => Color::Reset,
            s if s.starts_with('#') && s.len() == 7 => {
                // Parse #RRGGBB hex color
                let r = u8::from_str_radix(&s[1..3], 16).unwrap_or(255);
                let g = u8::from_str_radix(&s[3..5], 16).unwrap_or(255);
                let b = u8::from_str_radix(&s[5..7], 16).unwrap_or(255);
                Color::Rgb(r, g, b)
            }
            s => {
                if let Ok(n) = s.parse::<u8>() {
                    Color::Indexed(n)
                } else {
                    Color::White
                }
            }
        }
    }

    pub fn status_color(&self, status: &crate::types::Status) -> Color {
        use crate::types::Status;
        match status {
            Status::LookedInto => Self::parse_color(&self.status_looked_into),
            Status::Reading    => Self::parse_color(&self.status_reading),
            Status::UpToDate   => Self::parse_color(&self.status_up_to_date),
            Status::Paused     => Self::parse_color(&self.status_paused),
            Status::Completed  => Self::parse_color(&self.status_completed),
            Status::Dropped    => Self::parse_color(&self.status_dropped),
        }
    }

    pub fn accent(&self)         -> Color { Self::parse_color(&self.accent) }
    pub fn text(&self)           -> Color { Self::parse_color(&self.text) }
    pub fn text_secondary(&self) -> Color { Self::parse_color(&self.text_secondary) }
    pub fn text_bold(&self)      -> Color { Self::parse_color(&self.text_bold) }
    pub fn bar_fg(&self)         -> Color { Self::parse_color(&self.bar_fg) }
    pub fn bar_bg(&self)         -> Color { Self::parse_color(&self.bar_bg) }
    pub fn highlight_bg(&self)   -> Color { Self::parse_color(&self.highlight_bg) }
    pub fn unread_badge(&self)   -> Color { Self::parse_color(&self.unread_badge) }
    pub fn error(&self)          -> Color { Self::parse_color(&self.error) }
    pub fn success(&self)        -> Color { Self::parse_color(&self.success) }
    pub fn warning(&self)        -> Color { Self::parse_color(&self.warning) }
    pub fn border(&self)         -> Color { Self::parse_color(&self.border) }
}

// ---------------------------------------------------------------------------
// imv viewer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImvConfig {
    pub binary:  String,
    pub options: ImvOptions,
    pub binds:   HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImvOptions {
    pub initial_pan:            String,
    pub scaling_mode:           String,
    pub pan_limits:             String,
    pub suppress_default_binds: String,
}

impl Default for ImvOptions {
    fn default() -> Self {
        Self {
            initial_pan:            "50 0".into(),
            scaling_mode:           "none".into(),
            pan_limits:             "yes".into(),
            suppress_default_binds: "true".into(),
        }
    }
}

impl Default for ImvConfig {
    fn default() -> Self {
        let mut binds = HashMap::new();
        binds.insert("q".into(),                "quit".into());
        binds.insert("<Left>".into(),           "prev; pan 0 0".into());
        binds.insert("<Right>".into(),          "next; pan 0 0".into());
        binds.insert("j".into(),                "pan 0 -50".into());
        binds.insert("k".into(),                "pan 0 50".into());
        binds.insert("<Shift+J>".into(),        "pan 0 -500".into());
        binds.insert("<Shift+K>".into(),        "pan 0 500".into());
        binds.insert("h".into(),                "pan 50 0".into());
        binds.insert("l".into(),                "pan -50 0".into());
        binds.insert("<Up>".into(),             "zoom 1".into());
        binds.insert("<Down>".into(),           "zoom -1".into());
        binds.insert("f".into(),                "fullscreen".into());
        binds.insert("<scroll-up>".into(),      "pan 0 50".into());
        binds.insert("<scroll-down>".into(),    "pan 0 -50".into());
        binds.insert("<shift-scroll-up>".into(),   "pan 0 500".into());
        binds.insert("<shift-scroll-down>".into(), "pan 0 -500".into());
        Self { binary: "imv".into(), options: ImvOptions::default(), binds }
    }
}

impl ImvConfig {
    /// Render to imv config file format.
    pub fn to_config_string(&self) -> String {
        let mut s = String::from("[options]\n");
        s.push_str(&format!("initial_pan = {}\n", self.options.initial_pan));
        s.push_str(&format!("scaling_mode = {}\n", self.options.scaling_mode));
        s.push_str(&format!("pan_limits = {}\n", self.options.pan_limits));
        s.push_str(&format!("suppress_default_binds = {}\n", self.options.suppress_default_binds));
        s.push_str("\n[binds]\n");
        // Sort keys for deterministic output
        let mut pairs: Vec<_> = self.binds.iter().collect();
        pairs.sort_by_key(|(k, _)| (*k).clone());
        for (key, action) in pairs {
            s.push_str(&format!("{} = {}\n", key, action));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// rv viewer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RvConfig {
    pub binary:            String,
    pub scroll_speed:      u32,
    pub fast_scroll_speed: u32,
    pub fullscreen:        bool,
    pub binds:             HashMap<String, String>,
}

impl Default for RvConfig {
    fn default() -> Self {
        let mut binds = HashMap::new();
        binds.insert("q".into(),      "quit".into());
        binds.insert("j".into(),      "scroll_down".into());
        binds.insert("k".into(),      "scroll_up".into());
        binds.insert("J".into(),      "fast_scroll_down".into());
        binds.insert("K".into(),      "fast_scroll_up".into());
        binds.insert("space".into(),  "page_down".into());
        binds.insert("g".into(),      "top".into());
        binds.insert("G".into(),      "bottom".into());
        binds.insert("up".into(),     "zoom_in".into());
        binds.insert("down".into(),   "zoom_out".into());
        binds.insert("equals".into(), "zoom_reset".into());
        binds.insert("f".into(),      "fullscreen".into());
        binds.insert("h".into(),      "pan_left".into());
        binds.insert("l".into(),      "pan_right".into());
        Self {
            binary:            "rv".into(),
            scroll_speed:      80,
            fast_scroll_speed: 600,
            fullscreen:        true,
            binds,
        }
    }
}

impl RvConfig {
    /// Build CLI args for rv from this config.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        args.push("--scroll-speed".into());
        args.push(self.scroll_speed.to_string());
        args.push("--fast-scroll-speed".into());
        args.push(self.fast_scroll_speed.to_string());
        if self.fullscreen {
            args.push("--fullscreen".into());
        }
        for (key, action) in &self.binds {
            args.push("--bind".into());
            args.push(format!("{key}={action}"));
        }
        args
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Load config from config.toml, searching CWD then ~/.config/mrm/.
pub fn load_config() -> Result<Config> {
    let path = find_config_path()
        .with_context(|| "config.toml not found in CWD or ~/.config/mrm/")?;
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
    Ok(config)
}

fn find_config_path() -> Option<std::path::PathBuf> {
    let local = std::path::PathBuf::from("config.toml");
    if local.exists() {
        return Some(local);
    }
    if let Ok(home) = std::env::var("HOME") {
        let xdg = std::path::PathBuf::from(home).join(".config/mrm/config.toml");
        if xdg.exists() {
            return Some(xdg);
        }
    }
    None
}
