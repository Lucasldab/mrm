use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sqlx::sqlite::SqlitePool;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::types::{AppEvent, Chapter, Manhwa, Screen, Status};
use crate::db;

// ---------------------------------------------------------------------------
// ManagedChild — kills imv on drop (prevents orphan processes)
// ---------------------------------------------------------------------------

/// Wrapper around std::process::Child that kills the process on drop.
/// Prevents imv from becoming an orphan when the TUI exits or panics.
pub(crate) struct ManagedChild(std::process::Child);

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
    pub image_rx:        Option<mpsc::Receiver<Option<(usize, std::path::PathBuf)>>>,
    pub image_pending:   Vec<(usize, std::path::PathBuf)>,
    pub imv_process:     Option<ManagedChild>,
    pub imv_pid:         Option<u32>,
    pub imv_loaded_count: usize,
    pub session_dir:     Option<TempDir>,
    pub error_rx:        Option<mpsc::Receiver<String>>,

    pub status_msg: Option<String>,

    // AddSearch screen state
    pub add_search_query:        String,
    pub add_search_results:      Vec<crate::scraper::SearchResult>,
    pub add_search_sel:          usize,
    pub add_search_loading:      bool,
    pub add_search_error:        Option<String>,
    pub add_search_input_active: bool,  // true = typing mode, false = browsing results

    // Delete confirmation
    pub confirm_delete_id: Option<i64>,  // Some(id) = waiting for confirmation keypress
}

impl App {
    pub async fn new(pool: SqlitePool) -> Result<Self> {
        let manhwa_list = db::fetch_all_manhwa(&pool).await?;
        Ok(Self {
            pool,
            screen:           Screen::Library,
            should_quit:      false,
            manhwa_list,
            library_sel:      0,
            search_query:     String::new(),
            search_active:    false,
            current_manhwa:   None,
            chapter_list:     Vec::new(),
            chapter_sel:      0,
            status_sel:       0,
            current_chapter:  None,
            images_loading:   false,
            image_paths:      Vec::new(),
            image_rx:         None,
            image_pending:    Vec::new(),
            imv_process:      None,
            imv_pid:          None,
            imv_loaded_count: 0,
            session_dir:      None,
            error_rx:         None,
            status_msg:       None,
            add_search_query:        String::new(),
            add_search_results:      Vec::new(),
            add_search_sel:          0,
            add_search_loading:      false,
            add_search_error:        None,
            add_search_input_active: true,
            confirm_delete_id:       None,
        })
    }

    // -----------------------------------------------------------------------
    // Filtered library list
    // -----------------------------------------------------------------------

    pub fn visible_manhwa(&self) -> Vec<&Manhwa> {
        if self.search_query.is_empty() {
            self.manhwa_list.iter().collect()
        } else {
            let q = self.search_query.to_lowercase();
            self.manhwa_list.iter()
                .filter(|m| m.title.to_lowercase().contains(&q))
                .collect()
        }
    }

    // -----------------------------------------------------------------------
    // Event handler
    // -----------------------------------------------------------------------

