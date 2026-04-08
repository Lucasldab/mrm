//! TOML config loader — replaces Python scraper/config.py
//!
//! Reads config.toml from the project root (or path overridable via env).
//! Config is loaded once at startup and passed by clone into tasks that need it.

use std::collections::HashMap;
use anyhow::{Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub sources:       HashMap<String, SourceConfig>,
    pub notifications: NotificationsConfig,
    pub db:            DbConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub base_url: String,
    pub enabled:  bool,
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
// Loader
// ---------------------------------------------------------------------------

/// Load config from `config.toml` in the current working directory.
/// Returns an error with context if the file is missing or malformed.
pub fn load_config() -> Result<Config> {
    let path = "config.toml";
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse config file: {path}"))?;
    Ok(config)
}
