use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use sqlx::postgres::PgPool;
use tempfile::TempDir;
use std::sync::Arc;

use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

use crate::cover_cache::CoverCache;
use crate::scraper::{Registry, SearchResult, SeriesData};
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
    pub pool:          PgPool,
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
    /// imv config dir (private tempdir), alive for the viewer session.
    pub imv_config_dir:      Option<TempDir>,
    /// rv's socket path, delivered by its stdout-draining thread.
    rv_socket_rx:            Option<std::sync::mpsc::Receiver<String>>,
    /// Queue to the worker that runs imv-msg/rv-msg off the UI task.
    viewer_send_tx:          Option<mpsc::UnboundedSender<Vec<String>>>,
    /// Failures reported by that worker.
    viewer_send_err_rx:      Option<mpsc::UnboundedReceiver<String>>,
    pub(crate) host_fullscreen: Option<HostFullscreen>,
    pub session_dir:     Option<TempDir>,
    pub error_rx:        Option<mpsc::Receiver<String>>,
    /// Cancels the running `fetch_chapter_images` task (drops its downloads).
    fetch_cancel:        Option<tokio_util::sync::CancellationToken>,
    /// Spawning the viewer failed for this chapter; do not retry every tick.
    viewer_launch_failed: bool,
    /// A viewer was successfully launched for the current chapter.
    viewer_launched:     bool,
    /// The viewer child exited on its own; the Reader screen should close.
    viewer_exited:       bool,
    /// The fetch task reported completion (all pages resolved).
    loading_finished:    bool,
    pages_ok:            usize,
    pages_failed:        usize,

    pub status_msg: Option<String>,
    /// When `status_msg` was last set; expiry is measured from here.
    status_msg_at: Option<Instant>,

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
    /// Identifies the in-flight search so cancelled/stale results are dropped.
    pub search_job:              SearchJob,
    /// The spawned search task, aborted on Esc.
    search_task:                 Option<JoinHandle<()>>,

    /// Manhwa id whose Detail refresh (`R`) is in flight.
    pub refreshing: Option<i64>,

    /// The process's scraper registry (enabled sources only), shared with
    /// spawned tasks and the coordinator.
    pub registry: Arc<Registry>,
    /// Wakes the coordinator to poll immediately (Library `poll_now` key).
    pub poll_now: Arc<Notify>,
    /// Spawned network tasks report back through here into `handle_event`.
    app_tx: mpsc::UnboundedSender<AppEvent>,

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

    // Help overlay: `?` toggles, any key dismisses
    pub show_help: bool,
}

