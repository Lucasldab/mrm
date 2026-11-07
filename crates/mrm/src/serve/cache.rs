//! Disk caches of `mrm --serve`: `serve-pages/<chapter>/<n>` and
//! `serve-covers/<series>`. Keys are integers only, so a path can never
//! leave the cache root. Optional size cap, evicting least recently used
//! files first (reads touch the mtime).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::body::Bytes;

/// Shortest gap between two eviction sweeps triggered by writes.
const SWEEP_EVERY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct DiskCache {
    inner: Arc<Inner>,
}

struct Inner {
    root:       PathBuf,
    cap:        Option<u64>,
    last_sweep: Mutex<Option<Instant>>,
    tmp_seq:    AtomicU64,
}

impl DiskCache {
    pub fn new(root: PathBuf, cap: Option<u64>) -> Self {
        Self { inner: Arc::new(Inner { root, cap, last_sweep: Mutex::new(None), tmp_seq: AtomicU64::new(0) }) }
    }

    /// `root/<k0>/<k1>/…`, built from integers only.
    pub fn path(&self, key: &[i64]) -> PathBuf {
        let mut p = self.inner.root.clone();
        for k in key {
            p.push(k.to_string());
        }
        p
    }

    /// Cached bytes, marking the entry as recently used.
    pub async fn get(&self, key: &[i64]) -> Option<Bytes> {
        let path = self.path(key);
        tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path).ok()?;
            touch(&path);
            Some(Bytes::from(bytes))
        }).await.ok()?
    }

    /// Store bytes (atomically: temp file + rename), then sweep if due.
    /// Failures are logged; a cache that cannot write just stays cold.
    pub async fn put(&self, key: &[i64], bytes: Bytes) {
        let this = self.clone();
        let path = self.path(key);
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = this.write(&path, &bytes) {
                eprintln!("mrm serve: cache write {} failed: {e}", path.display());
            }
            this.maybe_sweep();
        }).await;
    }

    pub async fn remove(&self, key: &[i64]) {
        let _ = tokio::fs::remove_file(self.path(key)).await;
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or(&self.inner.root);
        std::fs::create_dir_all(dir)?;
        let seq = self.inner.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{}-{seq}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| { let _ = std::fs::remove_file(&tmp); })
    }

    fn maybe_sweep(&self) {
        if self.inner.cap.is_none() {
            return;
        }
        {
            let mut last = self.inner.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < SWEEP_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        self.sweep();
    }

    /// Delete the least recently used files until the cache fits its cap.
    /// Blocking.
    pub fn sweep(&self) {
        let Some(cap) = self.inner.cap else { return };
        let mut files = Vec::new();
        collect_files(&self.inner.root, 2, &mut files);
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= cap {
            return;
        }
        files.sort_by_key(|f| f.0);
        for (_, size, path) in files {
            if total <= cap {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                if let Some(dir) = path.parent().filter(|d| *d != self.inner.root) {
                    let _ = std::fs::remove_dir(dir); // only succeeds once empty
                }
            }
        }
    }
}

/// (mtime, size, path) of every file under `dir`, at most `depth` levels down.
fn collect_files(dir: &Path, depth: u32, out: &mut Vec<(SystemTime, u64, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth > 1 {
                collect_files(&entry.path(), depth - 1, out);
            }
        } else if meta.is_file() {
            out.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len(), entry.path()));
        }
    }
}

fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn age(path: &Path, secs_ago: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago)).unwrap();
    }

    #[tokio::test]
    async fn paths_are_integers_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let c = DiskCache::new(dir.path().join("p"), None);
        assert_eq!(c.path(&[12, 3]), dir.path().join("p/12/3"));
        assert_eq!(c.path(&[-1]), dir.path().join("p/-1"));
        c.put(&[12, 3], Bytes::from_static(b"abc")).await;
        assert_eq!(c.get(&[12, 3]).await.as_deref(), Some(&b"abc"[..]));
        assert!(c.get(&[12, 4]).await.is_none());
        let names: Vec<_> = std::fs::read_dir(dir.path().join("p/12")).unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(names, vec!["3"], "no temp files left behind");
        c.remove(&[12, 3]).await;
        assert!(c.get(&[12, 3]).await.is_none());
    }

    #[tokio::test]
    async fn sweep_evicts_least_recently_used_first() {
        let dir = tempfile::tempdir().unwrap();
        let c = DiskCache::new(dir.path().to_path_buf(), Some(25));
        for (ch, n) in [(1, 0), (1, 1), (2, 0)] {
            c.inner.last_sweep.lock().unwrap().replace(Instant::now()); // no sweep yet
            c.put(&[ch, n], Bytes::from(vec![0u8; 10])).await;
        }
        age(&c.path(&[1, 0]), 300);
        age(&c.path(&[1, 1]), 200);
        age(&c.path(&[2, 0]), 100);
        // A read makes 1/0 the most recently used.
        assert!(c.get(&[1, 0]).await.is_some());
        c.sweep();
        assert!(c.path(&[1, 0]).exists(), "recently read entry was evicted");
        assert!(!c.path(&[1, 1]).exists(), "oldest entry survived");
        assert!(c.path(&[2, 0]).exists());
        c.sweep();
        assert!(c.path(&[2, 0]).exists(), "under the cap nothing more goes");
    }

    #[tokio::test]
    async fn emptied_chapter_dirs_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let c = DiskCache::new(dir.path().to_path_buf(), Some(5));
        c.inner.last_sweep.lock().unwrap().replace(Instant::now());
        c.put(&[7, 0], Bytes::from(vec![0u8; 10])).await;
        c.sweep();
        assert!(!dir.path().join("7").exists());
    }
}
