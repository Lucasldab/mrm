// ---------------------------------------------------------------------------
// Cover image download and disk cache
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::path::PathBuf;

use image::DynamicImage;

use crate::types::Manhwa;

// ---------------------------------------------------------------------------
// CoverCache
// ---------------------------------------------------------------------------

pub struct CoverCache {
    cache_dir: PathBuf,
    images: HashMap<i64, Option<DynamicImage>>,
}

impl CoverCache {
    pub fn new() -> Self {
        let cache_dir = cache_dir_path();
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            cache_dir,
            images: HashMap::new(),
        }
    }

    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }

    /// Load a cover from disk cache if available. Does NOT do HTTP.
    /// Inserts None if no cover_url or file not on disk yet.
    pub fn ensure_loaded(&mut self, manhwa_id: i64, cover_url: Option<&str>) {
        if self.images.contains_key(&manhwa_id) {
            return;
        }
        if cover_url.is_none() {
            self.images.insert(manhwa_id, None);
            return;
        }
        let path = self.cache_dir.join(format!("{manhwa_id}.jpg"));
        if path.exists() {
            match image::open(&path) {
                Ok(img) => {
                    let resized = img.resize(80, 120, image::imageops::FilterType::Triangle);
                    self.images.insert(manhwa_id, Some(resized));
                }
                Err(_) => {
                    self.images.insert(manhwa_id, None);
                }
            }
        } else {
            self.images.insert(manhwa_id, None);
        }
    }

    /// Get a previously loaded cover image.
    /// If currently None (not yet downloaded), re-checks disk in case
    /// the background preload has finished downloading it.
    pub fn get(&mut self, manhwa_id: i64) -> Option<&DynamicImage> {
        // Re-check disk for entries that are None
        if let Some(None) = self.images.get(&manhwa_id) {
            let path = self.cache_dir.join(format!("{manhwa_id}.jpg"));
            if path.exists() {
                if let Ok(img) = image::open(&path) {
                    let resized = img.resize(80, 120, image::imageops::FilterType::Triangle);
                    self.images.insert(manhwa_id, Some(resized));
                }
            }
        }
        self.images.get(&manhwa_id).and_then(|opt| opt.as_ref())
    }

    /// Re-check disk for any manhwa IDs currently mapped to None.
    /// Called after background preload may have finished downloading.
    pub fn reload_from_disk(&mut self, manhwa_list: &[Manhwa]) {
        for m in manhwa_list {
            if m.cover_url.is_none() {
                continue;
            }
            // Only re-check entries that are None (not yet loaded)
            let should_retry = self.images.get(&m.id).map(|v| v.is_none()).unwrap_or(true);
            if !should_retry {
                continue;
            }
            let path = self.cache_dir.join(format!("{}.jpg", m.id));
            if path.exists() {
                if let Ok(img) = image::open(&path) {
                    let resized = img.resize(80, 120, image::imageops::FilterType::Triangle);
                    self.images.insert(m.id, Some(resized));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background preload (standalone async fn, spawned as tokio task)
// ---------------------------------------------------------------------------

/// Download covers that are not yet cached on disk.
/// Meant to be spawned with tokio::spawn. Silently skips failures.
pub async fn preload_covers(cache_dir: PathBuf, manhwa_list: Vec<(i64, Option<String>)>) {
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let sem = Arc::new(Semaphore::new(4));
    let client = match reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => Arc::new(c),
        Err(_) => return,
    };

    let mut set: JoinSet<()> = JoinSet::new();

    for (id, url) in manhwa_list {
        let url = match url {
            Some(u) => u,
            None => continue,
        };
        let path = cache_dir.join(format!("{id}.jpg"));
        if path.exists() {
            continue;
        }
        let client = client.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let bytes = match client.get(&url).send().await {
                Ok(resp) => match resp.bytes().await {
                    Ok(b) => b,
                    Err(_) => return,
                },
                Err(_) => return,
            };
            // Validate image data before saving (T-quick-01 mitigation)
            let img = match image::load_from_memory(&bytes) {
                Ok(i) => i,
                Err(_) => return,
            };
            // Max image size check (T-quick-02 mitigation)
            if img.width() > 4000 || img.height() > 4000 {
                return;
            }
            let resized = img.resize(80, 120, image::imageops::FilterType::Triangle);
            let _ = resized.save(&path);
        });
    }

    while set.join_next().await.is_some() {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cache_dir_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache/mrm/covers")
    } else {
        PathBuf::from("/tmp/mrm_covers")
    }
}