impl App {
    pub async fn new(
        pool: PgPool,
        picker: Option<Picker>,
        config: crate::config::Config,
        app_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Result<Self> {
        let manhwa_list = db::fetch_all_manhwa(&pool).await?;
        let registry = Arc::new(crate::scraper::build_registry(&config));

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
            imv_config_dir:      None,
            rv_socket_rx:        None,
            viewer_send_tx:      None,
            viewer_send_err_rx:  None,
            host_fullscreen:     None,
            session_dir:      None,
            error_rx:         None,
            fetch_cancel:     None,
            viewer_launch_failed: false,
            viewer_launched:  false,
            viewer_exited:    false,
            loading_finished: false,
            pages_ok:         0,
            pages_failed:     0,
            status_msg:       None,
            status_msg_at:    None,
            add_search_query:        String::new(),
            add_search_results:      Vec::new(),
            add_search_grid_cols:    1,
            search_cover_cache:      CoverCache::with_subdir(Some("search")),
            search_cover_protocols:  HashMap::new(),
            add_search_sel:          0,
            add_search_loading:      false,
            add_search_error:        None,
            add_search_input_active: true,
            search_job:              SearchJob::default(),
            search_task:             None,
            refreshing:              None,
            registry,
            poll_now:                Arc::new(Notify::new()),
            app_tx,
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
            show_help: false,
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
                if self.viewer_exited {
                    self.viewer_exited = false;
                    if let Screen::Reader { manhwa_id, chapter_id } = self.screen {
                        // Viewer closed on its own: save progress, back to Detail.
                        self.save_reader_progress_nonfatal(chapter_id, manhwa_id).await;
                        self.kill_viewer();
                        self.open_detail(manhwa_id).await?;
                    } else {
                        self.viewer_exited = false;
                    }
                }
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
            AppEvent::SearchDone { seq, results, errors } => {
                self.on_search_done(seq, results, errors);
            }
            AppEvent::AddDone { source, result } => self.on_add_done(source, result).await?,
            AppEvent::RefreshDone { manhwa_id, result } => {
                self.on_refresh_done(manhwa_id, result).await?;
            }
            AppEvent::DiscoveryAddDone { discovery_id, source, result } => {
                self.on_discovery_add_done(discovery_id, source, result).await?;
            }
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
        // Help overlay: `?` toggles, any key dismisses.
        if self.show_help {
            self.show_help = false;
            return Ok(());
        }
        if key.code == KeyCode::Char('?')
            && !self.search_active
            && !self.add_search_input_active
            && !matches!(self.screen, Screen::Reader { .. })
        {
            self.show_help = true;
            return Ok(());
        }

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
            if let Screen::Reader { manhwa_id, chapter_id } = self.screen {
                self.save_reader_progress_nonfatal(chapter_id, manhwa_id).await;
                self.kill_viewer();
            }
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
        // Ids may be reused — drop the cached cover file and any
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
        } else if k == self.keys.poll_now() {
            self.poll_now.notify_one();
            self.set_msg("Checking sources for new chapters...");
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
                    "UPDATE progress SET completed_at = NULL, scrolled_pct = 0.0 WHERE chapter_id = $1",
                )
                .bind(chapter_id)
                .execute(&self.pool)
                .await?;
                db::recompute_status(&self.pool, manhwa_id).await?;
                self.refresh_detail(manhwa_id).await?;
                self.set_msg("Marked as unread");
            }
        } else if k == self.keys.mark_read() {
            if let Some(ch) = self.chapter_list.get(self.chapter_sel) {
                let chapter_id = ch.id;
                db::start_chapter(&self.pool, chapter_id).await?;
                db::update_scroll(&self.pool, chapter_id, 1.0).await?;
                db::recompute_status(&self.pool, manhwa_id).await?;
                self.refresh_detail(manhwa_id).await?;
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                self.set_msg("Marked as read");
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
            self.do_refresh_metadata(manhwa_id);
        }
        Ok(())
    }

    /// Start a re-fetch of the series from its source (`R` on the detail
    /// screen). The result arrives as `AppEvent::RefreshDone`.
    pub fn do_refresh_metadata(&mut self, manhwa_id: i64) {
        if self.refreshing.is_some() {
            self.set_msg("Refresh already running");
            return;
        }
        let (source, source_url) = match self.current_manhwa.as_ref() {
            Some(m) => (m.source.clone(), m.source_url.clone()),
            None    => return,
        };
        if !self.registry.contains_key(source.as_str()) {
            self.set_msg(source_unavailable(&source));
            return;
        }
        self.refreshing = Some(manhwa_id);
        self.spawn_series_fetch(source, source_url, move |result| {
            AppEvent::RefreshDone { manhwa_id, result }
        });
    }

    /// Persist a refresh: new chapters through the poll write path (quiet,
    /// never claims), then title/cover/pub_status/description.
    async fn on_refresh_done(
        &mut self,
        manhwa_id: i64,
        result: std::result::Result<SeriesData, String>,
    ) -> Result<()> {
        if self.refreshing == Some(manhwa_id) {
            self.refreshing = None;
        }
        let series = match result {
            Ok(s)  => s,
            Err(e) => { self.set_msg(format!("Refresh failed: {e}")); return Ok(()); }
        };
        // Fresh pub_status from the DB: a poll may have changed it meanwhile,
        // and the series may have been deleted.
        let old_pub_status = match db::fetch_manhwa(&self.pool, manhwa_id).await {
            Ok(m)  => m.pub_status,
            Err(_) => { self.set_msg("Refresh skipped: series no longer in library"); return Ok(()); }
        };
        let new_chapters = db::apply_poll_result(&self.pool, manhwa_id, &series, &old_pub_status).await?;
        db::update_manhwa_metadata(&self.pool, manhwa_id, &series).await?;
        if self.screen == (Screen::Detail { manhwa_id }) {
            self.refresh_detail(manhwa_id).await?;
        }
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

        self.set_msg(match new_chapters {
            0 => "Refreshed — no new chapters".to_string(),
            n => format!("Refreshed — {n} new chapter(s)"),
        });
        Ok(())
    }

    /// Fetch `source_url` with the registry's scraper on a spawned task and
    /// send `done(result)` back to the event loop. Callers check that the
    /// source is in the registry first, so the UI can say so synchronously.
    fn spawn_series_fetch<F>(&self, source: String, source_url: String, done: F)
    where
        F: FnOnce(std::result::Result<SeriesData, String>) -> AppEvent + Send + 'static,
    {
        let registry = self.registry.clone();
        let tx = self.app_tx.clone();
        tokio::spawn(async move {
            let result = match registry.get(source.as_str()) {
                Some(scraper) => scraper.get_series(&source_url).await.map_err(|e| e.to_string()),
                None          => Err(source_unavailable(&source)),
            };
            let _ = tx.send(done(result));
        });
    }

    // -----------------------------------------------------------------------
    // Reader keys
    // -----------------------------------------------------------------------

    async fn handle_reader_key(&mut self, key: KeyEvent, manhwa_id: i64, chapter_id: i64) -> Result<()> {
        let k = key.code;
        if k == KeyCode::Esc || k == self.keys.back() {
            self.save_reader_progress_nonfatal(chapter_id, manhwa_id).await;
            self.kill_viewer();
            self.open_detail(manhwa_id).await?;
        } else if k == self.keys.next_chapter() {
            self.save_reader_progress_nonfatal(chapter_id, manhwa_id).await;
            self.kill_viewer();
            self.open_next_chapter(manhwa_id, chapter_id).await?;
        } else if k == self.keys.prev_chapter() {
            self.save_reader_progress_nonfatal(chapter_id, manhwa_id).await;
            self.kill_viewer();
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
        let k = key.code;
        if self.add_search_loading {
            // Only a running search can be cancelled; an add is a write and
            // finishes on its own.
            if (k == KeyCode::Esc || k == self.keys.back()) && self.cancel_search() {
                self.set_msg("Search cancelled");
            }
            return Ok(());
        }

        if self.add_search_input_active {
            if k == KeyCode::Esc || k == self.keys.back() {
                self.screen = Screen::Library;
                self.add_search_query.clear();
                self.add_search_results.clear();
                self.search_cover_protocols.clear();
                self.add_search_input_active = true;
            } else if k == KeyCode::Enter || k == self.keys.open() {
                self.start_search();
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
                self.do_add_manhwa();
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
                self.do_add_discovery();
            }
        } else if k == KeyCode::Char('x') {
            if count > 0 {
                self.do_dismiss_discovery().await?;
            }
        }
        Ok(())
    }

    /// Start a search across every enabled source on a spawned task. The
    /// result arrives as `AppEvent::SearchDone`; Esc cancels it meanwhile.
    pub fn start_search(&mut self) {
        let query = self.add_search_query.clone();
        if query.trim().is_empty() {
            self.add_search_results.clear();
            return;
        }
        self.cancel_search();
        let seq = self.search_job.start();
        self.add_search_loading = true;
        self.add_search_error   = None;
        self.add_search_results.clear();
        self.search_cover_protocols.clear();

        let registry = self.registry.clone();
        let tx = self.app_tx.clone();
        self.search_task = Some(tokio::spawn(async move {
            let (results, errors) = search_registry(&registry, &query).await;
            let _ = tx.send(AppEvent::SearchDone { seq, results, errors });
        }));
    }

    /// Abort the running search, if any. Returns whether one was running.
    fn cancel_search(&mut self) -> bool {
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
        let was_running = self.search_job.cancel();
        if was_running {
            self.add_search_loading = false;
        }
        was_running
    }

    fn on_search_done(&mut self, seq: u64, results: Vec<SearchResult>, errors: Vec<String>) {
        if !self.search_job.finish(seq) {
            return; // cancelled or superseded
        }
        self.search_task = None;
        self.add_search_error = if errors.is_empty() { None } else { Some(errors.join("  ")) };
        self.add_search_results = results;
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
    }

    /// Start fetching the selected search result's full series; the insert
    /// happens in `on_add_done`.
    pub fn do_add_manhwa(&mut self) {
        let result = match self.add_search_results.get(self.add_search_sel) {
            Some(r) => r.clone(),
            None    => {
                self.add_search_error = Some(
                    "No result selected. Run a search and pick a result before pressing Enter."
                        .into(),
                );
                return;
            }
        };
        if !self.registry.contains_key(result.source.as_str()) {
            self.add_search_error = Some(source_unavailable(&result.source));
            return;
        }

        self.add_search_loading = true;
        self.add_search_error   = None;
        let source = result.source.clone();
        self.spawn_series_fetch(result.source, result.source_url, move |result| {
            AppEvent::AddDone { source, result }
        });
    }

    async fn on_add_done(
        &mut self,
        source: String,
        result: std::result::Result<SeriesData, String>,
    ) -> Result<()> {
        self.add_search_loading = false;
        let series = match result {
            Ok(s)  => s,
            Err(e) => {
                self.add_search_error = Some(format!("Fetch failed: {e}"));
                return Ok(());
            }
        };

        match db::insert_manhwa_with_chapters(&self.pool, &series, &source).await {
            Ok(new_id) => {
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                // Kick off cover download for the newly added entry so it
                // shows up on the library grid without requiring a restart.
                tokio::spawn(crate::cover_cache::preload_covers(
                    self.cover_cache.cache_dir().clone(),
                    vec![(new_id, series.cover_url.clone())],
                ));
                self.set_msg(format!("Added: {}", series.title));
                // Reset search state and return to library (only if the user
                // is still on the Search screen).
                if self.screen == Screen::Search {
                    self.add_search_query   = String::new();
                    self.add_search_results = Vec::new();
                    self.add_search_sel     = 0;
                    self.add_search_input_active = true;
                    self.search_cover_protocols.clear();
                    self.screen = Screen::Library;
                }
            }
            Err(e) => {
                self.add_search_error = Some(e.to_string());
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
        self.reset_reader_state();
        self.images_loading       = true;
        self.screen           = Screen::Reader { manhwa_id, chapter_id };

        if let Some(ch) = &self.current_chapter {
            let source = self.current_manhwa.as_ref()
                .map(|m| m.source.clone()).unwrap_or_default();
            let registry = self.registry.clone();
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

            let cancel = tokio_util::sync::CancellationToken::new();
            self.fetch_cancel = Some(cancel.clone());
            tokio::spawn(async move {
                // Dropping the fetch future on cancel aborts its JoinSet.
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = fetch_chapter_images(&registry, &source, &url, session_path, tx, err_tx) => {}
                }
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
                self.rv_socket_rx    = None;
                self.viewer_send_tx  = None;
                self.imv_config_dir  = None;
                self.images_loading  = false;
                self.image_rx        = None;
                self.error_rx        = None;
                self.viewer_exited   = true;
                if let Some(c) = self.fetch_cancel.take() { c.cancel(); }
                if let Some(h) = self.host_fullscreen.take() {
                    restore_host_fullscreen(&h);
                }
                return;
            }
        }

        // Failures from the imv-msg/rv-msg worker.
        let mut send_errs = Vec::new();
        if let Some(rx) = &mut self.viewer_send_err_rx {
            while let Ok(m) = rx.try_recv() { send_errs.push(m); }
        }
        if let Some(m) = send_errs.into_iter().next() {
            if !self.viewer_drop_warned {
                self.viewer_drop_warned = true;
                self.set_msg(m);
            }
        }

        // Keep ticking after loading ends while pages still wait for the
        // viewer (e.g. rv's socket path not known yet).
        if self.viewer_exited { return; }
        if !self.images_loading
            && (self.viewer_launch_failed || self.viewer_loaded_count >= self.image_paths.len())
        {
            return;
        }

        // rv socket path, if the reader thread has produced it. Never blocks.
        if self.viewer_socket.is_none() {
            let mut gone = false;
            if let Some(rx) = &self.rv_socket_rx {
                match rx.try_recv() {
                    Ok(path) if !path.is_empty() => {
                        self.viewer_socket = Some(path);
                        gone = true;
                    }
                    Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        gone = true;
                        if !self.viewer_drop_warned {
                            self.viewer_drop_warned = true;
                            self.set_msg("rv gave no socket path; later pages not streamed");
                        }
                        // Treat everything as sent so the tick can go idle.
                        self.viewer_loaded_count = usize::MAX;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            if gone { self.rv_socket_rx = None; }
        }

        // Drain error channel — route errors to TUI status bar
        if let Some(err_rx) = &mut self.error_rx {
            let mut msgs = Vec::new();
            while let Ok(msg) = err_rx.try_recv() {
                msgs.push(msg);
            }
            for msg in msgs {
                self.set_msg(msg);
            }
        }

        let mut done = false;
        if let Some(rx) = &mut self.image_rx {
            loop {
                match rx.try_recv() {
                    Ok(Some((idx, path))) => {
                        if path.is_some() { self.pages_ok += 1; } else { self.pages_failed += 1; }
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

        if !had_viewer && !self.image_paths.is_empty() && !self.viewer_launch_failed {
            self.launch_viewer();
            if self.viewer_process.is_some() {
                self.viewer_launched = true;
            } else {
                // launch_* already set the one error message; don't retry per tick.
                self.viewer_launch_failed = true;
            }
        } else if had_viewer {
            // Feed new images to the running viewer, one batch per tick.
            // rv pages wait (stay queued) until its socket path is known.
            let ready = match self.viewer_kind {
                crate::config::ViewerKind::Imv => self.viewer_pid.is_some(),
                crate::config::ViewerKind::Rv  => self.viewer_socket.is_some(),
            };
            let total = self.image_paths.len();
            if ready && self.viewer_loaded_count < total {
                let batch: Vec<String> = self.image_paths[self.viewer_loaded_count..total]
                    .iter().map(|p| p.to_string_lossy().into_owned()).collect();
                self.viewer_loaded_count = total;
                self.send_viewer_open(batch);
            }
        }

        if done {
            self.loading_finished = true;
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
        // it up instead of the user's global config. The directory is a
        // private (0700) tempdir held for the viewer session.
        let cfg_dir = match tempfile::Builder::new().prefix("mrm_imv_").tempdir() {
            Ok(d) => d,
            Err(e) => { self.set_msg(format!("imv config dir: {e}")); return; }
        };
        let imv_dir = cfg_dir.path().join("imv");
        if let Err(e) = std::fs::create_dir(&imv_dir)
            .and_then(|_| std::fs::write(imv_dir.join("config"), self.imv_config.to_config_string()))
        {
            self.set_msg(format!("imv config: {e}"));
            return;
        }

        let mut args: Vec<String> = Vec::new();
        for path in &self.image_paths {
            args.push(path.to_string_lossy().into_owned());
        }
        let bin = &self.imv_config.binary;
        if !binary_exists(bin) {
            self.set_msg(format!("{bin} not found in PATH"));
            return;
        }
        let log = open_viewer_log("imv.log");
        let stderr = log.map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null);
        match std::process::Command::new(bin)
            .env("XDG_CONFIG_HOME", cfg_dir.path())
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                self.imv_config_dir      = Some(cfg_dir);
                self.viewer_pid          = Some(pid);
                self.viewer_loaded_count = self.image_paths.len();
                self.viewer_process      = Some(ManagedChild(child));
                self.start_send_worker(pid.to_string());
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
        let log = open_viewer_log("rv.log");
        let stderr = log.map(std::process::Stdio::from).unwrap_or_else(std::process::Stdio::null);
        match std::process::Command::new(&bin)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(stderr)
            .spawn()
        {
            Ok(mut child) => {
                // rv prints the socket path as its first stdout line. A helper
                // thread sends it over a channel that poll_images checks
                // without blocking, then keeps draining stdout so later
                // writes by rv never hit a closed pipe.
                if let Some(stdout) = child.stdout.take() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        use std::io::BufRead;
                        let mut lines = std::io::BufReader::new(stdout).lines();
                        if let Some(Ok(first)) = lines.next() {
                            let _ = tx.send(first.trim().to_string());
                        }
                        drop(tx);
                        for _ in lines {}
                    });
                    self.rv_socket_rx = Some(rx);
                }
                let pid = child.id();
                self.viewer_pid          = Some(pid);
                self.viewer_socket       = None;
                self.viewer_loaded_count = self.image_paths.len();
                self.viewer_process      = Some(ManagedChild(child));
            }
            Err(e) => self.set_msg(format!("{bin} error: {e}")),
        }
    }

    /// Spawn the worker that runs `imv-msg`/`rv-msg` for this viewer session.
    /// `target` is imv's pid or rv's socket path. Batches arrive over a
    /// channel and run sequentially (keeping page order) off the UI task.
    fn start_send_worker(&mut self, target: String) {
        use crate::config::ViewerKind;
        let prog = match self.viewer_kind {
            ViewerKind::Imv => "imv-msg",
            ViewerKind::Rv  => "rv-msg",
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<String>>();
        let (err_tx, err_rx) = mpsc::unbounded_channel::<String>();
        self.viewer_send_tx     = Some(tx);
        self.viewer_send_err_rx = Some(err_rx);
        tokio::spawn(async move {
            while let Some(batch) = rx.recv().await {
                for path in batch {
                    let status = tokio::process::Command::new(prog)
                        .args([target.as_str(), "open", path.as_str()])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .status()
                        .await;
                    if !matches!(status, Ok(s) if s.success()) {
                        let _ = err_tx.send(format!("{prog} failed; later pages may not stream"));
                    }
                }
            }
        });
    }

    fn send_viewer_open(&mut self, batch: Vec<String>) {
        use crate::config::ViewerKind;
        if self.viewer_send_tx.is_none() {
            // rv: the worker starts once the socket path is known.
            if let (ViewerKind::Rv, Some(sock)) = (self.viewer_kind, self.viewer_socket.clone()) {
                self.start_send_worker(sock);
            }
        }
        if let Some(tx) = &self.viewer_send_tx {
            let _ = tx.send(batch);
        }
    }

    /// Clear all per-chapter reader state and cancel the page download task.
    fn reset_reader_state(&mut self) {
        if let Some(c) = self.fetch_cancel.take() { c.cancel(); }
        self.images_loading       = false;
        self.image_rx             = None;
        self.error_rx             = None;
        self.image_paths          = Vec::new();
        self.image_pending        = Vec::new();
        self.next_expected_image  = 0;
        self.viewer_process       = None;  // Drop kills viewer
        self.viewer_pid           = None;
        self.viewer_socket        = None;
        self.rv_socket_rx         = None;
        self.viewer_send_tx       = None;
        self.viewer_send_err_rx   = None;
        self.imv_config_dir       = None;  // Drop deletes the private config dir
        self.viewer_loaded_count  = 0;
        self.viewer_drop_warned   = false;
        self.viewer_launch_failed = false;
        self.viewer_launched      = false;
        self.viewer_exited        = false;
        self.loading_finished     = false;
        self.pages_ok             = 0;
        self.pages_failed         = 0;
    }

    /// Tear down the viewer and the chapter's download state. Call
    /// `save_reader_progress` first: this clears what it reads.
    fn kill_viewer(&mut self) {
        self.reset_reader_state();
        self.session_dir         = None;  // Drop + TempDir deletes /tmp/mrm_*/
        if let Some(h) = self.host_fullscreen.take() {
            restore_host_fullscreen(&h);
        }
    }

    // -----------------------------------------------------------------------
    // Progress tracking
    // -----------------------------------------------------------------------

    /// Save progress; a failure becomes a status message so leaving the
    /// Reader (and the viewer teardown after it) never depends on the DB.
    async fn save_reader_progress_nonfatal(&mut self, chapter_id: i64, manhwa_id: i64) {
        if let Err(e) = self.save_reader_progress(chapter_id, manhwa_id).await {
            self.set_msg(format!("progress not saved: {e:#}"));
        }
    }

    /// Mark the chapter read only if the viewer was used and every page loaded.
    async fn save_reader_progress(&mut self, chapter_id: i64, manhwa_id: i64) -> Result<()> {
        if !should_mark_complete(
            self.viewer_launched,
            self.loading_finished,
            self.pages_ok,
            self.pages_ok + self.pages_failed,
        ) {
            return Ok(());
        }
        let just_completed = db::update_scroll(&self.pool, chapter_id, 1.0).await?;
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

    pub fn set_msg(&mut self, msg: impl Into<String>) {
        self.status_msg = Some(msg.into());
        self.status_msg_at = Some(Instant::now());
    }
    pub fn clear_msg(&mut self) {
        self.status_msg = None;
        self.status_msg_at = None;
    }
    /// Clear the status message if it has been showing for at least `STATUS_MSG_TTL`.
    pub fn expire_msg(&mut self, now: Instant) {
        if let Some(at) = self.status_msg_at {
            if msg_expired(at, now, STATUS_MSG_TTL) {
                self.clear_msg();
            }
        }
    }

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
    pub fn do_add_discovery(&mut self) {
        let discovery = match self.discoveries.get(self.discover_sel) {
            Some(d) => d.clone(),
            None    => return,
        };
        if !self.registry.contains_key(discovery.source.as_str()) {
            self.discover_error = Some(source_unavailable(&discovery.source));
            return;
        }

        self.discover_adding = true;
        self.discover_error  = None;
        let (discovery_id, source) = (discovery.id, discovery.source.clone());
        self.spawn_series_fetch(discovery.source, discovery.source_url, move |result| {
            AppEvent::DiscoveryAddDone { discovery_id, source, result }
        });
    }

    async fn on_discovery_add_done(
        &mut self,
        discovery_id: i64,
        source: String,
        result: std::result::Result<SeriesData, String>,
    ) -> Result<()> {
        self.discover_adding = false;
        let series = match result {
            Ok(s)  => s,
            Err(e) => {
                self.discover_error = Some(format!("Fetch failed: {e}"));
                return Ok(());
            }
        };

        match db::insert_manhwa_with_chapters(&self.pool, &series, &source).await {
            Ok(new_id) => {
                let _ = db::delete_discovery(&self.pool, discovery_id).await;
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                tokio::spawn(crate::cover_cache::preload_covers(
                    self.cover_cache.cache_dir().clone(),
                    vec![(new_id, series.cover_url.clone())],
                ));
                self.refresh_discoveries().await?;
                self.set_msg(format!("Added: {}", series.title));
            }
            Err(e) => {
                self.discover_error = Some(e.to_string());
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
// Spawned-task helpers
// ---------------------------------------------------------------------------

/// Status text for a source that is not in the registry.
fn source_unavailable(source: &str) -> String {
    format!("Source '{source}' is disabled or unknown (see [sources] in config.toml)")
}

/// Tracks the one in-flight search: `start` hands out a sequence number,
/// `finish` accepts only the current one, so a cancelled or superseded
/// search's late result is dropped.
#[derive(Debug, Default)]
pub struct SearchJob {
    last_seq: u64,
    current:  Option<u64>,
}

impl SearchJob {
    pub fn start(&mut self) -> u64 {
        self.last_seq += 1;
        self.current = Some(self.last_seq);
        self.last_seq
    }

    /// Returns whether a search was running.
    pub fn cancel(&mut self) -> bool {
        self.current.take().is_some()
    }

    /// Returns whether `seq` is the running search (and marks it finished).
    pub fn finish(&mut self, seq: u64) -> bool {
        if self.current == Some(seq) {
            self.current = None;
            true
        } else {
            false
        }
    }

    pub fn running(&self) -> bool {
        self.current.is_some()
    }
}

/// Search every source in the registry concurrently. Results are merged in a
/// fixed source order; failures become "Source: error" strings.
async fn search_registry(registry: &Registry, query: &str) -> (Vec<SearchResult>, Vec<String>) {
    let mut names: Vec<&'static str> = registry.keys().copied().collect();
    if names.is_empty() {
        return (Vec::new(), vec!["No sources enabled in config.toml".into()]);
    }
    names.sort_by_key(|n| (crate::scraper::source_rank(n), *n));
    let outcomes = futures::future::join_all(
        names.iter().map(|n| registry[*n].search(query)),
    ).await;

    let mut merged = Vec::new();
    let mut errors = Vec::new();
    for (name, outcome) in names.iter().zip(outcomes) {
        match outcome {
            Ok(r)  => merged.extend(r),
            Err(e) => errors.push(format!("{}: {e}", crate::scraper::source_label(name))),
        }
    }
    (merged, errors)
}

// ---------------------------------------------------------------------------
// Image fetching
// ---------------------------------------------------------------------------

async fn fetch_chapter_images(
    registry: &Registry,
    source: &str,
    chapter_url: &str,
    session_dir: std::path::PathBuf,
    tx: mpsc::Sender<Option<(usize, Option<std::path::PathBuf>)>>,
    err_tx: mpsc::Sender<String>,
) {
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let scraper = match registry.get(source) {
        Some(s) => s,
        None => {
            let _ = err_tx.send(source_unavailable(source)).await;
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
    // Every source gets the chapter URL as Referer (many CDNs hotlink-check).
    client_builder = if source == "asura" {
        client_builder.user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        )
    } else {
        client_builder.user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
    };
    let referer: Arc<str> = Arc::from(chapter_url);

    let client = Arc::new(client_builder.build().expect("reqwest client"));
    let sem = Arc::new(Semaphore::new(4));

    // Each task returns (index, Option<PathBuf>) — None = this index failed.
    // Reporting the index on failure lets poll_images skip it instead of stalling.
    let mut set: JoinSet<(usize, Option<std::path::PathBuf>)> = JoinSet::new();
    let mut task_page: std::collections::HashMap<tokio::task::Id, usize> =
        std::collections::HashMap::new();

    for (page, url) in urls.into_iter().enumerate() {
        let client      = client.clone();
        let sem         = sem.clone();
        let session_dir = session_dir.clone();
        let referer     = referer.clone();
        let handle = set.spawn(async move {
            let result = async {
                let _permit = sem.acquire_owned().await.ok()?;
                let resp = client.get(&url)
                    .header(reqwest::header::REFERER, &*referer)
                    .send().await.ok()?
                    .error_for_status().ok()?;
                let bytes = resp.bytes().await.ok()?;
                // Validation, file write and any fallback decode are blocking work.
                tokio::task::spawn_blocking(move || save_page(&bytes, &session_dir, page))
                    .await.ok()?
            }.await;
            (page, result)
        });
        task_page.insert(handle.id(), page);
    }

    // Collect results as they complete (streaming, not batch)
    let mut failures = 0usize;
    while let Some(joined) = set.join_next_with_id().await {
        match joined_page_result(joined, &task_page) {
            (page, Some(path)) => {
                let _ = tx.send(Some((page, Some(path)))).await;
            }
            (page, None) => {
                failures += 1;
                // Report failed index so poll_images can advance past it
                let _ = tx.send(Some((page, None))).await;
            }
        }
    }

    if failures > 0 {
        let _ = err_tx.send(
            format!("{failures}/{total} pages failed to load")
        ).await;
    }

    let _ = tx.send(None).await;
}

/// File extension for a downloaded page, from its magic bytes. Only formats
/// both viewers (imv, rv/SDL2_image) can open are accepted; anything else
/// (HTML error pages, unknown data) is an error.
fn page_extension(bytes: &[u8]) -> Result<&'static str> {
    use image::ImageFormat;
    match image::guess_format(bytes)? {
        ImageFormat::Jpeg => Ok("jpg"),
        ImageFormat::Png  => Ok("png"),
        ImageFormat::WebP => Ok("webp"),
        ImageFormat::Gif  => Ok("gif"),
        ImageFormat::Avif => Ok("avif"),
        other => Err(anyhow::anyhow!("unsupported page format {other:?}")),
    }
}

/// Write a page to `dir`. Original bytes are kept (no re-encode) when the
/// viewers can read the format; otherwise fall back to decoding to PNG.
/// Blocking: call from `spawn_blocking`.
fn save_page(bytes: &[u8], dir: &std::path::Path, page: usize) -> Option<std::path::PathBuf> {
    match page_extension(bytes) {
        Ok(ext) => {
            let path = dir.join(format!("page_{page}.{ext}"));
            std::fs::write(&path, bytes).ok()?;
            Some(path)
        }
        Err(_) => {
            let img = image::load_from_memory(bytes).ok()?;
            let path = dir.join(format!("page_{page}.png"));
            img.save(&path).ok()?;
            Some(path)
        }
    }
}

/// Directory for viewer logs: `$XDG_STATE_HOME/mrm`, else
/// `~/.local/state/mrm`. Empty or relative XDG values are ignored.
fn viewer_log_dir(xdg_state: Option<&str>, home: Option<&str>) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    match xdg_state.filter(|s| s.starts_with('/')) {
        Some(x) => Some(PathBuf::from(x).join("mrm")),
        None => home.filter(|h| !h.is_empty())
            .map(|h| PathBuf::from(h).join(".local/state/mrm")),
    }
}

/// Create the viewer log (0700 dir, 0600 file) and return it, or None.
fn open_viewer_log(name: &str) -> Option<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = viewer_log_dir(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok()?;
    std::fs::OpenOptions::new()
        .write(true).create(true).truncate(true).mode(0o600)
        .open(dir.join(name)).ok()
}

/// How long a status-bar message stays visible.
pub const STATUS_MSG_TTL: Duration = Duration::from_secs(2);

/// True once a message set at `set_at` has been visible for `ttl` as of `now`.
pub fn msg_expired(set_at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(set_at) >= ttl
}

#[cfg(test)]
mod msg_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn expiry_is_relative_to_when_set() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(2);
        assert!(!msg_expired(t0, t0 + Duration::from_millis(1999), ttl));
        assert!(msg_expired(t0, t0 + ttl, ttl));
        // A message set later is not expired by an earlier message's age.
        let t1 = t0 + Duration::from_millis(1900);
        assert!(!msg_expired(t1, t0 + Duration::from_millis(2100), ttl));
    }

    #[test]
    fn now_before_set_is_not_expired() {
        let t0 = Instant::now();
        assert!(!msg_expired(t0 + Duration::from_secs(1), t0, Duration::from_secs(2)));
    }
}

/// A chapter counts as read only when the user closed the viewer and every
/// page loaded: a mistaken open, a skipped chapter or a partly failed
/// download must not complete it.
pub fn should_mark_complete(
    user_closed: bool,
    loading_done: bool,
    pages_ok: usize,
    pages_total: usize,
) -> bool {
    user_closed && loading_done && pages_total > 0 && pages_ok == pages_total
}

#[cfg(test)]
mod complete_tests {
    use super::should_mark_complete;

    #[test]
    fn mistaken_open_then_esc_is_not_complete() {
        // Viewer never opened / still loading when closed.
        assert!(!should_mark_complete(false, true, 10, 10));
        assert!(!should_mark_complete(true, false, 1, 10));
    }

    #[test]
    fn partial_failures_are_not_complete() {
        assert!(!should_mark_complete(true, true, 20, 40));
    }

    #[test]
    fn all_loaded_and_closed_is_complete() {
        assert!(should_mark_complete(true, true, 40, 40));
    }

    #[test]
    fn zero_pages_is_not_complete() {
        assert!(!should_mark_complete(true, true, 0, 0));
    }
}

/// Flatten a joined page task into `(page, result)`. A panicked or cancelled
/// task is reported as a failed page so the received count stays equal to
/// the real page count.
fn joined_page_result(
    joined: Result<(tokio::task::Id, (usize, Option<std::path::PathBuf>)), tokio::task::JoinError>,
    task_page: &std::collections::HashMap<tokio::task::Id, usize>,
) -> (usize, Option<std::path::PathBuf>) {
    match joined {
        Ok((_, r)) => r,
        Err(e) => (task_page.get(&e.id()).copied().unwrap_or(usize::MAX), None),
    }
}

#[cfg(test)]
mod join_tests {
    use super::joined_page_result;
    use std::collections::HashMap;
    use tokio::task::JoinSet;

    #[tokio::test]
    async fn panicked_page_is_reported_as_failed_page() {
        let mut set: JoinSet<(usize, Option<std::path::PathBuf>)> = JoinSet::new();
        let mut map = HashMap::new();
        let h = set.spawn(async { (0usize, Some(std::path::PathBuf::from("a"))) });
        map.insert(h.id(), 0usize);
        let h = set.spawn(async { panic!("boom") });
        map.insert(h.id(), 1usize);
        let mut got = Vec::new();
        while let Some(j) = set.join_next_with_id().await {
            got.push(joined_page_result(j, &map));
        }
        got.sort_by_key(|(p, _)| *p);
        assert_eq!(got.len(), 2);
        assert!(got[0].1.is_some());
        assert_eq!(got[1], (1, None));
    }
}

#[cfg(test)]
mod page_ext_tests {
    use super::{page_extension, viewer_log_dir};
    use std::path::PathBuf;

    #[test]
    fn detects_formats_by_magic_bytes() {
        assert_eq!(page_extension(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F']).unwrap(), "jpg");
        assert_eq!(page_extension(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap(), "png");
        assert_eq!(page_extension(b"RIFF\x24\0\0\0WEBPVP8 ").unwrap(), "webp");
        assert_eq!(page_extension(b"GIF89a\x01\0\x01\0").unwrap(), "gif");
    }

    #[test]
    fn unknown_or_html_is_an_error() {
        assert!(page_extension(b"<html><body>403</body></html>").is_err());
        assert!(page_extension(b"").is_err());
    }

    #[test]
    fn log_dir_prefers_xdg_state_home() {
        assert_eq!(
            viewer_log_dir(Some("/x/state"), Some("/home/u")),
            Some(PathBuf::from("/x/state/mrm")),
        );
    }

    #[test]
    fn log_dir_falls_back_to_home_local_state() {
        assert_eq!(
            viewer_log_dir(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/mrm")),
        );
        // Empty and relative XDG values are ignored per the XDG spec.
        assert_eq!(
            viewer_log_dir(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/mrm")),
        );
        assert_eq!(
            viewer_log_dir(Some("rel/path"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/mrm")),
        );
    }

    #[test]
    fn log_dir_none_without_any_base() {
        assert_eq!(viewer_log_dir(None, None), None);
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::db::tests::{series, test_pool};
    use crate::scraper::{Scraper, SearchResult, SeriesData};
    use async_trait::async_trait;

    // --- SearchJob: start / done / cancel transitions -----------------------

    #[test]
    fn finished_search_is_accepted_once() {
        let mut j = SearchJob::default();
        let s = j.start();
        assert!(j.running());
        assert!(j.finish(s));
        assert!(!j.running());
        assert!(!j.finish(s), "a result must not be applied twice");
    }

    #[test]
    fn cancelled_search_result_is_dropped() {
        let mut j = SearchJob::default();
        let s = j.start();
        assert!(j.cancel());
        assert!(!j.running());
        assert!(!j.finish(s));
        assert!(!j.cancel(), "nothing left to cancel");
    }

    #[test]
    fn superseded_search_result_is_dropped() {
        let mut j = SearchJob::default();
        let old = j.start();
        let new = j.start();
        assert!(!j.finish(old));
        assert!(j.running());
        assert!(j.finish(new));
    }

    // --- search_registry ----------------------------------------------------

    /// Answers search with one result titled after itself, or fails.
    struct Fake { name: &'static str, fail: bool }

    #[async_trait]
    impl Scraper for Fake {
        fn source_name(&self) -> &'static str { self.name }
        async fn search(&self, q: &str) -> Result<Vec<SearchResult>> {
            if self.fail { anyhow::bail!("down"); }
            Ok(vec![SearchResult {
                title: format!("{} {q}", self.name),
                cover_url: None,
                source_url: format!("https://{}/{q}", self.name),
                pub_status: "ongoing".into(),
                source: self.name.into(),
            }])
        }
        async fn get_series(&self, url: &str) -> Result<SeriesData> {
            if self.fail { anyhow::bail!("down"); }
            Ok(series(3, url))
        }
        async fn get_chapter_image_urls(&self, _: &str) -> Result<Vec<String>> { Ok(vec![]) }
    }

    fn registry(entries: &[(&'static str, bool)]) -> Registry {
        let mut r = Registry::new();
        for &(name, fail) in entries {
            r.insert(name, Box::new(Fake { name, fail }));
        }
        r
    }

    #[tokio::test]
    async fn search_merges_in_source_order_and_labels_errors() {
        let reg = registry(&[("asura", false), ("mangack", true), ("mangadex", false)]);
        let (results, errors) = search_registry(&reg, "q").await;
        let titles: Vec<_> = results.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["mangadex q", "asura q"]);
        assert_eq!(errors, vec!["MangaCK: down".to_string()]);
    }

    #[tokio::test]
    async fn search_with_no_sources_says_so() {
        let (results, errors) = search_registry(&Registry::new(), "q").await;
        assert!(results.is_empty());
        assert_eq!(errors.len(), 1);
    }

    // --- App event flow (needs the test database) ---------------------------

    async fn test_app(reg: Registry) -> Option<(App, mpsc::UnboundedReceiver<AppEvent>)> {
        let pool = test_pool().await?;
        let config = crate::config::parse_config(
            "[notifications]\nenabled = false\npoll_interval_minutes = 30\n[sources]\n",
        ).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut app = App::new(pool, None, config, tx).await.unwrap();
        app.registry = Arc::new(reg);
        Some((app, rx))
    }

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[tokio::test]
    async fn esc_cancels_a_running_search_and_its_late_result_is_ignored() {
        let Some((mut app, _rx)) = test_app(registry(&[("mangadex", false)])).await else { return };
        app.screen = Screen::Search;
        app.add_search_query = "solo".into();
        app.start_search();
        assert!(app.add_search_loading, "loading flag must be set before the result arrives");
        let seq = app.search_job.last_seq;

        app.handle_event(key(KeyCode::Esc)).await.unwrap();
        assert!(!app.add_search_loading);
        assert!(!app.search_job.running());
        assert_eq!(app.screen, Screen::Search);

        app.handle_event(AppEvent::SearchDone {
            seq, results: vec![], errors: vec!["late".into()],
        }).await.unwrap();
        assert!(app.add_search_error.is_none(), "cancelled search result was applied");
        assert!(app.add_search_input_active);
    }

    #[tokio::test]
    async fn search_result_arrives_as_an_event() {
        let Some((mut app, mut rx)) = test_app(registry(&[("mangadex", false)])).await else { return };
        app.screen = Screen::Search;
        app.add_search_query = "solo".into();
        app.start_search();
        let ev = rx.recv().await.unwrap();
        app.handle_event(ev).await.unwrap();
        assert!(!app.add_search_loading);
        assert_eq!(app.add_search_results.len(), 1);
        assert!(!app.add_search_input_active);
    }

    #[tokio::test]
    async fn disabled_source_is_never_fetched_for_add() {
        let Some((mut app, _rx)) = test_app(Registry::new()).await else { return };
        app.screen = Screen::Search;
        app.add_search_results = vec![SearchResult {
            title: "t".into(), cover_url: None, source_url: "https://x/t".into(),
            pub_status: "ongoing".into(), source: "asura".into(),
        }];
        app.do_add_manhwa();
        assert!(!app.add_search_loading);
        assert!(app.add_search_error.as_deref().unwrap_or("").contains("disabled"));
    }

    #[tokio::test]
    async fn refresh_applies_new_chapters_without_claiming() {
        let Some((mut app, mut rx)) = test_app(registry(&[("mangadex", false)])).await else { return };
        let url = "https://x/refresh";
        let id = db::insert_manhwa_with_chapters(&app.pool, &series(1, url), "mangadex").await.unwrap();
        app.open_detail(id).await.unwrap();
        assert_eq!(app.chapter_list.len(), 1);

        app.handle_event(key(KeyCode::Char('R'))).await.unwrap();
        assert_eq!(app.refreshing, Some(id), "refresh flag must be set before the result arrives");
        let ev = rx.recv().await.unwrap();
        app.handle_event(ev).await.unwrap();

        assert_eq!(app.refreshing, None);
        assert_eq!(app.chapter_list.len(), 3, "fetched chapters were not applied");
        let unclaimed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chapter WHERE manhwa_id = $1 AND notified_at IS NULL",
        ).bind(id).fetch_one(&app.pool).await.unwrap();
        assert_eq!(unclaimed, 2, "refresh must leave new chapters for the notifying poller");
    }

    #[tokio::test]
    async fn poll_now_key_wakes_the_coordinator() {
        let Some((mut app, _rx)) = test_app(Registry::new()).await else { return };
        let poll_now = app.poll_now.clone();
        app.handle_event(key(KeyCode::Char('p'))).await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), poll_now.notified())
            .await
            .expect("poll_now key did not notify");
    }
}