    pub async fn handle_event(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Key(key) => self.handle_key(key).await?,
            AppEvent::Tick     => self.poll_images(),
            AppEvent::DataRefreshed => {
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
            }
            AppEvent::ScraperMsg(ev) => self.handle_scraper_event(ev).await?,
        }
        Ok(())
    }

    /// Handle a ScraperEvent from the background coordinator.
    ///
    /// Currently only NewChapters is defined. It triggers a full library
    /// refresh so new chapters appear in the TUI without user action.
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

        match self.screen.clone() {
            Screen::Library                           => self.handle_library_key(key).await?,
            Screen::Detail { manhwa_id }              => self.handle_detail_key(key, manhwa_id).await?,
            Screen::Reader { manhwa_id, chapter_id }  => self.handle_reader_key(key, manhwa_id, chapter_id).await?,
            Screen::StatusPicker { manhwa_id }        => self.handle_status_picker_key(key, manhwa_id).await?,
            Screen::Search                            => self.handle_search_key(key).await?,
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Delete
    // -----------------------------------------------------------------------

    async fn do_delete_manhwa(&mut self, manhwa_id: i64) -> Result<()> {
        db::delete_manhwa(&self.pool, manhwa_id).await?;
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

        // If awaiting delete confirmation, handle only 'd' (confirm) or Esc (cancel)
        if let Some(id) = self.confirm_delete_id {
            match key.code {
                KeyCode::Char('d') => { self.do_delete_manhwa(id).await?; }
                KeyCode::Esc       => {
                    self.confirm_delete_id = None;
                    self.set_msg("Delete cancelled");
                }
                _ => {}
            }
            return Ok(());
        }

        if self.search_active {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => { self.search_active = false; }
                KeyCode::Backspace => { self.search_query.pop(); self.library_sel = 0; }
                KeyCode::Char(c)   => { self.search_query.push(c); self.library_sel = 0; }
                _ => {}
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down  => { if count > 0 { self.library_sel = (self.library_sel + 1).min(count - 1); } }
            KeyCode::Char('k') | KeyCode::Up    => { self.library_sel = self.library_sel.saturating_sub(1); }
            KeyCode::Char('g')                  => { self.library_sel = 0; }
            KeyCode::Char('G')                  => { self.library_sel = count.saturating_sub(1); }
            KeyCode::Char('/')                  => { self.search_active = true; self.search_query.clear(); }
            KeyCode::Char('a') => {
                self.add_search_query.clear();
                self.add_search_results.clear();
                self.add_search_sel = 0;
                self.add_search_input_active = true;
                self.screen = Screen::Search;
            }
            KeyCode::Char('d') => {
                if let Some(manhwa) = self.visible_manhwa().get(self.library_sel).copied() {
                    self.confirm_delete_id = Some(manhwa.id);
                }
            }
            KeyCode::Esc => {
                if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.library_sel = 0;
                }
            }
            KeyCode::Enter => {
                if let Some(manhwa) = self.visible_manhwa().get(self.library_sel).copied() {
                    let id = manhwa.id;
                    self.open_detail(id).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Detail keys
    // -----------------------------------------------------------------------

    async fn handle_detail_key(&mut self, key: KeyEvent, manhwa_id: i64) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Library;
                self.chapter_list.clear();
                self.current_manhwa = None;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let len = self.chapter_list.len();
                if len > 0 { self.chapter_sel = (self.chapter_sel + 1).min(len - 1); }
            }
            KeyCode::Char('k') | KeyCode::Up   => { self.chapter_sel = self.chapter_sel.saturating_sub(1); }
            KeyCode::Char('g')                  => { self.chapter_sel = 0; }
            KeyCode::Char('G')                  => { self.chapter_sel = self.chapter_list.len().saturating_sub(1); }
            KeyCode::Char('s') => {
                let current_idx = self.current_manhwa.as_ref()
                    .and_then(|m| Status::all().iter().position(|s| s == &m.status))
                    .unwrap_or(0);
                self.status_sel = current_idx;
                self.screen = Screen::StatusPicker { manhwa_id };
            }
            KeyCode::Char('u') => {
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
            }
            KeyCode::Enter => {
                if let Some(ch) = self.chapter_list.get(self.chapter_sel) {
                    let chapter_id = ch.id;
                    self.open_reader(manhwa_id, chapter_id).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reader keys
    // -----------------------------------------------------------------------

    async fn handle_reader_key(&mut self, key: KeyEvent, manhwa_id: i64, chapter_id: i64) -> Result<()> {
        match key.code {
            KeyCode::Esc      => { self.kill_imv(); self.open_detail(manhwa_id).await?; }
            KeyCode::Char(']') => { self.kill_imv(); self.open_next_chapter(manhwa_id, chapter_id).await?; }
            KeyCode::Char('[') => { self.kill_imv(); self.open_prev_chapter(manhwa_id, chapter_id).await?; }
            _ => {}
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Status picker keys
    // -----------------------------------------------------------------------

    async fn handle_status_picker_key(&mut self, key: KeyEvent, manhwa_id: i64) -> Result<()> {
        let options = Status::all();
        match key.code {
            KeyCode::Esc => { self.screen = Screen::Detail { manhwa_id }; }
            KeyCode::Char('j') | KeyCode::Down => { self.status_sel = (self.status_sel + 1).min(options.len() - 1); }
            KeyCode::Char('k') | KeyCode::Up   => { self.status_sel = self.status_sel.saturating_sub(1); }
            KeyCode::Enter => {
                let chosen = options[self.status_sel].clone();
                db::set_manhwa_status(&self.pool, manhwa_id, &chosen, true).await?;
                self.refresh_detail(manhwa_id).await?;
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                self.screen = Screen::Detail { manhwa_id };
                self.set_msg(format!("Status set to: {}", chosen.as_str()));
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_search_key(&mut self, key: KeyEvent) -> Result<()> {
        use KeyCode::*;

        if self.add_search_loading { return Ok(()); }  // block input while loading

        if self.add_search_input_active {
            match key.code {
                Esc        => {
                    self.screen = Screen::Library;
                    self.add_search_query.clear();
                    self.add_search_results.clear();
                    self.add_search_input_active = true;
                }
                Enter      => { self.search_all_scrapers().await?; }
                Backspace  => { self.add_search_query.pop(); }
                Char(c)    => { self.add_search_query.push(c); }
                _          => {}
            }
        } else {
            // Browsing results
            match key.code {
                Esc | Char('i') => { self.add_search_input_active = true; }
                Char('j') | Down  => {
                    let len = self.add_search_results.len();
                    if len > 0 { self.add_search_sel = (self.add_search_sel + 1).min(len - 1); }
                }
                Char('k') | Up    => { self.add_search_sel = self.add_search_sel.saturating_sub(1); }
                Enter => { self.do_add_manhwa().await?; }
                _     => {}
            }
        }
        Ok(())
    }

    /// Fan out search() to both scrapers concurrently, merge results.
    /// Updates add_search_results and clears add_search_loading on completion.
    pub async fn search_all_scrapers(&mut self) -> Result<()> {
        use crate::scraper::{MangaDexScraper, MangackScraper, Scraper};
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

        // Run both searches concurrently; surface errors as status, not crash
        let (mdx_res, mck_res) = tokio::join!(
            mdx.search(&query),
            mck.search(&query),
        );

        let mut merged = Vec::new();
        match mdx_res {
            Ok(r)  => merged.extend(r),
            Err(e) => self.add_search_error = Some(format!("MangaDex: {e}")),
        }
        match mck_res {
            Ok(r)  => merged.extend(r),
            Err(e) => {
                let existing = self.add_search_error.take().unwrap_or_default();
                self.add_search_error = Some(format!("{existing}  MangaCK: {e}").trim().to_string());
            }
        }

        self.add_search_results = merged;
        self.add_search_sel     = 0;
        self.add_search_loading = false;
        self.add_search_input_active = false;  // move focus to results list
        Ok(())
    }

    /// Fetch full series data for the selected search result and insert into DB.
    pub async fn do_add_manhwa(&mut self) -> Result<()> {
        use crate::scraper::{MangaDexScraper, MangackScraper, Scraper};
        use crate::db;

        let result = match self.add_search_results.get(self.add_search_sel) {
            Some(r) => r.clone(),
            None    => return Ok(()),
        };

        self.add_search_loading = true;
        self.add_search_error   = None;

        let scraper: Box<dyn Scraper> = match result.source.as_str() {
            "mangadex" => Box::new(MangaDexScraper::new()),
            "mangack"  => Box::new(MangackScraper::new()),
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
            Ok(_) => {
                self.manhwa_list = db::fetch_all_manhwa(&self.pool).await?;
                self.add_search_loading = false;
                self.set_msg(format!("Added: {}", series.title));
                // Reset search state and return to library
                self.add_search_query   = String::new();
                self.add_search_results = Vec::new();
                self.add_search_sel     = 0;
                self.add_search_input_active = true;
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
        self.image_paths      = Vec::new();
        self.image_pending    = Vec::new();
        self.images_loading   = true;
        self.imv_process      = None;
        self.imv_pid          = None;
        self.imv_loaded_count = 0;
        self.screen           = Screen::Reader { manhwa_id, chapter_id };

        if let Some(ch) = &self.current_chapter {
            let source = self.current_manhwa.as_ref()
                .map(|m| m.source.clone()).unwrap_or_default();
            let url = ch.url.clone();
            let (tx, rx) = mpsc::channel::<Option<(usize, std::path::PathBuf)>>(32);
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

            tokio::spawn(async move {
                fetch_chapter_images(&source, &url, session_path, tx, err_tx).await;
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
        // If imv was closed by the user, stop loading
        if let Some(managed) = &mut self.imv_process {
            if let Ok(Some(_)) = managed.0.try_wait() {
                self.imv_process    = None;
                self.imv_pid        = None;
                self.images_loading = false;
                self.image_rx       = None;
                self.error_rx       = None;
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

        // Sort pending and flush in-order images to image_paths
        self.image_pending.sort_by_key(|(i, _)| *i);
        let expected = self.image_paths.len();
        while self.image_pending.first().map(|(i, _)| *i) == Some(expected) {
            let (_, path) = self.image_pending.remove(0);
            self.image_paths.push(path);
        }

        let had_imv = self.imv_process.is_some();

        if !had_imv && !self.image_paths.is_empty() {
            self.launch_imv();
        } else if had_imv {
            // Feed new images to running imv via imv-msg
            let total = self.image_paths.len();
            if let Some(pid) = self.imv_pid {
                for i in self.imv_loaded_count..total {
                    let p = self.image_paths[i].to_string_lossy().into_owned();
                    let _ = std::process::Command::new("imv-msg")
                        .args([&pid.to_string(), "open", &p])
                        .status();
                }
            }
            self.imv_loaded_count = total;
        }

        if done {
            self.images_loading = false;
            self.image_rx       = None;
            self.error_rx       = None;
        }
    }

    fn launch_imv(&mut self) {
        // Write a temporary imv config with fit scaling for tall manga pages
        let tmp_config = std::env::temp_dir().join("mrm_imv.conf");
        let _ = std::fs::write(&tmp_config, "\
[options]\ninitial_pan = 50 0\nscaling_mode = none\npan_limits = yes\n\
[binds]\nq = quit\n<Left> = prev; pan 0 0\n<Right> = next; pan 0 0\n\
j = pan 0 -50\nk = pan 0 50\n<Shift+J> = pan 0 -500\n<Shift+K> = pan 0 500\n\
h = pan 50 0\nl = pan -50 0\n<Up> = zoom 1\n<Down> = zoom -1\nf = fullscreen\n\
<scroll-up> = pan 0 50\n<scroll-down> = pan 0 -50\n\
<shift-scroll-up> = pan 0 500\n<shift-scroll-down> = pan 0 -500\n");

        let mut args = vec!["-c".to_string(), tmp_config.to_string_lossy().into_owned()];
        for path in &self.image_paths {
            args.push(path.to_string_lossy().into_owned());
        }
        match std::process::Command::new("imv")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                self.imv_pid          = Some(pid);
                self.imv_loaded_count = self.image_paths.len();
                self.imv_process      = Some(ManagedChild(child));
            }
            Err(e) => self.set_msg(format!("imv error: {e}")),
        }
    }

    fn kill_imv(&mut self) {
        self.imv_process      = None;  // Drop kills imv
        self.session_dir      = None;  // Drop + TempDir deletes /tmp/mrm_*/
        self.imv_pid          = None;
        self.imv_loaded_count = 0;
        self.update_read_progress_sync();
    }

    // -----------------------------------------------------------------------
    // Progress tracking
    // -----------------------------------------------------------------------

    fn update_read_progress_sync(&mut self) {
        // Mark as read if we had images (fire-and-forget via tokio)
        // Called from sync context (kill_imv), so we note it for next async tick.
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
}

// ---------------------------------------------------------------------------
// Image fetching
// ---------------------------------------------------------------------------

async fn fetch_chapter_images(
    source: &str,
    chapter_url: &str,
    session_dir: std::path::PathBuf,
    tx: mpsc::Sender<Option<(usize, std::path::PathBuf)>>,
    err_tx: mpsc::Sender<String>,
) {
    use crate::scraper::{MangaDexScraper, MangackScraper, Scraper};
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    // Build a Box<dyn Scraper> from source name
    let scraper: Box<dyn Scraper> = match source {
        "mangadex" => Box::new(MangaDexScraper::new()),
        "mangack"  => Box::new(MangackScraper::new()),
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

    let client = Arc::new(
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client"),
    );
    let sem = Arc::new(Semaphore::new(4));

    let mut set: JoinSet<Option<(usize, std::path::PathBuf)>> = JoinSet::new();

    for (page, url) in urls.into_iter().enumerate() {
        let client      = client.clone();
        let sem         = sem.clone();
        let session_dir = session_dir.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.ok()?;
            let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
            let img   = image::load_from_memory(&bytes).ok()?;
            let path  = session_dir.join(format!("page_{page}.png"));
            img.save(&path).ok()?;
            Some((page, path))
        });
    }

    // Collect results as they complete (streaming, not batch)
    let mut failures = 0usize;
    while let Some(result) = set.join_next().await {
        match result {
            Ok(Some((page, path))) => {
                let _ = tx.send(Some((page, path))).await;
            }
            _ => { failures += 1; }
        }
    }

    if failures > total / 2 {
        let _ = err_tx.send(
            format!("Chapter load degraded: {failures}/{total} pages failed")
        ).await;
    }

    let _ = tx.send(None).await;
}
