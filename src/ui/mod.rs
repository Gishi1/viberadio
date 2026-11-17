//! The native-egui three-pane UI.
//!
//! The app owns all state. Each panel is a `render_*` function that reads that
//! state and a read-only view model, mutates only UI-local state (filter, search,
//! sort, theme), and returns [`Action`]s. The app applies the actions after the
//! panels have finished, so no panel needs `&mut App` in the middle of a frame.

pub mod display;
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
pub const ROW_HEIGHT: f32 = 32.0;
pub const SIDEBAR_ITEM_HEIGHT: f32 = 32.0;

/// One type scale for every panel.
pub mod text {
    pub const TITLE: f32 = 20.0;
    pub const HEADING: f32 = 16.0;
    pub const BODY: f32 = 14.0;
    pub const CAPTION: f32 = 12.0;
    /// Uppercase section labels.
    pub const SECTION: f32 = 11.0;
}

/// One spacing scale for every panel.
pub mod space {
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    pub const M: f32 = 12.0;
    pub const L: f32 = 16.0;
}

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
    pub show_station_info: bool,
    /// Fullscreen media-centre view instead of the library layout.
    pub display_mode: bool,
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
            show_station_info: false,
            display_mode: false,
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
    pub broken: usize,
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
    /// A background stream kept alive for a watched station.
    pub watching: bool,
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
    OpenStationInfo(Uuid),
    ToggleDisplayMode,
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
/// next column. The cut ends with an ellipsis instead of a hard glyph edge.
pub(crate) fn paint_clipped_text(
    painter: &egui::Painter,
    rect: egui::Rect,
    pos: egui::Pos2,
    anchor: egui::Align2,
    text: &str,
    font: egui::FontId,
    color: egui::Color32,
) {
    if text.is_empty() || rect.width() <= 0.0 {
        return;
    }
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    job.wrap.overflow_character = Some('…');
    let galley = painter.layout_job(job);
    let draw = anchor.anchor_size(pos, galley.size());
    painter.with_clip_rect(rect).galley(draw.min, galley, color);
}

/// Uppercase section label used above every list group.
pub(crate) fn section_header(ui: &mut egui::Ui, theme: &theme::Theme, label: &str) {
    ui.label(
        egui::RichText::new(label.to_uppercase())
            .size(text::SECTION)
            .color(theme.text_secondary),
    );
}

/// The one primary action in a view.
pub(crate) fn primary_button(
    text: impl Into<String>,
    theme: &theme::Theme,
) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.into())
            .color(theme.bg_app)
            .strong(),
    )
    .fill(theme.accent)
    .rounding(4.0)
}

/// A quiet action that still reads as a button.
pub(crate) fn ghost_button(
    text: impl Into<String>,
    theme: &theme::Theme,
) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(theme.text_primary))
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .rounding(4.0)
}

/// One precedence rule for table and stream rows.
pub(crate) fn row_fill(
    theme: &theme::Theme,
    playing: bool,
    selected: bool,
    hovered: bool,
) -> egui::Color32 {
    if playing {
        theme.bg_row_playing
    } else if selected {
        theme.bg_row_selected
    } else if hovered {
        theme.bg_hover
    } else {
        theme.bg_row
    }
}

/// Short state label for the player bar and stream list (the engine keeps the
/// URL-bearing `status_text` for logs and tests).
pub fn status_label(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Connecting => crate::i18n::t("status.connecting"),
        PlaybackStatus::Buffering => crate::i18n::t("status.buffering"),
        PlaybackStatus::Live => crate::i18n::t("status.live"),
        PlaybackStatus::Stalled => crate::i18n::t("status.stalled"),
        PlaybackStatus::Error => crate::i18n::t("status.error"),
        PlaybackStatus::Stopped => crate::i18n::t("status.stopped"),
    }
}

pub(crate) fn status_color(theme: &theme::Theme, status: PlaybackStatus) -> egui::Color32 {
    match status {
        PlaybackStatus::Live => theme.status_ok,
        PlaybackStatus::Connecting | PlaybackStatus::Buffering => theme.status_warn,
        PlaybackStatus::Stalled => theme.status_warn,
        PlaybackStatus::Error => theme.danger,
        PlaybackStatus::Stopped => theme.text_disabled,
    }
}
