//! The native-egui three-pane UI.
//!
//! The app owns all state. Each panel is a `render_*` function that reads that
//! state and a read-only view model, mutates only UI-local state (filter, search,
//! sort, theme), and returns [`Action`]s. The app applies the actions after the
//! panels have finished, so no panel needs `&mut App` in the middle of a frame.

pub mod mini;
pub mod now_playing;
pub mod player;
pub mod sidebar;
pub mod table;
pub mod theme;

use uuid::Uuid;

use crate::player::{PlaybackId, PlaybackStatus};

pub const SIDEBAR_WIDTH: f32 = 200.0;
pub const PLAYER_HEIGHT: f32 = 80.0;
pub const TABLE_HEADER_HEIGHT: f32 = 28.0;
pub const ROW_HEIGHT: f32 = 28.0;
pub const SIDEBAR_ITEM_HEIGHT: f32 = 32.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryFilter {
    All,
    Favorites,
    RecentlyPlayed,
    Watched,
    Genre(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Genre,
    Bitrate,
    Country,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    pub fn toggled(self) -> Self {
        match self {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        }
    }
}

/// UI-local state. Changing any of these does not touch the library or engine.
#[derive(Debug, Clone)]
pub struct UiState {
    pub filter: LibraryFilter,
    pub sort: SortKey,
    pub sort_ascending: bool,
    pub theme: ThemeMode,
    pub show_streams: bool,
    pub show_metadata_debug: bool,
    pub show_history: bool,
    pub show_settings: bool,
    pub show_sessions: bool,
    pub show_schedules: bool,
    pub show_wishlist: bool,
    pub show_watch_history: bool,
    pub show_about: bool,
    pub show_help: bool,
    pub search: String,
    /// Stations ticked for a bulk action.
    pub selected_stations: std::collections::HashSet<Uuid>,
    /// Last station clicked, used as the shift-click anchor.
    pub selection_anchor: Option<Uuid>,
    /// Include stations whose stream failed to play.
    pub show_broken: bool,
    /// Bitrate probe progress: done and total, while a probe is running.
    pub probe_progress: Option<(usize, usize)>,
}

/// Catalog metadata and cover for the currently displayed song.
#[derive(Clone)]
pub struct TrackDisplay {
    pub title: String,
    pub detail: Option<String>,
    pub artwork: Option<egui::TextureHandle>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            filter: LibraryFilter::All,
            sort: SortKey::Name,
            sort_ascending: true,
            theme: ThemeMode::Dark,
            show_streams: false,
            show_metadata_debug: false,
            show_history: false,
            show_settings: false,
            show_sessions: false,
            show_schedules: false,
            show_wishlist: false,
            show_watch_history: false,
            show_about: false,
            show_help: false,
            search: String::new(),
            selected_stations: std::collections::HashSet::new(),
            selection_anchor: None,
            show_broken: false,
            probe_progress: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LibraryStats {
    pub total: usize,
    pub favorites: usize,
    pub recently_played: usize,
    pub watched: usize,
    pub genres: Vec<(String, usize)>,
}

#[derive(Debug, Clone)]
pub struct StreamRow {
    pub id: PlaybackId,
    pub name: String,
    pub status: PlaybackStatus,
    pub status_text: String,
    pub volume: f32,
    pub active: bool,
    pub recording: bool,
    /// A record-only stream; it has no playback controls.
    pub record_only: bool,
}

/// A user intent collected from the panels and applied by the app afterwards.
#[derive(Debug, Clone)]
pub enum Action {
    Play(Uuid),
    RecordBackground(Uuid),
    Select(Uuid),
    ToggleFavorite(Uuid),
    Restart(Uuid),
    StopAll,
    StopStream(PlaybackId),
    ActivateStream(PlaybackId),
    SetVolume(f32),
    SetStreamVolume(PlaybackId, f32),
    OpenAddStation,
    ImportRadioNet,
    ImportRadioBrowser,
    ExportPlaylist,
    ImportPlaylist,
    ExportHistory,
    ToggleRecord,
    OpenSessions,
    OpenSchedules,
    OpenWishlist,
    OpenWatchHistory,
    WantCurrent,
    WantSong {
        artist: Option<String>,
        title: String,
    },
    RefreshPlaylist,
    OpenSettings,
    OpenAbout,
    OpenHelp,
    NewSchedule,
    ScheduleStation(Uuid),
    EditSchedule(Uuid),
    SaveSchedule,
    CancelSchedule,
    DeleteSchedule(Uuid),
    SetScheduleEnabled(Uuid, bool),
    CycleSleep,
    ToggleMini,
    ClearSearch,
    AddStation(String, String),
    RemoveStations(Vec<Uuid>),
    RemoveBroken,
    ClearSelection,
    ProbeBitrates,
    StopBitrateProbe,
    ToggleWatched(Uuid),
}

/// Draw `text` inside `rect`, clipped so a long station name cannot spill into the
/// next column.
pub(crate) fn paint_clipped_text(
    painter: &egui::Painter,
    rect: egui::Rect,
    pos: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
) {
    painter
        .with_clip_rect(rect)
        .text(pos, anchor, text, font, color);
}

/// Short state label for the player bar and stream list (the engine keeps the
/// URL-bearing `status_text` for logs and tests).
pub fn status_label(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Connecting => crate::i18n::t("status.connecting"),
        PlaybackStatus::Buffering => crate::i18n::t("status.buffering"),
        PlaybackStatus::Live => crate::i18n::t("status.live"),
        PlaybackStatus::Error => crate::i18n::t("status.error"),
        PlaybackStatus::Stopped => crate::i18n::t("status.stopped"),
    }
}

pub(crate) fn status_color(theme: &theme::Theme, status: PlaybackStatus) -> egui::Color32 {
    match status {
        PlaybackStatus::Live => theme.status_ok,
        PlaybackStatus::Connecting | PlaybackStatus::Buffering => theme.status_warn,
        PlaybackStatus::Error => theme.danger,
        PlaybackStatus::Stopped => theme.text_disabled,
    }
}
