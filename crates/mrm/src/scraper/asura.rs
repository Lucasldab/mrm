//! AsuraScans scraper — delegates to the Python scraper via subprocess.
//!
//! AsuraScans uses Cloudflare, which blocks standard HTTP clients.
//! The Python side uses curl_cffi to bypass TLS fingerprinting.
//! This module shells out to the venv python in `scraper_dir` and runs
//! `python -m scraper.asura_bridge <command> <args>`, parsing JSON output.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;

use super::{ChapterData, Scraper, SearchResult, SeriesData};

// ---------------------------------------------------------------------------
// Struct
// ---------------------------------------------------------------------------

pub struct AsuraScraper {
    /// Path to the project root containing the `scraper/` Python package.
    scraper_dir: PathBuf,
}

impl AsuraScraper {
    pub fn new(scraper_dir: PathBuf) -> Self {
        Self { scraper_dir }
    }

    /// Resolve the venv Python binary path.
    fn python_path(&self) -> PathBuf {
        self.scraper_dir.join("scraper/.venv/bin/python3")
    }

    /// Run the Python bridge script and return its stdout as a string.
    async fn run_bridge(&self, args: &[&str]) -> Result<String> {
        let python = self.python_path();
        let mut cmd = tokio::process::Command::new(&python);
        cmd.arg("-m")
            .arg("scraper.asura_bridge")
            .args(args)
            .current_dir(&self.scraper_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
            .await
            .map_err(|_| anyhow!("Asura bridge timed out after 30s"))?
            .context("Failed to run asura_bridge")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("Asura bridge failed: {stderr}"));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

// ---------------------------------------------------------------------------
// Scraper trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Scraper for AsuraScraper {
    fn source_name(&self) -> &'static str {
        "asura"
    }

    async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let json_str = self.run_bridge(&["search", query]).await?;
        let items: Vec<serde_json::Value> = serde_json::from_str(&json_str)
            .context("Failed to parse asura search JSON")?;

        let mut results = Vec::new();
        for item in items {
            results.push(SearchResult {
                title:      item["title"].as_str().unwrap_or("").to_string(),
                cover_url:  item["cover_url"].as_str().map(|s| s.to_string()),
                source_url: item["source_url"].as_str().unwrap_or("").to_string(),
                pub_status: item["pub_status"].as_str().unwrap_or("ongoing").to_string(),
                source:     "asura".into(),
            });
        }

        Ok(results)
    }

    async fn get_series(&self, source_url: &str) -> Result<SeriesData> {
        let json_str = self.run_bridge(&["get_series", source_url]).await?;
        let data: serde_json::Value = serde_json::from_str(&json_str)
            .context("Failed to parse asura get_series JSON")?;

        let empty = vec![];
        let ch_array = data["chapters"].as_array().unwrap_or(&empty);
        let mut chapters = Vec::new();
        for ch in ch_array {
            chapters.push(ChapterData {
                number:      ch["number"].as_f64().unwrap_or(0.0),
                title:       ch["title"].as_str().map(|s| s.to_string()),
                url:         ch["url"].as_str().unwrap_or("").to_string(),
                released_at: ch["released_at"].as_str().map(|s| s.to_string()),
            });
        }

        Ok(SeriesData {
            title:      data["title"].as_str().unwrap_or("").to_string(),
            cover_url:  data["cover_url"].as_str().map(|s| s.to_string()),
            source_url: data["source_url"].as_str().unwrap_or(source_url).to_string(),
            pub_status: data["pub_status"].as_str().unwrap_or("ongoing").to_string(),
            chapters,
        })
    }

    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>> {
        let json_str = self.run_bridge(&["get_chapter_image_urls", chapter_url]).await?;
        let urls: Vec<String> = serde_json::from_str(&json_str)
            .context("Failed to parse asura chapter images JSON")?;
        Ok(urls)
    }
}
