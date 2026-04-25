use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    LookedInto,
    Reading,
    UpToDate,
    Paused,
    Completed,
    Dropped,
}

impl Status {
    pub fn from_str(s: &str) -> Self {
        match s {
            "reading"      => Self::Reading,
            "up_to_date"   => Self::UpToDate,
            "paused"       => Self::Paused,
            "completed"    => Self::Completed,
            "dropped"      => Self::Dropped,
            _              => Self::LookedInto,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LookedInto => "looked_into",
            Self::Reading    => "reading",
            Self::UpToDate   => "up_to_date",
            Self::Paused     => "paused",
            Self::Completed  => "completed",
            Self::Dropped    => "dropped",
        }
    }

    /// Short label shown in the UI
    pub fn label(&self, _unread: u32) -> String {
        match self {
            Self::LookedInto => "👀 Looked into".into(),
            Self::Reading    => "📖 Reading".into(),
            Self::UpToDate   => "✅ Up to date".into(),
            Self::Paused     => "⏸  Paused".into(),
            Self::Completed  => "🏁 Completed".into(),
            Self::Dropped    => "🗑  Dropped".into(),
        }
    }

    /// Label used when there are unread chapters after being up-to-date
    pub fn unread_label(unread: u32) -> String {
        format!("🔔 {} unread", unread)
    }

    /// Sort rank: Reading first, then UpToDate, etc.
    pub fn sort_rank(&self) -> u8 {
        match self {
            Self::Reading    => 0,
            Self::UpToDate   => 1,
            Self::LookedInto => 2,
            Self::Paused     => 3,
            Self::Completed  => 4,
            Self::Dropped    => 5,
        }
    }

    pub fn all() -> &'static [Status] {
        &[
            Status::LookedInto,
            Status::Reading,
            Status::UpToDate,
            Status::Paused,
            Status::Completed,
            Status::Dropped,
        ]
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label(0))
    }
}

// ---------------------------------------------------------------------------
// Manhwa
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Sort mode for library view
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortMode {
    Title,
    Unread,
    Status,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Title  => Self::Unread,
            Self::Unread => Self::Status,
            Self::Status => Self::Title,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Title  => "Title",
            Self::Unread => "Unread",
            Self::Status => "Status",
        }
    }
}

// ---------------------------------------------------------------------------
// Manhwa
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Manhwa {
    pub id:          i64,
    pub title:       String,
    pub cover_url:   Option<String>,
    pub source:      String,
    pub source_url:  String,
    pub pub_status:  String,   // "ongoing" | "hiatus" | "completed"
    pub status:      Status,
    pub status_override: bool,
    pub unread:      u32,      // computed, not stored
}

impl Manhwa {
    /// The label to display in the library list
    pub fn status_display(&self) -> String {
        if self.unread > 0
            && (self.status == Status::UpToDate || self.status == Status::Reading)
        {
            Status::unread_label(self.unread)
        } else {
            self.status.label(self.unread)
        }
    }
}

// ---------------------------------------------------------------------------
// Chapter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Chapter {
    pub id:          i64,
    #[allow(dead_code)]
    pub manhwa_id:   i64,
    pub number:      f64,
    pub title:       Option<String>,
    pub url:         String,
    pub released_at: Option<String>,
    pub completed:   bool,     // true if scrolled to bottom
    pub scroll_pct:  f64,      // 0.0–1.0
}

impl Chapter {
    pub fn display_title(&self) -> String {
        match &self.title {
            Some(t) if !t.is_empty() => t.clone(),
            _ => format!("Chapter {}", self.number),
        }
    }

    pub fn status_icon(&self) -> &'static str {
        if self.completed {
            "✓"
        } else if self.scroll_pct > 0.0 {
            "▶"   // in progress
        } else {
            "○"   // unread
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery (newly detected manhwa not yet in library)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Discovery {
    pub id:             i64,
    pub source:         String,
    pub source_url:     String,
    pub title:          String,
    pub cover_url:      Option<String>,
    pub chapter_number: Option<f64>,
    #[allow(dead_code)]
    pub released_at:    Option<String>,
    #[allow(dead_code)]
    pub first_seen_at:  Option<String>,
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Library,
    Detail { manhwa_id: i64 },
    Reader  { manhwa_id: i64, chapter_id: i64 },
    StatusPicker { manhwa_id: i64 },
    Search,
    Discover,
}

// ---------------------------------------------------------------------------
// App-level events
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AppEvent {
    Key(crossterm::event::KeyEvent),
    Tick,
    #[allow(dead_code)]
    DataRefreshed,
    ScraperMsg(crate::scraper::ScraperEvent),
}
