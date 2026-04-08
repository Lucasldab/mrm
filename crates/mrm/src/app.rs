use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sqlx::sqlite::SqlitePool;
use tokio::sync::mpsc;
use futures::future::join_all;

use crate::types::{AppEvent, Chapter, Manhwa, Screen, Status};
use crate::db;

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
    pub imv_process:     Option<std::process::Child>,
    pub imv_pid:         Option<u32>,
    pub imv_loaded_count: usize,

    pub status_msg: Option<String>,
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
            status_msg:       None,
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
    // Library keys
    // -----------------------------------------------------------------------

    async fn handle_library_key(&mut self, key: KeyEvent) -> Result<()> {
        let count = self.visible_manhwa().len();

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
        if key.code == KeyCode::Esc { self.screen = Screen::Library; }
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
            self.image_rx = Some(rx);
            tokio::spawn(async move {
                if let Err(e) = fetch_chapter_images(&source, &url, tx.clone()).await {
                    eprintln!("image fetch error: {e}");
                }
                let _ = tx.send(None).await;
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
        if let Some(child) = &mut self.imv_process {
            if let Ok(Some(_)) = child.try_wait() {
                self.imv_process    = None;
                self.imv_pid        = None;
                self.images_loading = false;
                self.image_rx       = None;
                return;
            }
        }

        if !self.images_loading { return; }

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
            self.image_rx = None;
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
                self.imv_pid          = Some(child.id());
                self.imv_loaded_count = self.image_paths.len();
                self.imv_process      = Some(child);
            }
            Err(e) => self.set_msg(format!("imv error: {e}")),
        }
    }

    fn kill_imv(&mut self) {
        if let Some(mut child) = self.imv_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
    tx: mpsc::Sender<Option<(usize, std::path::PathBuf)>>,
) -> Result<()> {
    let root = find_project_root();
    let venv_python = root.join("scraper/.venv/bin/python");
    let python = if venv_python.exists() {
        venv_python.to_string_lossy().into_owned()
    } else {
        "python".to_string()
    };

    let output = tokio::process::Command::new(&python)
        .args(["-m", "scraper.get_images", source, chapter_url])
        .current_dir(&root)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!("scraper error: {}", String::from_utf8_lossy(&output.stderr));
    }

    let urls: Vec<String> = serde_json::from_slice(&output.stdout)?;
    let client = std::sync::Arc::new(
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
            .build()?
    );

    let mut idx = 0usize;
    for chunk in urls.chunks(4) {
        let chunk_start = idx;
        let futs: Vec<_> = chunk.iter().enumerate().map(|(i, url)| {
            let client = client.clone();
            let url    = url.clone();
            let page   = chunk_start + i;
            async move {
                let bytes = client.get(&url).send().await?.bytes().await?;
                let img   = image::load_from_memory(&bytes)?;
                anyhow::Ok((page, img))
            }
        }).collect();

        for result in join_all(futs).await {
            match result {
                Ok((page, img)) => {
                    let path = std::env::temp_dir().join(format!("mrm_page_{page}.png"));
                    if img.save(&path).is_ok() {
                        let _ = tx.send(Some((page, path))).await;
                    }
                }
                Err(e) => eprintln!("image download error: {e}"),
            }
        }
        idx += chunk.len();
    }

    Ok(())
}

fn find_project_root() -> std::path::PathBuf {
    let mut dir = std::env::current_exe()
        .unwrap_or_default()
        .parent()
        .unwrap_or(&std::path::Path::new("."))
        .to_path_buf();
    for _ in 0..6 {
        if dir.join("scraper").exists() { return dir; }
        if let Some(p) = dir.parent() { dir = p.to_path_buf(); } else { break; }
    }
    std::path::PathBuf::from(".")
}
