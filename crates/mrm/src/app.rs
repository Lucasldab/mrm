use std::collections::HashMap;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use sqlx::sqlite::SqlitePool;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::cover_cache::CoverCache;
use crate::types::{AppEvent, Chapter, Discovery, Manhwa, Screen, SortMode, Status};
use crate::db;

/// Stable cache id for a search result, derived from its source_url so the
/// same series hits the on-disk cache across queries.
pub fn search_result_id(source_url: &str) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    source_url.hash(&mut h);
    h.finish() as i64
}

// ---------------------------------------------------------------------------
// ManagedChild — kills child viewer on drop (prevents orphan processes)
// ---------------------------------------------------------------------------

/// Wrapper around std::process::Child that kills the process on drop.
/// Prevents the viewer from becoming an orphan when the TUI exits or panics.
pub(crate) struct ManagedChild(std::process::Child);

/// Check if a binary exists on PATH (or is an absolute/relative path that resolves).
fn binary_exists(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).is_file();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if std::path::Path::new(dir).join(bin).is_file() {
                return true;
            }
        }
    }
    false
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// Hyprland host fullscreen preservation
// ---------------------------------------------------------------------------
//
// When the viewer (rv/imv) requests compositor fullscreen, Hyprland demotes
// whatever else on the workspace was fullscreen — including the terminal
// hosting mrm. On viewer exit, Hyprland does not auto-restore. We snapshot the
// active window's fullscreen state before launching the viewer and re-assert
// it on exit. No-op off Hyprland.

#[derive(Clone)]
pub(crate) struct HostFullscreen {
    address:          String,
    internal:         i64,
    client:           i64,
}

fn snapshot_host_fullscreen() -> Option<HostFullscreen> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return None;
    }
    let out = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output().ok()?;
    if !out.status.success() { return None; }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let address = v.get("address")?.as_str()?.to_string();
    let internal = v.get("fullscreen").and_then(|x| x.as_i64()).unwrap_or(0);
    let client   = v.get("fullscreenClient").and_then(|x| x.as_i64()).unwrap_or(0);
    if internal == 0 && client == 0 { return None; }
    Some(HostFullscreen { address, internal, client })
}

