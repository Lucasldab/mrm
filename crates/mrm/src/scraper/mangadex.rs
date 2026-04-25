//! MangaDex scraper — full implementation of the Scraper trait.
//!
//! Uses the public MangaDex REST API (no auth required).
//!
//! Key endpoints:
//!   Search   : GET /manga?title=...&availableTranslatedLanguage[]=en
//!   Series   : GET /manga/{id}?includes[]=cover_art
//!   Chapters : GET /manga/{id}/feed?translatedLanguage[]=en&order[chapter]=asc&limit=500
//!   Images   : GET /at-home/server/{chapter_id}
//!                -> baseUrl + /data/ + hash + / + filename

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;
use tokio::time::sleep;

use super::{ChapterData, DiscoveryEntry, Scraper, SearchResult, SeriesData};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BASE: &str = "https://api.mangadex.org";
const COVER_BASE: &str = "https://uploads.mangadex.org/covers";
const DELAY: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// Struct
// ---------------------------------------------------------------------------

pub struct MangaDexScraper {
    client: reqwest::Client,
}

impl MangaDexScraper {
    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Accept", "application/json".parse().unwrap());

        Self {
            client: reqwest::Client::builder()
                .user_agent("mrm/0.1 (https://github.com/Lucasldab/mrm)")
                .default_headers(headers)
                .timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest client build failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Map MangaDex status string to canonical pub_status value.
fn map_status(raw: Option<&str>) -> String {
    match raw.unwrap_or("").to_lowercase().as_str() {
        "ongoing"   => "ongoing".into(),
        "hiatus"    => "hiatus".into(),
        "completed" => "completed".into(),
        "cancelled" => "completed".into(),  // cancelled → completed
        _           => "ongoing".into(),
    }
}

/// Extract best English title from MangaDex attributes.
/// Preference: "en" → "ja-ro" → first available value.
fn extract_title(attributes: &serde_json::Value) -> String {
    let t = &attributes["title"];
    t["en"].as_str()
        .or_else(|| t["ja-ro"].as_str())
        .or_else(|| t.as_object().and_then(|m| m.values().next().and_then(|v| v.as_str())))
        .unwrap_or("")
        .to_string()
}

/// Extract series description from MangaDex attributes.
/// Preference: "en" → first available value. Returns None when blank.
fn extract_description(attributes: &serde_json::Value) -> Option<String> {
    let d = &attributes["description"];
    let raw = d["en"].as_str()
        .or_else(|| d.as_object().and_then(|m| m.values().next().and_then(|v| v.as_str())))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// Build cover URL from the cover_art relationship entry.
fn extract_cover(manga_id: &str, relationships: &serde_json::Value) -> Option<String> {
    relationships.as_array()?.iter().find_map(|rel| {
        if rel["type"].as_str()? == "cover_art" {
            let filename = rel["attributes"]["fileName"].as_str()?;
            Some(format!("{COVER_BASE}/{manga_id}/{filename}"))
        } else {
            None
        }
    })
}

/// Extract the last path segment from a URL (UUID).
fn id_from_url(url: &str) -> &str {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Scraper trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Scraper for MangaDexScraper {
    fn source_name(&self) -> &'static str {
        "mangadex"
    }

    /// Search MangaDex for a title. Returns up to 20 results.
    async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let url = format!("{BASE}/manga");
        let client = &self.client;

        let resp = super::retry(|| async {
            let r = client
                .get(&url)
                .query(&[
                    ("title", query),
                    ("limit", "20"),
                    ("availableTranslatedLanguage[]", "en"),
                    ("includes[]", "cover_art"),
                    ("contentRating[]", "safe"),
                    ("contentRating[]", "suggestive"),
                    ("contentRating[]", "erotica"),
                    ("order[relevance]", "desc"),
                ])
                .send()
                .await
                .with_context(|| format!("MangaDex GET {url} failed"))?;
            let r = r.error_for_status()
                .with_context(|| format!("MangaDex GET {url} HTTP error"))?;
            r.json::<serde_json::Value>()
                .await
                .with_context(|| format!("MangaDex GET {url} JSON parse failed"))
        })
        .await?;

        let empty = vec![];
        let items = resp["data"].as_array().unwrap_or(&empty);

        let mut results = Vec::new();
        for item in items {
            let manga_id = match item["id"].as_str() {
                Some(id) => id,
                None => continue,
            };
            let attr = &item["attributes"];
            let cover_url = extract_cover(manga_id, &item["relationships"]);

            results.push(SearchResult {
                title:      extract_title(attr),
                cover_url,
                source_url: format!("{BASE}/manga/{manga_id}"),
                pub_status: map_status(attr["status"].as_str()),
                source:     "mangadex".into(),
            });
        }

        Ok(results)
    }

    /// Fetch series metadata and full chapter list from source_url.
    async fn get_series(&self, source_url: &str) -> Result<SeriesData> {
        let manga_id = id_from_url(source_url).to_string();
        let url = format!("{BASE}/manga/{manga_id}");
        let client = &self.client;

        let resp = super::retry(|| async {
            let r = client
                .get(&url)
                .query(&[("includes[]", "cover_art")])
                .send()
                .await
                .with_context(|| format!("MangaDex GET {url} failed"))?;
            let r = r.error_for_status()
                .with_context(|| format!("MangaDex GET {url} HTTP error"))?;
            r.json::<serde_json::Value>()
                .await
                .with_context(|| format!("MangaDex GET {url} JSON parse failed"))
        })
        .await?;

        let item = &resp["data"];
        let attr = &item["attributes"];
        let cover_url = extract_cover(&manga_id, &item["relationships"]);
        let description = extract_description(attr);

        let chapters = self._fetch_all_chapters(&manga_id).await?;

        Ok(SeriesData {
            title:      extract_title(attr),
            cover_url,
            source_url: source_url.to_string(),
            pub_status: map_status(attr["status"].as_str()),
            description,
            chapters,
        })
    }

    /// Fetch ordered image URLs for a chapter from chapter_url.
    async fn get_chapter_image_urls(&self, chapter_url: &str) -> Result<Vec<String>> {
        let chapter_id = id_from_url(chapter_url).to_string();
        let url = format!("{BASE}/at-home/server/{chapter_id}");
        let client = &self.client;

        let resp = super::retry(|| async {
            let r = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("MangaDex GET {url} failed"))?;
            let r = r.error_for_status()
                .with_context(|| format!("MangaDex GET {url} HTTP error"))?;
            r.json::<serde_json::Value>()
                .await
                .with_context(|| format!("MangaDex GET {url} JSON parse failed"))
        })
        .await?;

        let base_url = resp["baseUrl"]
            .as_str()
            .with_context(|| format!("MangaDex at-home response missing baseUrl: {url}"))?;
        let hash = resp["chapter"]["hash"]
            .as_str()
            .with_context(|| format!("MangaDex at-home response missing chapter.hash: {url}"))?;

        let empty = vec![];
        let filenames = resp["chapter"]["data"].as_array().unwrap_or(&empty);

        let urls = filenames
            .iter()
            .filter_map(|f| f.as_str())
            .map(|filename| format!("{base_url}/data/{hash}/{filename}"))
            .collect();

        Ok(urls)
    }

    /// Fetch recently updated manga as discovery candidates.
    ///
    /// Uses GET /manga ordered by latestUploadedChapter desc. MangaDex exposes
    /// the latest chapter count as attributes.lastChapter but the per-chapter
    /// release date requires a separate /chapter lookup, which would cost one
    /// HTTP hit per entry. For discovery purposes the latestUploadedChapter
    /// order plus the series' updatedAt is good enough — the Discover screen
    /// shows cover + title, not chapter dates.
    async fn latest_chapters(&self) -> Result<Vec<DiscoveryEntry>> {
        let url = format!("{BASE}/manga");
        let client = &self.client;

        let resp = super::retry(|| async {
            let r = client
                .get(&url)
                .query(&[
                    ("limit", "30"),
                    ("availableTranslatedLanguage[]", "en"),
                    ("includes[]", "cover_art"),
                    ("contentRating[]", "safe"),
                    ("contentRating[]", "suggestive"),
                    ("contentRating[]", "erotica"),
                    ("hasAvailableChapters", "true"),
                    ("order[latestUploadedChapter]", "desc"),
                ])
                .send()
                .await
                .with_context(|| format!("MangaDex GET {url} failed"))?;
            let r = r.error_for_status()
                .with_context(|| format!("MangaDex GET {url} HTTP error"))?;
            r.json::<serde_json::Value>()
                .await
                .with_context(|| format!("MangaDex GET {url} JSON parse failed"))
        })
        .await?;

        let empty = vec![];
        let items = resp["data"].as_array().unwrap_or(&empty);

        let mut out = Vec::new();
        for item in items {
            let manga_id = match item["id"].as_str() {
                Some(id) => id,
                None => continue,
            };
            let attr = &item["attributes"];
            let title = extract_title(attr);
            if title.is_empty() {
                continue;
            }
            let cover_url = extract_cover(manga_id, &item["relationships"]);
            let chapter_number = attr["lastChapter"]
                .as_str()
                .and_then(|s| f64::from_str(s).ok());
            let released_at = attr["updatedAt"].as_str().map(|s| s.to_string());

            out.push(DiscoveryEntry {
                title,
                cover_url,
                source_url: format!("{BASE}/manga/{manga_id}"),
                chapter_number,
                released_at,
            });
        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

impl MangaDexScraper {
    /// Paginate through /manga/{id}/feed to collect every English chapter.
    /// Deduplicates by chapter number (keeps first occurrence — earliest uploaded).
    async fn _fetch_all_chapters(&self, manga_id: &str) -> Result<Vec<ChapterData>> {
        let mut chapters: Vec<ChapterData> = Vec::new();
        let mut seen: HashSet<u64> = HashSet::new();
        let mut offset: usize = 0;
        let limit: usize = 500;
        let client = &self.client;

        loop {
            // Polite delay before each paginated request (250ms as per Python _DELAY)
            sleep(DELAY).await;

            let url = format!("{BASE}/manga/{manga_id}/feed");
            let offset_str = offset.to_string();
            let limit_str = limit.to_string();

            let resp = super::retry(|| async {
                let r = client
                    .get(&url)
                    .query(&[
                        ("translatedLanguage[]", "en"),
                        ("order[chapter]", "asc"),
                        ("limit", &limit_str),
                        ("offset", &offset_str),
                        ("contentRating[]", "safe"),
                        ("contentRating[]", "suggestive"),
                        ("contentRating[]", "erotica"),
                    ])
                    .send()
                    .await
                    .with_context(|| format!("MangaDex GET {url} failed"))?;
                r.json::<serde_json::Value>()
                    .await
                    .with_context(|| format!("MangaDex GET {url} JSON parse failed"))
            })
            .await?;

            let empty = vec![];
            let items = resp["data"].as_array().unwrap_or(&empty);
            let total = resp["total"].as_u64().unwrap_or(0) as usize;

            for item in items {
                let attr = &item["attributes"];

                // Parse chapter number — skip if absent or unparseable
                let ch_str = match attr["chapter"].as_str() {
                    Some(s) => s,
                    None => continue,
                };
                let number = match f64::from_str(ch_str) {
                    Ok(n) => n,
                    Err(_) => continue,
                };

                // Deduplicate: use bit representation for hash-equality on f64
                let key = number.to_bits();
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);

                let item_id = match item["id"].as_str() {
                    Some(id) => id,
                    None => continue,
                };

                // released_at: take first 10 chars (YYYY-MM-DD) from publishAt
                let released_at = attr["publishAt"]
                    .as_str()
                    .filter(|s| s.len() >= 10)
                    .map(|s| s[..10].to_string());

                let title = attr["title"].as_str().map(|s| s.to_string());

                chapters.push(ChapterData {
                    number,
                    title,
                    url:         format!("{BASE}/chapter/{item_id}"),
                    released_at,
                });
            }

            offset += items.len();
            if offset >= total || items.is_empty() {
                break;
            }
        }

        Ok(chapters)
    }
}