fn restore_host_fullscreen(h: &HostFullscreen) {
    let _ = std::process::Command::new("hyprctl")
        .args(["dispatch", "focuswindow", &format!("address:{}", h.address)])
        .status();
    let _ = std::process::Command::new("hyprctl")
        .args([
            "dispatch",
            "fullscreenstate",
            &h.internal.to_string(),
            &h.client.to_string(),
        ])
        .status();
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

pub struct App {
    pub pool:          SqlitePool,
    pub screen:        Screen,
    pub should_quit:   bool,

    // Library
    pub manhwa_list:   Vec<Manhwa>,
    pub library_sel:   usize,
    pub search_query:  String,
    pub search_active: bool,
    pub sort_mode:     SortMode,

    // Detail
    pub current_manhwa: Option<Manhwa>,
    pub chapter_list:   Vec<Chapter>,
    pub chapter_sel:    usize,

    // Status picker
    pub status_sel: usize,

    // Reader
    pub current_chapter: Option<Chapter>,
    pub images_loading:  bool,
    pub image_paths:     Vec<std::path::PathBuf>,
    pub image_rx:            Option<mpsc::Receiver<Option<(usize, Option<std::path::PathBuf>)>>>,
    pub image_pending:       Vec<(usize, Option<std::path::PathBuf>)>,
    pub next_expected_image: usize,
    pub viewer_process:      Option<ManagedChild>,
    pub viewer_pid:          Option<u32>,
    pub viewer_socket:       Option<String>,
    pub viewer_loaded_count: usize,
    pub viewer_drop_warned:  bool,
    pub(crate) host_fullscreen: Option<HostFullscreen>,
    pub session_dir:     Option<TempDir>,
    pub error_rx:        Option<mpsc::Receiver<String>>,

    pub status_msg: Option<String>,

    // AddSearch screen state
    pub add_search_query:        String,
    pub add_search_results:      Vec<crate::scraper::SearchResult>,
    pub add_search_grid_cols:    usize,
    pub search_cover_cache:      CoverCache,
    pub search_cover_protocols:  HashMap<i64, StatefulProtocol>,
    pub add_search_sel:          usize,
    pub add_search_loading:      bool,
    pub add_search_error:        Option<String>,
    pub add_search_input_active: bool,  // true = typing mode, false = browsing results

    // Delete confirmation
    pub confirm_delete_id: Option<i64>,  // Some(id) = waiting for confirmation keypress

    // Cover cache and image rendering
    pub cover_cache: CoverCache,
    pub picker: Option<Picker>,
    pub cover_protocols: HashMap<i64, StatefulProtocol>,
    pub cover_tick: u8,

    // Discover screen state — undismissed candidates found by the coordinator's
    // daily latest-chapters pass. Covers go in a separate cache so their ids
    // can't collide with library manhwa ids on disk.
    pub discoveries:        Vec<Discovery>,
    pub discover_sel:       usize,
    pub discover_grid_cols: usize,
    pub discover_cover_cache:     CoverCache,
    pub discover_cover_protocols: HashMap<i64, StatefulProtocol>,
    pub discover_adding:    bool,
    pub discover_error:     Option<String>,

    // Grid layout (computed during render, used for navigation)
    pub grid_cols: usize,

    // Config
    pub config: crate::config::Config,
    pub keys: crate::config::KeysConfig,
    pub theme: crate::config::ThemeConfig,
    pub imv_config: crate::config::ImvConfig,
    pub rv_config: crate::config::RvConfig,
    pub viewer_kind: crate::config::ViewerKind,

    // Vim-style gg: true when first g was pressed, waiting for second
    pub pending_g: bool,
}

impl App {
    pub async fn new(pool: SqlitePool, picker: Option<Picker>, config: crate::config::Config) -> Result<Self> {
        let manhwa_list = db::fetch_all_manhwa(&pool).await?;

        Ok(Self {
            pool,
            screen:           Screen::Library,
            should_quit:      false,
            manhwa_list,
            library_sel:      0,
            search_query:     String::new(),
            search_active:    false,
            sort_mode:        SortMode::Title,
            current_manhwa:   None,
            chapter_list:     Vec::new(),
            chapter_sel:      0,
            status_sel:       0,
            current_chapter:  None,
            images_loading:   false,
            image_paths:         Vec::new(),
            image_rx:            None,
            image_pending:       Vec::new(),
            next_expected_image: 0,
            viewer_process:      None,
            viewer_pid:          None,
            viewer_socket:       None,
            viewer_loaded_count: 0,
            viewer_drop_warned:  false,
            host_fullscreen:     None,
            session_dir:      None,
            error_rx:         None,
            status_msg:       None,
            add_search_query:        String::new(),
            add_search_results:      Vec::new(),
            add_search_grid_cols:    1,
            search_cover_cache:      CoverCache::with_subdir(Some("search")),
            search_cover_protocols:  HashMap::new(),
            add_search_sel:          0,
            add_search_loading:      false,
            add_search_error:        None,
            add_search_input_active: true,
            confirm_delete_id:       None,
            cover_cache:             CoverCache::new(),
            picker,
            cover_protocols:         HashMap::new(),
            cover_tick:              0,
            discoveries:             Vec::new(),
            discover_sel:            0,
            discover_grid_cols:      1,
            discover_cover_cache:    CoverCache::with_subdir(Some("discover")),
            discover_cover_protocols: HashMap::new(),
            discover_adding:         false,
            discover_error:          None,
            grid_cols:               1,
            keys:                    config.keys.clone(),
            theme:                   config.theme.clone(),
            imv_config:              config.imv.clone(),
            rv_config:               config.rv.clone(),
            viewer_kind:             config.viewer_kind(),
            config,
            pending_g: false,
        })
    }

    // -----------------------------------------------------------------------
    // Filtered library list
    // -----------------------------------------------------------------------

    pub fn visible_manhwa(&self) -> Vec<&Manhwa> {
        let mut list: Vec<&Manhwa> = if self.search_query.is_empty() {
            self.manhwa_list.iter().collect()
        } else {
            let q = self.search_query.to_lowercase();
            self.manhwa_list.iter()
                .filter(|m| m.title.to_lowercase().contains(&q))
                .collect()
        };

        match self.sort_mode {
            SortMode::Title => list.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
            SortMode::Unread => list.sort_by(|a, b| b.unread.cmp(&a.unread).then(a.title.cmp(&b.title))),
            SortMode::Status => list.sort_by(|a, b| {
                let sa = a.status.sort_rank();
                let sb = b.status.sort_rank();
                sa.cmp(&sb).then(a.title.cmp(&b.title))
            }),
        }

        list
    }

    // -----------------------------------------------------------------------
    // Event handler
    // -----------------------------------------------------------------------

    pub async fn handle_event(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Key(key) => self.handle_key(key).await?,
            AppEvent::Tick     => {
                self.poll_images();
                // Periodically re-check disk for newly downloaded covers
                // (background preload runs async, covers appear over time).
                // Every ~2 seconds (8 ticks × 250ms).
                self.cover_tick += 1;
                if self.cover_tick >= 8 {
                    self.cover_tick = 0;
                    self.cover_cache.reload_from_disk(&self.manhwa_list);
                    // Invalidate protocols for covers that just became available
                    // so they get recreated with the actual image data.
                    let stale_ids: Vec<i64> = self.cover_protocols.keys().copied().collect();
                    for id in stale_ids {
                        if self.cover_cache.get(id).is_some() {
                            // Protocol exists and image exists — keep it
                        } else {
                            // Protocol was for a placeholder — remove so it gets recreated
                            self.cover_protocols.remove(&id);
                        }
                    }
                    // Same sweep for Discover covers.
                    if !self.discoveries.is_empty() {
                        self.discover_cover_cache.reload_from_disk_ids(
                            self.discoveries.iter().map(|d| (d.id, d.cover_url.as_deref())),
                        );
                        let stale: Vec<i64> = self.discover_cover_protocols.keys().copied().collect();
                        for id in stale {
                            if self.discover_cover_cache.get(id).is_none() {
                                self.discover_cover_protocols.remove(&id);
                            }
                        }
                    }
                    // Same sweep for Search covers.
                    if !self.add_search_results.is_empty() {
                        self.search_cover_cache.reload_from_disk_ids(
                            self.add_search_results.iter().map(|r| {
                                (search_result_id(&r.source_url), r.cover_url.as_deref())
                            }),
                        );
                        let stale: Vec<i64> = self.search_cover_protocols.keys().copied().collect();
                        for id in stale {
                            if self.search_cover_cache.get(id).is_none() {
                                self.search_cover_protocols.remove(&id);
                            }
                        }
                    }
                }
            }
            AppEvent::DataRefreshed => {
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                self.cover_cache.reload_from_disk(&self.manhwa_list);
            }
            AppEvent::ScraperMsg(ev) => self.handle_scraper_event(ev).await?,
        }
        Ok(())
    }

    /// Handle a ScraperEvent from the background coordinator.
    pub async fn handle_scraper_event(
        &mut self,
        event: crate::scraper::ScraperEvent,
    ) -> Result<()> {
        use crate::scraper::ScraperEvent;
        match event {
            ScraperEvent::NewChapters { titles } => {
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                if !titles.is_empty() {
                    let count = titles.len();
                    let preview = titles.first().map(|s| s.as_str()).unwrap_or("");
                    if count == 1 {
                        self.set_msg(format!("New chapters: {preview}"));
                    } else {
                        self.set_msg(format!("New chapters in {count} titles"));
                    }
                }
            }
            ScraperEvent::NewDiscoveries { count } => {
                // Refresh so the Discover screen shows them immediately if
                // the user has it open, and surface a banner either way.
                self.refresh_discoveries().await?;
                self.set_msg(format!(
                    "{count} new manhwa discovered — press D to browse"
                ));
            }
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('q')
            && !self.search_active
            && self.screen == Screen::Library
        {
            self.should_quit = true;
            return Ok(());
        }
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.should_quit = true;
            return Ok(());
        }

        // Vim-style gg: first g sets pending, second g fires top()
        if self.pending_g {
            self.pending_g = false;
            if key.code == KeyCode::Char('g') {
                let top_key = KeyEvent::new(self.keys.top(), KeyModifiers::NONE);
                match self.screen.clone() {
                    Screen::Library                           => self.handle_library_key(top_key).await?,
                    Screen::Detail { manhwa_id }              => self.handle_detail_key(top_key, manhwa_id).await?,
                    _ => {}
                }
                return Ok(());
            }
            // Not g — fall through and process the actual key normally
        }
        if key.code == KeyCode::Char('g')
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !self.search_active
            && !self.add_search_input_active
            && matches!(self.screen, Screen::Library | Screen::Detail { .. })
        {
            self.pending_g = true;
            return Ok(());
        }

        match self.screen.clone() {
            Screen::Library                           => self.handle_library_key(key).await?,
            Screen::Detail { manhwa_id }              => self.handle_detail_key(key, manhwa_id).await?,
            Screen::Reader { manhwa_id, chapter_id }  => self.handle_reader_key(key, manhwa_id, chapter_id).await?,
            Screen::StatusPicker { manhwa_id }        => self.handle_status_picker_key(key, manhwa_id).await?,
            Screen::Search                            => self.handle_search_key(key).await?,
            Screen::Discover                          => self.handle_discover_key(key).await?,
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Delete
    // -----------------------------------------------------------------------

    async fn do_delete_manhwa(&mut self, manhwa_id: i64) -> Result<()> {
        db::delete_manhwa(&self.pool, manhwa_id).await?;
        // SQLite reuses freed rowids — drop the cached cover file and any
        // in-memory entry so a future series that lands on this id doesn't
        // inherit stale artwork.
        let _ = std::fs::remove_file(self.cover_cache.cache_dir().join(format!("{manhwa_id}.jpg")));
        self.cover_cache.invalidate(manhwa_id);
        self.cover_protocols.remove(&manhwa_id);
        self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
        // Keep cursor in bounds
        let max = self.manhwa_list.len().saturating_sub(1);
        self.library_sel = self.library_sel.min(max);
        self.confirm_delete_id = None;
        self.set_msg("Deleted");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Library keys
    // -----------------------------------------------------------------------

    async fn handle_library_key(&mut self, key: KeyEvent) -> Result<()> {
        let count = self.visible_manhwa().len();
        let k = key.code;

        // If awaiting delete confirmation, handle only delete (confirm) or back (cancel)
        if let Some(id) = self.confirm_delete_id {
            if k == self.keys.delete() {
                self.do_delete_manhwa(id).await?;
            } else if k == KeyCode::Esc || k == self.keys.back() {
                self.confirm_delete_id = None;
                self.set_msg("Delete cancelled");
            }
            return Ok(());
        }

        if self.search_active {
            match k {
                KeyCode::Esc | KeyCode::Enter => { self.search_active = false; }
                KeyCode::Backspace => { self.search_query.pop(); self.library_sel = 0; }
                KeyCode::Char(c)   => { self.search_query.push(c); self.library_sel = 0; }
                _ => {}
            }
            return Ok(());
        }

        let cols = self.grid_cols.max(1);
        if k == self.keys.down() || k == KeyCode::Down {
            if count > 0 {
                let next = self.library_sel + cols;
                self.library_sel = if next < count { next } else { count - 1 };
            }
        } else if k == self.keys.up() || k == KeyCode::Up {
            self.library_sel = self.library_sel.saturating_sub(cols);
        } else if k == self.keys.right() || k == KeyCode::Right {
            if count > 0 { self.library_sel = (self.library_sel + 1).min(count - 1); }
        } else if k == self.keys.left() || k == KeyCode::Left {
            self.library_sel = self.library_sel.saturating_sub(1);
        } else if k == self.keys.top() {
            self.library_sel = 0;
        } else if k == self.keys.bottom() {
            self.library_sel = count.saturating_sub(1);
        } else if k == self.keys.search() {
            self.search_active = true; self.search_query.clear();
        } else if k == self.keys.add() {
            self.add_search_query.clear();
            self.add_search_results.clear();
            self.add_search_sel = 0;
            self.add_search_input_active = true;
            self.screen = Screen::Search;
        } else if k == KeyCode::Char('D') {
            // Capital D (shift+d) opens Discover — lowercase 'd' is delete.
            self.refresh_discoveries().await?;
            self.discover_sel = 0;
            self.screen = Screen::Discover;
        } else if k == self.keys.sort() {
            self.sort_mode = self.sort_mode.next();
            self.library_sel = 0;
            self.set_msg(format!("Sort: {}", self.sort_mode.label()));
        } else if k == self.keys.delete() {
            if let Some(manhwa) = self.visible_manhwa().get(self.library_sel).copied() {
                self.confirm_delete_id = Some(manhwa.id);
            }
        } else if k == KeyCode::Esc || k == self.keys.back() {
            if !self.search_query.is_empty() {
                self.search_query.clear();
                self.library_sel = 0;
            }
        } else if k == self.keys.open() || k == KeyCode::Enter {
            if let Some(manhwa) = self.visible_manhwa().get(self.library_sel).copied() {
                self.open_detail(manhwa.id).await?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Detail keys
    // -----------------------------------------------------------------------

    async fn handle_detail_key(&mut self, key: KeyEvent, manhwa_id: i64) -> Result<()> {
        let k = key.code;
        if k == KeyCode::Esc || k == self.keys.back() {
            self.screen = Screen::Library;
            self.chapter_list.clear();
            self.current_manhwa = None;
        } else if k == self.keys.down() || k == KeyCode::Down {
            let len = self.chapter_list.len();
            if len > 0 { self.chapter_sel = (self.chapter_sel + 1).min(len - 1); }
        } else if k == self.keys.up() || k == KeyCode::Up {
            self.chapter_sel = self.chapter_sel.saturating_sub(1);
        } else if k == self.keys.top() {
            self.chapter_sel = 0;
        } else if k == self.keys.bottom() {
            self.chapter_sel = self.chapter_list.len().saturating_sub(1);
        } else if k == self.keys.set_status() {
            let current_idx = self.current_manhwa.as_ref()
                .and_then(|m| Status::all().iter().position(|s| s == &m.status))
                .unwrap_or(0);
            self.status_sel = current_idx;
            self.screen = Screen::StatusPicker { manhwa_id };
        } else if k == self.keys.mark_unread() {
            if let Some(ch) = self.chapter_list.get(self.chapter_sel) {
                let chapter_id = ch.id;
                sqlx::query(
                    "UPDATE progress SET completed_at = NULL, scrolled_pct = 0.0 WHERE chapter_id = ?",
                )
                .bind(chapter_id)
                .execute(&self.pool)
                .await?;
                db::recompute_status(&self.pool, manhwa_id).await?;
                self.refresh_detail(manhwa_id).await?;
                self.set_msg("Marked as unread");
            }
        } else if k == self.keys.clear_override() {
            if let Some(manhwa) = &self.current_manhwa {
                if manhwa.status_override {
                    db::clear_status_override(&self.pool, manhwa_id).await?;
                    db::recompute_status(&self.pool, manhwa_id).await?;
                    self.refresh_detail(manhwa_id).await?;
                    self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                    self.set_msg("Status override cleared — auto-tracking resumed");
                } else {
                    self.set_msg("No override active");
                }
            }
        } else if k == self.keys.open() || k == KeyCode::Enter {
            if let Some(ch) = self.chapter_list.get(self.chapter_sel) {
                let chapter_id = ch.id;
                self.open_reader(manhwa_id, chapter_id).await?;
            }
        } else if k == KeyCode::Char('R') {
            self.do_refresh_metadata(manhwa_id).await?;
        }
        Ok(())
    }

    /// Re-fetch series metadata from the source and persist title/cover/
    /// pub_status/description. Triggered by `R` on the detail screen.
    pub async fn do_refresh_metadata(&mut self, manhwa_id: i64) -> Result<()> {
        use crate::scraper::{AsuraScraper, MangaDexScraper, MangackScraper, Scraper};

        let (source, source_url) = match self.current_manhwa.as_ref() {
            Some(m) => (m.source.clone(), m.source_url.clone()),
            None    => return Ok(()),
        };

        self.set_msg("Refreshing metadata...");

        let scraper: Box<dyn Scraper> = match source.as_str() {
            "mangadex" => Box::new(MangaDexScraper::new()),
            "mangack"  => Box::new(MangackScraper::new()),
            "asura"    => Box::new(AsuraScraper::new(
                self.config.sources.get("asura")
                    .and_then(|s| s.scraper_dir.as_deref())
                    .unwrap_or(".").into(),
            )),
            other => {
                self.set_msg(format!("Unknown source: {other}"));
                return Ok(());
            }
        };

        let series = match scraper.get_series(&source_url).await {
            Ok(s)  => s,
            Err(e) => { self.set_msg(format!("Refresh failed: {e}")); return Ok(()); }
        };

        db::update_manhwa_metadata(&self.pool, manhwa_id, &series).await?;
        self.refresh_detail(manhwa_id).await?;
        self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;

        // Re-pull cover so any new artwork shows up — invalidate cached
        // protocol so the next render re-creates it from the fresh image.
        // Use refetch_covers (not preload) so a stale on-disk file gets
        // overwritten instead of skipped.
        self.cover_protocols.remove(&manhwa_id);
        self.cover_cache.invalidate(manhwa_id);
        tokio::spawn(crate::cover_cache::refetch_covers(
            self.cover_cache.cache_dir().clone(),
            vec![(manhwa_id, series.cover_url.clone())],
        ));

        self.set_msg("Metadata refreshed");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reader keys
    // -----------------------------------------------------------------------

    async fn handle_reader_key(&mut self, key: KeyEvent, manhwa_id: i64, chapter_id: i64) -> Result<()> {
        let k = key.code;
        if k == KeyCode::Esc || k == self.keys.back() {
            self.kill_viewer();
            self.update_read_progress(chapter_id, manhwa_id).await?;
            self.open_detail(manhwa_id).await?;
        } else if k == self.keys.next_chapter() {
            self.kill_viewer();
            self.update_read_progress(chapter_id, manhwa_id).await?;
            self.open_next_chapter(manhwa_id, chapter_id).await?;
        } else if k == self.keys.prev_chapter() {
            self.kill_viewer();
            self.update_read_progress(chapter_id, manhwa_id).await?;
            self.open_prev_chapter(manhwa_id, chapter_id).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Status picker keys
    // -----------------------------------------------------------------------

    async fn handle_status_picker_key(&mut self, key: KeyEvent, manhwa_id: i64) -> Result<()> {
        let options = Status::all();
        let k = key.code;
        if k == KeyCode::Esc || k == self.keys.back() {
            self.screen = Screen::Detail { manhwa_id };
        } else if k == self.keys.down() || k == KeyCode::Down {
            self.status_sel = (self.status_sel + 1).min(options.len() - 1);
        } else if k == self.keys.up() || k == KeyCode::Up {
            self.status_sel = self.status_sel.saturating_sub(1);
        } else if k == self.keys.open() || k == KeyCode::Enter {
            let chosen = options[self.status_sel].clone();
            if chosen == Status::UpToDate {
                db::mark_all_chapters_read(&self.pool, manhwa_id).await?;
                db::clear_status_override(&self.pool, manhwa_id).await?;
                db::recompute_status(&self.pool, manhwa_id).await?;
            } else {
                db::set_manhwa_status(&self.pool, manhwa_id, &chosen, true).await?;
            }
            self.refresh_detail(manhwa_id).await?;
            self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
            self.screen = Screen::Detail { manhwa_id };
            self.set_msg(format!("Status set to: {}", chosen.as_str()));
        }
        Ok(())
    }

    async fn handle_search_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.add_search_loading { return Ok(()); }
        let k = key.code;

        if self.add_search_input_active {
            if k == KeyCode::Esc || k == self.keys.back() {
                self.screen = Screen::Library;
                self.add_search_query.clear();
                self.add_search_results.clear();
                self.search_cover_protocols.clear();
                self.add_search_input_active = true;
            } else if k == KeyCode::Enter || k == self.keys.open() {
                self.search_all_scrapers().await?;
            } else if k == KeyCode::Backspace {
                self.add_search_query.pop();
            } else if let KeyCode::Char(c) = k {
                self.add_search_query.push(c);
            }
        } else {
            // Browsing results grid
            let count = self.add_search_results.len();
            let cols  = self.add_search_grid_cols.max(1);
            if k == KeyCode::Esc || k == self.keys.back() || k == self.keys.input_mode() {
                self.add_search_input_active = true;
            } else if k == self.keys.down() || k == KeyCode::Down {
                if count > 0 {
                    let next = self.add_search_sel + cols;
                    self.add_search_sel = if next < count { next } else { count - 1 };
                }
            } else if k == self.keys.up() || k == KeyCode::Up {
                self.add_search_sel = self.add_search_sel.saturating_sub(cols);
            } else if k == self.keys.right() || k == KeyCode::Right {
                if count > 0 { self.add_search_sel = (self.add_search_sel + 1).min(count - 1); }
            } else if k == self.keys.left() || k == KeyCode::Left {
                self.add_search_sel = self.add_search_sel.saturating_sub(1);
            } else if k == self.keys.top() {
                self.add_search_sel = 0;
            } else if k == self.keys.bottom() {
                self.add_search_sel = count.saturating_sub(1);
            } else if k == KeyCode::Enter || k == self.keys.open() {
                self.do_add_manhwa().await?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Discover keys
    // -----------------------------------------------------------------------

    async fn handle_discover_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.discover_adding { return Ok(()); }
        let k = key.code;
        let count = self.discoveries.len();
        let cols = self.discover_grid_cols.max(1);

        if k == KeyCode::Esc || k == self.keys.back() {
            self.screen = Screen::Library;
            self.discover_error = None;
        } else if k == self.keys.down() || k == KeyCode::Down {
            if count > 0 {
                let next = self.discover_sel + cols;
                self.discover_sel = if next < count { next } else { count - 1 };
            }
        } else if k == self.keys.up() || k == KeyCode::Up {
            self.discover_sel = self.discover_sel.saturating_sub(cols);
        } else if k == self.keys.right() || k == KeyCode::Right {
            if count > 0 { self.discover_sel = (self.discover_sel + 1).min(count - 1); }
        } else if k == self.keys.left() || k == KeyCode::Left {
            self.discover_sel = self.discover_sel.saturating_sub(1);
        } else if k == self.keys.top() {
            self.discover_sel = 0;
        } else if k == self.keys.bottom() {
            self.discover_sel = count.saturating_sub(1);
        } else if k == KeyCode::Char('a') || k == KeyCode::Enter || k == self.keys.open() {
            if count > 0 {
                self.do_add_discovery().await?;
            }
        } else if k == KeyCode::Char('x') {
            if count > 0 {
                self.do_dismiss_discovery().await?;
            }
        }
        Ok(())
    }

    /// Fan out search() to both scrapers concurrently, merge results.
    /// Updates add_search_results and clears add_search_loading on completion.
    pub async fn search_all_scrapers(&mut self) -> Result<()> {
        use crate::scraper::{AsuraScraper, MangaDexScraper, MangackScraper, Scraper};
        let query = self.add_search_query.clone();
        if query.trim().is_empty() {
            self.add_search_results.clear();
            return Ok(());
        }

        self.add_search_loading = true;
        self.add_search_error   = None;
        self.add_search_results.clear();

        let mdx   = MangaDexScraper::new();
        let mck   = MangackScraper::new();
        let asura = AsuraScraper::new(self.config.sources.get("asura").and_then(|s| s.scraper_dir.as_deref()).unwrap_or(".").into());

        // Run all searches concurrently; surface errors as status, not crash
        let (mdx_res, mck_res, asura_res) = tokio::join!(
            mdx.search(&query),
            mck.search(&query),
            asura.search(&query),
        );

        let mut merged = Vec::new();
        let mut errors = Vec::new();
        match mdx_res {
            Ok(r)  => merged.extend(r),
            Err(e) => errors.push(format!("MangaDex: {e}")),
        }
        match mck_res {
            Ok(r)  => merged.extend(r),
            Err(e) => errors.push(format!("MangaCK: {e}")),
        }
        match asura_res {
            Ok(r)  => merged.extend(r),
            Err(e) => errors.push(format!("Asura: {e}")),
        }
        if !errors.is_empty() {
            self.add_search_error = Some(errors.join("  "));
        }

        self.add_search_results = merged;
        self.add_search_sel     = 0;
        self.add_search_loading = false;
        self.add_search_input_active = false;  // move focus to results grid
        self.search_cover_protocols.clear();

        // Kick off cover preload for all results so the grid fills in covers.
        let preload: Vec<(i64, Option<String>)> = self.add_search_results.iter()
            .map(|r| (search_result_id(&r.source_url), r.cover_url.clone()))
            .collect();
        if !preload.is_empty() {
            tokio::spawn(crate::cover_cache::preload_covers(
                self.search_cover_cache.cache_dir().clone(),
                preload,
            ));
        }
        Ok(())
    }

    /// Fetch full series data for the selected search result and insert into DB.
    pub async fn do_add_manhwa(&mut self) -> Result<()> {
        use crate::scraper::{AsuraScraper, MangaDexScraper, MangackScraper, Scraper};
        use crate::db;

        let result = match self.add_search_results.get(self.add_search_sel) {
            Some(r) => r.clone(),
            None    => {
                self.add_search_error = Some(
                    "No result selected. Run a search and pick a result before pressing Enter."
                        .into(),
                );
                return Ok(());
            }
        };

        self.add_search_loading = true;
        self.add_search_error   = None;

        let scraper: Box<dyn Scraper> = match result.source.as_str() {
            "mangadex" => Box::new(MangaDexScraper::new()),
            "mangack"  => Box::new(MangackScraper::new()),
            "asura"    => Box::new(AsuraScraper::new(self.config.sources.get("asura").and_then(|s| s.scraper_dir.as_deref()).unwrap_or(".").into())),
            other      => {
                self.add_search_error = Some(format!("Unknown source: {other}"));
                self.add_search_loading = false;
                return Ok(());
            }
        };

        let series = match scraper.get_series(&result.source_url).await {
            Ok(s)  => s,
            Err(e) => {
                self.add_search_error   = Some(format!("Fetch failed: {e}"));
                self.add_search_loading = false;
                return Ok(());
            }
        };

        match db::insert_manhwa_with_chapters(&self.pool, &series, &result.source).await {
            Ok(new_id) => {
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                // Kick off cover download for the newly added entry so it
                // shows up on the library grid without requiring a restart.
                tokio::spawn(crate::cover_cache::preload_covers(
                    self.cover_cache.cache_dir().clone(),
                    vec![(new_id, series.cover_url.clone())],
                ));
                self.add_search_loading = false;
                self.set_msg(format!("Added: {}", series.title));
                // Reset search state and return to library
                self.add_search_query   = String::new();
                self.add_search_results = Vec::new();
                self.add_search_sel     = 0;
                self.add_search_input_active = true;
                self.search_cover_protocols.clear();
                self.screen = crate::types::Screen::Library;
            }
            Err(e) => {
                self.add_search_error   = Some(e.to_string());
                self.add_search_loading = false;
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    pub async fn open_detail(&mut self, manhwa_id: i64) -> Result<()> {
        self.current_manhwa = Some(db::fetch_manhwa(&self.pool, manhwa_id).await?);
        self.chapter_list   = db::fetch_chapters(&self.pool, manhwa_id).await?;
        self.chapter_sel    = self.first_unread_idx();
        self.screen         = Screen::Detail { manhwa_id };
        Ok(())
    }

    async fn refresh_detail(&mut self, manhwa_id: i64) -> Result<()> {
        self.current_manhwa = Some(db::fetch_manhwa(&self.pool, manhwa_id).await?);
        self.chapter_list   = db::fetch_chapters(&self.pool, manhwa_id).await?;
        Ok(())
    }

    pub async fn open_reader(&mut self, manhwa_id: i64, chapter_id: i64) -> Result<()> {
        db::start_chapter(&self.pool, chapter_id).await?;
        self.current_chapter  = self.chapter_list.iter().find(|c| c.id == chapter_id).cloned();
        self.image_paths          = Vec::new();
        self.image_pending        = Vec::new();
        self.next_expected_image  = 0;
        self.images_loading       = true;
        self.viewer_process       = None;
        self.viewer_pid           = None;
        self.viewer_socket        = None;
        self.viewer_loaded_count  = 0;
        self.viewer_drop_warned   = false;
        self.screen           = Screen::Reader { manhwa_id, chapter_id };

        if let Some(ch) = &self.current_chapter {
            let source = self.current_manhwa.as_ref()
                .map(|m| m.source.clone()).unwrap_or_default();
            let url = ch.url.clone();
            let (tx, rx) = mpsc::channel::<Option<(usize, Option<std::path::PathBuf>)>>(32);
            let (err_tx, err_rx) = mpsc::channel::<String>(4);
            self.image_rx  = Some(rx);
            self.error_rx  = Some(err_rx);

            // Create a unique session directory for this chapter's images
            let session_dir = tempfile::Builder::new()
                .prefix("mrm_")
                .tempdir()
                .ok();
            let session_path = session_dir.as_ref()
                .map(|d| d.path().to_path_buf())
                .unwrap_or_else(std::env::temp_dir);
            self.session_dir = session_dir;

            let asura_dir: std::path::PathBuf = self.config.sources.get("asura")
                .and_then(|s| s.scraper_dir.as_deref())
                .unwrap_or(".").into();
            tokio::spawn(async move {
                fetch_chapter_images(&source, &url, session_path, tx, err_tx, asura_dir).await;
            });
        }
        Ok(())
    }

    async fn open_next_chapter(&mut self, manhwa_id: i64, chapter_id: i64) -> Result<()> {
        let next = self.chapter_list.iter()
            .skip_while(|c| c.id != chapter_id)
            .nth(1).map(|c| c.id);
        if let Some(id) = next {
            self.open_reader(manhwa_id, id).await?;
        } else {
            self.set_msg("No next chapter");
            self.open_detail(manhwa_id).await?;
        }
        Ok(())
    }

    async fn open_prev_chapter(&mut self, manhwa_id: i64, chapter_id: i64) -> Result<()> {
        let prev = self.chapter_list.iter()
            .position(|c| c.id == chapter_id)
            .and_then(|i| i.checked_sub(1))
            .map(|i| self.chapter_list[i].id);
        if let Some(id) = prev {
            self.open_reader(manhwa_id, id).await?;
        } else {
            self.set_msg("No previous chapter");
            self.open_detail(manhwa_id).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Image polling (called on Tick)
    // -----------------------------------------------------------------------

    pub fn poll_images(&mut self) {
        // If viewer was closed by the user, stop loading
        if let Some(managed) = &mut self.viewer_process {
            if let Ok(Some(_)) = managed.0.try_wait() {
                self.viewer_process  = None;
                self.viewer_pid      = None;
                self.viewer_socket   = None;
                self.images_loading  = false;
                self.image_rx        = None;
                self.error_rx        = None;
                if let Some(h) = self.host_fullscreen.take() {
                    restore_host_fullscreen(&h);
                }
                return;
            }
        }

        if !self.images_loading { return; }

        // Drain error channel — route errors to TUI status bar
        if let Some(err_rx) = &mut self.error_rx {
            while let Ok(msg) = err_rx.try_recv() {
                self.status_msg = Some(msg);
            }
        }

        let mut done = false;
        if let Some(rx) = &mut self.image_rx {
            loop {
                match rx.try_recv() {
                    Ok(Some((idx, path))) => {
                        self.image_pending.push((idx, path));
                    }
                    Ok(None) => { done = true; break; }
                    Err(mpsc::error::TryRecvError::Empty)        => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => { done = true; break; }
                }
            }
        }

        // Sort pending and flush in-order. Failed pages (None) advance
        // next_expected_image without being pushed, so later pages aren't blocked.
        self.image_pending.sort_by_key(|(i, _)| *i);
        while self.image_pending.first().map(|(i, _)| *i) == Some(self.next_expected_image) {
            let (_, maybe_path) = self.image_pending.remove(0);
            self.next_expected_image += 1;
            if let Some(path) = maybe_path {
                self.image_paths.push(path);
            }
        }

        let had_viewer = self.viewer_process.is_some();

        if !had_viewer && !self.image_paths.is_empty() {
            self.launch_viewer();
        } else if had_viewer {
            // Feed new images to running viewer
            let total = self.image_paths.len();
            for i in self.viewer_loaded_count..total {
                let p = self.image_paths[i].to_string_lossy().into_owned();
                self.send_viewer_open(&p);
            }
            self.viewer_loaded_count = total;
        }

        if done {
            self.images_loading = false;
            self.image_rx       = None;
            self.error_rx       = None;
        }
    }

    fn launch_viewer(&mut self) {
        use crate::config::ViewerKind;
        // Snapshot host window's fullscreen state so we can restore it after
        // the viewer (which steals fullscreen on Hyprland) exits.
        self.host_fullscreen = snapshot_host_fullscreen();
        match self.viewer_kind {
            ViewerKind::Imv => self.launch_imv(),
            ViewerKind::Rv  => self.launch_rv(),
        }
    }

    fn launch_imv(&mut self) {
        // Write a temporary imv config to a fake XDG_CONFIG_HOME so imv picks
        // it up instead of the user's global config.
        // PID-scoped path so concurrent mrm instances don't stomp on each other's config.
        let tmp_xdg = std::env::temp_dir().join(format!("mrm_imv_xdg_{}", std::process::id()));
        let tmp_imv_dir = tmp_xdg.join("imv");
        let _ = std::fs::create_dir_all(&tmp_imv_dir);
        let _ = std::fs::write(tmp_imv_dir.join("config"), self.imv_config.to_config_string());

        let mut args: Vec<String> = Vec::new();
        for path in &self.image_paths {
            args.push(path.to_string_lossy().into_owned());
        }
        let bin = &self.imv_config.binary;
        if !binary_exists(bin) {
            self.set_msg(format!("{bin} not found in PATH"));
            return;
        }
        let log = std::fs::File::create("/tmp/mrm-imv.log").ok();
        let stderr = log.map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null);
        match std::process::Command::new(bin)
            .env("XDG_CONFIG_HOME", &tmp_xdg)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                self.viewer_pid          = Some(pid);
                self.viewer_loaded_count = self.image_paths.len();
                self.viewer_process      = Some(ManagedChild(child));
            }
            Err(e) => self.set_msg(format!("{bin} error: {e}")),
        }
    }

    fn launch_rv(&mut self) {
        let mut args = self.rv_config.to_args();
        for path in &self.image_paths {
            args.push(path.to_string_lossy().into_owned());
        }
        let bin = self.rv_config.binary.clone();
        if !binary_exists(&bin) {
            self.set_msg(format!("{bin} not found in PATH"));
            return;
        }
        let log = std::fs::File::create("/tmp/mrm-rv.log").ok();
        let stderr = log.map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null);
        match std::process::Command::new(&bin)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(stderr)
            .spawn()
        {
            Ok(mut child) => {
                // rv prints the socket path to stdout on startup.
                // Read with timeout on a worker thread so a silent rv can't hang the TUI.
                let stdout = child.stdout.take();
                let socket_path = stdout.and_then(|s| {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        let mut reader = std::io::BufReader::new(s);
                        let mut line = String::new();
                        let _ = reader.read_line(&mut line);
                        let _ = tx.send(line.trim().to_string());
                    });
                    rx.recv_timeout(std::time::Duration::from_secs(3))
                        .ok()
                        .filter(|s| !s.is_empty())
                });
                if socket_path.is_none() {
                    self.set_msg(format!("{bin}: no socket path (see /tmp/mrm-rv.log)"));
                }
                let pid = child.id();
                self.viewer_pid          = Some(pid);
                self.viewer_socket       = socket_path;
                self.viewer_loaded_count = self.image_paths.len();
                self.viewer_process      = Some(ManagedChild(child));
            }
            Err(e) => self.set_msg(format!("{bin} error: {e}")),
        }
    }

    fn send_viewer_open(&mut self, path: &str) {
        use crate::config::ViewerKind;
        match self.viewer_kind {
            ViewerKind::Imv => {
                if let Some(pid) = self.viewer_pid {
                    let _ = std::process::Command::new("imv-msg")
                        .args([&pid.to_string(), "open", path])
                        .status();
                }
            }
            ViewerKind::Rv => {
                if let Some(sock) = &self.viewer_socket {
                    let status = std::process::Command::new("rv-msg")
                        .args([sock, "open", path])
                        .status();
                    let failed = !matches!(status, Ok(s) if s.success());
                    if failed && !self.viewer_drop_warned {
                        self.viewer_drop_warned = true;
                        self.set_msg("rv-msg failed; later pages not streamed");
                    }
                } else if !self.viewer_drop_warned {
                    self.viewer_drop_warned = true;
                    self.set_msg("rv socket unknown; later pages not streamed");
                }
            }
        }
    }

    fn kill_viewer(&mut self) {
        self.viewer_process      = None;  // Drop kills viewer
        self.session_dir         = None;  // Drop + TempDir deletes /tmp/mrm_*/
        self.viewer_pid          = None;
        self.viewer_socket       = None;
        self.viewer_loaded_count = 0;
        self.viewer_drop_warned  = false;
        if let Some(h) = self.host_fullscreen.take() {
            restore_host_fullscreen(&h);
        }
        self.update_read_progress_sync();
    }

    // -----------------------------------------------------------------------
    // Progress tracking
    // -----------------------------------------------------------------------

    fn update_read_progress_sync(&mut self) {
        // Mark as read if we had images (fire-and-forget via tokio)
        // Called from sync context (kill_viewer), so we note it for next async tick.
        // Progress is updated properly via update_read_progress in async context.
    }

    async fn update_read_progress(&mut self, chapter_id: i64, manhwa_id: i64) -> Result<()> {
        let pct = if self.image_paths.is_empty() { 0.0 } else { 1.0 };
        let just_completed = db::update_scroll(&self.pool, chapter_id, pct).await?;
        if just_completed {
            db::recompute_status(&self.pool, manhwa_id).await?;
            self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
            self.refresh_detail(manhwa_id).await?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn first_unread_idx(&self) -> usize {
        self.chapter_list.iter().position(|c| !c.completed).unwrap_or(0)
    }

    pub fn set_msg(&mut self, msg: impl Into<String>) { self.status_msg = Some(msg.into()); }
    pub fn clear_msg(&mut self)                       { self.status_msg = None; }

    /// Get or create a StatefulProtocol for a manhwa's cover image.
    pub fn get_cover_protocol(&mut self, manhwa_id: i64) -> Option<&mut StatefulProtocol> {
        if self.cover_protocols.contains_key(&manhwa_id) {
            return self.cover_protocols.get_mut(&manhwa_id);
        }
        let img = self.cover_cache.get(manhwa_id)?.clone();
        let picker = self.picker.as_mut()?;
        let protocol = picker.new_resize_protocol(img);
        self.cover_protocols.insert(manhwa_id, protocol);
        self.cover_protocols.get_mut(&manhwa_id)
    }

    /// Same as get_cover_protocol but for the Discover cache namespace.
    pub fn get_discover_cover_protocol(&mut self, discover_id: i64) -> Option<&mut StatefulProtocol> {
        if self.discover_cover_protocols.contains_key(&discover_id) {
            return self.discover_cover_protocols.get_mut(&discover_id);
        }
        let img = self.discover_cover_cache.get(discover_id)?.clone();
        let picker = self.picker.as_mut()?;
        let protocol = picker.new_resize_protocol(img);
        self.discover_cover_protocols.insert(discover_id, protocol);
        self.discover_cover_protocols.get_mut(&discover_id)
    }

    /// Same as get_cover_protocol but for the Search cache namespace.
    pub fn get_search_cover_protocol(&mut self, result_id: i64) -> Option<&mut StatefulProtocol> {
        if self.search_cover_protocols.contains_key(&result_id) {
            return self.search_cover_protocols.get_mut(&result_id);
        }
        let img = self.search_cover_cache.get(result_id)?.clone();
        let picker = self.picker.as_mut()?;
        let protocol = picker.new_resize_protocol(img);
        self.search_cover_protocols.insert(result_id, protocol);
        self.search_cover_protocols.get_mut(&result_id)
    }

    // -----------------------------------------------------------------------
    // Discover
    // -----------------------------------------------------------------------

    /// Load undismissed discoveries from the DB and spawn a background preload
    /// for any covers not yet on disk. Called when opening the Discover screen
    /// and whenever the coordinator signals new finds.
    pub async fn refresh_discoveries(&mut self) -> Result<()> {
        self.discoveries = db::fetch_discoveries(&self.pool).await?;
        let list: Vec<(i64, Option<String>)> = self
            .discoveries
            .iter()
            .map(|d| (d.id, d.cover_url.clone()))
            .collect();
        tokio::spawn(crate::cover_cache::preload_covers(
            self.discover_cover_cache.cache_dir().clone(),
            list,
        ));
        if self.discover_sel >= self.discoveries.len() {
            self.discover_sel = self.discoveries.len().saturating_sub(1);
        }
        Ok(())
    }

    /// Fetch the selected discovery's full series data and insert into the
    /// library. Removes the entry from discovered_manhwa on success and spawns
    /// a cover preload for the library copy.
    pub async fn do_add_discovery(&mut self) -> Result<()> {
        use crate::scraper::{AsuraScraper, MangaDexScraper, MangackScraper, Scraper};
        use crate::db;

        let discovery = match self.discoveries.get(self.discover_sel) {
            Some(d) => d.clone(),
            None    => return Ok(()),
        };

        self.discover_adding = true;
        self.discover_error  = None;

        let scraper: Box<dyn Scraper> = match discovery.source.as_str() {
            "mangadex" => Box::new(MangaDexScraper::new()),
            "mangack"  => Box::new(MangackScraper::new()),
            "asura"    => Box::new(AsuraScraper::new(
                self.config.sources.get("asura")
                    .and_then(|s| s.scraper_dir.as_deref())
                    .unwrap_or(".").into(),
            )),
            other => {
                self.discover_error = Some(format!("Unknown source: {other}"));
                self.discover_adding = false;
                return Ok(());
            }
        };

        let series = match scraper.get_series(&discovery.source_url).await {
            Ok(s)  => s,
            Err(e) => {
                self.discover_error = Some(format!("Fetch failed: {e}"));
                self.discover_adding = false;
                return Ok(());
            }
        };

        match db::insert_manhwa_with_chapters(&self.pool, &series, &discovery.source).await {
            Ok(new_id) => {
                let _ = db::delete_discovery(&self.pool, discovery.id).await;
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                tokio::spawn(crate::cover_cache::preload_covers(
                    self.cover_cache.cache_dir().clone(),
                    vec![(new_id, series.cover_url.clone())],
                ));
                self.refresh_discoveries().await?;
                self.set_msg(format!("Added: {}", series.title));
                self.discover_adding = false;
            }
            Err(e) => {
                self.discover_error   = Some(e.to_string());
                self.discover_adding  = false;
            }
        }
        Ok(())
    }

    /// Dismiss the selected discovery (soft-delete so it won't resurface on
    /// future discovery polls).
    pub async fn do_dismiss_discovery(&mut self) -> Result<()> {
        let id = match self.discoveries.get(self.discover_sel) {
            Some(d) => d.id,
            None    => return Ok(()),
        };
        db::dismiss_discovery(&self.pool, id).await?;
        self.discover_cover_protocols.remove(&id);
        self.discover_cover_cache.invalidate(id);
        self.refresh_discoveries().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Image fetching
// ---------------------------------------------------------------------------

async fn fetch_chapter_images(
    source: &str,
    chapter_url: &str,
    session_dir: std::path::PathBuf,
    tx: mpsc::Sender<Option<(usize, Option<std::path::PathBuf>)>>,
    err_tx: mpsc::Sender<String>,
    asura_scraper_dir: std::path::PathBuf,
) {
    use crate::scraper::{AsuraScraper, MangaDexScraper, MangackScraper, Scraper};
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    // Build a Box<dyn Scraper> from source name
    let scraper: Box<dyn Scraper> = match source {
        "mangadex" => Box::new(MangaDexScraper::new()),
        "mangack"  => Box::new(MangackScraper::new()),
        "asura"    => Box::new(AsuraScraper::new(asura_scraper_dir)),
        other => {
            let _ = err_tx.send(format!("Unknown source: {other}")).await;
            let _ = tx.send(None).await;
            return;
        }
    };

    // Fetch image URLs (with the scraper's built-in retry)
    let urls = match scraper.get_chapter_image_urls(chapter_url).await {
        Ok(u) => u,
        Err(e) => {
            let _ = err_tx.send(format!("Could not load chapter: {e}")).await;
            let _ = tx.send(None).await;
            return;
        }
    };

    let total = urls.len();
    if total == 0 {
        let _ = err_tx.send("Chapter has no images".into()).await;
        let _ = tx.send(None).await;
        return;
    }

    let mut client_builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30));

    // AsuraScans' CDN rejects non-browser requests and hotlinked fetches.
    // Send a realistic Chrome UA plus a Referer so the CDN treats us like
    // an in-page image request.
    let referer: Option<&'static str> = if source == "asura" {
        client_builder = client_builder.user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        );
        Some("https://asurascans.com/")
    } else {
        client_builder = client_builder
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36");
        None
    };

    let client = Arc::new(client_builder.build().expect("reqwest client"));
    let sem = Arc::new(Semaphore::new(4));

    // Each task returns (index, Option<PathBuf>) — None = this index failed.
    // Reporting the index on failure lets poll_images skip it instead of stalling.
    let mut set: JoinSet<(usize, Option<std::path::PathBuf>)> = JoinSet::new();

    for (page, url) in urls.into_iter().enumerate() {
        let client      = client.clone();
        let sem         = sem.clone();
        let session_dir = session_dir.clone();
        set.spawn(async move {
            let result = async {
                let _permit = sem.acquire_owned().await.ok()?;
                let mut req = client.get(&url);
                if let Some(r) = referer {
                    req = req.header(reqwest::header::REFERER, r);
                }
                let bytes = req.send().await.ok()?.bytes().await.ok()?;
                let img   = image::load_from_memory(&bytes).ok()?;
                let path  = session_dir.join(format!("page_{page}.png"));
                img.save(&path).ok()?;
                Some(path)
            }.await;
            (page, result)
        });
    }

    // Collect results as they complete (streaming, not batch)
    let mut failures = 0usize;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((page, Some(path))) => {
                let _ = tx.send(Some((page, Some(path)))).await;
            }
            Ok((page, None)) => {
                failures += 1;
                // Report failed index so poll_images can advance past it
                let _ = tx.send(Some((page, None))).await;
            }
            Err(_) => { failures += 1; }
        }
    }

    if failures > 0 {
        let _ = err_tx.send(
            format!("{failures}/{total} pages failed to load")
        ).await;
    }

    let _ = tx.send(None).await;
}
