//! Application state and the top-level three-pane frame.
//!
//! The heavy rendering lives in `crate::ui`. This module owns library, database
//! and playback state, turns panel [`Action`]s into effects after the frame, and
//! keeps the engine's terminal states in step with `play_instances`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::{Local, Utc, Weekday};
use eframe::egui;
use uuid::Uuid;

use crate::database::DbManager;
use crate::dsp::DspPreset;
use crate::duration::TrackInfo;
use crate::i18n::{self, t, tf};
use crate::media::{self, MediaEvent, MediaState};
use crate::models::{AppSettings, SessionSummary, Station, StationHistory, StationSeed};
use crate::player::{
    EngineSnapshot, PlayRequest, PlaybackId, PlaybackStatus, RadioPlayer, ReconnectPolicy,
};
use crate::radionet;
use crate::recording::RecordedSummary;
use crate::scheduler::{
    self, FinishAction, Recurrence, Schedule, ScheduleAction, ScheduleDraft, SchedulerEffect,
    SchedulerRuntime, WEEKDAYS,
};
use crate::station_playlist::{self, StationPlaylist};
use crate::tray::{self, TrayEvent, TrayState};
use crate::ui::theme::Theme;
use crate::ui::{
    self, Action, LibraryFilter, LibraryStats, SortKey, StreamRow, ThemeMode, UiState,
};

enum HelpBlock {
    Heading(String),
    Subheading(String),
    Bullet(String),
    Table(String),
    Paragraph(String),
}

/// Join the hard-wrapped lines in the bundled markdown into paragraphs and
/// bullets, so egui re-wraps to the window instead of reproducing ragged source
/// line breaks.
fn parse_help(text: &str) -> Vec<HelpBlock> {
    fn flush_paragraph(blocks: &mut Vec<HelpBlock>, paragraph: &mut String) {
        if !paragraph.is_empty() {
            blocks.push(HelpBlock::Paragraph(std::mem::take(paragraph)));
        }
    }
    fn flush_bullet(blocks: &mut Vec<HelpBlock>, bullet: &mut Option<String>) {
        if let Some(text) = bullet.take() {
            blocks.push(HelpBlock::Bullet(text));
        }
    }

    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut bullet: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_paragraph(&mut blocks, &mut paragraph);
            flush_bullet(&mut blocks, &mut bullet);
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("# ") {
            flush_paragraph(&mut blocks, &mut paragraph);
            flush_bullet(&mut blocks, &mut bullet);
            blocks.push(HelpBlock::Heading(heading.to_string()));
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("## ") {
            flush_paragraph(&mut blocks, &mut paragraph);
            flush_bullet(&mut blocks, &mut bullet);
            blocks.push(HelpBlock::Subheading(heading.to_string()));
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            flush_paragraph(&mut blocks, &mut paragraph);
            flush_bullet(&mut blocks, &mut bullet);
            bullet = Some(item.to_string());
            continue;
        }
        if trimmed.starts_with('|') {
            flush_paragraph(&mut blocks, &mut paragraph);
            flush_bullet(&mut blocks, &mut bullet);
            blocks.push(HelpBlock::Table(trimmed.to_string()));
            continue;
        }
        if let Some(text) = &mut bullet {
            text.push(' ');
            text.push_str(trimmed);
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(trimmed);
        }
    }
    flush_paragraph(&mut blocks, &mut paragraph);
    flush_bullet(&mut blocks, &mut bullet);
    blocks
}

enum ArtworkState {
    Loading,
    Ready(egui::TextureHandle),
    Missing,
}

struct DecodedArtwork {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

/// One catalog lookup to try: cache key, artist, title, and whether it is a
/// fallback rather than the station's own artist-title split.
type CatalogCandidate = (String, String, String, bool);

pub struct VibeRadioApp {
    stations: Vec<Station>,
    selected_station: Option<Uuid>,
    /// Stations awaiting a bulk-remove confirmation.
    confirm_remove: Option<Vec<Uuid>>,
    /// Streams that have already cleared a station's broken flag.
    station_live_seen: HashSet<PlaybackId>,
    /// A bitrate probe in flight.
    bitrate_probe: Option<std::sync::mpsc::Receiver<crate::bitrate::ProbeEvent>>,
    bitrate_probe_cancel: Option<std::sync::Arc<AtomicBool>>,
    bitrate_probe_total: usize,
    bitrate_probe_done: usize,
    bitrate_probe_pending: Vec<(Uuid, u32)>,
    /// Whether the one-time automatic bitrate pass has been considered.
    bitrate_probe_checked: bool,
    settings: AppSettings,
    db: Option<DbManager>,
    player: RadioPlayer,
    history: Vec<StationHistory>,
    ui: UiState,
    applied_theme: Option<ThemeMode>,
    last_engine_revision: u64,
    last_active_id: Option<PlaybackId>,
    /// Bumped whenever the station list changes, so the filtered view cache is
    /// invalidated.
    library_revision: u64,
    /// Scripts found in the station names, and the revision they were computed
    /// from. Font fallbacks are only rebuilt when the set changes.
    font_scripts: u32,
    font_revision: u64,
    /// Bumped whenever history rows are added or closed.
    history_version: u64,
    /// Library counts are derived from the station list and history; cache them
    /// so a large imported library is not rescanned on every frame.
    stats: LibraryStats,
    stats_key: Option<(u64, u64)>,
    view_key: Option<ViewKey>,
    view_indices: Vec<usize>,
    last_message: Option<String>,
    /// When the current toast appeared, so transient text cannot live forever.
    message_at: Option<Instant>,
    shown_message: Option<String>,
    connecting_station: Option<Uuid>,
    /// Imported ignore rules; titles matching these are logged, not recorded.
    ignore_list: Vec<(Option<String>, Option<String>)>,
    /// Wish-list rules; matching titles fire their action (recording is M5).
    wishlist: Vec<crate::metadata::WishEntry>,
    recording_dir: String,
    recording_template: String,
    recording_split: bool,
    recording_wishlist_only: bool,
    /// Recording stops below this much free space; 0 disables the check.
    recording_min_free_mb: u32,
    /// Keep only the largest copy of repeated, identified songs.
    recording_keep_best: bool,
    /// Skip recordings below this bitrate; 0 disables the check.
    recording_min_kbps: u32,
    /// Skip recordings whose codec does not match; "any" disables the check.
    recording_codec: String,
    /// Discard split fragments shorter than this; 0 keeps every fragment.
    recording_min_track_seconds: u32,
    /// Remove split files shorter than this before fingerprinting; 0 keeps
    /// every file.
    recording_min_fingerprint_seconds: u32,
    last_space_check: Instant,
    acoustid_api_key: String,
    fingerprint_scan:
        Option<std::sync::mpsc::Receiver<Result<crate::fingerprint::FingerprintReport, String>>>,
    wish_verify:
        Option<std::sync::mpsc::Receiver<Result<crate::fingerprint::WishVerification, String>>>,
    pending_wish_verify: Vec<std::path::PathBuf>,
    /// Files waiting for the single fingerprint worker.
    pending_fingerprint: Vec<std::path::PathBuf>,
    /// Files whose AcoustID lookup failed, retried after a delay.
    fingerprint_retry: HashMap<std::path::PathBuf, (Instant, u8)>,
    /// A verified-song organizer pass in flight.
    verified_organize:
        Option<std::sync::mpsc::Receiver<Result<crate::verified::VerifiedReport, String>>>,
    /// Run another organizer pass after the current one.
    verified_pending: bool,
    wish_artist: String,
    wish_song: String,
    wish_action: String,
    wish_compare: crate::metadata::CompareMode,
    wish_search: String,
    /// Editable custom tag format in the metadata debug window.
    tag_format_input: String,
    tag_format_station: Option<Uuid>,
    /// Global Dynamic Boost preset (Off/Light/Medium/Strong).
    dsp_preset: DspPreset,
    /// Remembered output device; empty means the system default.
    output_device: String,
    /// Device actually in use, which differs from the setting after a fallback.
    applied_device: String,
    /// Cached device list for the Settings combo box.
    output_devices: Vec<String>,
    /// Whether the device list was refreshed for the current Settings visit.
    devices_loaded: bool,
    /// Decoded-audio buffer in seconds for streams opened from now on.
    stream_buffer_secs: u32,
    /// UI language.
    language: i18n::Lang,
    /// Whether the compact always-on-top player is shown.
    mini_open: bool,
    /// OS media controls (MPRIS/SMTC), when available.
    media: Option<media::Media>,
    /// Last state published to the media controls.
    last_media: Option<MediaState>,
    /// Linux tray icon, when the desktop provides one.
    tray: Option<tray::Tray>,
    /// Last state pushed to the tray, so D-Bus is only touched on change.
    last_tray: Option<TrayState>,
    /// A radio.net fetch in flight; the worker sends the parsed stations.
    radionet: Option<std::sync::mpsc::Receiver<Result<Vec<StationSeed>, String>>>,
    /// A radio-browser fetch in flight.
    radiobrowser: Option<std::sync::mpsc::Receiver<Result<Vec<StationSeed>, String>>>,
    /// Recording streams and the station each belongs to, for playlist polling.
    recording_stations: HashMap<PlaybackId, Uuid>,
    /// Background watch streams, keyed by station.
    watch_streams: HashMap<Uuid, PlaybackId>,
    /// Whether each recording splits per track; duration scheduling only
    /// applies in that mode.
    recording_splits: HashMap<PlaybackId, bool>,
    /// When each recording started, so a playlist split waits for a static ICY
    /// title before taking over.
    recording_started_at: HashMap<PlaybackId, chrono::DateTime<chrono::Utc>>,
    /// How many times the ICY title has changed since recording started. Once it
    /// updates, ICY is the authoritative splitter and the scraped page is not.
    icy_changes: HashMap<PlaybackId, u32>,
    /// Last track label sent to each recording stream.
    recording_labels: HashMap<PlaybackId, String>,
    /// A scheduled close at the expected end of the current track.
    split_due: HashMap<PlaybackId, (String, chrono::DateTime<chrono::Utc>)>,
    /// The raw ICY title each recording's duration lookup belongs to.
    duration_title: HashMap<PlaybackId, String>,
    /// Catalog entries from iTunes, keyed by `duration::cache_key`.
    duration_cache: HashMap<String, crate::duration::LookupResult>,
    /// A duration lookup in flight.
    duration_poll: Option<(
        PlaybackId,
        String,
        chrono::DateTime<chrono::Utc>,
        std::sync::mpsc::Receiver<Result<crate::duration::LookupResult, String>>,
        String,
        bool,
        Vec<CatalogCandidate>,
    )>,
    /// A failed duration lookup to retry, and how many times it has failed.
    duration_retry: HashMap<PlaybackId, (String, Instant)>,
    duration_failures: HashMap<PlaybackId, u8>,
    /// Live level windows for the current title of each split recording.
    live_levels: HashMap<PlaybackId, Vec<crate::levels::LevelSample>>,
    /// The title the live boundary already closed.
    live_closed: HashMap<PlaybackId, String>,
    /// Last RMS serial seen per recording.
    live_serial: HashMap<PlaybackId, u64>,
    /// The title the live level windows belong to.
    live_title: HashMap<PlaybackId, String>,
    /// When to fingerprint the opening seconds of the current split file.
    live_fingerprint_due: HashMap<PlaybackId, (String, chrono::DateTime<chrono::Utc>)>,
    /// An AcoustID identification in flight for a live file.
    live_fingerprint_poll: Option<(
        PlaybackId,
        String,
        chrono::DateTime<chrono::Utc>,
        std::sync::mpsc::Receiver<Result<Option<crate::fingerprint::AcoustIdMatch>, String>>,
        String,
    )>,
    /// Live files whose identification failed, retried after a delay.
    live_fingerprint_retry: HashMap<PlaybackId, u8>,
    /// AcoustID-verified lengths by `duration::cache_key`.
    verified_durations: HashMap<String, u32>,
    /// Catalog metadata for the stream bar, keyed by the raw ICY title.
    track_info: HashMap<String, crate::duration::TrackInfo>,
    /// Cover textures for the stream bar, keyed by the raw ICY title.
    artwork: HashMap<String, ArtworkState>,
    /// Artwork fetch in flight.
    artwork_poll:
        Option<(String, std::sync::mpsc::Receiver<Result<Option<DecodedArtwork>, String>>)>,
    /// Artwork URLs waiting for the single fetch worker.
    artwork_pending: Vec<(String, String)>,
    /// A catalog lookup for the stream bar, independent of recording.
    metadata_poll: Option<(
        String,
        String,
        std::sync::mpsc::Receiver<Result<crate::duration::LookupResult, String>>,
        bool,
        Vec<CatalogCandidate>,
    )>,
    /// Titles waiting for the single metadata worker.
    metadata_pending: VecDeque<String>,
    /// A failed metadata lookup to retry, and how many times it has failed.
    metadata_retry: Option<(String, Instant)>,
    metadata_failures: HashMap<String, u8>,
    /// A recording-playlist poll in flight.
    recording_playlist_poll:
        Option<std::sync::mpsc::Receiver<(PlaybackId, Result<StationPlaylist, String>)>>,
    last_recording_poll: Instant,
    recording_poll_cursor: usize,
    /// A station-playlist scrape in flight, with the station it belongs to.
    playlist_fetch: Option<(
        Uuid,
        std::sync::mpsc::Receiver<Result<StationPlaylist, String>>,
    )>,
    /// Scraped playlists, cached per station.
    playlists: HashMap<Uuid, StationPlaylist>,
    /// Last scrape error per station.
    playlist_errors: HashMap<Uuid, String>,
    /// Buffer actually applied, so an unsaved edit can be reverted on open.
    applied_stream_buffer: u32,
    /// Cached data folder and database path; the windows must not call
    /// `create_dir_all` or read the DB every frame.
    data_dir_path: Option<std::path::PathBuf>,
    /// Editable data-folder path shown in Settings; applied on restart.
    data_dir_input: String,
    schema_version: String,
    latest_backup: Option<std::path::PathBuf>,
    sessions: Vec<SessionSummary>,
    session_name: String,
    schedules: Vec<Schedule>,
    scheduler: SchedulerRuntime,
    schedule_editor: Option<ScheduleEditor>,
    /// Streams a record-only schedule started muted. Their 0.0 engine volume
    /// must not be copied into the user's persisted/default volume.
    scheduler_muted: HashSet<PlaybackId>,
    /// Recordings whose writer is still closing on its own thread.
    finalising: Vec<FinalisingRecording>,
    /// Finish actions waiting for every recording file to close.
    pending_finish: Vec<FinishAction>,
    sleep_minutes: u32,
    sleep_at: Option<Instant>,
    /// Ask the OS to power off when the sleep timer ends.
    sleep_shutdown: bool,
    add_station: AddStationDialog,
    /// One row per playback attempt, keyed by the engine handle.
    attempts: HashMap<PlaybackId, Attempt>,
    /// When each stream's ICY title last changed, so a lagging scraped
    /// playlist cannot split a recording the live title already handled.
    now_playing_at: HashMap<PlaybackId, chrono::DateTime<chrono::Utc>>,
    last_heartbeat: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewKey {
    library_revision: u64,
    history_version: u64,
    filter: LibraryFilter,
    search: String,
    sort: SortKey,
    ascending: bool,
    show_broken: bool,
}

struct Attempt {
    history_id: Uuid,
    instance_id: Uuid,
    started: Instant,
    /// Currently playing track, linked to this attempt. Closed on the next ICY
    /// title or when the stream stops.
    track: Option<TrackRecord>,
    /// Last title seen, including ignored/DB-less ones, so a suppressed title is
    /// not reprocessed every frame.
    last_title: Option<String>,
}

struct TrackRecord {
    id: Uuid,
    started: Instant,
}

/// One entry in the wishlist import/export file. Kept separate from
/// `WishEntry` so the database row id is never part of the file.
#[derive(serde::Serialize, serde::Deserialize)]
struct WishFileEntry {
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    song_title: Option<String>,
    #[serde(default)]
    action: String,
    #[serde(default)]
    compare: String,
}

#[derive(Default)]
struct AddStationDialog {
    showing: bool,
    name: String,
    url: String,
}

struct ScheduleEditor {
    /// `Some` when editing an existing row; `None` for a new schedule.
    id: Option<Uuid>,
    draft: ScheduleDraft,
    error: Option<String>,
}

struct FinalisingRecording {
    playback_id: PlaybackId,
    handle: JoinHandle<RecordedSummary>,
}

pub(crate) fn data_dir() -> Option<std::path::PathBuf> {
    let explicit = std::env::var("VIBERADIO_DATA_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    resolve_data_dir(default_data_dir()?, explicit)
}

/// The OS default location. The `location` pointer file lives here even when
/// the active data folder is elsewhere.
pub(crate) fn default_data_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|dir| dir.join("VibeRadio"))
}

static DATA_DIR_WARNED: AtomicBool = AtomicBool::new(false);

fn resolve_data_dir(
    default: std::path::PathBuf,
    explicit: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    resolve_data_dir_with_warning(default, explicit).0
}

fn resolve_data_dir_with_warning(
    default: std::path::PathBuf,
    explicit: Option<std::path::PathBuf>,
) -> (Option<std::path::PathBuf>, Option<String>) {
    if let Some(dir) = explicit {
        return match std::fs::create_dir_all(&dir) {
            Ok(()) => (Some(dir), None),
            Err(e) => (
                None,
                Some(format!("Could not create VIBERADIO_DATA_DIR: {e}")),
            ),
        };
    }
    if let Some(custom) = read_data_dir_pointer(&default) {
        let pending_copy = default.join("viberadio-copy.pending").exists();
        if custom.is_absolute()
            && custom.is_dir()
            && (custom.join("viberadio.db").is_file() || pending_copy)
        {
            return (Some(custom), None);
        }
        let warning = format!(
            "The data folder pointer is invalid ({}); using the default folder.",
            custom.display()
        );
        if !DATA_DIR_WARNED.swap(true, Ordering::Relaxed) {
            eprintln!("{warning}");
            log::warn!("{warning}");
        }
        return match std::fs::create_dir_all(&default) {
            Ok(()) => (Some(default), Some(warning)),
            Err(e) => (
                None,
                Some(format!("Could not create the default data folder: {e}")),
            ),
        };
    }
    match std::fs::create_dir_all(&default) {
        Ok(()) => (Some(default), None),
        Err(e) => (
            None,
            Some(format!("Could not create the default data folder: {e}")),
        ),
    }
}

/// The warning from the current data-folder resolution, if any. The startup
/// toast shows it; the log already has the same text.
pub(crate) fn data_dir_warning() -> Option<String> {
    let default = default_data_dir()?;
    let explicit = std::env::var("VIBERADIO_DATA_DIR")
        .ok()
        .map(std::path::PathBuf::from);
    resolve_data_dir_with_warning(default, explicit).1
}

fn read_data_dir_pointer(default: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(default.join("location")).ok()?;
    let path = std::path::PathBuf::from(text.trim());
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Point the next start at `target`, or remove the pointer when `target` is the
/// default folder.
pub(crate) fn set_data_dir_pointer(
    default: &std::path::Path,
    target: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let pointer = default.join("location");
    match target {
        Some(path) => {
            std::fs::create_dir_all(default)?;
            std::fs::write(pointer, path.display().to_string())
        }
        None => match std::fs::remove_file(pointer) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
    }
}

impl VibeRadioApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // A data-folder switch copies the newest library here, before the
        // database is opened, so edits made between the click and the restart
        // are included. A restore marker is applied next, also before opening.
        let mut startup_warning = None;
        if let Some(default) = default_data_dir() {
            if let Some(message) = DbManager::apply_pending_copy(&default) {
                startup_warning = Some(message);
            }
        }
        let data_dir_path = data_dir();
        if startup_warning.is_none() {
            startup_warning = data_dir_warning();
        }
        if let Some(dir) = &data_dir_path {
            if let Some(message) = DbManager::apply_pending_restore(dir) {
                startup_warning = Some(message);
            }
        }

        let db = data_dir_path
            .as_ref()
            .map(|dir| dir.join("viberadio.db"))
            .and_then(|path| match DbManager::new(path.to_str()?) {
                Ok(db) => Some(db),
                Err(e) => {
                    log::error!("could not open database at {}: {e}", path.display());
                    None
                }
            });

        let mut schema_ok = false;
        let mut schema_version = "?".to_string();
        if let Some(db) = &db {
            match db.init_schema() {
                Ok(()) => {
                    schema_ok = true;
                    if let Ok(Some(version)) = db.get_setting("schema_version") {
                        log::info!("database schema version {version}");
                        schema_version = version;
                    }
                    match db.should_sweep_orphans(30) {
                        Ok(true) => match db.close_orphaned_play_instances() {
                            Ok(n) if n > 0 => log::warn!("closed {n} orphaned play instance(s)"),
                            Ok(_) => {}
                            Err(e) => log::warn!("could not close orphaned play instances: {e}"),
                        },
                        Ok(false) => log::info!(
                            "another VibeRadio instance appears to be running; skipping orphan sweep"
                        ),
                        Err(e) => log::warn!("could not check the heartbeat: {e}"),
                    }
                    if let Err(e) = db.write_heartbeat() {
                        log::warn!("could not write the library heartbeat: {e}");
                    }
                }
                Err(e) => {
                    log::error!("could not initialise the database schema: {e}");
                    startup_warning = Some(format!(
                        "The database could not be prepared ({e}); the library may be read-only"
                    ));
                }
            }
        }

        let (stations, _load_failed) = match db.as_ref() {
            Some(db) if schema_ok => match db.load_stations() {
                Ok(list) => (list, false),
                Err(e) => {
                    log::error!("could not load stations: {e}");
                    (Vec::new(), true)
                }
            },
            _ => (Vec::new(), true),
        };

        let history = match db.as_ref().map(|db| db.load_recent_history(1_000)) {
            Some(Ok(list)) => list,
            Some(Err(e)) => {
                log::error!("could not load history: {e}");
                Vec::new()
            }
            None => Vec::new(),
        };

        let mut last_message = startup_warning;

        let ignore_list = match db.as_ref().map(|db| db.load_ignore_list()) {
            Some(Ok(list)) => list,
            Some(Err(e)) => {
                log::warn!("could not load the ignore list: {e}");
                Vec::new()
            }
            None => Vec::new(),
        };
        let wishlist = match db.as_ref().map(|db| db.load_wishlist()) {
            Some(Ok(list)) => list,
            Some(Err(e)) => {
                log::warn!("could not load the wish list: {e}");
                Vec::new()
            }
            None => Vec::new(),
        };

        let recording_template = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_template").ok().flatten())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "{station} - {artist} - {title}".to_string());
        let recording_split = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_mode").ok().flatten())
            .map(|mode| mode != "continuous")
            .unwrap_or(true);
        let recording_wishlist_only = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_wishlist_only").ok().flatten())
            .map(|value| value == "1")
            .unwrap_or(false);
        let recording_min_free_mb = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_min_free_mb").ok().flatten())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(500);
        let recording_keep_best = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_keep_best").ok().flatten())
            .map(|value| value != "0")
            .unwrap_or(true);
        let recording_min_kbps = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_min_kbps").ok().flatten())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let recording_codec = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_codec").ok().flatten())
            .unwrap_or_else(|| "any".to_string());
        let recording_min_track_seconds = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_min_track_seconds").ok().flatten())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let recording_min_fingerprint_seconds = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_min_fingerprint_seconds").ok().flatten())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(30);
        let sleep_shutdown = db
            .as_ref()
            .and_then(|db| db.get_setting("sleep_shutdown").ok().flatten())
            .map(|value| value == "1")
            .unwrap_or(false);
        let acoustid_api_key = db
            .as_ref()
            .and_then(|db| db.get_setting("acoustid_api_key").ok().flatten())
            .unwrap_or_default();
        log::info!(
            "AcoustID key {}",
            if acoustid_api_key.trim().is_empty() {
                "not configured"
            } else {
                "configured"
            }
        );
        let recording_dir = db
            .as_ref()
            .and_then(|db| db.get_setting("recording_dir").ok().flatten())
            .filter(|value| !value.trim().is_empty())
            .or_else(|| data_dir().map(|dir| dir.join("recordings").display().to_string()))
            .unwrap_or_else(|| "recordings".to_string());

        let sessions = db
            .as_ref()
            .and_then(|db| db.load_sessions().ok())
            .unwrap_or_default();
        let latest_backup = data_dir_path
            .as_ref()
            .and_then(|dir| DbManager::latest_backup(dir));
        let data_dir_input = data_dir_path
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let schedules = db
            .as_ref()
            .and_then(|db| db.load_schedules().ok())
            .unwrap_or_default();
        let dsp_preset = db
            .as_ref()
            .and_then(|db| db.get_setting("dsp_preset").ok().flatten())
            .and_then(|value| DspPreset::parse(&value))
            .unwrap_or_default();
        let output_device = db
            .as_ref()
            .and_then(|db| db.get_setting("output_device").ok().flatten())
            .unwrap_or_default();
        let language = db
            .as_ref()
            .and_then(|db| db.get_setting("language").ok().flatten())
            .and_then(|value| i18n::Lang::parse(&value))
            .unwrap_or(i18n::Lang::English);
        i18n::set_language(language);
        let stream_buffer_secs = db
            .as_ref()
            .and_then(|db| db.get_setting("stream_buffer_secs").ok().flatten())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(crate::player::DEFAULT_STREAM_BUFFER_SECS)
            .clamp(
                crate::player::MIN_STREAM_BUFFER_SECS,
                crate::player::MAX_STREAM_BUFFER_SECS,
            );
        let tray = {
            let ctx = cc.egui_ctx.clone();
            tray::Tray::start(TrayState::default(), move || ctx.request_repaint())
        };
        let media = {
            let ctx = cc.egui_ctx.clone();
            media::Media::start(MediaState::default(), move || ctx.request_repaint())
        };
        let player =
            RadioPlayer::with_device((!output_device.is_empty()).then(|| output_device.clone()));
        player.set_dsp_preset(dsp_preset);
        player.set_stream_buffer(stream_buffer_secs);
        let device_notice = player.take_device_notice();
        let applied_device = if device_notice.is_some() {
            // The remembered device did not open; the system default did.
            String::new()
        } else {
            output_device.clone()
        };
        if let Some(notice) = device_notice {
            last_message = Some(tf("msg.audio_notice", &[&notice]));
        }

        let mut app = Self {
            stations,
            selected_station: None,
            confirm_remove: None,
            station_live_seen: HashSet::new(),
            bitrate_probe: None,
            bitrate_probe_cancel: None,
            bitrate_probe_total: 0,
            bitrate_probe_done: 0,
            bitrate_probe_pending: Vec::new(),
            bitrate_probe_checked: false,
            settings: AppSettings::default(),
            db,
            player,
            history,
            ui: UiState::default(),
            applied_theme: None,
            last_engine_revision: 0,
            last_active_id: None,
            library_revision: 0,
            font_scripts: 0,
            font_revision: 0,
            history_version: 0,
            stats: LibraryStats::default(),
            stats_key: None,
            view_key: None,
            view_indices: Vec::new(),
            last_message,
            message_at: None,
            shown_message: None,
            connecting_station: None,
            ignore_list,
            wishlist,
            recording_dir,
            recording_template,
            recording_split,
            recording_wishlist_only,
            recording_min_free_mb,
            recording_keep_best,
            recording_min_kbps,
            recording_codec,
            recording_min_track_seconds,
            recording_min_fingerprint_seconds,
            last_space_check: Instant::now(),
            acoustid_api_key,
            fingerprint_scan: None,
            wish_verify: None,
            pending_wish_verify: Vec::new(),
            pending_fingerprint: Vec::new(),
            fingerprint_retry: HashMap::new(),
            verified_organize: None,
            verified_pending: false,
            wish_artist: String::new(),
            wish_song: String::new(),
            wish_action: "record".to_string(),
            wish_compare: crate::metadata::CompareMode::Contains,
            wish_search: String::new(),
            tag_format_input: String::new(),
            tag_format_station: None,
            dsp_preset,
            output_device,
            applied_device,
            output_devices: Vec::new(),
            devices_loaded: false,
            stream_buffer_secs,
            applied_stream_buffer: stream_buffer_secs,
            language,
            mini_open: false,
            media,
            last_media: None,
            tray,
            last_tray: None,
            radionet: None,
            radiobrowser: None,
            recording_stations: HashMap::new(),
            watch_streams: HashMap::new(),
            recording_splits: HashMap::new(),
            recording_started_at: HashMap::new(),
            icy_changes: HashMap::new(),
            recording_labels: HashMap::new(),
            split_due: HashMap::new(),
            duration_title: HashMap::new(),
            duration_cache: HashMap::new(),
            duration_poll: None,
            duration_retry: HashMap::new(),
            duration_failures: HashMap::new(),
            live_levels: HashMap::new(),
            live_closed: HashMap::new(),
            live_serial: HashMap::new(),
            live_title: HashMap::new(),
            live_fingerprint_due: HashMap::new(),
            live_fingerprint_poll: None,
            live_fingerprint_retry: HashMap::new(),
            verified_durations: HashMap::new(),
            track_info: HashMap::new(),
            artwork: HashMap::new(),
            artwork_poll: None,
            artwork_pending: Vec::new(),
            metadata_poll: None,
            metadata_pending: VecDeque::new(),
            metadata_retry: None,
            metadata_failures: HashMap::new(),
            recording_playlist_poll: None,
            last_recording_poll: Instant::now(),
            recording_poll_cursor: 0,
            playlist_fetch: None,
            playlists: HashMap::new(),
            playlist_errors: HashMap::new(),
            data_dir_path,
            data_dir_input,
            schema_version,
            latest_backup,
            sessions,
            session_name: String::new(),
            schedules,
            scheduler: SchedulerRuntime::new(),
            schedule_editor: None,
            scheduler_muted: HashSet::new(),
            finalising: Vec::new(),
            pending_finish: Vec::new(),
            sleep_minutes: 0,
            sleep_at: None,
            sleep_shutdown,
            add_station: AddStationDialog::default(),
            attempts: HashMap::new(),
            now_playing_at: HashMap::new(),
            last_heartbeat: Instant::now(),
        };
        // Importing a directory is the user's choice from the sidebar. A fresh
        // install starts with an empty library and never fetches stations on
        // its own.
        let scripts = crate::fonts::scripts_in(app.stations.iter().map(|s| s.name.as_str()));
        ui::theme::install_fonts(&cc.egui_ctx, scripts);
        app.font_scripts = scripts;
        app.font_revision = app.library_revision;
        app
    }

    // -----------------------------------------------------------------------
    // Playback
    // -----------------------------------------------------------------------

    /// Stop every audible stream except `keep`. Scheduled record-only streams
    /// are muted and must survive a user's station switch.
    fn stop_other_audible_streams(&mut self, keep: Option<PlaybackId>) {
        let ids = streams_to_replace(&self.player.snapshot().streams, &self.scheduler_muted, keep);
        for id in ids {
            self.stop_one_stream(id);
        }
    }

    fn start_station(&mut self, station_id: Uuid) {
        let _ = self.start_station_with(station_id, None);
    }

    /// Start a station with an optional engine-volume override. A scheduled
    /// record-only action passes `Some(0.0)` so a *new* stream is captured
    /// without being audible; an already-playing stream is never muted, and the
    /// station's persisted volume is left untouched.
    ///
    /// Returns the stream handle and whether this call opened it. A schedule uses
    /// that to stop only what it started.
    fn start_station_with(
        &mut self,
        station_id: Uuid,
        volume_override: Option<f32>,
    ) -> (Option<PlaybackId>, bool) {
        let Some(station) = self
            .stations
            .iter()
            .find(|station| station.id == station_id)
            .cloned()
        else {
            return (None, false);
        };
        if station.url.trim().is_empty() {
            self.last_message = Some(tf("msg.no_stream_url", &[&station.name]));
            return (None, false);
        }

        // Already open and alive: make it active instead of opening a duplicate.
        // A dead (error/stopped) stream must be replaced, not just selected.
        if let Some(stream) = self
            .player
            .snapshot()
            .streams
            .into_iter()
            .find(|stream| stream.station_id == Some(station_id) && !stream.record_only)
        {
            if stream.status.is_active() {
                if volume_override.is_none() {
                    self.stop_other_audible_streams(Some(stream.id));
                }
                self.player.set_active(stream.id);
                self.selected_station = Some(station_id);
                if volume_override.is_none() {
                    // An explicit Play takes the stream back from a record-only
                    // schedule: it must become audible, not stay silently muted.
                    if self.scheduler_muted.remove(&stream.id) {
                        let volume = station.volume.clamp(0.0, 1.0);
                        self.player.set_stream_volume(stream.id, volume);
                        self.settings.volume = volume;
                    } else {
                        self.settings.volume = stream.volume;
                    }
                }
                self.last_message = Some(tf("msg.active", &[&station.name]));
                return (Some(stream.id), false);
            }
            self.stop_one_stream(stream.id);
        }

        let stream = match &self.db {
            Some(db) => match db.ensure_stream_for_station(&station) {
                Ok(stream) => Some(stream),
                Err(e) => {
                    log::warn!("could not prepare a stream row: {e}");
                    None
                }
            },
            None => None,
        };
        let url = stream
            .as_ref()
            .map(|stream| stream.url.clone())
            .unwrap_or_else(|| station.url.clone());
        let volume = volume_override.unwrap_or_else(|| station.volume.clamp(0.0, 1.0));
        if volume_override.is_none() {
            self.settings.volume = volume;
        }

        if volume_override.is_none() {
            // A user play replaces the audible stream; scheduled recordings
            // stay muted and keep running.
            self.stop_other_audible_streams(None);
        }
        let playback_id = self.player.play_stream(PlayRequest {
            station_id: Some(station.id),
            stream_id: stream.as_ref().map(|stream| stream.id),
            volume,
            gain_db: station.gain_db,
            reconnect: Some(ReconnectPolicy::default()),
            ..PlayRequest::new(url)
        });
        self.selected_station = Some(station_id);
        self.connecting_station = Some(station_id);
        self.last_message = Some(tf("msg.connecting", &[&station.name]));

        if let Some(db) = &self.db {
            match db.begin_play(&station, stream.as_ref(), volume) {
                Ok((instance, entry)) => {
                    self.attempts.insert(
                        playback_id,
                        Attempt {
                            history_id: entry.id,
                            instance_id: instance.id,
                            started: Instant::now(),
                            track: None,
                            last_title: None,
                        },
                    );
                    self.history.insert(0, entry);
                    self.history_version += 1;
                }
                Err(e) => {
                    log::warn!("could not record history: {e}");
                    self.last_message = Some(tf(
                        "msg.connecting_history",
                        &[&station.name, &e.to_string()],
                    ));
                }
            }
        }
        (Some(playback_id), true)
    }

    /// Close the attempt behind one engine handle and finalise its history row.
    fn finalise_attempt(
        &mut self,
        playback_id: PlaybackId,
        status: &str,
        end_reason: Option<&str>,
    ) {
        let Some(attempt) = self.attempts.remove(&playback_id) else {
            return;
        };
        self.now_playing_at.remove(&playback_id);
        let elapsed = attempt.started.elapsed().as_secs().min(i64::MAX as u64) as i64;
        let error = if status == "error" { end_reason } else { None };
        if let Some(db) = &self.db {
            if let Some(track) = &attempt.track {
                let track_elapsed = track.started.elapsed().as_secs().min(i64::MAX as u64) as i64;
                if let Err(e) = db.end_track(track.id, track_elapsed, status) {
                    log::warn!("could not close the track row: {e}");
                }
                if let Some(entry) = self.history.iter_mut().find(|entry| entry.id == track.id) {
                    entry.duration_seconds = track_elapsed;
                    entry.status = status.to_string();
                }
            }
            if let Err(e) = db.set_play_instance_recording(attempt.instance_id, false) {
                log::warn!("could not clear the recording flag: {e}");
            }
            if let Err(e) = db.end_play(
                attempt.instance_id,
                attempt.history_id,
                elapsed,
                status,
                end_reason,
                error,
            ) {
                log::warn!("could not close the play instance: {e}");
                // The normal close failed; force the attempt shut so it does not
                // stay open until the next startup sweep.
                if let Err(e) = db.abandon_play_instance(attempt.instance_id, attempt.history_id) {
                    log::warn!("could not abandon the play instance: {e}");
                } else if let Some(entry) = self
                    .history
                    .iter_mut()
                    .find(|entry| entry.id == attempt.history_id)
                {
                    entry.status = "error".to_string();
                }
            } else if let Ok(Some(closed)) = db.load_play_instance(attempt.instance_id) {
                log::info!(
                    "play instance {} finished after {}s ({})",
                    closed.id,
                    closed.duration_seconds,
                    closed.status
                );
            }
        }
        if let Some(entry) = self
            .history
            .iter_mut()
            .find(|entry| entry.id == attempt.history_id)
        {
            entry.duration_seconds = elapsed;
            entry.status = status.to_string();
        }
        self.history_version += 1;
    }

    fn reload_stations(&mut self) -> bool {
        let Some(db) = &self.db else {
            return false;
        };
        match db.load_stations() {
            Ok(list) => {
                self.stations = list;
                self.library_revision += 1;
                true
            }
            Err(e) => {
                log::warn!("could not reload stations: {e}");
                self.last_message = Some(tf("msg.reload_failed", &[&e.to_string()]));
                false
            }
        }
    }

    fn play_selected(&mut self) {
        if let Some(id) = self.selected_station {
            self.start_station(id);
        } else {
            self.last_message = Some(t("player.select_station").to_string());
        }
    }

    fn toggle_playback(&mut self) {
        if self.player.snapshot().active.is_some() {
            self.stop_all_streams();
        } else {
            self.play_selected();
        }
    }

    /// Play the next or previous station in the visible order.
    ///
    /// An empty view means the current search/filter matches nothing, not "no
    /// view yet": Next must not fall back to stations the user has hidden.
    fn skip_station(&mut self, delta: i32) {
        self.ensure_view();
        let order: Vec<Uuid> = self
            .view_indices
            .iter()
            .filter_map(|&index| self.stations.get(index).map(|station| station.id))
            .collect();
        let current = self
            .player
            .snapshot()
            .active
            .as_ref()
            .and_then(|stream| stream.station_id)
            .or(self.selected_station);
        let position = current.and_then(|id| order.iter().position(|candidate| *candidate == id));
        let Some(next) = next_index(order.len(), position, delta) else {
            return;
        };
        let id = order[next];
        self.selected_station = Some(id);
        self.start_station(id);
    }

    fn stop_all_streams(&mut self) {
        // Stop playback only. Record-only streams and their writers keep
        // running until the REC control or their station is stopped.
        let snapshot = self.player.snapshot();
        let recording_ids: HashSet<PlaybackId> = snapshot
            .streams
            .iter()
            .filter(|stream| stream.record_only)
            .map(|stream| stream.id)
            .collect();
        let playback: Vec<PlaybackId> = snapshot
            .streams
            .iter()
            .filter(|stream| !stream.record_only)
            .map(|stream| stream.id)
            .collect();
        for id in &playback {
            let _ = self.stop_recording_on(*id);
        }
        let attempts: Vec<PlaybackId> = self
            .attempts
            .keys()
            .copied()
            .filter(|id| !recording_ids.contains(id))
            .collect();
        for id in attempts {
            self.finalise_attempt(id, "ended", Some("stopped"));
        }
        for id in playback {
            self.player.stop_stream(id);
        }
        self.last_message = Some(t("msg.stopped").to_string());
    }

    fn stop_one_stream(&mut self, playback_id: PlaybackId) {
        // Join the writer before the stream goes away, so a scheduled finish
        // command cannot run while the recording file is still open.
        let _ = self.stop_recording_on(playback_id);
        if let Some(summary) = self.player.take_finished_recording(playback_id) {
            self.finish_recording_summary(&summary);
            self.last_message = Some(recording_summary_message(&summary));
        }
        self.player.stop_stream(playback_id);
        self.scheduler_muted.remove(&playback_id);
        self.finalise_attempt(playback_id, "ended", Some("stopped"));
    }

    /// If the engine reports a terminal state, close the matching attempt. This
    /// handles a stalled stream or a finite file ending without a Stop click.
    fn reconcile_attempts(&mut self, engine: &EngineSnapshot) {
        let terminal: Vec<(PlaybackId, &'static str, String, Option<Uuid>)> = engine
            .streams
            .iter()
            .filter_map(|stream| match stream.status {
                PlaybackStatus::Error => Some((
                    stream.id,
                    "error",
                    stream
                        .error
                        .clone()
                        .unwrap_or_else(|| stream.status_text.clone()),
                    stream.station_id,
                )),
                PlaybackStatus::Stopped => Some((
                    stream.id,
                    "ended",
                    "stream ended".to_string(),
                    stream.station_id,
                )),
                _ => None,
            })
            .collect();
        for (id, status, message, station_id) in terminal {
            self.scheduler_muted.remove(&id);
            if self.attempts.contains_key(&id) {
                // The attempt is removed below, so one failed stream counts
                // once instead of on every frame it stays in Error.
                if status == "error" {
                    if let Some(station_id) = station_id {
                        self.record_station_failure(station_id);
                    }
                }
                self.finalise_attempt(id, status, Some(&message));
            }
            if let Some(summary) = self.player.take_finished_recording(id) {
                self.finish_recording_summary(&summary);
                self.last_message = Some(recording_summary_message(&summary));
            }
        }

        // A successful play clears an earlier broken mark.
        let live: Vec<(PlaybackId, Option<Uuid>)> = engine
            .streams
            .iter()
            .filter(|stream| stream.status == PlaybackStatus::Live)
            .map(|stream| (stream.id, stream.station_id))
            .collect();
        self.station_live_seen
            .retain(|id| live.iter().any(|(live_id, _)| live_id == id));
        for (id, station_id) in live {
            if let Some(station_id) = station_id {
                if self.station_live_seen.insert(id) {
                    self.clear_station_failure(station_id);
                }
            }
        }
        self.adopt_reported_bitrates(engine);
    }

    /// Persist the bitrate a stream reported in its ICY headers, which is the
    /// only source for directories such as radio.net that do not send one.
    fn adopt_reported_bitrates(&mut self, engine: &EngineSnapshot) {
        for stream in &engine.streams {
            let (Some(station_id), Some(bitrate)) =
                (stream.station_id, stream.reported_bitrate)
            else {
                continue;
            };
            let needs = self
                .stations
                .iter()
                .find(|station| station.id == station_id)
                .map(|station| station.bitrate.unwrap_or(0) == 0)
                .unwrap_or(false);
            if !needs {
                continue;
            }
            let Some(db) = &self.db else {
                continue;
            };
            if let Err(e) = db.set_station_bitrate(station_id, bitrate) {
                log::warn!("could not save the reported bitrate: {e}");
                continue;
            }
            if let Some(station) = self
                .stations
                .iter_mut()
                .find(|station| station.id == station_id)
            {
                station.bitrate = Some(bitrate);
            }
            self.library_revision += 1;
            log::info!("station {station_id}: stream reported {bitrate} kbps");
        }
    }

    fn record_station_failure(&mut self, station_id: Uuid) {
        let Some(db) = &self.db else {
            return;
        };
        let result = db.record_station_failure(station_id);
        match result {
            Ok((failures, broken)) => {
                if let Some(station) = self
                    .stations
                    .iter_mut()
                    .find(|station| station.id == station_id)
                {
                    station.failure_count = failures;
                    if broken {
                        station.broken = true;
                        station.broken_at = Some(Utc::now().to_rfc3339());
                    }
                }
                if broken {
                    self.library_revision += 1;
                    log::info!("station {station_id} marked broken after {failures} failures");
                }
            }
            Err(e) => log::warn!("could not record the station failure: {e}"),
        }
    }

    fn clear_station_failure(&mut self, station_id: Uuid) {
        let was_marked = self
            .stations
            .iter()
            .find(|station| station.id == station_id)
            .map(|station| station.broken || station.failure_count > 0)
            .unwrap_or(false);
        if !was_marked {
            return;
        }
        let Some(db) = &self.db else {
            return;
        };
        if let Err(e) = db.clear_station_failure(station_id) {
            log::warn!("could not clear the station failure: {e}");
            return;
        }
        if let Some(station) = self
            .stations
            .iter_mut()
            .find(|station| station.id == station_id)
        {
            station.broken = false;
            station.broken_at = None;
            station.failure_count = 0;
        }
        self.library_revision += 1;
    }

    /// Probe the streams that have no bitrate yet, reading each one's ICY
    /// headers. A selection limits the pass to those stations.
    fn start_bitrate_probe(&mut self) {
        if self.bitrate_probe.is_some() {
            return;
        }
        let targets: Vec<(Uuid, String)> = if !self.ui.selected_stations.is_empty() {
            self.stations
                .iter()
                .filter(|station| {
                    self.ui.selected_stations.contains(&station.id)
                        && station.bitrate.unwrap_or(0) == 0
                })
                .map(|station| (station.id, station.url.clone()))
                .collect()
        } else {
            self.view_indices
                .iter()
                .filter_map(|index| self.stations.get(*index))
                .filter(|station| station.bitrate.unwrap_or(0) == 0)
                .map(|station| (station.id, station.url.clone()))
                .collect()
        };
        if targets.is_empty() {
            self.last_message = Some(t("msg.no_bitrate_targets").to_string());
            return;
        }
        self.bitrate_probe_total = targets.len();
        self.bitrate_probe_done = 0;
        self.bitrate_probe_pending.clear();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        self.bitrate_probe_cancel = Some(std::sync::Arc::clone(&cancel));
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-bitrate".to_string())
            .spawn(move || crate::bitrate::probe_streams(targets, cancel, tx));
        match spawned {
            Ok(_) => self.bitrate_probe = Some(rx),
            Err(e) => {
                log::warn!("could not start the bitrate probe: {e}");
                self.bitrate_probe_cancel = None;
                self.last_message =
                    Some(tf("msg.bitrate_probe_failed", &[&e.to_string()]));
            }
        }
    }

    fn stop_bitrate_probe(&mut self) {
        if let Some(cancel) = &self.bitrate_probe_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.bitrate_probe = None;
        self.bitrate_probe_cancel = None;
        self.ui.probe_progress = None;
    }

    fn poll_bitrate_probe(&mut self) {
        let Some(rx) = self.bitrate_probe.take() else {
            self.ui.probe_progress = None;
            return;
        };
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(crate::bitrate::ProbeEvent::Result { id, bitrate }) => {
                    self.bitrate_probe_done += 1;
                    if let Some(bitrate) = bitrate {
                        if let Some(station) = self
                            .stations
                            .iter_mut()
                            .find(|station| station.id == id)
                        {
                            station.bitrate = Some(bitrate);
                        }
                        self.bitrate_probe_pending.push((id, bitrate));
                        if self.bitrate_probe_pending.len() >= 100 {
                            self.flush_bitrate_probe();
                        }
                    }
                }
                Ok(crate::bitrate::ProbeEvent::Done) => {
                    done = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        self.flush_bitrate_probe();
        if done {
            self.bitrate_probe = None;
            self.bitrate_probe_cancel = None;
            self.ui.probe_progress = None;
            self.library_revision += 1;
            self.last_message = Some(tf(
                "msg.bitrates_done",
                &[&self.bitrate_probe_done.to_string()],
            ));
        } else {
            self.bitrate_probe = Some(rx);
            self.ui.probe_progress =
                Some((self.bitrate_probe_done, self.bitrate_probe_total));
        }
    }

    fn flush_bitrate_probe(&mut self) {
        if self.bitrate_probe_pending.is_empty() {
            return;
        }
        let values = std::mem::take(&mut self.bitrate_probe_pending);
        if let Some(db) = &self.db {
            if let Err(e) = db.set_station_bitrates(&values) {
                log::warn!("could not save the probed bitrates: {e}");
            }
        }
    }

    /// Toggle the background watch stream for a station.
    fn toggle_watched(&mut self, id: Uuid) {
        let Some(station) = self
            .stations
            .iter_mut()
            .find(|station| station.id == id)
        else {
            return;
        };
        station.watched = !station.watched;
        let watched = station.watched;
        if let Some(db) = &self.db {
            if let Err(e) = db.set_station_watched(id, watched) {
                log::warn!("could not save the watch flag: {e}");
            }
        }
        self.library_revision += 1;
        if watched {
            if let Some(playback_id) = self.start_watch_stream(id) {
                self.watch_streams.insert(id, playback_id);
            }
        } else {
            self.stop_watch_stream_for(id);
        }
        self.last_message = Some(
            t(if watched {
                "msg.watched"
            } else {
                "msg.unwatched"
            })
            .to_string(),
        );
    }

    /// Turn ICY title changes into track rows linked to the running attempt.
    /// Ignored titles are logged and recorded nowhere (FR-M3).
    fn record_track_changes(&mut self, engine: &EngineSnapshot) {
        let changes: Vec<(PlaybackId, Option<String>)> = engine
            .streams
            .iter()
            .filter_map(|stream| {
                let attempt = self.attempts.get(&stream.id)?;
                let next = stream
                    .now_playing
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .map(str::to_string);
                if attempt.last_title == next {
                    None
                } else {
                    Some((stream.id, next))
                }
            })
            .collect();

        for (id, title) in changes {
            // Always close the previous track and mark the raw title as seen,
            // even if the station disappeared from the library.
            let old_track = self
                .attempts
                .get_mut(&id)
                .and_then(|attempt| attempt.track.take());
            if let Some(old) = old_track {
                let seconds = old.started.elapsed().as_secs().min(i64::MAX as u64) as i64;
                if let Some(db) = &self.db {
                    if let Err(e) = db.end_track(old.id, seconds, "ended") {
                        log::warn!("could not close the previous track: {e}");
                    }
                }
                if let Some(entry) = self.history.iter_mut().find(|entry| entry.id == old.id) {
                    entry.duration_seconds = seconds;
                    entry.status = "ended".to_string();
                }
                self.history_version += 1;
            }
            let had_title = self
                .attempts
                .get(&id)
                .and_then(|attempt| attempt.last_title.as_ref())
                .is_some();
            if had_title && title.is_some() {
                *self.icy_changes.entry(id).or_insert(0) += 1;
            }
            if let Some(attempt) = self.attempts.get_mut(&id) {
                attempt.last_title = title.clone();
            }
            if title.is_some() {
                self.now_playing_at.insert(id, Utc::now());
            } else {
                self.now_playing_at.remove(&id);
            }
            if let Some(title) = title.as_deref() {
                self.ensure_track_metadata(title);
            }

            let Some(title) = title else {
                continue;
            };
            let Some(stream) = engine.streams.iter().find(|stream| stream.id == id) else {
                continue;
            };
            let Some(station) = stream.station_id.and_then(|station_id| {
                self.stations
                    .iter()
                    .find(|station| station.id == station_id)
            }) else {
                continue;
            };
            let station_id = station.id;
            let station_name = station.name.clone();
            let station_url = stream.url.clone();

            let format = self
                .db
                .as_ref()
                .and_then(|db| match db.custom_tag_format(station_id) {
                    Ok(format) => format,
                    Err(e) => {
                        log::warn!("could not read the tag format: {e}");
                        None
                    }
                })
                .unwrap_or_else(|| crate::metadata::default_tag_format(&title).to_string());

            if crate::metadata::title_is_ignored_with_format(
                &self.ignore_list,
                &title,
                Some(&format),
            ) {
                log::info!("ignored track on {station_name}: {title}");
                continue;
            }
            let hits =
                crate::metadata::wish_matches_with_format(&self.wishlist, &title, Some(&format));
            if !hits.is_empty() {
                let labels: Vec<String> = hits
                    .iter()
                    .map(|hit| {
                        format!(
                            "{} — {}",
                            hit.artist.as_deref().unwrap_or("any artist"),
                            hit.song_title.as_deref().unwrap_or("any song")
                        )
                    })
                    .collect();
                let mut record = false;
                let mut activate = false;
                for hit in &hits {
                    match hit.action.trim().to_ascii_lowercase().as_str() {
                        "record" => record = true,
                        "activate" | "play" => activate = true,
                        "record+activate" | "record_activate" => {
                            record = true;
                            activate = true;
                        }
                        _ => {}
                    }
                }
                let mut done: Vec<&str> = Vec::new();
                let mut record_failed = false;
                if record {
                    let watching = self
                        .watch_streams
                        .values()
                        .any(|watch_id| *watch_id == id);
                    let started = if watching {
                        self.promote_watch_recording(id, station_id, &title)
                    } else {
                        self.start_recording_on(id, true, true)
                    };
                    if started {
                        done.push("recording");
                    } else {
                        record_failed = true;
                    }
                }
                if activate {
                    let volume = self
                        .stations
                        .iter()
                        .find(|station| station.id == station_id)
                        .map(|station| station.volume)
                        .unwrap_or(1.0)
                        .clamp(0.0, 1.0);
                    if self.player.snapshot().active.map(|active| active.id) != Some(id) {
                        self.player.set_active(id);
                    }
                    self.scheduler_muted.remove(&id);
                    self.player.set_stream_volume(id, volume);
                    self.settings.volume = volume;
                    done.push("activated");
                }
                if record_failed {
                    done.push("recording failed");
                }
                let detail = if done.is_empty() {
                    "no action".to_string()
                } else {
                    done.join(", ")
                };
                log::info!(
                    "wish-list hits on {station_name}: {}; {detail}",
                    labels.join("; ")
                );
                // A refused record already set the specific reason (low disk or
                // quality filter); do not replace it with a generic hit toast.
                if !(record_failed && !activate) {
                    self.last_message = Some(tf(
                        "msg.wish_hit",
                        &[&format!("{} ({detail})", labels.join("; "))],
                    ));
                }
            }

            let Some(instance_id) = self.attempts.get(&id).map(|attempt| attempt.instance_id)
            else {
                continue;
            };
            let Some(db) = &self.db else {
                continue;
            };
            let tag = crate::metadata::split_title(&title, &format);
            match db.start_track(
                instance_id,
                station_id,
                &station_name,
                Some(&station_url),
                tag.artist.as_deref(),
                tag.title.as_deref(),
            ) {
                Ok(row) => {
                    let track_id = row.id;
                    self.history.insert(0, row);
                    self.history_version += 1;
                    if let Some(attempt) = self.attempts.get_mut(&id) {
                        attempt.track = Some(TrackRecord {
                            id: track_id,
                            started: Instant::now(),
                        });
                    }
                }
                Err(e) => log::warn!("could not record the track: {e}"),
            }
        }
    }

    /// Catalog metadata and cover for the title the speakers are on.
    fn display_track(&self, engine: &EngineSnapshot) -> Option<ui::TrackDisplay> {
        let title = engine
            .active
            .as_ref()
            .and_then(|stream| {
                stream
                    .display_now_playing
                    .as_deref()
                    .or(stream.now_playing.as_deref())
            })
            .filter(|title| !title.trim().is_empty())?;
        let info = self.track_info.get(title)?;
        let mut parts = Vec::new();
        if !info.artist.trim().is_empty() {
            parts.push(info.artist.clone());
        }
        if let Some(album) = info
            .album
            .as_deref()
            .filter(|album| !album.trim().is_empty())
        {
            parts.push(album.to_string());
        }
        Some(ui::TrackDisplay {
            title: if info.title.trim().is_empty() {
                title.to_string()
            } else {
                info.title.clone()
            },
            detail: (!parts.is_empty()).then(|| parts.join(" · ")),
            artwork: self.artwork.get(title).and_then(|state| match state {
                ArtworkState::Ready(texture) => Some(texture.clone()),
                _ => None,
            }),
        })
    }

    /// Schedule a close at the expected end of each recorded track. A live ICY
    /// change always splits first; this only covers a station that stops
    /// sending titles, where an ad or a talking segment would otherwise keep
    /// growing the song file.
    fn refresh_duration_schedules(&mut self) {
        if let Some((id, raw_title, anchor, rx, key, fallback, candidates)) =
            self.duration_poll.take()
        {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("the duration worker stopped".to_string()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            };
            match result {
                None => {
                    self.duration_poll =
                        Some((id, raw_title, anchor, rx, key, fallback, candidates))
                }
                Some(Ok(result)) => {
                    self.duration_cache.insert(key, result.clone());
                    self.duration_failures.remove(&id);
                    self.duration_retry.remove(&id);
                    if let Some(display) = result.display.clone() {
                        self.track_info.insert(raw_title.clone(), display.clone());
                        self.queue_artwork(&raw_title, &display);
                        if fallback {
                            self.retitle_recording(&raw_title, &display);
                        }
                    }
                    if let Some(info) = result.strict {
                        self.arm_split_if_current(
                            id,
                            &raw_title,
                            anchor,
                            info.duration_seconds,
                        );
                        if fallback {
                            self.retitle_recording(&raw_title, &info);
                        }
                    } else {
                        self.continue_duration_lookup(id, &raw_title, anchor, candidates);
                    }
                }
                Some(Err(e)) => {
                    log::warn!("duration lookup failed for {raw_title}: {e}");
                    if !candidates.is_empty() {
                        self.continue_duration_lookup(id, &raw_title, anchor, candidates);
                    } else {
                        self.retry_duration_later(id, &raw_title);
                    }
                }
            }
        }

        let ids: Vec<PlaybackId> = self
            .recording_stations
            .keys()
            .copied()
            .filter(|id| self.recording_splits.get(id).copied().unwrap_or(false))
            .collect();
        for id in ids {
            let title = self
                .attempts
                .get(&id)
                .and_then(|attempt| attempt.last_title.clone());
            if self.duration_title.get(&id) != title.as_ref() {
                match &title {
                    Some(title) => {
                        self.duration_title.insert(id, title.clone());
                    }
                    None => {
                        self.duration_title.remove(&id);
                    }
                }
                self.split_due.remove(&id);
                self.live_fingerprint_due.remove(&id);
                if let Some(title) = title {
                    self.schedule_duration_lookup(id, &title);
                }
            }
        }

        let stale: Vec<PlaybackId> = self
            .split_due
            .iter()
            .filter(|(id, (title, _))| {
                !self.recording_stations.contains_key(id)
                    || self
                        .attempts
                        .get(id)
                        .and_then(|attempt| attempt.last_title.as_ref())
                        != Some(title)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.split_due.remove(&id);
        }

        let now = Utc::now();
        let due: Vec<PlaybackId> = self
            .split_due
            .iter()
            .filter(|(_, (_, at))| now >= *at)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.split_due.remove(&id);
            log::info!("recording {id}: closing at the expected track end");
            self.player.set_recording_boundary(id);
        }

        // Retry one failed lookup at a time; the worker is a single slot.
        if self.duration_poll.is_none() {
            let retry = self
                .duration_retry
                .iter()
                .find(|(_, (_, at))| Instant::now() >= *at)
                .map(|(id, (title, _))| (*id, title.clone()));
            if let Some((id, title)) = retry {
                self.duration_retry.remove(&id);
                if self.recording_stations.contains_key(&id) {
                    self.schedule_duration_lookup(id, &title);
                }
            }
        }
    }

    /// Close a split recording on a live fade, silence or hard drop. A duration
    /// lookup cannot know the station's edit, and an ad break often starts
    /// without a new ICY title.
    fn watch_live_levels(&mut self, engine: &EngineSnapshot) {
        let ids: Vec<PlaybackId> = self
            .recording_stations
            .keys()
            .copied()
            .filter(|id| self.recording_splits.get(id).copied().unwrap_or(false))
            .collect();
        for id in ids {
            let Some(stream) = engine.streams.iter().find(|stream| stream.id == id) else {
                continue;
            };
            let Some(title) = stream.now_playing.clone() else {
                continue;
            };
            if self.live_title.get(&id) != Some(&title) {
                self.live_title.insert(id, title.clone());
                self.live_levels.remove(&id);
                self.live_closed.remove(&id);
                self.live_serial.remove(&id);
                self.live_fingerprint_retry.remove(&id);
            }
            let last_serial = self.live_serial.get(&id).copied().unwrap_or(0);
            if stream.rms_serial <= last_serial {
                continue;
            }
            self.live_serial.insert(id, stream.rms_serial);
            if self.live_closed.get(&id) == Some(&title) {
                continue;
            }
            let elapsed = self
                .now_playing_at
                .get(&id)
                .map(|at| (Utc::now() - *at).num_milliseconds() as f32 / 1000.0)
                .unwrap_or(0.0);
            let format = crate::metadata::default_tag_format(&title);
            let tag = crate::metadata::split_title(&title, format);
            let expected = match (tag.artist, tag.title) {
                (Some(artist), Some(song))
                    if !artist.trim().is_empty() && !song.trim().is_empty() =>
                {
                    let key = crate::duration::cache_key(&artist, &song);
                    self.verified_durations
                        .get(&key)
                        .copied()
                        .or_else(|| {
                            self.duration_cache
                                .get(&key)
                                .and_then(|result| result.strict.as_ref())
                                .map(|info| info.duration_seconds)
                        })
                }
                _ => None,
            };
            let detected = {
                let samples = self.live_levels.entry(id).or_default();
                samples.push(crate::levels::LevelSample {
                    rms: stream.rms,
                    at: elapsed,
                });
                if samples.len() > 240 {
                    samples.drain(..samples.len() - 240);
                }
                crate::levels::song_end(samples, expected)
            };
            if detected {
                self.live_closed.insert(id, title.clone());
                log::info!("recording {id}: live boundary after {elapsed:.1}s");
                self.player.set_recording_boundary(id);
            }
        }
    }

    /// Build the catalog lookups to try, in order: the station's own split,
    /// then an embedded "Artist - Title", then the whole raw title. The last
    /// two cover uploader-style and dash-containing titles.
    fn catalog_candidates(artist: &str, title: &str, raw_title: &str) -> Vec<CatalogCandidate> {
        fn push(
            out: &mut Vec<CatalogCandidate>,
            seen: &mut Vec<String>,
            artist: &str,
            title: &str,
            fallback: bool,
        ) {
            if title.trim().is_empty() {
                return;
            }
            let key = crate::duration::cache_key(artist, title);
            if seen.iter().any(|existing| *existing == key) {
                return;
            }
            seen.push(key.clone());
            out.push((key, artist.to_string(), title.to_string(), fallback));
        }
        let mut out = Vec::new();
        let mut seen = Vec::new();
        push(&mut out, &mut seen, artist, title, false);
        if let Some((inner_artist, inner_title)) = crate::metadata::inner_pair(title) {
            push(&mut out, &mut seen, &inner_artist, &inner_title, true);
        }
        if crate::duration::normalize(raw_title) != crate::duration::normalize(title) {
            push(&mut out, &mut seen, "", raw_title, true);
        }
        out
    }

    fn schedule_duration_lookup(&mut self, id: PlaybackId, raw_title: &str) {
        let format = crate::metadata::default_tag_format(raw_title);
        let tag = crate::metadata::split_title(raw_title, format);
        let (Some(artist), Some(title)) = (tag.artist, tag.title) else {
            return;
        };
        if artist.trim().is_empty() || title.trim().is_empty() {
            return;
        }
        let anchor = self
            .now_playing_at
            .get(&id)
            .copied()
            .unwrap_or_else(Utc::now);
        let candidates = Self::catalog_candidates(&artist, &title, raw_title);
        self.continue_duration_lookup(id, raw_title, anchor, candidates);
        if !self.acoustid_api_key.trim().is_empty() {
            self.live_fingerprint_due.insert(
                id,
                (
                    raw_title.to_string(),
                    Utc::now() + chrono::Duration::seconds(15),
                ),
            );
        }
    }

    /// Try each candidate in order until one yields a strict length. A cached
    /// miss is skipped, and a cached hit is used without another request.
    fn continue_duration_lookup(
        &mut self,
        id: PlaybackId,
        raw_title: &str,
        anchor: chrono::DateTime<chrono::Utc>,
        mut candidates: Vec<CatalogCandidate>,
    ) {
        while !candidates.is_empty() {
            let (key, artist, title, fallback) = candidates.remove(0);
            if let Some(seconds) = self.verified_durations.get(&key).copied() {
                self.arm_split_if_current(id, raw_title, anchor, seconds);
                return;
            }
            if let Some(cached) = self.duration_cache.get(&key).cloned() {
                if let Some(display) = cached.display.clone() {
                    self.track_info.insert(raw_title.to_string(), display.clone());
                    self.queue_artwork(raw_title, &display);
                    if fallback {
                        self.retitle_recording(raw_title, &display);
                    }
                }
                if let Some(info) = cached.strict {
                    self.arm_split_if_current(id, raw_title, anchor, info.duration_seconds);
                    if fallback {
                        self.retitle_recording(raw_title, &info);
                    }
                    return;
                }
                continue;
            }
            self.start_duration_lookup(
                id,
                raw_title.to_string(),
                anchor,
                key,
                artist,
                title,
                fallback,
                candidates,
            );
            return;
        }
    }

    fn arm_split_if_current(
        &mut self,
        id: PlaybackId,
        raw_title: &str,
        anchor: chrono::DateTime<chrono::Utc>,
        seconds: u32,
    ) {
        let same_title = self
            .attempts
            .get(&id)
            .map(|attempt| attempt.last_title.as_deref() == Some(raw_title))
            .unwrap_or(false)
            && self.recording_stations.contains_key(&id);
        if same_title {
            self.arm_split(id, raw_title.to_string(), anchor, seconds);
        }
    }

    fn retry_duration_later(&mut self, id: PlaybackId, raw_title: &str) {
        let failures = self.duration_failures.entry(id).or_insert(0);
        *failures = failures.saturating_add(1);
        if *failures <= 3 {
            self.duration_retry.insert(
                id,
                (
                    raw_title.to_string(),
                    Instant::now() + Duration::from_secs(20),
                ),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start_duration_lookup(
        &mut self,
        id: PlaybackId,
        raw_title: String,
        anchor: chrono::DateTime<chrono::Utc>,
        key: String,
        artist: String,
        title: String,
        fallback: bool,
        candidates: Vec<CatalogCandidate>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-duration".to_string())
            .spawn(move || {
                let _ = tx.send(crate::duration::lookup(&artist, &title));
            });
        match spawned {
            Ok(_) => {
                self.duration_poll =
                    Some((id, raw_title, anchor, rx, key, fallback, candidates));
            }
            Err(e) => {
                log::warn!("could not start the duration lookup: {e}");
                self.retry_duration_later(id, &raw_title);
            }
        }
    }

    /// Rename the open split file when a fallback lookup recovered the real
    /// artist and title from the station's uploader-style label.
    fn retitle_recording(&mut self, raw_title: &str, info: &TrackInfo) {
        let format = crate::metadata::default_tag_format(raw_title);
        let tag = crate::metadata::split_title(raw_title, format);
        let from = crate::metadata::label(tag.artist.as_deref(), tag.title.as_deref());
        if from.is_empty() {
            return;
        }
        let ids: Vec<PlaybackId> = self.recording_stations.keys().copied().collect();
        for id in ids {
            if self
                .attempts
                .get(&id)
                .and_then(|attempt| attempt.last_title.as_deref())
                != Some(raw_title)
            {
                continue;
            }
            self.player
                .retitle_recording(id, &from, &info.artist, &info.title);
            let track_id = self
                .attempts
                .get(&id)
                .and_then(|attempt| attempt.track.as_ref())
                .map(|track| track.id);
            if let (Some(db), Some(track_id)) = (&self.db, track_id) {
                if let Err(e) =
                    db.update_track_metadata(track_id, &info.artist, &info.title)
                {
                    log::warn!("could not update the track metadata: {e}");
                }
            }
            if let Some(track_id) = track_id {
                if let Some(entry) = self.history.iter_mut().find(|entry| entry.id == track_id)
                {
                    entry.artist = Some(info.artist.clone());
                    entry.song_title = Some(info.title.clone());
                    self.history_version += 1;
                }
            }
        }
    }

    fn queue_artwork(&mut self, raw_title: &str, info: &TrackInfo) {
        if self.artwork.contains_key(raw_title) {
            return;
        }
        let Some(url) = info.artwork_url.clone() else {
            self.artwork
                .insert(raw_title.to_string(), ArtworkState::Missing);
            return;
        };
        if self.artwork.len() >= 128 {
            // Drop misses first so ready textures survive the next song.
            self.artwork
                .retain(|_, state| !matches!(state, ArtworkState::Missing));
            if self.artwork.len() >= 128 {
                self.artwork.clear();
            }
        }
        self.artwork
            .insert(raw_title.to_string(), ArtworkState::Loading);
        self.artwork_pending.push((raw_title.to_string(), url));
        self.start_artwork_worker();
    }

    fn start_artwork_worker(&mut self) {
        if self.artwork_poll.is_some() {
            return;
        }
        let Some((title, url)) = self.artwork_pending.pop() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-artwork".to_string())
            .spawn(move || {
                let _ = tx.send(fetch_artwork(&url));
            });
        match spawned {
            Ok(_) => self.artwork_poll = Some((title, rx)),
            Err(e) => log::warn!("could not start the artwork worker: {e}"),
        }
    }

    /// Fetch catalog metadata for the stream bar as soon as a title appears,
    /// whether or not the stream is being recorded.
    fn ensure_track_metadata(&mut self, raw_title: &str) {
        if self.track_info.contains_key(raw_title) {
            return;
        }
        let format = crate::metadata::default_tag_format(raw_title);
        let tag = crate::metadata::split_title(raw_title, format);
        let (Some(artist), Some(title)) = (tag.artist, tag.title) else {
            return;
        };
        if artist.trim().is_empty() || title.trim().is_empty() {
            return;
        }
        let candidates = Self::catalog_candidates(&artist, &title, raw_title);
        if self.metadata_poll.is_some() {
            if !self.metadata_pending.iter().any(|pending| pending == raw_title) {
                self.metadata_pending.push_back(raw_title.to_string());
            }
            return;
        }
        self.continue_metadata_lookup(raw_title, candidates);
    }

    /// Try each candidate in order until one yields a display match. A cached
    /// miss is skipped, and a cached hit is used without another request.
    fn continue_metadata_lookup(
        &mut self,
        raw_title: &str,
        mut candidates: Vec<CatalogCandidate>,
    ) {
        while !candidates.is_empty() {
            let (key, artist, title, fallback) = candidates.remove(0);
            if let Some(cached) = self.duration_cache.get(&key).cloned() {
                if let Some(display) = cached.display {
                    self.track_info.insert(raw_title.to_string(), display.clone());
                    self.queue_artwork(raw_title, &display);
                    if fallback {
                        self.retitle_recording(raw_title, &display);
                    }
                    return;
                }
                continue;
            }
            self.start_metadata_lookup(
                raw_title.to_string(),
                key,
                artist,
                title,
                fallback,
                candidates,
            );
            return;
        }
    }

    fn start_metadata_lookup(
        &mut self,
        raw_title: String,
        key: String,
        artist: String,
        title: String,
        fallback: bool,
        candidates: Vec<CatalogCandidate>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-metadata".to_string())
            .spawn(move || {
                let _ = tx.send(crate::duration::lookup(&artist, &title));
            });
        match spawned {
            Ok(_) => self.metadata_poll = Some((raw_title, key, rx, fallback, candidates)),
            Err(e) => {
                log::warn!("could not start the metadata worker: {e}");
                self.retry_metadata_later(&raw_title);
            }
        }
    }

    fn retry_metadata_later(&mut self, raw_title: &str) {
        let failures = self
            .metadata_failures
            .entry(raw_title.to_string())
            .or_insert(0);
        *failures = failures.saturating_add(1);
        if *failures <= 3 {
            self.metadata_retry = Some((
                raw_title.to_string(),
                Instant::now() + Duration::from_secs(20),
            ));
        }
    }

    /// Identify the opening seconds of a new song and replace the catalog
    /// duration with the exact recording length. Runs 15 seconds after the
    /// title, so the file has enough audio and the song still has minutes left.
    fn refresh_live_fingerprint(&mut self) {
        if let Some((id, raw_title, anchor, rx, key)) = self.live_fingerprint_poll.take() {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("the identification worker stopped".to_string()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            };
            match result {
                None => self.live_fingerprint_poll = Some((id, raw_title, anchor, rx, key)),
                Some(Ok(Some(matched))) => {
                    self.live_fingerprint_retry.remove(&id);
                    if let Some(seconds) =
                        matched.expected_duration_seconds.filter(|seconds| *seconds >= 30)
                    {
                        self.verified_durations.insert(key, seconds);
                        let same_title = self
                            .attempts
                            .get(&id)
                            .map(|attempt| {
                                attempt.last_title.as_deref() == Some(raw_title.as_str())
                            })
                            .unwrap_or(false)
                            && self.recording_stations.contains_key(&id);
                        if same_title {
                            log::info!(
                                "recording {id}: AcoustID length {seconds}s for {raw_title}"
                            );
                            self.arm_split(id, raw_title, anchor, seconds);
                        }
                    }
                }
                Some(Ok(None)) => log::debug!("no AcoustID match for {raw_title}"),
                Some(Err(e)) => {
                    log::warn!("live identification failed for {raw_title}: {e}");
                    let retries = self.live_fingerprint_retry.entry(id).or_insert(0);
                    *retries = retries.saturating_add(1);
                    if *retries <= 3 {
                        self.live_fingerprint_due.insert(
                            id,
                            (
                                raw_title.clone(),
                                Utc::now() + chrono::Duration::seconds(30),
                            ),
                        );
                    }
                }
            }
        }
        if self.live_fingerprint_poll.is_some() {
            return;
        }
        let now = Utc::now();
        let due: Vec<PlaybackId> = self
            .live_fingerprint_due
            .iter()
            .filter(|(_, (_, at))| now >= *at)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Some((raw_title, _)) = self.live_fingerprint_due.remove(&id) else {
                continue;
            };
            if !self.recording_stations.contains_key(&id) {
                continue;
            }
            if self
                .attempts
                .get(&id)
                .and_then(|attempt| attempt.last_title.as_deref())
                != Some(raw_title.as_str())
            {
                continue;
            }
            let Some(path) = self.player.current_recording_path(id) else {
                self.live_fingerprint_due
                    .insert(id, (raw_title, now + chrono::Duration::seconds(1)));
                continue;
            };
            let format = crate::metadata::default_tag_format(&raw_title);
            let tag = crate::metadata::split_title(&raw_title, format);
            let (Some(artist), Some(title)) = (tag.artist, tag.title) else {
                continue;
            };
            if artist.trim().is_empty() || title.trim().is_empty() {
                continue;
            }
            let key = crate::duration::cache_key(&artist, &title);
            let anchor = self.now_playing_at.get(&id).copied().unwrap_or(now);
            let api_key = self.acoustid_api_key.trim().to_string();
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("viberadio-identify".to_string())
                .spawn(move || {
                    let api_key = (!api_key.is_empty()).then_some(api_key.as_str());
                    let _ = tx.send(crate::fingerprint::identify_prefix(&path, 12, api_key));
                });
            match spawned {
                Ok(_) => {
                    self.live_fingerprint_poll = Some((id, raw_title, anchor, rx, key));
                    break;
                }
                Err(e) => log::warn!("could not start the identification worker: {e}"),
            }
        }
    }

    fn arm_split(
        &mut self,
        id: PlaybackId,
        raw_title: String,
        anchor: chrono::DateTime<chrono::Utc>,
        seconds: u32,
    ) {
        if self.live_closed.get(&id) == Some(&raw_title) {
            return;
        }
        // The live level boundary handles most ends; this close is the fallback
        // for a hard cut into an ad. A small grace avoids cutting a song whose
        // station edit runs a little past the catalog length.
        const GRACE_SECONDS: i64 = 3;
        let due = anchor + chrono::Duration::seconds(i64::from(seconds) + GRACE_SECONDS);
        if due <= Utc::now() + chrono::Duration::seconds(2) {
            return;
        }
        log::info!("recording {id}: scheduled close at {due} for {raw_title}");
        self.split_due.insert(id, (raw_title, due));
    }

    fn set_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.settings.volume = volume;
        let engine = self.player.snapshot();
        if let Some(active) = &engine.active {
            self.scheduler_muted.remove(&active.id);
            self.player.set_stream_volume(active.id, volume);
            if let Some(station_id) = active.station_id {
                self.persist_station_volume(station_id, volume);
            }
            self.persist_attempt_volume(active.id, volume);
        } else if let Some(station_id) = self.selected_station {
            self.persist_station_volume(station_id, volume);
        }
    }

    fn set_stream_volume(&mut self, playback_id: PlaybackId, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        self.scheduler_muted.remove(&playback_id);
        self.player.set_stream_volume(playback_id, volume);
        if let Some(stream) = self
            .player
            .snapshot()
            .streams
            .into_iter()
            .find(|stream| stream.id == playback_id)
        {
            if let Some(station_id) = stream.station_id {
                self.persist_station_volume(station_id, volume);
            }
            if self.player.snapshot().active.map(|active| active.id) == Some(stream.id) {
                self.settings.volume = volume;
            }
        }
        self.persist_attempt_volume(playback_id, volume);
    }

    fn persist_station_volume(&mut self, station_id: Uuid, volume: f32) {
        if let Some(station) = self
            .stations
            .iter_mut()
            .find(|station| station.id == station_id)
        {
            station.volume = volume;
        }
        if let Some(db) = &self.db {
            if let Err(e) = db.set_station_volume(station_id, volume) {
                log::warn!("could not persist the station volume: {e}");
            }
        }
    }

    fn persist_attempt_volume(&mut self, playback_id: PlaybackId, volume: f32) {
        let Some(attempt) = self.attempts.get(&playback_id) else {
            return;
        };
        if let Some(db) = &self.db {
            if let Err(e) = db.update_play_instance_volume(attempt.instance_id, volume) {
                log::warn!("could not persist the attempt volume: {e}");
            }
        }
    }

    /// Apply a per-station gain trim live to every open stream of that station.
    /// The DSP chain runs after decoding, so recordings are unaffected.
    fn apply_station_gain(&mut self, station_id: Uuid, gain_db: f32) {
        let gain_db = gain_db.clamp(-12.0, 12.0);
        if let Some(station) = self
            .stations
            .iter_mut()
            .find(|station| station.id == station_id)
        {
            station.gain_db = gain_db;
        }
        let ids: Vec<PlaybackId> = self
            .player
            .snapshot()
            .streams
            .iter()
            .filter(|stream| stream.station_id == Some(station_id))
            .map(|stream| stream.id)
            .collect();
        for playback_id in ids {
            self.player.set_stream_gain(playback_id, gain_db);
        }
    }

    /// Write the current trim once the slider is released or the window closes,
    /// rather than once per drag frame.
    fn persist_station_gain(&mut self, station_id: Uuid, gain_db: f32) {
        if let Some(db) = &self.db {
            if let Err(e) = db.set_station_gain(station_id, gain_db.clamp(-12.0, 12.0)) {
                log::warn!("could not persist the station gain: {e}");
                self.last_message = Some(tf("msg.gain_save_failed", &[&e.to_string()]));
            }
        }
    }

    fn refresh_latest_backup(&mut self) {
        self.latest_backup = self
            .data_dir_path
            .as_ref()
            .and_then(|dir| DbManager::latest_backup(dir));
    }

    fn backup_now(&mut self) {
        let Some(dir) = self.data_dir_path.clone() else {
            self.last_message = Some(t("msg.no_data_folder").to_string());
            return;
        };
        let Some(db) = &self.db else {
            self.last_message = Some(t("msg.no_database").to_string());
            return;
        };
        let path = dir.join(format!(
            "viberadio-backup-{}.db",
            chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
        ));
        let result = db.backup_to(&path);
        match result {
            Ok(()) => {
                DbManager::prune_backups(&dir, 10);
                self.refresh_latest_backup();
                self.last_message = Some(tf("msg.backup_saved", &[&path.display().to_string()]));
            }
            Err(e) => self.last_message = Some(tf("msg.backup_failed", &[&e.to_string()])),
        }
    }

    fn request_restore_latest(&mut self) {
        let Some(dir) = self.data_dir_path.clone() else {
            self.last_message = Some(t("msg.no_data_folder").to_string());
            return;
        };
        let Some(name) = self
            .latest_backup
            .as_ref()
            .and_then(|backup| backup.file_name())
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            self.last_message = Some(t("msg.no_backup").to_string());
            return;
        };
        let marker = dir.join("viberadio-restore.pending");
        match std::fs::write(&marker, name) {
            Ok(()) => self.last_message = Some(t("msg.restore_restart").to_string()),
            Err(e) => {
                self.last_message = Some(tf("msg.restore_schedule_failed", &[&e.to_string()]))
            }
        }
    }

    fn cancel_pending_restore(&mut self) {
        let Some(dir) = self.data_dir_path.as_ref() else {
            self.last_message = Some(t("msg.no_data_folder").to_string());
            return;
        };
        match std::fs::remove_file(dir.join("viberadio-restore.pending")) {
            Ok(()) => self.last_message = Some(t("msg.restore_cancelled").to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.last_message = Some(t("msg.no_pending_restore").to_string())
            }
            Err(e) => self.last_message = Some(tf("msg.restore_cancel_failed", &[&e.to_string()])),
        }
    }

    fn change_data_folder(&mut self) {
        if std::env::var_os("VIBERADIO_DATA_DIR").is_some() {
            self.last_message = Some(t("msg.data_env_override").to_string());
            return;
        }
        let input = self.data_dir_input.trim().to_string();
        if input.is_empty() {
            self.last_message = Some(t("msg.data_folder_empty").to_string());
            return;
        }
        let target = std::path::PathBuf::from(&input);
        if !target.is_absolute() {
            self.last_message = Some(t("msg.data_folder_absolute").to_string());
            return;
        }
        let target =
            match std::fs::create_dir_all(&target).and_then(|()| std::fs::canonicalize(&target)) {
                Ok(path) => path,
                Err(e) => {
                    self.last_message = Some(tf("msg.data_folder_failed", &[&e.to_string()]));
                    return;
                }
            };
        let current = self
            .data_dir_path
            .as_ref()
            .map(|dir| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()))
            .unwrap_or_default();
        if target == current {
            self.last_message = Some(t("msg.data_folder_same").to_string());
            return;
        }
        let target_db = target.join("viberadio.db");
        if target_db.exists() {
            self.last_message = Some(t("msg.data_folder_exists").to_string());
            return;
        }
        let Some(default) = default_data_dir() else {
            self.last_message = Some(t("msg.no_default_data_folder").to_string());
            return;
        };
        let Some(source_dir) = self.data_dir_path.clone() else {
            self.last_message = Some(t("msg.no_data_folder").to_string());
            return;
        };
        let marker = default.join("viberadio-copy.pending");
        let marker_json = serde_json::json!({
            "source": source_dir.join("viberadio.db").display().to_string(),
            "target": target_db.display().to_string(),
        })
        .to_string();
        if let Err(e) =
            std::fs::create_dir_all(&default).and_then(|()| std::fs::write(&marker, marker_json))
        {
            self.last_message = Some(tf("msg.data_copy_failed", &[&e.to_string()]));
            return;
        }
        let target_is_default = std::fs::canonicalize(&default)
            .map(|path| path == target)
            .unwrap_or(default == target);
        let pointer = if target_is_default {
            None
        } else {
            Some(target.as_path())
        };
        match set_data_dir_pointer(&default, pointer) {
            Ok(()) => {
                self.data_dir_input = target.display().to_string();
                self.last_message = Some(tf("msg.data_restart", &[&target.display().to_string()]));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&marker);
                self.last_message = Some(tf("msg.data_pointer_failed", &[&e.to_string()]));
            }
        }
    }

    fn set_dsp_preset(&mut self, preset: DspPreset) {
        self.dsp_preset = preset;
        self.player.set_dsp_preset(preset);
        if let Some(db) = &self.db {
            if let Err(e) = db.set_setting("dsp_preset", preset.as_str()) {
                log::warn!("could not persist the DSP preset: {e}");
            }
        }
        self.last_message = Some(tf("msg.dsp", &[preset.label()]));
    }

    // -----------------------------------------------------------------------
    // Library actions
    // -----------------------------------------------------------------------

    fn toggle_favorite(&mut self, id: Uuid) {
        if let Some(db) = &self.db {
            if let Some(mut station) = self.stations.iter().find(|s| s.id == id).cloned() {
                station.is_favorite = !station.is_favorite;
                if let Err(e) = db.save_station(&station) {
                    self.last_message = Some(tf("msg.favorite_failed", &[&e.to_string()]));
                    return;
                }
                self.reload_stations();
            }
        } else if let Some(station) = self.stations.iter_mut().find(|s| s.id == id) {
            station.is_favorite = !station.is_favorite;
            self.library_revision += 1;
        }
    }

    fn export_playlist(&mut self) {
        match (&self.db, data_dir()) {
            (Some(db), Some(dir)) => {
                let path = dir.join("viberadio-playlist.m3u");
                match db.export_m3u(&path) {
                    Ok(n) => {
                        self.last_message = Some(tf(
                            "msg.exported",
                            &[&n.to_string(), &path.display().to_string()],
                        ))
                    }
                    Err(e) => self.last_message = Some(tf("msg.export_failed", &[&e.to_string()])),
                }
            }
            (None, _) => self.last_message = Some(t("msg.no_database").to_string()),
            (_, None) => self.last_message = Some(t("msg.no_writable_dir").to_string()),
        }
    }

    fn cycle_sleep_timer(&mut self) {
        let next = next_sleep_minutes(self.sleep_minutes);
        self.sleep_minutes = next;
        if next == 0 {
            self.sleep_at = None;
            self.last_message = Some(t("msg.sleep_off").to_string());
        } else {
            self.sleep_at = Some(Instant::now() + Duration::from_secs(u64::from(next) * 60));
            self.last_message = Some(tf("msg.sleep_set", &[&next.to_string()]));
        }
    }

    fn reload_sessions(&mut self) {
        if let Some(db) = &self.db {
            match db.load_sessions() {
                Ok(sessions) => self.sessions = sessions,
                Err(e) => log::warn!("could not load sessions: {e}"),
            }
        }
    }

    fn save_current_session(&mut self) {
        let Some(db) = &self.db else {
            self.last_message = Some(t("msg.no_database").to_string());
            return;
        };
        let mut station_ids: Vec<uuid::Uuid> = self
            .player
            .snapshot()
            .streams
            .iter()
            .filter_map(|stream| stream.station_id)
            .collect();
        station_ids.sort();
        station_ids.dedup();
        if station_ids.is_empty() {
            if let Some(selected) = self.selected_station {
                station_ids.push(selected);
            }
        }
        if station_ids.is_empty() {
            self.last_message = Some(t("msg.play_or_select").to_string());
            return;
        }
        let name = if self.session_name.trim().is_empty() {
            format!("Session {}", chrono::Local::now().format("%Y-%m-%d %H:%M"))
        } else {
            self.session_name.trim().to_string()
        };
        match db.save_session(&name, &station_ids) {
            Ok(_) => {
                self.session_name.clear();
                self.reload_sessions();
                self.last_message = Some(tf("msg.session_saved", &[&name]));
            }
            Err(e) => self.last_message = Some(tf("msg.session_save_failed", &[&e.to_string()])),
        }
    }

    fn open_session(&mut self, id: uuid::Uuid) {
        let Some(db) = &self.db else {
            return;
        };
        match db.load_session_stations(id) {
            Ok(stations) => {
                let count = stations.len();
                for station_id in stations {
                    self.start_station(station_id);
                }
                self.last_message = Some(tf("msg.session_opened", &[&count.to_string()]));
            }
            Err(e) => self.last_message = Some(tf("msg.session_open_failed", &[&e.to_string()])),
        }
    }

    fn delete_session(&mut self, id: uuid::Uuid) {
        if let Some(db) = &self.db {
            if let Err(e) = db.delete_session(id) {
                self.last_message = Some(tf("msg.session_delete_failed", &[&e.to_string()]));
            } else {
                self.reload_sessions();
                self.last_message = Some(t("msg.session_deleted").to_string());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Schedules
    // -----------------------------------------------------------------------

    fn reload_schedules(&mut self) {
        if let Some(db) = &self.db {
            match db.load_schedules() {
                Ok(schedules) => self.schedules = schedules,
                Err(e) => log::warn!("could not load schedules: {e}"),
            }
        }
    }

    fn begin_schedule(&mut self, station_id: Option<Uuid>) {
        let station_id = station_id
            .or(self.selected_station)
            .or_else(|| self.stations.first().map(|station| station.id));
        self.schedule_editor = Some(ScheduleEditor {
            id: None,
            draft: ScheduleDraft::new(station_id, Local::now().naive_local()),
            error: None,
        });
        self.ui.show_schedules = true;
    }

    fn edit_schedule(&mut self, id: Uuid) {
        if let Some(schedule) = self.schedules.iter().find(|schedule| schedule.id == id) {
            self.schedule_editor = Some(ScheduleEditor {
                id: Some(id),
                draft: ScheduleDraft::from_schedule(schedule),
                error: None,
            });
        }
    }

    fn save_schedule_editor(&mut self) {
        let Some(editor) = self.schedule_editor.as_ref() else {
            return;
        };
        let id = editor.id.unwrap_or_else(Uuid::new_v4);
        let parsed = editor.draft.to_schedule(id, Local::now().naive_local());
        match parsed {
            Ok(mut schedule) => {
                // Editing a schedule must not forget which occurrence already fired.
                if let Some(existing) = self.schedules.iter().find(|existing| existing.id == id) {
                    schedule.last_fired_at = existing.last_fired_at;
                }
                let Some(db) = &self.db else {
                    if let Some(editor) = self.schedule_editor.as_mut() {
                        editor.error = Some(t("msg.no_database").to_string());
                    }
                    return;
                };
                match db.save_schedule(&schedule) {
                    Ok(()) => {
                        // The active window holds the old times/action/finish.
                        // Cancel it; an enabled, still-due schedule re-fires on
                        // the next tick with the new values.
                        self.cancel_schedule_window(id);
                        self.schedule_editor = None;
                        self.reload_schedules();
                        self.last_message = Some(t("msg.schedule_saved").to_string());
                    }
                    Err(e) => {
                        if let Some(editor) = self.schedule_editor.as_mut() {
                            editor.error = Some(tf("msg.schedule_save_failed", &[&e.to_string()]));
                        }
                    }
                }
            }
            Err(e) => {
                if let Some(editor) = self.schedule_editor.as_mut() {
                    editor.error = Some(e);
                }
            }
        }
    }

    fn delete_schedule(&mut self, id: Uuid) {
        // A deleted schedule must not keep stopping a stream or closing the app.
        self.cancel_schedule_window(id);
        if let Some(db) = &self.db {
            if let Err(e) = db.delete_schedule(id) {
                self.last_message = Some(tf("msg.schedule_delete_failed", &[&e.to_string()]));
                return;
            }
        }
        self.schedules.retain(|schedule| schedule.id != id);
        if self.schedule_editor.as_ref().and_then(|editor| editor.id) == Some(id) {
            self.schedule_editor = None;
        }
        self.last_message = Some(t("msg.schedule_deleted").to_string());
    }

    /// Remove stations after the confirmation dialog. Live streams for them are
    /// stopped first so no writer is left with a deleted row.
    fn remove_stations(&mut self, ids: &[Uuid]) {
        if ids.is_empty() {
            return;
        }
        let snapshot = self.player.snapshot();
        for stream in snapshot.streams {
            if stream.station_id.is_some_and(|id| ids.contains(&id)) {
                self.stop_one_stream(stream.id);
            }
        }
        let Some(db) = &self.db else {
            self.last_message = Some(t("msg.no_database").to_string());
            return;
        };
        match db.delete_stations(ids) {
            Ok(removed) => {
                self.reload_stations();
                self.ui.selected_stations.clear();
                self.ui.selection_anchor = None;
                if self.selected_station.is_some_and(|id| ids.contains(&id)) {
                    self.selected_station = None;
                }
                self.last_message =
                    Some(tf("msg.stations_removed", &[&removed.to_string()]));
            }
            Err(e) => {
                log::warn!("could not remove stations: {e}");
                self.last_message = Some(tf("msg.remove_failed", &[&e.to_string()]));
            }
        }
    }

    fn render_remove_dialog(&mut self, ctx: &egui::Context) {
        let Some(ids) = self.confirm_remove.clone() else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new(t("dialog.remove_stations"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(tf(
                    "dialog.remove_stations_body",
                    &[&ids.len().to_string()],
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(t("dialog.remove")).clicked() {
                        confirm = true;
                    }
                    if ui.button(t("dialog.cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
        if confirm {
            self.confirm_remove = None;
            self.remove_stations(&ids);
        } else if cancel || !open {
            self.confirm_remove = None;
        }
    }

    fn set_schedule_enabled(&mut self, id: Uuid, enabled: bool) {
        let Some(schedule) = self.schedules.iter_mut().find(|schedule| schedule.id == id) else {
            return;
        };
        schedule.enabled = enabled;
        let updated = schedule.clone();
        if !enabled {
            self.cancel_schedule_window(id);
        }
        if let Some(db) = &self.db {
            if let Err(e) = db.save_schedule(&updated) {
                log::warn!("could not save the schedule: {e}");
                self.last_message = Some(tf("msg.schedule_save_failed", &[&e.to_string()]));
            }
        }
    }

    /// Remove the active window of a schedule without running its finish
    /// action, and stop the stream/recording it owns.
    fn cancel_schedule_window(&mut self, id: Uuid) {
        if let Some(effect) = self.scheduler.cancel(id) {
            self.apply_scheduler_effect(effect);
        }
    }

    /// Evaluate schedules against the wall clock, persist the fired markers
    /// before running effects, then apply start/stop/finish in order.
    fn tick_schedules(&mut self) {
        let now = Utc::now();
        let known: HashSet<Uuid> = self.stations.iter().map(|station| station.id).collect();
        let result = self
            .scheduler
            .tick(&self.schedules, &known, now, &scheduler::resolve_local);

        // Persist before effects: a crash after starting a stream must not let
        // the same occurrence start it again on the next launch.
        for (id, at) in &result.marks {
            if let Some(schedule) = self
                .schedules
                .iter_mut()
                .find(|schedule| schedule.id == *id)
            {
                schedule.last_fired_at = Some(*at);
            }
            if let Some(db) = &self.db {
                if let Err(e) = db.mark_schedule_fired(*id, *at) {
                    log::warn!("could not mark schedule {id} fired: {e}");
                }
            }
        }

        for effect in result.effects {
            self.apply_scheduler_effect(effect);
        }

        if let Some(message) = self.scheduler.message.take() {
            self.last_message = Some(message);
        }
    }

    fn apply_scheduler_effect(&mut self, effect: SchedulerEffect) {
        match effect {
            SchedulerEffect::Start {
                schedule_id,
                station_id,
                action,
            } => {
                let (playback_id, owns_stream, owns_recording) =
                    self.start_scheduled_station(station_id, action);
                self.scheduler
                    .attach(schedule_id, playback_id, owns_stream, owns_recording);
            }
            SchedulerEffect::Stop {
                playback_id,
                owns_stream,
                owns_recording,
                ..
            } => match playback_id {
                // Stop only what this schedule opened; an already-playing
                // station it merely recorded must keep playing.
                Some(id) if owns_stream => self.stop_one_stream(id),
                Some(id) if owns_recording => {
                    self.stop_recording_on(id);
                }
                _ => {}
            },
            SchedulerEffect::Finish { action, .. } => self.pending_finish.push(action),
        }
    }

    fn start_scheduled_station(
        &mut self,
        station_id: Uuid,
        action: ScheduleAction,
    ) -> (Option<PlaybackId>, bool, bool) {
        let muted = !action.plays();
        let previous_active = self.player.snapshot().active.map(|stream| stream.id);
        let previous_selection = self.selected_station;
        let (playback_id, owns_stream) = self.start_station_with(station_id, muted.then_some(0.0));
        if muted && owns_stream {
            if let Some(id) = playback_id {
                self.scheduler_muted.insert(id);
            }
        }
        let owns_recording = if action.records() {
            playback_id
                .map(|id| self.start_recording_on(id, action.split(), false))
                .unwrap_or(false)
        } else {
            false
        };
        if muted && owns_stream {
            // A record-only schedule must not take over the player bar or the
            // user's selection; it only adds a muted recording stream.
            self.selected_station = previous_selection;
            self.connecting_station = None;
            if let Some(previous) = previous_active {
                if self
                    .player
                    .snapshot()
                    .streams
                    .iter()
                    .any(|stream| stream.id == previous)
                {
                    self.player.set_active(previous);
                }
            }
        }
        (playback_id, owns_stream, owns_recording)
    }

    fn next_schedule_label(&self) -> Option<(String, String)> {
        let now = Utc::now();
        let (schedule, occurrence) =
            scheduler::next_schedule(&self.schedules, now, &scheduler::resolve_local)?;
        let station = self
            .stations
            .iter()
            .find(|station| station.id == schedule.station_id)?;
        Some((
            format!(
                "Next {}",
                scheduler::format_countdown(occurrence.start - now)
            ),
            format!(
                "Next scheduled station: {} at {}",
                station.name,
                occurrence.start.with_timezone(&Local).format("%a %H:%M")
            ),
        ))
    }

    fn split_latest_recording(&mut self) {
        let dir = std::path::Path::new(&self.recording_dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.last_message = Some(t("msg.recording_dir_missing").to_string());
            return;
        };
        let mut logs: Vec<std::path::PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
            .collect();
        logs.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        });
        let Some(log) = logs.pop() else {
            self.last_message = Some(t("msg.no_recording_log").to_string());
            return;
        };
        let Some(stem) = log.file_stem() else {
            self.last_message = Some(t("msg.recording_log_no_name").to_string());
            return;
        };
        let Some(audio) = std::fs::read_dir(dir).ok().and_then(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .find(|path| {
                    path.file_stem() == Some(stem)
                        && path.extension().and_then(|value| value.to_str()) != Some("jsonl")
                })
        }) else {
            self.last_message = Some(t("msg.recording_not_found").to_string());
            return;
        };
        let log_text = std::fs::read_to_string(&log).unwrap_or_default();
        let first: serde_json::Value = log_text
            .lines()
            .find_map(|line| serde_json::from_str(line).ok())
            .unwrap_or_default();
        let log_audio = first
            .get("file")
            .and_then(serde_json::Value::as_str)
            .map(|name| dir.join(name))
            .filter(|path| path.is_file());
        let station = first
            .get("station")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("VibeRadio");
        let Some(audio) = log_audio.or(Some(audio)) else {
            self.last_message = Some(t("msg.recording_not_found").to_string());
            return;
        };
        let out = dir.join("split");
        match crate::recording::split_continuous_recording(
            &audio,
            &log,
            &out,
            station,
            &self.recording_template,
        ) {
            Ok(files) => {
                self.last_message = Some(tf(
                    "msg.split_done",
                    &[&files.len().to_string(), &out.display().to_string()],
                ));
            }
            Err(e) => self.last_message = Some(tf("msg.split_failed", &[&e.to_string()])),
        }
    }

    fn reload_wishlist(&mut self) {
        if let Some(db) = &self.db {
            match db.load_wishlist() {
                Ok(list) => self.wishlist = list,
                Err(e) => log::warn!("could not reload the wish list: {e}"),
            }
        }
    }

    fn insert_wish(&mut self, entry: crate::metadata::WishEntry) {
        if self.wishlist.iter().any(|existing| {
            existing.artist == entry.artist
                && existing.song_title == entry.song_title
                && existing.action == entry.action
        }) {
            self.last_message = Some(t("wishlist.exists").to_string());
            return;
        }
        let result = self.db.as_ref().map(|db| db.add_wishlist_entry(&entry));
        match result {
            Some(Ok(_)) => {
                self.reload_wishlist();
                self.last_message = Some(t("msg.wish_added").to_string());
            }
            Some(Err(e)) => {
                self.last_message = Some(tf("msg.wish_failed", &[&e.to_string()]));
            }
            None => self.last_message = Some(t("msg.no_database").to_string()),
        }
    }

    fn export_wishlist(&mut self) {
        let Some(dir) = self.data_dir_path.clone() else {
            self.last_message = Some(t("msg.no_writable_dir").to_string());
            return;
        };
        let path = dir.join("viberadio-wishlist.json");
        let entries: Vec<WishFileEntry> = self
            .wishlist
            .iter()
            .map(|entry| WishFileEntry {
                artist: entry.artist.clone(),
                song_title: entry.song_title.clone(),
                action: entry.action.clone(),
                compare: entry.compare.as_str().to_string(),
            })
            .collect();
        let result = serde_json::to_string_pretty(&entries)
            .map_err(|e| e.to_string())
            .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()));
        match result {
            Ok(()) => {
                self.last_message = Some(tf(
                    "msg.wishlist_exported",
                    &[&entries.len().to_string(), &path.display().to_string()],
                ))
            }
            Err(e) => self.last_message = Some(tf("msg.wishlist_export_failed", &[&e])),
        }
    }

    fn import_wishlist(&mut self) {
        let Some(dir) = self.data_dir_path.clone() else {
            self.last_message = Some(t("msg.no_writable_dir").to_string());
            return;
        };
        let path = dir.join("viberadio-wishlist.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                self.last_message = Some(tf(
                    "msg.wishlist_import_failed",
                    &[&format!("{}: {e}", path.display())],
                ));
                return;
            }
        };
        let entries: Vec<WishFileEntry> = match serde_json::from_str(&text) {
            Ok(entries) => entries,
            Err(e) => {
                self.last_message =
                    Some(tf("msg.wishlist_import_failed", &[&e.to_string()]));
                return;
            }
        };
        let Some(db) = &self.db else {
            self.last_message = Some(t("msg.no_database").to_string());
            return;
        };
        let mut seen: HashSet<(Option<String>, Option<String>, String, String)> = self
            .wishlist
            .iter()
            .map(|entry| {
                (
                    entry.artist.clone(),
                    entry.song_title.clone(),
                    entry.action.clone(),
                    entry.compare.as_str().to_string(),
                )
            })
            .collect();
        let mut imported = 0usize;
        let mut skipped = 0usize;
        for entry in entries {
            let action = if entry.action.trim().is_empty() {
                "record".to_string()
            } else {
                entry.action
            };
            let compare = crate::metadata::CompareMode::parse(&entry.compare);
            let key = (
                entry.artist.clone(),
                entry.song_title.clone(),
                action.clone(),
                compare.as_str().to_string(),
            );
            if !seen.insert(key) {
                skipped += 1;
                continue;
            }
            let wish = crate::metadata::WishEntry {
                id: 0,
                artist: entry.artist,
                song_title: entry.song_title,
                action,
                compare,
            };
            match db.add_wishlist_entry(&wish) {
                Ok(_) => imported += 1,
                Err(e) => {
                    log::warn!("could not import a wish: {e}");
                    skipped += 1;
                }
            }
        }
        self.reload_wishlist();
        self.last_message = Some(tf(
            "msg.wishlist_imported",
            &[&imported.to_string(), &skipped.to_string()],
        ));
    }

    fn add_wish_entry(&mut self) {
        let artist = self.wish_artist.trim().to_string();
        let song = self.wish_song.trim().to_string();
        if artist.is_empty() && song.is_empty() {
            self.last_message = Some(t("wishlist.need_field").to_string());
            return;
        }
        let entry = crate::metadata::WishEntry {
            id: 0,
            artist: (!artist.is_empty()).then_some(artist),
            song_title: (!song.is_empty()).then_some(song),
            action: self.wish_action.clone(),
            compare: self.wish_compare,
        };
        self.wish_artist.clear();
        self.wish_song.clear();
        self.insert_wish(entry);
    }

    fn delete_wish(&mut self, id: i64) {
        let result = self.db.as_ref().map(|db| db.delete_wishlist_entry(id));
        match result {
            Some(Ok(())) => {
                self.reload_wishlist();
                self.last_message = Some(t("msg.wish_removed").to_string());
            }
            Some(Err(e)) => {
                self.last_message = Some(tf("msg.wish_failed", &[&e.to_string()]));
            }
            None => self.last_message = Some(t("msg.no_database").to_string()),
        }
    }

    fn want_song(&mut self, artist: Option<String>, title: String) {
        let title = title.trim().to_string();
        if title.is_empty() {
            self.last_message = Some(t("msg.no_track").to_string());
            return;
        }
        self.insert_wish(crate::metadata::WishEntry {
            id: 0,
            artist: artist
                .map(|artist| artist.trim().to_string())
                .filter(|artist| !artist.is_empty()),
            song_title: Some(title),
            action: "record".to_string(),
            compare: crate::metadata::CompareMode::Contains,
        });
    }

    fn want_current_track(&mut self) {
        let Some(raw) = self
            .player
            .snapshot()
            .active
            .and_then(|stream| stream.now_playing)
        else {
            self.last_message = Some(t("msg.no_track").to_string());
            return;
        };
        let format = crate::metadata::default_tag_format(&raw);
        let tag = crate::metadata::split_title(&raw, format);
        if tag.artist.is_none() && tag.title.is_none() {
            self.last_message = Some(t("msg.no_track").to_string());
            return;
        }
        self.insert_wish(crate::metadata::WishEntry {
            id: 0,
            artist: tag.artist,
            song_title: tag.title,
            action: "record".to_string(),
            compare: crate::metadata::CompareMode::Contains,
        });
    }

    fn toggle_recording(&mut self) {
        let Some(active) = self.player.snapshot().active else {
            self.last_message = Some(t("msg.select_play").to_string());
            return;
        };
        let Some(station_id) = active.station_id else {
            self.last_message = Some(t("msg.select_play").to_string());
            return;
        };
        self.toggle_background_recording(station_id);
    }

    /// Close the recording writer for one stream and hand its join to the
    /// per-frame poll. Returns false when the stream was not recording.
    fn stop_recording_on(&mut self, playback_id: PlaybackId) -> bool {
        self.recording_stations.remove(&playback_id);
        self.recording_splits.remove(&playback_id);
        self.recording_started_at.remove(&playback_id);
        self.icy_changes.remove(&playback_id);
        self.recording_labels.remove(&playback_id);
        self.split_due.remove(&playback_id);
        self.duration_title.remove(&playback_id);
        self.live_levels.remove(&playback_id);
        self.live_closed.remove(&playback_id);
        self.live_serial.remove(&playback_id);
        self.live_title.remove(&playback_id);
        self.live_fingerprint_due.remove(&playback_id);
        if self
            .live_fingerprint_poll
            .as_ref()
            .map(|(id, ..)| *id == playback_id)
            .unwrap_or(false)
        {
            self.live_fingerprint_poll = None;
        }
        if self
            .duration_poll
            .as_ref()
            .map(|(id, ..)| *id == playback_id)
            .unwrap_or(false)
        {
            self.duration_poll = None;
        }
        if let (Some(db), Some(attempt)) = (&self.db, self.attempts.get(&playback_id)) {
            if let Err(e) = db.set_play_instance_recording(attempt.instance_id, false) {
                log::warn!("could not clear the recording flag: {e}");
            }
        }
        match self.player.stop_recording_async(playback_id) {
            Some(handle) => {
                self.finalising.push(FinalisingRecording {
                    playback_id,
                    handle,
                });
                true
            }
            None => false,
        }
    }

    /// Join writers that have finished, without blocking the UI frame.
    fn poll_finalising(&mut self) {
        if self.finalising.is_empty() {
            return;
        }
        let mut still = Vec::new();
        let mut finished = Vec::new();
        for item in self.finalising.drain(..) {
            if item.handle.is_finished() {
                finished.push(item);
            } else {
                still.push(item);
            }
        }
        self.finalising = still;
        for item in finished {
            match item.handle.join() {
                Ok(summary) => {
                    log::info!("recording for stream {} finalised", item.playback_id);
                    self.finish_recording_summary(&summary);
                    self.last_message = Some(recording_summary_message(&summary));
                }
                Err(_) => self.last_message = Some(t("msg.recording_writer_failed").to_string()),
            }
        }
    }

    /// Run deferred finish actions once no recording file is still closing.
    fn run_pending_finishes(&mut self) {
        if self.pending_finish.is_empty() || !self.finalising.is_empty() {
            return;
        }
        // Only a close has to wait for an unrelated recording; a command or a
        // no-op must not be blocked by a manual background recording.
        let must_wait = self
            .pending_finish
            .iter()
            .any(|action| matches!(action, crate::scheduler::FinishAction::CloseApp));
        if must_wait
            && self
                .player
                .snapshot()
                .streams
                .iter()
                .any(|stream| stream.recording)
        {
            return;
        }
        let actions: Vec<FinishAction> = self.pending_finish.drain(..).collect();
        for action in actions {
            self.scheduler.apply_finish(&action);
        }
        if let Some(message) = self.scheduler.message.take() {
            self.last_message = Some(message);
        }
    }

    /// Start recording on a specific stream, which need not be the active one.
    /// Background recordings use a record-only stream, so it never becomes the
    /// audible player-bar stream.
    fn start_recording_on(
        &mut self,
        playback_id: PlaybackId,
        split: bool,
        wishlist_only: bool,
    ) -> bool {
        let Some(stream) = self
            .player
            .snapshot()
            .streams
            .into_iter()
            .find(|stream| stream.id == playback_id)
        else {
            self.last_message = Some(t("msg.select_play").to_string());
            return false;
        };
        if stream.recording {
            return false;
        }
        if !crate::disk::has_space(
            std::path::Path::new(self.recording_dir.trim()),
            self.recording_min_free_mb,
        ) {
            self.last_message = Some(tf(
                "msg.recording_low_space",
                &[&self.recording_min_free_mb.to_string()],
            ));
            return false;
        }
        let station = stream
            .station_id
            .and_then(|id| self.stations.iter().find(|station| station.id == id));
        if let Some(station) = station {
            if !recording_quality_allowed(
                station.bitrate,
                station.codec.as_deref(),
                self.recording_min_kbps,
                &self.recording_codec,
            ) {
                self.last_message = Some(tf("msg.recording_quality", &[&station.name]));
                return false;
            }
        }
        let wishlist_only = wishlist_only || self.recording_wishlist_only;
        let mut config = crate::recording::RecordingConfig::new(&self.recording_dir);
        // Wishlist filtering needs per-track files; force split mode when it is on.
        let split = split || wishlist_only;
        config.mode = if split {
            crate::recording::RecordingMode::SplitPerTrack
        } else {
            crate::recording::RecordingMode::Continuous
        };
        config.template = self.recording_template.clone();
        config.min_track_seconds = self.recording_min_track_seconds;
        config.min_fingerprint_seconds = self.recording_min_fingerprint_seconds;
        if wishlist_only {
            config.wishlist = self.wishlist.clone();
            config.tag_format = stream.station_id.and_then(|id| {
                self.db
                    .as_ref()
                    .and_then(|db| db.custom_tag_format(id).ok().flatten())
            });
        }
        let station = stream
            .station_id
            .and_then(|id| self.stations.iter().find(|station| station.id == id))
            .map(|station| station.name.clone())
            .unwrap_or_else(|| "Unknown station".to_string());
        match self.player.start_recording(stream.id, config, station) {
            Ok(()) => {
                if let (Some(db), Some(attempt)) = (&self.db, self.attempts.get(&stream.id)) {
                    if let Err(e) = db.set_play_instance_recording(attempt.instance_id, true) {
                        log::warn!("could not mark the attempt as recording: {e}");
                    }
                }
                if let Some(title) = stream.now_playing.as_deref() {
                    let format = stream
                        .station_id
                        .and_then(|id| {
                            self.db
                                .as_ref()
                                .and_then(|db| db.custom_tag_format(id).ok().flatten())
                        })
                        .unwrap_or_else(|| crate::metadata::default_tag_format(title).to_string());
                    let tag = crate::metadata::split_title(title, &format);
                    self.player.set_recording_track(
                        stream.id,
                        tag.artist.as_deref(),
                        tag.title.as_deref(),
                    );
                }
                if let Some(station_id) = stream.station_id {
                    self.recording_stations.insert(stream.id, station_id);
                    self.recording_splits.insert(stream.id, split);
                    self.recording_started_at.insert(stream.id, Utc::now());
                    self.recording_labels
                        .insert(stream.id, stream.now_playing.clone().unwrap_or_default());
                    self.last_recording_poll = Instant::now()
                        .checked_sub(Duration::from_secs(30))
                        .unwrap_or_else(Instant::now);
                }
                self.last_message = Some(tf("msg.recording_to", &[&self.recording_dir]));
                true
            }
            Err(e) => {
                self.last_message = Some(tf("msg.recording_start_failed", &[&e.to_string()]));
                false
            }
        }
    }

    /// Start a record-only stream for a station, independent of playback.
    /// Pressing REC again on a station that is already recording stops it.
    fn toggle_background_recording(&mut self, station_id: Uuid) {
        if let Some(id) = background_recording_target(&self.player.snapshot().streams, station_id) {
            self.stop_one_stream(id);
            self.last_message = Some(t("msg.background_stopped").to_string());
            return;
        }
        let Some(id) = self.start_recording_stream(station_id) else {
            return;
        };
        if self.start_recording_on(id, self.recording_split, false) {
            let name = self
                .stations
                .iter()
                .find(|station| station.id == station_id)
                .map(|station| station.name.clone())
                .unwrap_or_default();
            self.last_message = Some(tf("msg.background_recording", &[&name]));
        } else {
            self.stop_one_stream(id);
        }
    }

    /// Open a record-only stream: it fetches and demuxes the station, feeds the
    /// recorder and updates ICY titles, but never decodes or plays audio.
    fn start_recording_stream(&mut self, station_id: Uuid) -> Option<PlaybackId> {
        self.start_background_stream(station_id, false)
    }

    fn start_watch_stream(&mut self, station_id: Uuid) -> Option<PlaybackId> {
        self.start_background_stream(station_id, true)
    }

    fn start_background_stream(
        &mut self,
        station_id: Uuid,
        watch: bool,
    ) -> Option<PlaybackId> {
        let station = self
            .stations
            .iter()
            .find(|station| station.id == station_id)
            .cloned()?;
        if station.url.trim().is_empty() {
            self.last_message = Some(tf("msg.no_stream_url", &[&station.name]));
            return None;
        }
        let stream = match &self.db {
            Some(db) => match db.ensure_stream_for_station(&station) {
                Ok(stream) => Some(stream),
                Err(e) => {
                    log::warn!("could not prepare a stream row: {e}");
                    None
                }
            },
            None => None,
        };
        let url = stream
            .as_ref()
            .map(|stream| stream.url.clone())
            .unwrap_or_else(|| station.url.clone());
        let playback_id = self.player.play_stream(PlayRequest {
            station_id: Some(station.id),
            stream_id: stream.as_ref().map(|stream| stream.id),
            volume: 0.0,
            gain_db: station.gain_db,
            reconnect: Some(ReconnectPolicy::default()),
            record_only: true,
            watch,
            ..PlayRequest::new(url)
        });
        if let Some(db) = &self.db {
            match db.begin_play(&station, stream.as_ref(), 0.0) {
                Ok((instance, entry)) => {
                    self.attempts.insert(
                        playback_id,
                        Attempt {
                            history_id: entry.id,
                            instance_id: instance.id,
                            started: Instant::now(),
                            track: None,
                            last_title: None,
                        },
                    );
                    self.history.insert(0, entry);
                    self.history_version += 1;
                }
                Err(e) => log::warn!("could not record history: {e}"),
            }
        }
        Some(playback_id)
    }

    /// Keep one background watch stream alive per watched station.
    fn ensure_watch_streams(&mut self, engine: &EngineSnapshot) {
        let watched: Vec<Uuid> = self
            .stations
            .iter()
            .filter(|station| station.watched)
            .map(|station| station.id)
            .collect();
        let live: HashSet<PlaybackId> = engine.streams.iter().map(|stream| stream.id).collect();
        self.watch_streams
            .retain(|station_id, id| watched.contains(station_id) && live.contains(id));
        for station_id in watched {
            if self.watch_streams.contains_key(&station_id) {
                continue;
            }
            if let Some(existing) = engine
                .streams
                .iter()
                .find(|stream| stream.station_id == Some(station_id) && stream.watching)
            {
                self.watch_streams.insert(station_id, existing.id);
                continue;
            }
            // A manual recording already captures this station.
            if engine
                .streams
                .iter()
                .any(|stream| stream.station_id == Some(station_id) && stream.record_only)
            {
                continue;
            }
            if let Some(id) = self.start_watch_stream(station_id) {
                self.watch_streams.insert(station_id, id);
            }
        }
    }

    fn stop_watch_stream_for(&mut self, station_id: Uuid) {
        if let Some(id) = self.watch_streams.remove(&station_id) {
            self.stop_one_stream(id);
        }
    }

    /// Promote a watch stream's rolling buffer to a recording when a wish hits.
    fn promote_watch_recording(
        &mut self,
        id: PlaybackId,
        station_id: Uuid,
        raw_title: &str,
    ) -> bool {
        let Some(station) = self
            .stations
            .iter()
            .find(|station| station.id == station_id)
            .cloned()
        else {
            return false;
        };
        if !crate::disk::has_space(
            std::path::Path::new(self.recording_dir.trim()),
            self.recording_min_free_mb,
        ) {
            self.last_message = Some(tf(
                "msg.recording_low_space",
                &[&self.recording_min_free_mb.to_string()],
            ));
            return false;
        }
        if !recording_quality_allowed(
            station.bitrate,
            station.codec.as_deref(),
            self.recording_min_kbps,
            &self.recording_codec,
        ) {
            self.last_message = Some(tf("msg.recording_quality", &[&station.name]));
            return false;
        }
        let mut config = crate::recording::RecordingConfig::new(&self.recording_dir);
        config.mode = crate::recording::RecordingMode::SplitPerTrack;
        config.template = self.recording_template.clone();
        config.min_track_seconds = self.recording_min_track_seconds;
        config.min_fingerprint_seconds = self.recording_min_fingerprint_seconds;
        config.wishlist = self.wishlist.clone();
        config.tag_format = self
            .db
            .as_ref()
            .and_then(|db| db.custom_tag_format(station_id).ok().flatten());
        let format = config
            .tag_format
            .clone()
            .unwrap_or_else(|| crate::metadata::default_tag_format(raw_title).to_string());
        let tag = crate::metadata::split_title(raw_title, &format);
        match self.player.promote_watch(
            id,
            config,
            station.name.clone(),
            tag.artist.as_deref(),
            tag.title.as_deref(),
        ) {
            Ok(true) => {
                self.recording_stations.insert(id, station_id);
                self.recording_splits.insert(id, true);
                self.recording_started_at.insert(id, Utc::now());
                self.recording_labels
                    .insert(id, raw_title.to_string());
                if let (Some(db), Some(attempt)) = (&self.db, self.attempts.get(&id)) {
                    if let Err(e) = db.set_play_instance_recording(attempt.instance_id, true) {
                        log::warn!("could not mark the attempt as recording: {e}");
                    }
                }
                true
            }
            // Already recording this stream; the wish is satisfied.
            Ok(false) => true,
            Err(e) => {
                self.last_message =
                    Some(tf("msg.recording_start_failed", &[&e.to_string()]));
                false
            }
        }
    }

    fn export_history(&mut self) {
        match (&self.db, self.data_dir_path.clone()) {
            (Some(db), Some(dir)) => {
                let path = dir.join("viberadio-history.html");
                match db.export_history_html(&path) {
                    Ok(n) => {
                        self.last_message = Some(tf(
                            "msg.history_exported",
                            &[&n.to_string(), &path.display().to_string()],
                        ));
                    }
                    Err(e) => {
                        self.last_message = Some(tf("msg.history_export_failed", &[&e.to_string()]))
                    }
                }
            }
            (None, _) => self.last_message = Some(t("msg.no_database").to_string()),
            (_, None) => self.last_message = Some(t("msg.no_writable_dir").to_string()),
        }
    }

    /// Start a playlist scrape for the selected station when it is not cached.
    fn ensure_playlist(&mut self) {
        let Some(station_id) = self.selected_station else {
            return;
        };
        if self.playlists.contains_key(&station_id)
            || self.playlist_errors.contains_key(&station_id)
        {
            return;
        }
        if self
            .playlist_fetch
            .as_ref()
            .map(|(id, _)| *id == station_id)
            .unwrap_or(false)
        {
            return;
        }
        let Some((source_id, stream_url, website)) = self
            .stations
            .iter()
            .find(|station| station.id == station_id)
            .and_then(|station| {
                let source_id = station.source_id.clone().filter(|id| !id.is_empty())?;
                Some((source_id, station.url.clone(), station.website.clone()))
            })
        else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-playlist".to_string())
            .spawn(move || {
                let _ = tx.send(station_playlist::fetch_playlist(
                    &source_id,
                    &stream_url,
                    website.as_deref(),
                ));
            });
        if spawned.is_ok() {
            self.playlist_fetch = Some((station_id, rx));
        }
    }

    fn poll_playlist(&mut self) {
        let received = match self.playlist_fetch.as_ref().map(|(_, rx)| rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some(Err("the playlist worker stopped".to_string()))
            }
            _ => None,
        };
        if let Some(result) = received {
            if let Some((station_id, _)) = self.playlist_fetch.take() {
                match result {
                    Ok(playlist) => {
                        self.playlists.insert(station_id, playlist);
                    }
                    Err(error) => {
                        self.playlist_errors.insert(station_id, error);
                    }
                }
            }
        }
    }

    /// After fingerprinting, keep only the largest copy of repeated songs.
    fn dedupe_recordings(&mut self) {
        if self.recording_keep_best {
            let dir = std::path::PathBuf::from(self.recording_dir.trim());
            match crate::fingerprint::dedupe_versions(&dir) {
                Ok(0) => {}
                Ok(n) => {
                    self.last_message = Some(tf("msg.duplicates_removed", &[&n.to_string()]));
                }
                Err(e) => log::warn!("could not deduplicate recordings: {e}"),
            }
        }
        self.start_verified_organize();
    }

    /// Build the station-free verified tree after a fingerprint or dedupe pass.
    fn start_verified_organize(&mut self) {
        if self.verified_organize.is_some() {
            self.verified_pending = true;
            return;
        }
        let dir = std::path::PathBuf::from(self.recording_dir.trim());
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-verified".to_string())
            .spawn(move || {
                let _ = tx.send(crate::verified::organize(&dir));
            });
        match spawned {
            Ok(_) => self.verified_organize = Some(rx),
            Err(e) => log::warn!("could not start the verified organizer: {e}"),
        }
    }

    /// Hand a finished recording's files to fingerprinting, and metadata-less
    /// wishlist files to the wish verifier.
    fn finish_recording_summary(&mut self, summary: &RecordedSummary) {
        let unverified: Vec<std::path::PathBuf> = summary
            .unverified
            .iter()
            .filter(|path| !crate::fingerprint::sidecar_path(path).exists())
            .cloned()
            .collect();
        if !unverified.is_empty() {
            self.start_wish_verify(unverified);
        }
        let verified: Vec<std::path::PathBuf> = summary
            .paths
            .iter()
            .filter(|path| !summary.unverified.contains(path))
            .cloned()
            .collect();
        self.start_fingerprint_paths(verified);
    }

    /// Hand each song file to fingerprinting as soon as it closes, instead of
    /// waiting for the whole recording to stop.
    fn poll_completed_recordings(&mut self) {
        let ids: Vec<PlaybackId> = self.recording_stations.keys().copied().collect();
        for id in ids {
            for file in self.player.take_completed_recordings(id) {
                log::info!("recording {id}: completed {}", file.path.display());
                if file.unverified {
                    self.start_wish_verify(vec![file.path]);
                } else {
                    self.start_fingerprint_paths(vec![file.path]);
                }
            }
        }
    }

    /// Fingerprint metadata-less wishlist recordings and delete the ones that
    /// do not match a `record` wish.
    fn start_wish_verify(&mut self, paths: Vec<std::path::PathBuf>) {
        self.pending_wish_verify.extend(paths);
        if self.wish_verify.is_some() || self.pending_wish_verify.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.pending_wish_verify);
        let worker_paths = paths.clone();
        let wishes = self.wishlist.clone();
        let key = self.acoustid_api_key.trim().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-wish-verify".to_string())
            .spawn(move || {
                let key = (!key.is_empty()).then_some(key.as_str());
                let _ = tx.send(crate::fingerprint::verify_wishes(&worker_paths, key, &wishes));
            });
        match spawned {
            Ok(_) => {
                self.wish_verify = Some(rx);
                self.last_message = Some(t("msg.wish_verify_running").to_string());
            }
            Err(e) => {
                log::warn!("could not start the wish-list verifier: {e}");
                self.pending_wish_verify.extend(paths);
                self.last_message = Some(tf("msg.wish_verify_failed", &[&e.to_string()]));
            }
        }
    }

    /// Fingerprint recordings that have no sidecar yet. Blocking work stays on
    /// its own thread; the UI polls for the result.
    fn start_fingerprint_scan(&mut self) {
        if self.fingerprint_scan.is_some() {
            self.last_message = Some(t("msg.fingerprinting_busy").to_string());
            return;
        }
        let dir = std::path::PathBuf::from(self.recording_dir.trim());
        let key = self.acoustid_api_key.trim().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-fingerprint".to_string())
            .spawn(move || {
                let key = (!key.is_empty()).then_some(key.as_str());
                let _ = tx.send(crate::fingerprint::scan_recordings(&dir, key));
            });
        match spawned {
            Ok(_) => {
                self.fingerprint_scan = Some(rx);
                self.last_message = Some(t("msg.fingerprinting").to_string());
            }
            Err(e) => {
                self.last_message = Some(tf("msg.fingerprint_failed", &[&e.to_string()]));
            }
        }
    }

    /// Fingerprint the exact files a finished recording kept.
    fn start_fingerprint_paths(&mut self, paths: Vec<std::path::PathBuf>) {
        self.pending_fingerprint.extend(paths);
        if self.fingerprint_scan.is_some() || self.pending_fingerprint.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.pending_fingerprint);
        let worker_paths = paths.clone();
        let key = self.acoustid_api_key.trim().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-fingerprint".to_string())
            .spawn(move || {
                let key = (!key.is_empty()).then_some(key.as_str());
                let _ = tx.send(crate::fingerprint::fingerprint_paths(&worker_paths, key));
            });
        match spawned {
            Ok(_) => self.fingerprint_scan = Some(rx),
            Err(e) => {
                log::warn!("could not start the fingerprint worker: {e}");
                self.pending_fingerprint.extend(paths);
            }
        }
    }

    /// Fetch the radio-browser directory on a worker thread.
    fn start_radiobrowser_import(&mut self) {
        if self.radiobrowser.is_some() {
            self.last_message = Some(t("msg.radiobrowser_busy").to_string());
            return;
        }
        if let Some(db) = &self.db {
            // A failed fetch is retried only from the button, not every launch.
            let _ = db.set_setting("radiobrowser_seeded", "1");
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-radiobrowser".to_string())
            .spawn(move || {
                let _ = tx.send(crate::radiobrowser::fetch_stations());
            });
        match spawned {
            Ok(_) => {
                self.radiobrowser = Some(rx);
                self.last_message = Some(t("msg.radiobrowser_fetching").to_string());
            }
            Err(e) => {
                self.last_message = Some(tf("msg.radiobrowser_failed", &[&e.to_string()]));
            }
        }
    }

    /// Fetch the default station set from radio.net on a worker thread.
    fn start_radionet_import(&mut self) {
        if self.radionet.is_some() {
            self.last_message = Some(t("msg.radionet_busy").to_string());
            return;
        }
        if let Some(db) = &self.db {
            // A failed fetch is retried only from the button, not every launch.
            let _ = db.set_setting("radionet_seeded", "1");
            let _ = db.set_setting("radionet_directory", radionet::DIRECTORY_VERSION);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("viberadio-radionet".to_string())
            .spawn(move || {
                let _ = tx.send(radionet::fetch_stations());
            });
        match spawned {
            Ok(_) => {
                self.radionet = Some(rx);
                if self.last_message.is_none() {
                    self.last_message = Some(t("msg.radionet_fetching").to_string());
                }
            }
            Err(e) => {
                self.last_message = Some(tf("msg.radionet_failed", &[&e.to_string()]));
            }
        }
    }

    fn import_playlist(&mut self) {
        match (&self.db, self.data_dir_path.clone()) {
            (Some(db), Some(dir)) => {
                let path = dir.join("viberadio-playlist.m3u");
                match db.import_m3u(&path) {
                    Ok(n) => {
                        let reloaded = self.reload_stations();
                        self.last_message = Some(if reloaded {
                            tf(
                                "msg.imported_n",
                                &[&n.to_string(), &path.display().to_string()],
                            )
                        } else {
                            t("msg.imported_reload_failed").to_string()
                        });
                    }
                    Err(e) => self.last_message = Some(tf("msg.import_failed", &[&e.to_string()])),
                }
            }
            (None, _) => self.last_message = Some(t("msg.no_database").to_string()),
            (_, None) => self.last_message = Some(t("msg.no_writable_dir").to_string()),
        }
    }

    fn add_station_and_play(&mut self, name: String, url: String) {
        let name = name.trim().to_string();
        let url = url.trim().to_string();
        if name.is_empty() || url.is_empty() {
            self.last_message = Some(t("msg.station_fields").to_string());
            return;
        }
        let mut station = Station::new(name, url);
        station.volume = self.settings.volume;
        match &self.db {
            Some(db) => {
                if let Err(e) = db
                    .save_station(&station)
                    .and_then(|()| db.ensure_stream_for_station(&station))
                {
                    self.last_message = Some(tf("msg.station_save_failed", &[&e.to_string()]));
                    return;
                }
                self.reload_stations();
            }
            None => {
                self.stations.push(station.clone());
                self.library_revision += 1;
            }
        }
        if !self
            .stations
            .iter()
            .any(|existing| existing.id == station.id)
        {
            self.stations.push(station.clone());
            self.library_revision += 1;
        }
        self.selected_station = Some(station.id);
        self.start_station(station.id);
    }

    // -----------------------------------------------------------------------
    // View model
    // -----------------------------------------------------------------------

    fn ensure_view(&mut self) {
        let key = ViewKey {
            library_revision: self.library_revision,
            history_version: self.history_version,
            filter: self.ui.filter.clone(),
            search: self.ui.search.clone(),
            sort: self.ui.sort,
            ascending: self.ui.sort_ascending,
            show_broken: self.ui.show_broken,
        };
        if self.view_key.as_ref() == Some(&key) {
            return;
        }

        let recent: HashSet<Uuid> = self.history.iter().map(|entry| entry.station_id).collect();
        let indices = build_view(
            &self.stations,
            &recent,
            &key.filter,
            &key.search,
            key.sort,
            key.ascending,
            key.show_broken,
        );
        self.view_indices = indices;
        self.view_key = Some(key);
    }

    fn library_stats(&self) -> LibraryStats {
        let shown = |station: &Station| self.ui.show_broken || !station.broken;
        let mut genres: BTreeMap<String, usize> = BTreeMap::new();
        for station in self.stations.iter().filter(|station| shown(station)) {
            if let Some(genre) = station.genre.as_deref().filter(|genre| !genre.is_empty()) {
                *genres.entry(genre.to_string()).or_insert(0) += 1;
            }
        }
        let recent: HashSet<Uuid> = self.history.iter().map(|entry| entry.station_id).collect();
        LibraryStats {
            total: self
                .stations
                .iter()
                .filter(|station| shown(station))
                .count(),
            favorites: self
                .stations
                .iter()
                .filter(|station| shown(station))
                .filter(|station| station.is_favorite)
                .count(),
            recently_played: recent.len(),
            watched: self
                .stations
                .iter()
                .filter(|station| shown(station))
                .filter(|station| station.watched)
                .count(),
            genres: genres.into_iter().collect(),
        }
    }

    fn stream_rows(&self, engine: &EngineSnapshot) -> Vec<StreamRow> {
        engine
            .streams
            .iter()
            .map(|stream| {
                let name = stream
                    .station_id
                    .and_then(|id| self.stations.iter().find(|station| station.id == id))
                    .map(|station| station.name.clone())
                    .unwrap_or_else(|| stream.url.clone());
                let status_text = match stream.status {
                    PlaybackStatus::Error => {
                        stream.error.clone().unwrap_or_else(|| "Error".to_string())
                    }
                    status => ui::status_label(status).to_string(),
                };
                StreamRow {
                    id: stream.id,
                    name,
                    status: stream.status,
                    status_text,
                    volume: stream.volume,
                    active: engine.active.as_ref().map(|active| active.id) == Some(stream.id),
                    recording: stream.recording,
                    record_only: stream.record_only,
                }
            })
            .collect()
    }

    fn active_station(&self, engine: &EngineSnapshot) -> Option<&Station> {
        engine
            .active
            .as_ref()
            .and_then(|stream| stream.station_id)
            .and_then(|id| self.stations.iter().find(|station| station.id == id))
    }

    // -----------------------------------------------------------------------
    // Actions and input
    // -----------------------------------------------------------------------

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::Play(id) => self.start_station(id),
            Action::RecordBackground(id) => self.toggle_background_recording(id),
            Action::Select(id) => {
                self.selected_station = Some(id);
                if self.player.snapshot().active.is_none() {
                    if let Some(station) = self.stations.iter().find(|station| station.id == id) {
                        self.settings.volume = station.volume;
                    }
                }
            }
            Action::ToggleFavorite(id) => self.toggle_favorite(id),
            Action::Restart(id) => {
                let ids: Vec<PlaybackId> = self
                    .player
                    .snapshot()
                    .streams
                    .into_iter()
                    .filter(|stream| stream.station_id == Some(id))
                    .map(|stream| stream.id)
                    .collect();
                for playback_id in ids {
                    self.stop_one_stream(playback_id);
                }
                self.start_station(id);
            }
            Action::StopAll => self.stop_all_streams(),
            Action::StopStream(id) => self.stop_one_stream(id),
            Action::ActivateStream(id) => {
                if self.player.set_active(id) {
                    if let Some(stream) = self
                        .player
                        .snapshot()
                        .streams
                        .into_iter()
                        .find(|stream| stream.id == id)
                    {
                        self.selected_station = stream.station_id;
                        if !self.scheduler_muted.contains(&id) {
                            self.settings.volume = stream.volume;
                        }
                    }
                }
            }
            Action::SetVolume(volume) => self.set_volume(volume),
            Action::SetStreamVolume(id, volume) => self.set_stream_volume(id, volume),
            Action::OpenAddStation => self.add_station.showing = true,
            Action::ImportRadioNet => self.start_radionet_import(),
            Action::ImportRadioBrowser => self.start_radiobrowser_import(),
            Action::ExportPlaylist => self.export_playlist(),
            Action::ImportPlaylist => self.import_playlist(),
            Action::ExportHistory => self.export_history(),
            Action::ToggleRecord => self.toggle_recording(),
            Action::CycleSleep => self.cycle_sleep_timer(),
            Action::OpenSessions => {
                self.reload_sessions();
                self.ui.show_sessions = true;
            }
            Action::OpenSchedules => {
                self.reload_schedules();
                self.ui.show_schedules = true;
            }
            Action::OpenWishlist => self.ui.show_wishlist = true,
            Action::OpenWatchHistory => self.ui.show_watch_history = true,
            Action::WantCurrent => self.want_current_track(),
            Action::WantSong { artist, title } => self.want_song(artist, title),
            Action::RefreshPlaylist => {
                if let Some(id) = self.selected_station {
                    self.playlists.remove(&id);
                    self.playlist_errors.remove(&id);
                    self.ensure_playlist();
                }
            }
            Action::OpenSettings => {
                self.refresh_latest_backup();
                self.ui.show_settings = true;
            }
            Action::OpenAbout => self.ui.show_about = true,
            Action::OpenHelp => self.ui.show_help = true,
            Action::NewSchedule => self.begin_schedule(None),
            Action::ScheduleStation(id) => self.begin_schedule(Some(id)),
            Action::EditSchedule(id) => self.edit_schedule(id),
            Action::SaveSchedule => self.save_schedule_editor(),
            Action::CancelSchedule => self.schedule_editor = None,
            Action::DeleteSchedule(id) => self.delete_schedule(id),
            Action::SetScheduleEnabled(id, enabled) => self.set_schedule_enabled(id, enabled),
            Action::ClearSearch => self.ui.search.clear(),
            Action::AddStation(name, url) => self.add_station_and_play(name, url),
            Action::ToggleMini => self.mini_open = !self.mini_open,
            Action::RemoveStations(ids) => {
                if !ids.is_empty() {
                    self.confirm_remove = Some(ids);
                }
            }
            Action::RemoveBroken => {
                let ids: Vec<Uuid> = self
                    .stations
                    .iter()
                    .filter(|station| station.broken)
                    .map(|station| station.id)
                    .collect();
                if ids.is_empty() {
                    self.last_message = Some(t("msg.no_broken").to_string());
                } else {
                    self.confirm_remove = Some(ids);
                }
            }
            Action::ClearSelection => {
                self.ui.selected_stations.clear();
                self.ui.selection_anchor = None;
            }
            Action::ProbeBitrates => self.start_bitrate_probe(),
            Action::StopBitrateProbe => self.stop_bitrate_probe(),
            Action::ToggleWatched(id) => self.toggle_watched(id),
        }
    }

    /// Returns the actions plus whether `/` asked for the search field. The focus
    /// request is applied after the panels, so the same frame's text event cannot
    /// land in the newly focused field as a literal slash.
    fn handle_shortcuts(
        &mut self,
        ctx: &egui::Context,
        engine: &EngineSnapshot,
    ) -> (Vec<Action>, bool) {
        let focused = ctx.memory(|memory| memory.focused());
        let editing = focused.is_some();
        let (space, left, right, slash, escape, debug, history, record, settings, mini, ctrl) = ctx
            .input(|input| {
                (
                    input.key_pressed(egui::Key::Space),
                    input.key_pressed(egui::Key::ArrowLeft),
                    input.key_pressed(egui::Key::ArrowRight),
                    input.key_pressed(egui::Key::Slash),
                    input.key_pressed(egui::Key::Escape),
                    input.key_pressed(egui::Key::D),
                    input.key_pressed(egui::Key::H),
                    input.key_pressed(egui::Key::R),
                    input.key_pressed(egui::Key::Comma),
                    input.key_pressed(egui::Key::M),
                    input.modifiers.ctrl,
                )
            });
        let focus_search = slash && !editing;
        let search_focused = focused == Some(ui::table::search_id());

        let mut actions = Vec::new();
        if escape {
            if self.add_station.showing {
                self.add_station.showing = false;
                self.add_station.name.clear();
                self.add_station.url.clear();
                return (actions, false);
            }
            self.ui.search.clear();
            if search_focused {
                ctx.memory_mut(|memory| memory.surrender_focus(ui::table::search_id()));
            }
        }
        if self.add_station.showing {
            return (actions, false);
        }
        if editing {
            return (actions, focus_search);
        }
        if ctrl && debug {
            self.ui.show_metadata_debug = !self.ui.show_metadata_debug;
        }
        if ctrl && history {
            self.ui.show_history = !self.ui.show_history;
        }
        if ctrl && record {
            actions.push(Action::ToggleRecord);
        }
        if ctrl && mini {
            actions.push(Action::ToggleMini);
        }
        if ctrl && settings {
            self.ui.show_settings = !self.ui.show_settings;
            if self.ui.show_settings {
                self.refresh_latest_backup();
            }
        }

        if space {
            let any_playing = engine
                .streams
                .iter()
                .any(|stream| stream.status.is_active());
            if any_playing {
                actions.push(Action::StopAll);
            } else if let Some(id) = self.selected_station {
                actions.push(Action::Play(id));
            }
        }
        if left && !ctrl {
            actions.push(Action::SetVolume(
                (self.settings.volume - 0.05).clamp(0.0, 1.0),
            ));
        }
        if right && !ctrl {
            actions.push(Action::SetVolume(
                (self.settings.volume + 0.05).clamp(0.0, 1.0),
            ));
        }
        if ctrl && (left || right) {
            let ids: Vec<PlaybackId> = engine.streams.iter().map(|stream| stream.id).collect();
            if !ids.is_empty() {
                let current = engine.active.as_ref().map(|stream| stream.id);
                let position =
                    current.and_then(|id| ids.iter().position(|candidate| *candidate == id));
                let target = match (position, right) {
                    (Some(position), true) => ids[(position + 1) % ids.len()],
                    (Some(position), false) => ids[(position + ids.len() - 1) % ids.len()],
                    (None, _) => ids[0],
                };
                actions.push(Action::ActivateStream(target));
            }
        }
        (actions, focus_search)
    }

    fn render_add_dialog(&mut self, ctx: &egui::Context, theme: &Theme) -> Vec<Action> {
        if !self.add_station.showing {
            return Vec::new();
        }
        let mut submitted = None;
        let mut close = false;
        egui::Window::new(t("add.title"))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(t("add.name"))
                        .size(12.0)
                        .color(theme.text_secondary),
                );
                ui.add(egui::TextEdit::singleline(&mut self.add_station.name).desired_width(320.0));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(t("add.url"))
                        .size(12.0)
                        .color(theme.text_secondary),
                );
                ui.add(egui::TextEdit::singleline(&mut self.add_station.url).desired_width(320.0));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(t("add.submit")).color(theme.bg_app),
                            )
                            .fill(theme.accent),
                        )
                        .clicked()
                    {
                        submitted =
                            Some((self.add_station.name.clone(), self.add_station.url.clone()));
                    }
                    if ui.button(t("common.cancel")).clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.add_station.showing = false;
            self.add_station.name.clear();
            self.add_station.url.clear();
        }
        match submitted {
            Some((name, url)) => {
                self.add_station.showing = false;
                self.add_station.name.clear();
                self.add_station.url.clear();
                vec![Action::AddStation(name, url)]
            }
            None => Vec::new(),
        }
    }

    fn render_about_window(&mut self, ctx: &egui::Context) {
        if !self.ui.show_about {
            return;
        }
        let mut open = true;
        let data = self
            .data_dir_path
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| t("settings.unavailable").to_string());
        let database = self
            .data_dir_path
            .as_ref()
            .map(|dir| dir.join("viberadio.db").display().to_string())
            .unwrap_or_else(|| t("settings.unavailable").to_string());
        let schema = self.schema_version.clone();
        let diagnostics = format!(
            "{} {}\n{}\n{}\n{}\n{}\n{}",
            t("app.name"),
            env!("CARGO_PKG_VERSION"),
            tf("diag.data", &[&data]),
            tf("diag.database", &[&database]),
            tf("diag.schema", &[&schema]),
            tf("diag.preset", &[&format!("{:?}", self.dsp_preset)]),
            tf("diag.recording", &[&self.recording_dir]),
        );
        let mut close = false;
        egui::Window::new(t("about.title"))
            .open(&mut open)
            .default_width(520.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading(t("about.heading"));
                        ui.label(tf("about.version", &[env!("CARGO_PKG_VERSION")]));
                        ui.label(t("about.tagline"));
                        ui.separator();
                        ui.strong(t("about.formats"));
                        ui.label(t("about.formats_text"));
                        ui.separator();
                        ui.strong(t("about.boost"));
                        ui.label(t("about.boost_text"));
                        ui.separator();
                        ui.label(tf("about.data", &[&data]));
                        ui.label(tf("about.database", &[&database]));
                        ui.label(tf("about.schema", &[&schema]));
                        ui.horizontal(|ui| {
                            if ui.button(t("about.copy")).clicked() {
                                ui.ctx().copy_text(diagnostics.clone());
                            }
                            if ui.button(t("common.close")).clicked() {
                                close = true;
                            }
                        });
                    });
            });
        if close {
            open = false;
        }
        self.ui.show_about = open;
    }

    fn render_help_window(&mut self, ctx: &egui::Context) {
        if !self.ui.show_help {
            return;
        }
        let mut open = true;
        egui::Window::new(t("help.title"))
            .open(&mut open)
            .default_width(560.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for block in parse_help(i18n::help()) {
                            match block {
                                HelpBlock::Heading(text) => {
                                    ui.heading(text);
                                }
                                HelpBlock::Subheading(text) => {
                                    ui.add_space(4.0);
                                    ui.strong(text);
                                }
                                HelpBlock::Bullet(text) => {
                                    ui.label(format!("• {text}"));
                                }
                                HelpBlock::Table(text) => {
                                    ui.label(egui::RichText::new(text).monospace());
                                }
                                HelpBlock::Paragraph(text) => {
                                    ui.label(text);
                                }
                            }
                        }
                    });
            });
        self.ui.show_help = open;
    }

    fn render_schedules_window(&mut self, ctx: &egui::Context, theme: &Theme) -> Vec<Action> {
        if !self.ui.show_schedules {
            return Vec::new();
        }
        let mut actions = Vec::new();
        let mut open = true;
        let schedules = &self.schedules;
        let stations = &self.stations;
        let editor = &mut self.schedule_editor;
        let active_count = self.scheduler.active_count();

        egui::Window::new(t("schedules.title"))
            .open(&mut open)
            .default_width(520.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                let Some(editor) = editor else {
                    ui.horizontal(|ui| {
                        if ui.button(t("schedules.new")).clicked() {
                            actions.push(Action::NewSchedule);
                        }
                        let count = schedules.len();
                        ui.label(if count == 1 {
                            tf("schedules.count_one", &[&count.to_string()])
                        } else {
                            tf("schedules.count_many", &[&count.to_string()])
                        });
                        if active_count > 0 {
                            ui.label(tf("schedules.running", &[&active_count.to_string()]));
                        }
                    });
                    ui.label(
                        egui::RichText::new(t("schedules.note"))
                            .size(12.0)
                            .color(theme.text_secondary),
                    );
                    ui.separator();
                    if schedules.is_empty() {
                        ui.label(t("schedules.empty"));
                        return;
                    }
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for schedule in schedules {
                                let station_name = stations
                                    .iter()
                                    .find(|station| station.id == schedule.station_id)
                                    .map(|station| station.name.as_str())
                                    .unwrap_or(t("schedules.station_missing"));
                                let next = scheduler::next_occurrence(
                                    schedule,
                                    Utc::now(),
                                    &scheduler::resolve_local,
                                )
                                .map(|occurrence| {
                                    occurrence
                                        .start
                                        .with_timezone(&Local)
                                        .format("%a %H:%M")
                                        .to_string()
                                })
                                .unwrap_or_else(|| "—".to_string());
                                ui.horizontal_wrapped(|ui| {
                                    let mut enabled = schedule.enabled;
                                    if ui
                                        .checkbox(&mut enabled, "")
                                        .on_hover_text(t("schedules.enabled"))
                                        .changed()
                                    {
                                        actions
                                            .push(Action::SetScheduleEnabled(schedule.id, enabled));
                                    }
                                    ui.strong(station_name);
                                    ui.label(schedule.action.label());
                                    ui.label(schedule.recurrence.label());
                                    if !matches!(schedule.finish_action, FinishAction::None) {
                                        ui.label(schedule.finish_action.label());
                                    }
                                    ui.label(tf("schedules.next", &[&next]));
                                    if ui.small_button(t("schedules.edit")).clicked() {
                                        actions.push(Action::EditSchedule(schedule.id));
                                    }
                                    if ui.small_button(t("schedules.delete")).clicked() {
                                        actions.push(Action::DeleteSchedule(schedule.id));
                                    }
                                });
                                ui.separator();
                            }
                        });
                    return;
                };

                ui.horizontal(|ui| {
                    ui.strong(if editor.id.is_some() {
                        t("schedules.edit_title")
                    } else {
                        t("schedules.new_title")
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(t("schedules.back")).clicked() {
                            actions.push(Action::CancelSchedule);
                        }
                    });
                });
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label(t("schedules.station"));
                    let selected = editor
                        .draft
                        .station_id
                        .and_then(|id| stations.iter().find(|station| station.id == id))
                        .map(|station| station.name.clone())
                        .unwrap_or_else(|| t("schedules.pick_station").to_string());
                    egui::ComboBox::from_id_salt("schedule-station")
                        .selected_text(selected)
                        .width(280.0)
                        .show_ui(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    for station in stations {
                                        let selected = editor.draft.station_id == Some(station.id);
                                        if ui.selectable_label(selected, &station.name).clicked() {
                                            editor.draft.station_id = Some(station.id);
                                        }
                                    }
                                });
                        });
                });

                ui.horizontal(|ui| {
                    ui.label(t("schedules.action"));
                    egui::ComboBox::from_id_salt("schedule-action")
                        .selected_text(editor.draft.action.label())
                        .show_ui(ui, |ui| {
                            for action in ScheduleAction::ALL {
                                ui.selectable_value(
                                    &mut editor.draft.action,
                                    action,
                                    action.label(),
                                );
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label(t("schedules.repeat"));
                    let mut kind = match &editor.draft.recurrence {
                        Recurrence::Once => 0,
                        Recurrence::Daily => 1,
                        Recurrence::Weekly(_) => 2,
                    };
                    let label = match kind {
                        0 => "Once",
                        1 => "Daily",
                        _ => "Weekly",
                    };
                    egui::ComboBox::from_id_salt("schedule-recurrence")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut kind, 0, "Once");
                            ui.selectable_value(&mut kind, 1, "Daily");
                            ui.selectable_value(&mut kind, 2, "Weekly");
                        });
                    match kind {
                        0 => editor.draft.recurrence = Recurrence::Once,
                        1 => editor.draft.recurrence = Recurrence::Daily,
                        2 => {
                            if !matches!(editor.draft.recurrence, Recurrence::Weekly(_)) {
                                editor.draft.recurrence = Recurrence::Weekly(vec![Weekday::Mon]);
                            }
                        }
                        _ => {}
                    }
                });
                if let Recurrence::Weekly(days) = &mut editor.draft.recurrence {
                    ui.horizontal_wrapped(|ui| {
                        for weekday in WEEKDAYS {
                            let mut on = days.contains(&weekday);
                            if ui
                                .checkbox(&mut on, scheduler::weekday_short(weekday))
                                .changed()
                            {
                                if on {
                                    if !days.contains(&weekday) {
                                        days.push(weekday);
                                    }
                                } else {
                                    days.retain(|day| *day != weekday);
                                }
                                days.sort_by_key(|day| day.num_days_from_monday());
                            }
                        }
                    });
                }

                ui.horizontal(|ui| {
                    ui.label(t("schedules.start"));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.draft.start_date)
                            .desired_width(100.0)
                            .hint_text(t("schedules.date_hint")),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.draft.start_time)
                            .desired_width(60.0)
                            .hint_text(t("schedules.time_hint")),
                    );
                    ui.label(t("schedules.end"));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.draft.end_time)
                            .desired_width(60.0)
                            .hint_text(t("schedules.time_hint")),
                    );
                });
                ui.label(
                    egui::RichText::new(t("schedules.end_note"))
                        .size(12.0)
                        .color(theme.text_secondary),
                );

                ui.horizontal(|ui| {
                    ui.label(t("schedules.finish"));
                    let mut kind = match &editor.draft.finish {
                        FinishAction::None => 0,
                        FinishAction::CloseApp => 1,
                        FinishAction::RunCommand(_) => 2,
                    };
                    let label = match kind {
                        0 => "None",
                        1 => "Close the app",
                        _ => "Run a command",
                    };
                    egui::ComboBox::from_id_salt("schedule-finish")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut kind, 0, "None");
                            ui.selectable_value(&mut kind, 1, "Close the app");
                            ui.selectable_value(&mut kind, 2, "Run a command");
                        });
                    match kind {
                        0 => editor.draft.finish = FinishAction::None,
                        1 => editor.draft.finish = FinishAction::CloseApp,
                        2 => {
                            if !matches!(editor.draft.finish, FinishAction::RunCommand(_)) {
                                editor.draft.finish = FinishAction::RunCommand(String::new());
                            }
                        }
                        _ => {}
                    }
                    if let FinishAction::RunCommand(command) = &mut editor.draft.finish {
                        ui.add(
                            egui::TextEdit::singleline(command)
                                .desired_width(220.0)
                                .hint_text(t("schedules.command_hint")),
                        );
                    }
                });
                ui.checkbox(&mut editor.draft.enabled, t("schedules.enabled"));

                if let Some(error) = &editor.error {
                    ui.colored_label(theme.danger, error);
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(t("schedules.save")).color(theme.bg_app),
                            )
                            .fill(theme.accent),
                        )
                        .clicked()
                    {
                        actions.push(Action::SaveSchedule);
                    }
                    if ui.button(t("common.cancel")).clicked() {
                        actions.push(Action::CancelSchedule);
                    }
                });
            });

        self.ui.show_schedules = open;
        if !open {
            self.schedule_editor = None;
        }
        actions
    }
}

/// Filter and sort the library. Kept free of app state so the view logic is
/// testable without constructing a database, an engine or a window.
fn fetch_artwork(url: &str) -> Result<Option<DecodedArtwork>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the artwork client: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent("VibeRadio/0.1")
            .build()
            .map_err(|e| e.to_string())?;
        let response = client.get(url).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        let image = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        Ok(Some(DecodedArtwork {
            width: width as usize,
            height: height as usize,
            rgba: rgba.into_raw(),
        }))
    })
}

fn recording_summary_message(summary: &RecordedSummary) -> String {
    tf(
        "msg.recording_stopped",
        &[
            &summary.files.to_string(),
            &summary.bytes.to_string(),
            if summary.dropped {
                t("msg.recording_dropped")
            } else {
                ""
            },
            &summary
                .error
                .as_deref()
                .map(|error| tf("msg.recording_error", &[error]))
                .unwrap_or_default(),
        ],
    )
}

/// The now-playing label for the selected station, never another stream's.
fn selected_now_playing<'a>(
    active: Option<&'a crate::player::StreamSnapshot>,
    selected: Option<Uuid>,
) -> Option<&'a str> {
    active
        .filter(|stream| stream.station_id == selected)
        .and_then(|stream| {
            stream
                .display_now_playing
                .as_deref()
                .or(stream.now_playing.as_deref())
        })
}

/// The app's own history first, then provider entries not already listed, so
/// a scraped page that lags behind ICY cannot hide the songs just heard.
fn merged_playlist(
    station_id: Option<Uuid>,
    history: &[crate::models::StationHistory],
    provider: Option<&crate::station_playlist::StationPlaylist>,
) -> crate::station_playlist::StationPlaylist {
    let mut merged = provider.cloned().unwrap_or_default();
    let mut rows: Vec<(
        Option<chrono::DateTime<Utc>>,
        crate::station_playlist::PlaylistEntry,
    )> = Vec::new();
    if let Some(station_id) = station_id {
        for row in history
            .iter()
            .filter(|row| row.station_id == station_id && row.song_title.is_some())
            .take(30)
        {
            rows.push((
                Some(row.played_at),
                crate::station_playlist::PlaylistEntry {
                    at: Some(
                        row.played_at
                            .with_timezone(&Local)
                            .format("%H:%M")
                            .to_string(),
                    ),
                    artist: row.artist.clone().unwrap_or_default(),
                    title: row.song_title.clone().unwrap_or_default(),
                    image: None,
                },
            ));
        }
    }
    if let Some(provider) = provider {
        for entry in &provider.entries {
            let at = entry
                .at
                .as_deref()
                .and_then(crate::station_playlist::parse_time);
            rows.push((at, entry.clone()));
        }
    }
    // Newest first; entries without a usable timestamp follow the dated ones.
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    let mut entries = Vec::new();
    for (_, entry) in rows {
        if entries.len() >= 30 {
            break;
        }
        if !entries.iter().any(|existing| same_track(existing, &entry)) {
            entries.push(entry);
        }
    }
    merged.entries = entries;
    merged
}

fn same_track(
    a: &crate::station_playlist::PlaylistEntry,
    b: &crate::station_playlist::PlaylistEntry,
) -> bool {
    a.artist.trim().eq_ignore_ascii_case(b.artist.trim())
        && a.title.trim().eq_ignore_ascii_case(b.title.trim())
}

/// Whether a station meets the configured recording quality filter.
fn recording_quality_allowed(
    bitrate: Option<u32>,
    codec: Option<&str>,
    min_kbps: u32,
    codec_filter: &str,
) -> bool {
    if min_kbps > 0 {
        let Some(bitrate) = bitrate else {
            return false;
        };
        if bitrate < min_kbps {
            return false;
        }
    }
    if codec_filter != "any" {
        let Some(codec) = codec else {
            return false;
        };
        let codec = codec.to_ascii_lowercase();
        let matches = match codec_filter {
            "mp3" => codec.contains("mp3") || codec.contains("mpeg"),
            "aac" => codec.contains("aac"),
            _ => true,
        };
        if !matches {
            return false;
        }
    }
    true
}

fn next_sleep_minutes(current: u32) -> u32 {
    match current {
        0 => 15,
        15 => 30,
        30 => 60,
        _ => 0,
    }
}

fn background_recording_target(
    streams: &[crate::player::StreamSnapshot],
    station_id: Uuid,
) -> Option<PlaybackId> {
    streams
        .iter()
        .find(|stream| stream.station_id == Some(station_id) && stream.recording)
        .map(|stream| stream.id)
}

fn streams_to_replace(
    streams: &[crate::player::StreamSnapshot],
    muted: &HashSet<PlaybackId>,
    keep: Option<PlaybackId>,
) -> Vec<PlaybackId> {
    streams
        .iter()
        .filter(|stream| Some(stream.id) != keep)
        .filter(|stream| !muted.contains(&stream.id))
        .filter(|stream| !stream.record_only)
        .map(|stream| stream.id)
        .collect()
}

fn compare_label(mode: crate::metadata::CompareMode) -> &'static str {
    match mode {
        crate::metadata::CompareMode::Contains => t("wishlist.compare_contains"),
        crate::metadata::CompareMode::Exact => t("wishlist.compare_exact"),
        crate::metadata::CompareMode::StartsWith => t("wishlist.compare_starts"),
    }
}

fn build_view(
    stations: &[Station],
    recent: &HashSet<Uuid>,
    filter: &LibraryFilter,
    search: &str,
    sort: SortKey,
    ascending: bool,
    show_broken: bool,
) -> Vec<usize> {
    let needle = search.trim().to_lowercase();
    let mut indices: Vec<usize> = stations
        .iter()
        .enumerate()
        .filter(|(_, station)| {
            if station.broken && !show_broken {
                return false;
            }
            let filter_match = match filter {
                LibraryFilter::All => true,
                LibraryFilter::Favorites => station.is_favorite,
                LibraryFilter::RecentlyPlayed => recent.contains(&station.id),
                LibraryFilter::Watched => station.watched,
                LibraryFilter::Genre(genre) => station.genre.as_deref() == Some(genre.as_str()),
            };
            filter_match && (needle.is_empty() || search_matches(station, &needle))
        })
        .map(|(index, _)| index)
        .collect();

    indices.sort_by(|left, right| {
        let a = &stations[*left];
        let b = &stations[*right];
        let ordering = match sort {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Genre => a
                .genre
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b.genre.as_deref().unwrap_or("").to_lowercase()),
            SortKey::Bitrate => a.bitrate.cmp(&b.bitrate),
            SortKey::Country => a
                .country
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b.country.as_deref().unwrap_or("").to_lowercase()),
        };
        let ordering = if ascending {
            ordering
        } else {
            ordering.reverse()
        };
        ordering.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    indices
}

fn search_matches(station: &Station, needle: &str) -> bool {
    let field = |value: Option<&str>| {
        value
            .map(|value| value.to_lowercase().contains(needle))
            .unwrap_or(false)
    };
    station.name.to_lowercase().contains(needle)
        || station.url.to_lowercase().contains(needle)
        || field(station.genre.as_deref())
        || field(station.country.as_deref())
        || field(station.country_state.as_deref())
        || field(station.city.as_deref())
        || field(station.tags.as_deref())
}

impl eframe::App for VibeRadioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // An import can add scripts after startup; rebuild the font fallbacks
        // only when the set of scripts actually changes.
        if self.font_revision != self.library_revision {
            self.font_revision = self.library_revision;
            let scripts =
                crate::fonts::scripts_in(self.stations.iter().map(|s| s.name.as_str()));
            if scripts != self.font_scripts {
                self.font_scripts = scripts;
                crate::ui::theme::install_fonts(ctx, scripts);
            }
        }
        let mut tray_events = Vec::new();
        if let Some(tray) = &self.tray {
            while let Some(event) = tray.poll() {
                tray_events.push(event);
            }
        }
        let mut tray_closed = false;
        for event in tray_events {
            match event {
                TrayEvent::Show => {
                    let root = egui::ViewportId::ROOT;
                    ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Focus);
                }
                TrayEvent::ToggleMini => self.mini_open = !self.mini_open,
                TrayEvent::TogglePlayback => self.toggle_playback(),
                TrayEvent::Quit => {
                    self.stop_all_streams();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                TrayEvent::Closed => {
                    log::warn!("tray icon service stopped");
                    tray_closed = true;
                }
            }
        }
        if tray_closed {
            self.tray = None;
        }

        let mut media_events = Vec::new();
        if let Some(media) = &self.media {
            while let Some(event) = media.poll() {
                media_events.push(event);
            }
        }
        // A media widget slider can send many volume events per frame; keep only
        // the newest and persist once.
        let mut pending_volume: Option<f32> = None;
        let mut volume_changed = false;
        for event in media_events {
            if let MediaEvent::Volume(volume) = event {
                pending_volume = Some(volume);
                continue;
            }
            if let Some(volume) = pending_volume.take() {
                self.set_volume(volume.clamp(0.0, 1.0));
                volume_changed = true;
            }
            match event {
                MediaEvent::Play => self.play_selected(),
                MediaEvent::Pause | MediaEvent::Stop => {
                    if self.player.snapshot().active.is_some() {
                        self.stop_all_streams();
                    }
                }
                MediaEvent::Toggle => self.toggle_playback(),
                MediaEvent::Next => self.skip_station(1),
                MediaEvent::Previous => self.skip_station(-1),
                MediaEvent::Volume(_) => {}
                MediaEvent::Raise => {
                    let root = egui::ViewportId::ROOT;
                    ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Focus);
                }
                MediaEvent::Quit => {
                    self.stop_all_streams();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if let Some(volume) = pending_volume {
            self.set_volume(volume.clamp(0.0, 1.0));
            volume_changed = true;
        }
        if volume_changed {
            // Re-publish the stored value, so a client that sent something
            // outside 0..=1 sees the correction.
            self.last_media = None;
        }

        let fetched = match self.radionet.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some(Err("the radio.net worker stopped".to_string()))
            }
            _ => None,
        };
        if let Some(result) = fetched {
            self.radionet = None;
            match result {
                Ok(seeds) if seeds.is_empty() => {
                    self.last_message = Some(t("msg.radionet_empty").to_string())
                }
                Ok(seeds) => {
                    let imported = self.db.as_ref().map(|db| db.import_seeds(&seeds));
                    match imported {
                        Some(Ok(count)) => {
                            self.reload_stations();
                            self.last_message =
                                Some(tf("msg.radionet_imported", &[&count.to_string()]));
                        }
                        Some(Err(e)) => {
                            self.last_message = Some(tf("msg.radionet_failed", &[&e.to_string()]))
                        }
                        None => self.last_message = Some(t("msg.no_database").to_string()),
                    }
                }
                Err(e) => {
                    self.last_message = Some(tf("msg.radionet_failed", &[&e]));
                }
            }
        }

        let scanned = match self.fingerprint_scan.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some(Err("the fingerprint worker stopped".to_string()))
            }
            _ => None,
        };
        let verified = match self.wish_verify.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some(Err("the wish-list verifier stopped".to_string()))
            }
            _ => None,
        };
        if let Some(result) = verified {
            self.wish_verify = None;
            match result {
                Ok(check) => {
                    self.last_message = Some(tf(
                        "msg.wish_verify_done",
                        &[
                            &check.kept.to_string(),
                            &check.removed.to_string(),
                            &check.unverified.to_string(),
                        ],
                    ))
                }
                Err(e) => {
                    self.last_message = Some(tf("msg.wish_verify_failed", &[&e]))
                }
            }
            // A second recording may have finished while this one was checked.
            self.start_wish_verify(Vec::new());
            self.dedupe_recordings();
        }
        if let Some(result) = scanned {
            self.fingerprint_scan = None;
            let retries = match &result {
                Ok(report) => report.retry.clone(),
                Err(_) => Vec::new(),
            };
            match result {
                Ok(report) => {
                    self.last_message = Some(if report.incomplete > 0 || report.interrupted > 0 {
                        tf(
                            "msg.fingerprinted_issues",
                            &[
                                &report.fingerprinted.to_string(),
                                &report.incomplete.to_string(),
                                &report.interrupted.to_string(),
                            ],
                        )
                    } else {
                        tf("msg.fingerprinted", &[&report.fingerprinted.to_string()])
                    });
                    self.dedupe_recordings();
                }
                Err(e) => self.last_message = Some(tf("msg.fingerprint_failed", &[&e])),
            }
            for path in retries {
                self.fingerprint_retry.insert(
                    path,
                    (Instant::now() + Duration::from_secs(30), 3),
                );
            }
            // A song may have completed while this batch was running.
            self.start_fingerprint_paths(Vec::new());
        }

        // Retry files whose AcoustID lookup failed. One at a time, and only
        // while the single fingerprint worker is idle.
        if self.fingerprint_scan.is_none() {
            let retry = self
                .fingerprint_retry
                .iter()
                .find(|(_, (at, _))| Instant::now() >= *at)
                .map(|(path, (_, attempts))| (path.clone(), *attempts));
            if let Some((path, attempts)) = retry {
                self.fingerprint_retry.remove(&path);
                if attempts > 0 && !crate::fingerprint::sidecar_path(&path).exists() {
                    self.fingerprint_retry.insert(
                        path.clone(),
                        (
                            Instant::now() + Duration::from_secs(60),
                            attempts.saturating_sub(1),
                        ),
                    );
                    self.start_fingerprint_paths(vec![path]);
                }
            }
        }

        let organized = match self.verified_organize.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some(Err("the verified-song worker stopped".to_string()))
            }
            _ => None,
        };
        if let Some(result) = organized {
            self.verified_organize = None;
            match result {
                Ok(report) => {
                    if report.copied > 0 || report.updated > 0 || report.errors > 0 {
                        log::info!(
                            "verified songs: {} copied, {} updated, {} skipped, {} errors",
                            report.copied,
                            report.updated,
                            report.skipped,
                            report.errors
                        );
                    }
                }
                Err(e) => log::warn!("could not organize verified songs: {e}"),
            }
            if self.verified_pending {
                self.verified_pending = false;
                self.start_verified_organize();
            }
        }

        if let Some((title, rx)) = self.artwork_poll.take() {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("the artwork worker stopped".to_string()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            };
            match result {
                None => self.artwork_poll = Some((title, rx)),
                Some(Ok(Some(decoded))) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [decoded.width, decoded.height],
                        &decoded.rgba,
                    );
                    let texture = ctx.load_texture(
                        format!("artwork-{title}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.artwork.insert(title, ArtworkState::Ready(texture));
                }
                Some(Ok(None)) => {
                    self.artwork.insert(title, ArtworkState::Missing);
                }
                Some(Err(e)) => {
                    log::warn!("could not fetch artwork: {e}");
                    self.artwork.insert(title, ArtworkState::Missing);
                }
            }
            self.start_artwork_worker();
        }

        if let Some((raw_title, key, rx, fallback, candidates)) = self.metadata_poll.take() {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err("the metadata worker stopped".to_string()))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            };
            match result {
                None => {
                    self.metadata_poll = Some((raw_title, key, rx, fallback, candidates));
                }
                Some(Ok(result)) => {
                    self.duration_cache.insert(key, result.clone());
                    self.metadata_failures.remove(&raw_title);
                    self.metadata_retry = None;
                    if let Some(display) = result.display {
                        self.track_info.insert(raw_title.clone(), display.clone());
                        self.queue_artwork(&raw_title, &display);
                        if fallback {
                            self.retitle_recording(&raw_title, &display);
                        }
                    } else {
                        self.continue_metadata_lookup(&raw_title, candidates);
                    }
                }
                Some(Err(e)) => {
                    log::warn!("metadata lookup failed for {raw_title}: {e}");
                    if !candidates.is_empty() {
                        self.continue_metadata_lookup(&raw_title, candidates);
                    } else {
                        self.retry_metadata_later(&raw_title);
                    }
                }
            }
            if let Some(pending) = self.metadata_pending.pop_front() {
                self.ensure_track_metadata(&pending);
            }
        }

        if self.metadata_poll.is_none() {
            if let Some((title, at)) = self.metadata_retry.clone() {
                if Instant::now() >= at {
                    self.metadata_retry = None;
                    self.ensure_track_metadata(&title);
                }
            }
        }

        let fetched = match self.radiobrowser.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some(Err("the radio-browser worker stopped".to_string()))
            }
            _ => None,
        };
        if let Some(result) = fetched {
            self.radiobrowser = None;
            match result {
                Ok(seeds) if seeds.is_empty() => {
                    self.last_message = Some(t("msg.radiobrowser_empty").to_string())
                }
                Ok(seeds) => {
                    let imported = self.db.as_ref().map(|db| db.import_seeds(&seeds));
                    match imported {
                        Some(Ok(count)) => {
                            self.reload_stations();
                            self.last_message =
                                Some(tf("msg.radiobrowser_imported", &[&count.to_string()]));
                        }
                        Some(Err(e)) => {
                            self.last_message =
                                Some(tf("msg.radiobrowser_failed", &[&e.to_string()]))
                        }
                        None => self.last_message = Some(t("msg.no_database").to_string()),
                    }
                }
                Err(e) => {
                    self.last_message = Some(tf("msg.radiobrowser_failed", &[&e]));
                }
            }
        }

        if let Some(at) = self.sleep_at {
            if Instant::now() >= at {
                self.stop_all_streams();
                self.sleep_at = None;
                self.sleep_minutes = 0;
                if self.sleep_shutdown {
                    match crate::system::shutdown() {
                        Ok(()) => {
                            self.last_message = Some(t("msg.shutdown_requested").to_string())
                        }
                        Err(e) => {
                            self.last_message =
                                Some(tf("msg.shutdown_failed", &[&e.to_string()]))
                        }
                    }
                } else {
                    self.last_message = Some(t("msg.sleep_stopped").to_string());
                }
            } else {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }

        // Close finished writers and run any deferred finish action before a
        // new window can queue another one.
        self.poll_finalising();
        self.poll_bitrate_probe();
        self.run_pending_finishes();

        // Wall-clock evaluation, not `Instant`: a suspend/resume must not skip a
        // schedule, and a missed occurrence is caught up from its stored window.
        self.tick_schedules();
        let mut engine = self.player.snapshot();

        // Forget recording streams that ended on their own.
        let recording_ids: HashSet<PlaybackId> = engine
            .streams
            .iter()
            .filter(|stream| stream.recording)
            .map(|stream| stream.id)
            .collect();
        self.recording_stations
            .retain(|id, _| recording_ids.contains(id));
        self.recording_labels
            .retain(|id, _| recording_ids.contains(id));

        // Stations like KNHC send one show-level ICY title for the whole show.
        // Poll their published playlist and feed each new song to the splitter,
        // so recordings still become per-song files.
        if self.recording_playlist_poll.is_none()
            && self.last_recording_poll.elapsed() >= Duration::from_secs(20)
        {
            let ids: Vec<PlaybackId> = self.recording_stations.keys().copied().collect();
            let target = if ids.is_empty() {
                None
            } else {
                let id = ids[self.recording_poll_cursor % ids.len()];
                self.recording_poll_cursor = self.recording_poll_cursor.wrapping_add(1);
                self.recording_stations.get(&id).and_then(|station_id| {
                    self.stations
                        .iter()
                        .find(|station| station.id == *station_id)
                        .and_then(|station| {
                            let source_id =
                                station.source_id.clone().filter(|id| !id.is_empty())?;
                            Some((id, source_id, station.url.clone(), station.website.clone()))
                        })
                })
            };
            if let Some((playback_id, source_id, url, website)) = target {
                let (tx, rx) = std::sync::mpsc::channel();
                let spawned = std::thread::Builder::new()
                    .name("viberadio-recording-playlist".to_string())
                    .spawn(move || {
                        let result =
                            station_playlist::fetch_playlist(&source_id, &url, website.as_deref());
                        let _ = tx.send((playback_id, result));
                    });
                if spawned.is_ok() {
                    self.recording_playlist_poll = Some(rx);
                    self.last_recording_poll = Instant::now();
                }
            }
        }

        let polled = match self
            .recording_playlist_poll
            .as_ref()
            .map(|rx| rx.try_recv())
        {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                Some((0, Err("the recording-playlist worker stopped".to_string())))
            }
            _ => None,
        };
        if let Some((playback_id, result)) = polled {
            self.recording_playlist_poll = None;
            if self.recording_stations.contains_key(&playback_id) {
                if let Ok(playlist) = result {
                    if let Some(entry) = playlist.entries.first() {
                        let artist = entry.artist.trim();
                        let title = entry.title.trim();
                        let label = if artist.is_empty() {
                            title.to_string()
                        } else {
                            format!("{artist} - {title}")
                        };
                        // A scraped page can lag several songs behind ICY. Only
                        // let an entry newer than the last title change split the
                        // recording; otherwise it would cut a song that the live
                        // title already handled correctly.
                        let icy_changed = self.now_playing_at.get(&playback_id).copied();
                        let started = self
                            .recording_started_at
                            .get(&playback_id)
                            .copied()
                            .unwrap_or_else(Utc::now);
                        let anchor = icy_changed.unwrap_or(started);
                        let static_for = Utc::now() - anchor;
                        let icy_updates = self.icy_changes.get(&playback_id).copied().unwrap_or(0);
                        // Once ICY updates per song it is the authoritative
                        // splitter; the scraped page is only for static show
                        // titles, and only after it has been static long enough
                        // to rule out a normal song.
                        let playlist_ok = icy_updates == 0
                            && static_for >= chrono::Duration::seconds(90)
                            && crate::station_playlist::entry_is_newer_than(entry, icy_changed);
                        if playlist_ok {
                            let changed = self
                                .recording_labels
                                .get(&playback_id)
                                .map(|last| last != &label)
                                .unwrap_or(true);
                            if changed && !title.is_empty() {
                                self.player.set_recording_track(
                                    playback_id,
                                    (!artist.is_empty()).then_some(artist),
                                    Some(title),
                                );
                                self.recording_labels.insert(playback_id, label.clone());
                                log::info!("recording {playback_id}: playlist split at {label}");
                            }
                        } else {
                            log::debug!(
                                "recording {playback_id}: ignoring playlist entry {label} (ICY updates: {icy_updates}, static for {}s)",
                                static_for.num_seconds()
                            );
                        }
                    }
                }
            }
        }

        if self.applied_theme != Some(self.ui.theme) {
            Theme::for_mode(self.ui.theme).apply(ctx);
            self.applied_theme = Some(self.ui.theme);
        }
        let theme = Theme::for_mode(self.ui.theme);

        if engine.revision != self.last_engine_revision {
            self.last_engine_revision = engine.revision;
            ctx.request_repaint();
        }

        // Live station ids for the table: every active stream is represented, not
        // only the active one.
        let live_stations: HashSet<Uuid> = engine
            .streams
            .iter()
            .filter(|stream| stream.status.is_active())
            .filter_map(|stream| stream.station_id)
            .collect();
        let table_active_station = engine
            .active
            .as_ref()
            .filter(|stream| stream.status.is_active())
            .and_then(|stream| stream.station_id);

        let active_id = engine.active.as_ref().map(|stream| stream.id);
        if active_id != self.last_active_id {
            self.last_active_id = active_id;
            if let Some(active) = &engine.active {
                // A record-only schedule starts its stream muted. Keep the
                // user's audible default instead of persisting 0.0.
                if !self.scheduler_muted.contains(&active.id) {
                    self.settings.volume = active.volume;
                }
                if let Some(station_id) = active.station_id {
                    self.selected_station = Some(station_id);
                }
            }
        }

        self.reconcile_attempts(&engine);
        self.ensure_watch_streams(&engine);
        self.record_track_changes(&engine);
        self.refresh_duration_schedules();
        self.refresh_live_fingerprint();
        self.watch_live_levels(&engine);
        self.poll_completed_recordings();
        // Ignored titles are recorded nowhere and shown as the station name, not
        // as a track change (FR-M3).
        let ignored_titles: HashSet<String> = engine
            .streams
            .iter()
            .filter_map(|stream| stream.now_playing.as_ref())
            .filter(|title| crate::metadata::title_is_ignored(&self.ignore_list, title))
            .cloned()
            .collect();
        if !ignored_titles.is_empty() {
            for stream in &mut engine.streams {
                if stream
                    .now_playing
                    .as_ref()
                    .is_some_and(|title| ignored_titles.contains(title))
                {
                    stream.now_playing = None;
                }
            }
            if let Some(active) = &mut engine.active {
                if active
                    .now_playing
                    .as_ref()
                    .is_some_and(|title| ignored_titles.contains(title))
                {
                    active.now_playing = None;
                }
            }
        }

        if self.ui.show_sessions {
            let mut open = true;
            let mut open_session = None;
            let mut delete_session = None;
            egui::Window::new(t("sessions.title"))
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.session_name);
                        if ui.button(t("sessions.save")).clicked() {
                            self.save_current_session();
                        }
                    });
                    ui.label(t("sessions.note"));
                    ui.separator();
                    if self.sessions.is_empty() {
                        ui.label(t("sessions.empty"));
                    }
                    for session in &self.sessions {
                        ui.horizontal(|ui| {
                            ui.label(tf(
                                "sessions.row",
                                &[&session.name, &session.station_count.to_string()],
                            ));
                            if ui.small_button(t("sessions.open")).clicked() {
                                open_session = Some(session.id);
                            }
                            if ui.small_button(t("sessions.delete")).clicked() {
                                delete_session = Some(session.id);
                            }
                        });
                    }
                });
            if let Some(id) = open_session {
                self.open_session(id);
            }
            if let Some(id) = delete_session {
                self.delete_session(id);
            }
            self.ui.show_sessions = open;
        }

        if self.ui.show_wishlist {
            let mut open = true;
            let mut add = false;
            let mut remove = None;
            let mut import = false;
            let mut export = false;
            egui::Window::new(t("wishlist.title"))
                .open(&mut open)
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.label(t("wishlist.note"));
                    ui.horizontal(|ui| {
                        ui.label(t("wishlist.search"));
                        ui.text_edit_singleline(&mut self.wish_search);
                        if ui.button(t("wishlist.import")).clicked() {
                            import = true;
                        }
                        if ui.button(t("wishlist.export")).clicked() {
                            export = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(t("wishlist.artist"));
                        ui.text_edit_singleline(&mut self.wish_artist);
                        ui.label(t("wishlist.song"));
                        ui.text_edit_singleline(&mut self.wish_song);
                    });
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("wish-action")
                            .selected_text(&self.wish_action)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.wish_action,
                                    "record".to_string(),
                                    t("wishlist.action_record"),
                                );
                                ui.selectable_value(
                                    &mut self.wish_action,
                                    "activate".to_string(),
                                    t("wishlist.action_activate"),
                                );
                                ui.selectable_value(
                                    &mut self.wish_action,
                                    "record+activate".to_string(),
                                    t("wishlist.action_record_activate"),
                                );
                                ui.selectable_value(
                                    &mut self.wish_action,
                                    "ignore".to_string(),
                                    t("wishlist.action_ignore"),
                                );
                            });
                        egui::ComboBox::from_id_salt("wish-compare")
                            .selected_text(compare_label(self.wish_compare))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.wish_compare,
                                    crate::metadata::CompareMode::Contains,
                                    t("wishlist.compare_contains"),
                                );
                                ui.selectable_value(
                                    &mut self.wish_compare,
                                    crate::metadata::CompareMode::Exact,
                                    t("wishlist.compare_exact"),
                                );
                                ui.selectable_value(
                                    &mut self.wish_compare,
                                    crate::metadata::CompareMode::StartsWith,
                                    t("wishlist.compare_starts"),
                                );
                            });
                        if ui.button(t("wishlist.add")).clicked() {
                            add = true;
                        }
                    });
                    ui.separator();
                    let needle = self.wish_search.trim().to_lowercase();
                    let matches = |entry: &crate::metadata::WishEntry| {
                        needle.is_empty()
                            || entry
                                .artist
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&needle)
                            || entry
                                .song_title
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&needle)
                            || entry.action.to_lowercase().contains(&needle)
                            || compare_label(entry.compare)
                                .to_lowercase()
                                .contains(&needle)
                    };
                    if self.wishlist.is_empty() {
                        ui.label(t("wishlist.empty"));
                    } else {
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .show(ui, |ui| {
                                let mut shown = 0usize;
                                for entry in
                                    self.wishlist.iter().filter(|entry| matches(entry))
                                {
                                    shown += 1;
                                    ui.horizontal(|ui| {
                                        let artist = entry.artist.as_deref().unwrap_or("*");
                                        let song = entry.song_title.as_deref().unwrap_or("*");
                                        ui.label(format!(
                                            "{artist} — {song}  [{} / {}]",
                                            entry.action,
                                            compare_label(entry.compare)
                                        ));
                                        if ui
                                            .small_button(t("wishlist.remove"))
                                            .clicked()
                                        {
                                            remove = Some(entry.id);
                                        }
                                    });
                                }
                                if shown == 0 {
                                    ui.label(t("wishlist.no_matches"));
                                }
                            });
                    }
                });
            if add {
                self.add_wish_entry();
            }
            if let Some(id) = remove {
                self.delete_wish(id);
            }
            if import {
                self.import_wishlist();
            }
            if export {
                self.export_wishlist();
            }
            self.ui.show_wishlist = open;
        }

        if self.ui.show_watch_history {
            let mut open = true;
            let watched: HashSet<Uuid> = self
                .stations
                .iter()
                .filter(|station| station.watched)
                .map(|station| station.id)
                .collect();
            let mut wish: Option<(Option<String>, String)> = None;
            egui::Window::new(t("watch_history.title"))
                .open(&mut open)
                .default_width(620.0)
                .show(ctx, |ui| {
                    ui.label(t("watch_history.note"));
                    ui.separator();
                    let rows: Vec<&crate::models::StationHistory> = self
                        .history
                        .iter()
                        .filter(|row| {
                            watched.contains(&row.station_id) && row.song_title.is_some()
                        })
                        .take(200)
                        .collect();
                    if rows.is_empty() {
                        ui.label(t("watch_history.empty"));
                    }
                    egui::ScrollArea::vertical()
                        .max_height(420.0)
                        .show(ui, |ui| {
                            for row in rows {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(
                                            row.played_at
                                                .with_timezone(&Local)
                                                .format("%H:%M")
                                                .to_string(),
                                        )
                                        .size(12.0)
                                        .color(theme.text_secondary),
                                    );
                                    ui.label(
                                        egui::RichText::new(&row.station_name)
                                            .size(12.0)
                                            .color(theme.text_disabled),
                                    );
                                    ui.label(format!(
                                        "{} — {}",
                                        row.artist.as_deref().unwrap_or(""),
                                        row.song_title.as_deref().unwrap_or("")
                                    ));
                                    if ui
                                        .small_button(t("watch_history.wish"))
                                        .clicked()
                                    {
                                        wish = Some((
                                            row.artist.clone(),
                                            row.song_title.clone().unwrap_or_default(),
                                        ));
                                    }
                                });
                            }
                        });
                });
            if let Some((artist, title)) = wish {
                self.want_song(artist, title);
            }
            self.ui.show_watch_history = open;
        }

        if self.ui.show_settings && !self.devices_loaded {
            self.output_devices = RadioPlayer::output_devices();
            // Discard an edit that was never saved, so reopening shows the
            // values that are actually in use.
            self.output_device = self.applied_device.clone();
            self.stream_buffer_secs = self.applied_stream_buffer;
            // The language is applied only on Save, so discard an unsaved pick.
            self.language = i18n::language();
            self.devices_loaded = true;
        } else if !self.ui.show_settings {
            self.devices_loaded = false;
        }

        if self.ui.show_settings {
            let mut open = true;
            let mut save = false;
            let mut split_latest = false;
            let mut fingerprint_now = false;
            let mut gain_change: Option<(Uuid, f32)> = None;
            let mut gain_commit: Option<(Uuid, f32)> = None;
            let mut gain_dirty: Option<(Uuid, f32)> = None;
            let mut preset_change: Option<DspPreset> = None;
            let mut backup_now = false;
            let mut restore_latest = false;
            let mut cancel_restore = false;
            let mut change_data_folder = false;
            let gain_station = self
                .selected_station
                .or_else(|| engine.active.as_ref().and_then(|stream| stream.station_id));
            egui::Window::new(t("settings.title"))
                .open(&mut open)
                .default_width(520.0)
                .default_height(520.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(t("settings.language"));
                                egui::ComboBox::from_id_salt("language")
                                    .selected_text(self.language.label())
                                    .show_ui(ui, |ui| {
                                        for lang in i18n::Lang::ALL {
                                            ui.selectable_value(
                                                &mut self.language,
                                                lang,
                                                lang.label(),
                                            );
                                        }
                                    });
                            });
                            ui.separator();
                            ui.strong(t("settings.audio"));
                            let devices = self.output_devices.clone();
                            ui.horizontal(|ui| {
                                ui.label(t("settings.output_device"));
                                let selected = if self.output_device.is_empty() {
                                    t("settings.system_default").to_string()
                                } else {
                                    self.output_device.clone()
                                };
                                egui::ComboBox::from_id_salt("output-device")
                                    .selected_text(selected)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.output_device,
                                            String::new(),
                                            t("settings.system_default"),
                                        );
                                        for device in &devices {
                                            ui.selectable_value(
                                                &mut self.output_device,
                                                device.clone(),
                                                device,
                                            );
                                        }
                                    });
                                if ui.button(t("settings.refresh")).clicked() {
                                    self.output_devices = RadioPlayer::output_devices();
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label(t("settings.stream_buffer"));
                                egui::ComboBox::from_id_salt("stream-buffer")
                                    .selected_text(tf(
                                        "settings.seconds_value",
                                        &[&self.stream_buffer_secs.to_string()],
                                    ))
                                    .show_ui(ui, |ui| {
                                        for seconds in [2u32, 4, 8] {
                                            ui.selectable_value(
                                                &mut self.stream_buffer_secs,
                                                seconds,
                                                tf(
                                                    "settings.seconds_value",
                                                    &[&seconds.to_string()],
                                                ),
                                            );
                                        }
                                    });
                            });
                            ui.label(
                                egui::RichText::new(t("settings.device_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            let mut preset = self.dsp_preset;
                            ui.horizontal(|ui| {
                                ui.label(t("settings.dynamic_boost"));
                                egui::ComboBox::from_id_salt("dsp-preset")
                                    .selected_text(preset.label())
                                    .show_ui(ui, |ui| {
                                        for option in DspPreset::ALL {
                                            ui.selectable_value(
                                                &mut preset,
                                                option,
                                                option.label(),
                                            );
                                        }
                                    });
                            });
                            if preset != self.dsp_preset {
                                preset_change = Some(preset);
                            }
                            if let Some(id) = gain_station {
                                if let Some(station) =
                                    self.stations.iter().find(|station| station.id == id)
                                {
                                    let mut gain = station.gain_db;
                                    ui.horizontal(|ui| {
                                        ui.label(t("settings.station_gain"));
                                        let response = ui.add(
                                            egui::Slider::new(&mut gain, -12.0..=12.0)
                                                .step_by(0.5)
                                                .suffix(" dB"),
                                        );
                                        if response.changed() {
                                            gain_change = Some((id, gain));
                                        }
                                        if response.drag_stopped() || response.lost_focus() {
                                            gain_commit = Some((id, gain));
                                        }
                                    });
                                    ui.label(tf("settings.gain_applies", &[&station.name]));
                                }
                            } else {
                                ui.label(t("settings.select_station_gain"));
                            }
                            ui.label(
                                egui::RichText::new(t("settings.gain_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            ui.separator();
                            ui.strong(t("settings.recording"));
                            ui.label(t("settings.folder"));
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(&mut self.recording_dir);
                                if ui.small_button(t("settings.open_folder")).clicked() {
                                    if let Err(e) = crate::system::open_path(
                                        std::path::Path::new(self.recording_dir.trim()),
                                    ) {
                                        self.last_message = Some(tf(
                                            "msg.open_folder_failed",
                                            &[&e.to_string()],
                                        ));
                                    }
                                }
                            });
                            ui.label(t("settings.template"));
                            ui.text_edit_singleline(&mut self.recording_template);
                            let preview = crate::recording::render_name(
                                &self.recording_template,
                                "Station",
                                "Artist - Title",
                                Local::now(),
                            );
                            ui.label(
                                egui::RichText::new(tf(
                                    "settings.template_preview",
                                    &[&preview],
                                ))
                                .size(12.0)
                                .color(theme.text_secondary),
                            );
                            let unknown =
                                crate::recording::unknown_tokens(&self.recording_template);
                            if !unknown.is_empty() {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    tf(
                                        "settings.template_unknown",
                                        &[&unknown.join(", ")],
                                    ),
                                );
                            }
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.recording_split, t("settings.split"));
                            });
                            ui.horizontal(|ui| {
                                ui.label(t("settings.min_track"));
                                ui.add(
                                    egui::DragValue::new(&mut self.recording_min_track_seconds)
                                        .range(0..=60)
                                        .suffix(" s"),
                                );
                            });
                            ui.label(
                                egui::RichText::new(t("settings.min_track_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            ui.horizontal(|ui| {
                                ui.label(t("settings.min_fingerprint"));
                                ui.add(
                                    egui::DragValue::new(
                                        &mut self.recording_min_fingerprint_seconds,
                                    )
                                    .range(0..=300)
                                    .suffix(" s"),
                                );
                            });
                            ui.label(
                                egui::RichText::new(t("settings.min_fingerprint_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            ui.checkbox(
                                &mut self.recording_wishlist_only,
                                t("settings.wishlist_only"),
                            );
                            ui.horizontal(|ui| {
                                ui.label(t("settings.min_free_space"));
                                ui.add(
                                    egui::DragValue::new(&mut self.recording_min_free_mb)
                                        .range(0..=100_000)
                                        .suffix(" MB"),
                                );
                            });
                            ui.label(
                                egui::RichText::new(t("settings.min_free_space_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            ui.checkbox(
                                &mut self.recording_keep_best,
                                t("settings.keep_best"),
                            );
                            ui.label(
                                egui::RichText::new(t("settings.keep_best_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            let codec_text = match self.recording_codec.as_str() {
                                "mp3" => t("settings.codec_mp3"),
                                "aac" => t("settings.codec_aac"),
                                _ => t("settings.codec_any"),
                            };
                            ui.horizontal(|ui| {
                                ui.label(t("settings.min_bitrate"));
                                ui.add(
                                    egui::DragValue::new(&mut self.recording_min_kbps)
                                        .range(0..=320)
                                        .suffix(" kbps"),
                                );
                                ui.label(t("settings.codec"));
                                egui::ComboBox::from_id_salt("recording-codec")
                                    .selected_text(codec_text)
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.recording_codec,
                                            "any".to_string(),
                                            t("settings.codec_any"),
                                        );
                                        ui.selectable_value(
                                            &mut self.recording_codec,
                                            "mp3".to_string(),
                                            t("settings.codec_mp3"),
                                        );
                                        ui.selectable_value(
                                            &mut self.recording_codec,
                                            "aac".to_string(),
                                            t("settings.codec_aac"),
                                        );
                                    });
                            });
                            ui.label(
                                egui::RichText::new(t("settings.min_bitrate_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            ui.separator();
                            ui.strong(t("settings.fingerprinting"));
                            ui.label(t("settings.acoustid_key"));
                            let key_response =
                                ui.text_edit_singleline(&mut self.acoustid_api_key);
                            if key_response.changed() {
                                if let Some(db) = &self.db {
                                    let _ = db.set_setting(
                                        "acoustid_api_key",
                                        self.acoustid_api_key.trim(),
                                    );
                                }
                            }
                            ui.label(
                                egui::RichText::new(t("settings.acoustid_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            if ui.button(t("settings.fingerprint_now")).clicked() {
                                fingerprint_now = true;
                            }
                            ui.label(t("settings.tokens"));
                            ui.horizontal(|ui| {
                                if ui.button(t("settings.save")).clicked() {
                                    save = true;
                                }
                                if ui.button(t("settings.split_last")).clicked() {
                                    split_latest = true;
                                }
                            });
                            ui.separator();
                            ui.strong(t("settings.sleep"));
                            ui.checkbox(
                                &mut self.sleep_shutdown,
                                t("settings.sleep_shutdown"),
                            );
                            ui.label(
                                egui::RichText::new(t("settings.sleep_shutdown_note"))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                            );
                            ui.separator();
                            ui.strong(t("settings.backup"));
                            let data = self
                                .data_dir_path
                                .as_ref()
                                .map(|dir| dir.display().to_string())
                                .unwrap_or_else(|| t("settings.unavailable").to_string());
                            ui.label(tf("settings.backups_written", &[&data]));
                            if let Some(latest) = &self.latest_backup {
                                let name = latest
                                    .file_name()
                                    .map(|name| name.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                ui.label(tf("settings.latest", &[&name]));
                            }
                            ui.horizontal(|ui| {
                                if ui.button(t("settings.backup_now")).clicked() {
                                    backup_now = true;
                                }
                                if ui.button(t("settings.restore_latest")).clicked() {
                                    restore_latest = true;
                                }
                            });
                            ui.label(t("settings.close_other"));
                            let pending = self
                                .data_dir_path
                                .as_ref()
                                .map(|dir| dir.join("viberadio-restore.pending").exists())
                                .unwrap_or(false);
                            if pending {
                                ui.label(t("settings.restore_pending"));
                                if ui.button(t("settings.cancel_restore")).clicked() {
                                    cancel_restore = true;
                                }
                            }
                            ui.separator();
                            ui.strong(t("settings.data_folder"));
                            if std::env::var("VIBERADIO_DATA_DIR").is_ok() {
                                ui.label(t("settings.data_dir_override"));
                            }
                            ui.label(t("settings.restart_required"));
                            ui.text_edit_singleline(&mut self.data_dir_input);
                            if ui.button(t("settings.copy_and_switch")).clicked() {
                                change_data_folder = true;
                            }
                        });
                });
            if let Some((id, gain)) = gain_change {
                self.apply_station_gain(id, gain);
                gain_dirty = Some((id, gain));
            }
            if let Some((id, gain)) = gain_commit {
                self.persist_station_gain(id, gain);
                gain_dirty = None;
            }
            if !open {
                if let Some((id, gain)) = gain_dirty {
                    self.persist_station_gain(id, gain);
                }
            }
            if let Some(preset) = preset_change {
                self.set_dsp_preset(preset);
            }
            if backup_now {
                self.backup_now();
            }
            if restore_latest {
                self.request_restore_latest();
            }
            if cancel_restore {
                self.cancel_pending_restore();
            }
            if change_data_folder {
                self.change_data_folder();
            }
            if fingerprint_now {
                self.start_fingerprint_scan();
            }
            if save {
                if self.recording_template.trim().is_empty() {
                    self.recording_template = "{station} - {date} {time}".to_string();
                }
                if self.recording_dir.trim().is_empty() {
                    self.recording_dir = "recordings".to_string();
                }
                let dir = std::path::PathBuf::from(self.recording_dir.trim());
                let validated = std::fs::create_dir_all(&dir).and_then(|()| {
                    let probe = dir.join(".viberadio-write-test");
                    std::fs::write(&probe, b"ok")?;
                    std::fs::remove_file(probe)?;
                    std::fs::canonicalize(&dir)
                });
                match validated {
                    Ok(path) => self.recording_dir = path.display().to_string(),
                    Err(e) => {
                        self.last_message =
                            Some(tf("msg.recording_dir_unwritable", &[&e.to_string()]));
                        self.ui.show_settings = open;
                        return;
                    }
                }
                let previous_device = self.applied_device.clone();
                if let Some(db) = &self.db {
                    let _ = db.set_setting("recording_dir", self.recording_dir.trim());
                    let _ = db.set_setting("recording_template", self.recording_template.trim());
                    let _ = db.set_setting(
                        "recording_mode",
                        if self.recording_split {
                            "split"
                        } else {
                            "continuous"
                        },
                    );
                    let _ = db.set_setting(
                        "recording_wishlist_only",
                        if self.recording_wishlist_only {
                            "1"
                        } else {
                            "0"
                        },
                    );
                    let _ = db.set_setting(
                        "recording_min_free_mb",
                        &self.recording_min_free_mb.to_string(),
                    );
                    let _ = db.set_setting(
                        "recording_keep_best",
                        if self.recording_keep_best { "1" } else { "0" },
                    );
                    let _ = db.set_setting(
                        "recording_min_kbps",
                        &self.recording_min_kbps.to_string(),
                    );
                    let _ = db.set_setting("recording_codec", &self.recording_codec);
                    let _ = db.set_setting("acoustid_api_key", self.acoustid_api_key.trim());
                    let _ = db.set_setting(
                        "recording_min_track_seconds",
                        &self.recording_min_track_seconds.to_string(),
                    );
                    let _ = db.set_setting(
                        "recording_min_fingerprint_seconds",
                        &self.recording_min_fingerprint_seconds.to_string(),
                    );
                    let _ = db.set_setting(
                        "sleep_shutdown",
                        if self.sleep_shutdown { "1" } else { "0" },
                    );
                    let _ = db.set_setting("output_device", &self.output_device);
                    let _ =
                        db.set_setting("stream_buffer_secs", &self.stream_buffer_secs.to_string());
                    let _ = db.set_setting("language", self.language.code());
                }
                i18n::set_language(self.language);
                self.player.set_stream_buffer(self.stream_buffer_secs);
                self.applied_stream_buffer = self.stream_buffer_secs;
                if previous_device != self.output_device {
                    // Sinks belong to the old output stream, so stop first.
                    self.stop_all_streams();
                    let target =
                        (!self.output_device.is_empty()).then(|| self.output_device.clone());
                    match self.player.set_output_device(target) {
                        Ok(None) => {
                            self.applied_device = self.output_device.clone();
                            let name = if self.output_device.is_empty() {
                                "the system default".to_string()
                            } else {
                                format!("\"{}\"", self.output_device)
                            };
                            self.last_message = Some(tf("msg.output_device", &[&name]));
                        }
                        Ok(Some(warning)) => {
                            self.applied_device = String::new();
                            self.last_message = Some(tf("msg.output_fallback", &[&warning]));
                        }
                        Err(e) => {
                            self.last_message =
                                Some(tf("msg.output_switch_failed", &[&e.to_string()]))
                        }
                    }
                } else {
                    self.last_message = Some(t("msg.audio_saved").to_string());
                }
            }
            if split_latest {
                self.split_latest_recording();
            }
            if !open {
                if let Some(db) = &self.db {
                    let _ = db.set_setting("acoustid_api_key", self.acoustid_api_key.trim());
                }
            }
            self.ui.show_settings = open;
        }

        if self.ui.show_history {
            let mut open = true;
            egui::Window::new(t("history.title"))
                .open(&mut open)
                .default_width(760.0)
                .default_height(420.0)
                .show(ctx, |ui| {
                    if self.history.is_empty() {
                        ui.label(t("history.empty"));
                        return;
                    }
                    ui.label(tf("history.count", &[&self.history.len().to_string()]));
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new("history-grid")
                            .striped(true)
                            .num_columns(6)
                            .show(ui, |ui| {
                                ui.strong(t("history.played"));
                                ui.strong(t("history.station"));
                                ui.strong(t("history.artist"));
                                ui.strong(t("history.song"));
                                ui.strong(t("history.duration"));
                                ui.strong(t("history.status"));
                                ui.end_row();
                                for row in self.history.iter().take(200) {
                                    ui.label(
                                        row.played_at
                                            .with_timezone(&Local)
                                            .format("%Y-%m-%d %H:%M")
                                            .to_string(),
                                    );
                                    ui.label(&row.station_name);
                                    ui.label(row.artist.as_deref().unwrap_or("—"));
                                    ui.label(row.song_title.as_deref().unwrap_or("—"));
                                    ui.label(format!("{}s", row.duration_seconds));
                                    ui.label(&row.status);
                                    ui.end_row();
                                }
                            });
                    });
                });
            self.ui.show_history = open;
        }

        if self.ui.show_metadata_debug {
            let mut open = true;
            let active = engine.active.as_ref();
            let station_id = active.and_then(|stream| stream.station_id);
            let station_name = station_id
                .and_then(|id| self.stations.iter().find(|station| station.id == id))
                .map(|station| station.name.clone());
            let raw = active
                .and_then(|stream| stream.now_playing.as_deref())
                .unwrap_or(t("debug.none"));
            // Start editing from the stored format when the station changes.
            if let Some(id) = station_id {
                if self.tag_format_station != Some(id) {
                    self.tag_format_station = Some(id);
                    self.tag_format_input = self
                        .db
                        .as_ref()
                        .and_then(|db| db.custom_tag_format(id).ok().flatten())
                        .unwrap_or_default();
                }
            }
            let format = station_id
                .and_then(|id| {
                    self.db
                        .as_ref()
                        .and_then(|db| db.custom_tag_format(id).ok().flatten())
                })
                .unwrap_or_else(|| crate::metadata::default_tag_format(raw).to_string());
            let parsed = crate::metadata::split_title(raw, &format);
            let ignored = crate::metadata::title_is_ignored_with_format(
                &self.ignore_list,
                raw,
                Some(&format),
            );
            let hits =
                crate::metadata::wish_matches_with_format(&self.wishlist, raw, Some(&format)).len();
            egui::Window::new(t("debug.title"))
                .open(&mut open)
                .default_width(560.0)
                .show(ctx, |ui| {
                    ui.label(tf(
                        "debug.station",
                        &[station_name.as_deref().unwrap_or("—")],
                    ));
                    ui.label(tf(
                        "debug.status",
                        &[active
                            .map(|stream| stream.status_text.as_str())
                            .unwrap_or(t("debug.idle"))],
                    ));
                    ui.label(tf("debug.raw", &[raw]));
                    ui.label(tf("debug.format", &[&format]));
                    ui.horizontal(|ui| {
                        ui.label(t("debug.format_edit"));
                        ui.text_edit_singleline(&mut self.tag_format_input);
                        if ui
                            .add_enabled(
                                station_id.is_some() && !self.tag_format_input.trim().is_empty(),
                                egui::Button::new(t("debug.save_format")),
                            )
                            .clicked()
                        {
                            if let (Some(db), Some(id)) = (&self.db, station_id) {
                                match db.set_custom_tag_format(
                                    id,
                                    Some(self.tag_format_input.trim()),
                                ) {
                                    Ok(()) => {
                                        self.last_message =
                                            Some(t("msg.format_saved").to_string())
                                    }
                                    Err(e) => {
                                        self.last_message = Some(tf(
                                            "msg.format_save_failed",
                                            &[&e.to_string()],
                                        ))
                                    }
                                }
                            }
                        }
                        if ui
                            .add_enabled(
                                station_id.is_some(),
                                egui::Button::new(t("debug.reset_format")),
                            )
                            .clicked()
                        {
                            if let (Some(db), Some(id)) = (&self.db, station_id) {
                                let _ = db.set_custom_tag_format(id, None);
                                self.tag_format_input.clear();
                                self.last_message = Some(t("msg.format_reset").to_string());
                            }
                        }
                    });
                    ui.label(tf(
                        "debug.artist_song",
                        &[
                            parsed.artist.as_deref().unwrap_or("—"),
                            parsed.title.as_deref().unwrap_or("—"),
                        ],
                    ));
                    ui.label(tf(
                        "debug.ignored",
                        &[
                            if ignored {
                                t("debug.yes")
                            } else {
                                t("debug.no")
                            },
                            &hits.to_string(),
                        ],
                    ));
                    ui.label(tf(
                        "debug.url",
                        &[active.map(|stream| stream.url.as_str()).unwrap_or("—")],
                    ));
                    if let Some(error) = active.and_then(|stream| stream.error.as_deref()) {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    ui.separator();
                    ui.label(t("debug.hint"));
                });
            self.ui.show_metadata_debug = open;
        }

        // Toast lifecycle: track changes centrally, hide after a few seconds, and
        // drop a "Connecting…" toast as soon as audio is actually live.
        if self.last_message != self.shown_message {
            self.shown_message = self.last_message.clone();
            self.message_at = Some(Instant::now());
        }
        let stale = self
            .message_at
            .map(|at| at.elapsed() > Duration::from_secs(6))
            .unwrap_or(false);
        let connecting_id = self.connecting_station;
        let connecting_toast = connecting_id.map(|id| {
            let name = self
                .stations
                .iter()
                .find(|station| station.id == id)
                .map(|station| station.name.as_str())
                .unwrap_or_default();
            tf("msg.connecting", &[name])
        });
        // Only clear the toast if it is still the connecting one; another
        // message may have replaced it while the stream was connecting.
        let toast_is_connecting = connecting_toast
            .as_deref()
            .map(|prefix| {
                self.last_message
                    .as_deref()
                    .map(|message| message.starts_with(prefix))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let still_connecting = connecting_id
            .map(|id| {
                engine.streams.iter().any(|stream| {
                    stream.station_id == Some(id)
                        && matches!(
                            stream.status,
                            PlaybackStatus::Connecting | PlaybackStatus::Buffering
                        )
                })
            })
            .unwrap_or(false);
        let connecting_live = connecting_id
            .map(|id| {
                engine.streams.iter().any(|stream| {
                    stream.status == PlaybackStatus::Live && stream.station_id == Some(id)
                })
            })
            .unwrap_or(false);
        if !still_connecting || !toast_is_connecting {
            self.connecting_station = None;
        }
        if stale || (toast_is_connecting && connecting_live) {
            self.last_message = None;
            self.connecting_station = None;
        }

        let (mut actions, focus_search) = self.handle_shortcuts(ctx, &engine);

        if self.last_heartbeat.elapsed() >= Duration::from_secs(5) {
            self.last_heartbeat = Instant::now();
            if let Some(db) = &self.db {
                if let Err(e) = db.write_heartbeat() {
                    log::warn!("could not write the library heartbeat: {e}");
                }
            }
        }
        if !self.recording_stations.is_empty()
            && self.last_space_check.elapsed() >= Duration::from_secs(30)
        {
            self.last_space_check = Instant::now();
            if !crate::disk::has_space(
                std::path::Path::new(self.recording_dir.trim()),
                self.recording_min_free_mb,
            ) {
                let ids: Vec<PlaybackId> = self.recording_stations.keys().copied().collect();
                let active = self.player.snapshot().active.map(|stream| stream.id);
                for id in ids {
                    if active == Some(id) {
                        // Keep the audible stream playing; only the recorder stops.
                        self.stop_recording_on(id);
                    } else {
                        // A muted background or scheduled stream has no reason
                        // to keep downloading once its recording is gone.
                        self.stop_one_stream(id);
                    }
                }
                self.last_message = Some(t("msg.recording_stopped_low_space").to_string());
            }
        }

        self.ensure_view();
        if !self.bitrate_probe_checked {
            self.bitrate_probe_checked = true;
            let seeded = self
                .db
                .as_ref()
                .and_then(|db| db.get_setting("bitrate_probe_seeded").ok().flatten())
                .is_some();
            let missing = self
                .stations
                .iter()
                .any(|station| station.bitrate.unwrap_or(0) == 0);
            if !seeded && missing {
                if let Some(db) = &self.db {
                    let _ = db.set_setting("bitrate_probe_seeded", "1");
                }
                self.start_bitrate_probe();
            }
        }
        self.poll_playlist();
        self.ensure_playlist();
        let stats_key = (self.library_revision, self.history_version);
        if self.stats_key != Some(stats_key) {
            self.stats = self.library_stats();
            self.stats_key = Some(stats_key);
        }
        let stats = self.stats.clone();
        let active_station = self.active_station(&engine).cloned();
        let stream_rows = self.stream_rows(&engine);

        let tray_state = TrayState {
            station: active_station
                .as_ref()
                .map(|station| station.name.clone())
                .unwrap_or_default(),
            now_playing: engine.active.as_ref().and_then(|stream| {
                stream
                    .display_now_playing
                    .clone()
                    .or_else(|| stream.now_playing.clone())
            }),
            playing: engine.active.is_some(),
            language: self.language as u8,
        };
        if self.last_tray.as_ref() != Some(&tray_state) {
            if let Some(tray) = &self.tray {
                tray.update(tray_state.clone());
            }
            self.last_tray = Some(tray_state);
        }

        let media_state = MediaState {
            station: active_station
                .as_ref()
                .map(|station| station.name.clone())
                .unwrap_or_default(),
            title: engine.active.as_ref().and_then(|stream| {
                stream
                    .display_now_playing
                    .clone()
                    .or_else(|| stream.now_playing.clone())
            }),
            playing: engine.active.is_some(),
            volume: engine
                .active
                .as_ref()
                .map(|stream| stream.volume)
                .unwrap_or(self.settings.volume),
        };
        if self.last_media.as_ref() != Some(&media_state) {
            if let Some(media) = &mut self.media {
                media.update(&media_state);
            }
            self.last_media = Some(media_state);
        }

        let next_schedule = self.next_schedule_label();
        let display_track = self.display_track(&engine);

        // Player bar pinned to the bottom, then the optional stream list above it.
        egui::TopBottomPanel::bottom("player-bar")
            .exact_height(ui::PLAYER_HEIGHT)
            .frame(
                egui::Frame::none()
                    .fill(theme.bg_player)
                    .inner_margin(egui::Margin {
                        left: 12.0,
                        right: 12.0,
                        top: 0.0,
                        bottom: 8.0,
                    }),
            )
            .show(ctx, |ui| {
                actions.extend(ui::player::render_bar(
                    ui,
                    &theme,
                    &mut self.ui,
                    &engine,
                    active_station.as_ref(),
                    display_track.as_ref(),
                    self.selected_station,
                    engine
                        .active
                        .as_ref()
                        .map(|stream| stream.volume)
                        .unwrap_or(self.settings.volume),
                    self.sleep_at
                        .map(|at| at.saturating_duration_since(Instant::now())),
                    next_schedule
                        .as_ref()
                        .map(|(label, tooltip)| (label.as_str(), tooltip.as_str())),
                ));
            });

        if self.ui.show_streams {
            let cap = (ctx.available_rect().height() * 0.4).max(96.0);
            let height = ui::player::streams_height(stream_rows.len()).min(cap);
            egui::TopBottomPanel::bottom("stream-list")
                .exact_height(height)
                .frame(
                    egui::Frame::none()
                        .fill(theme.bg_app)
                        .inner_margin(egui::Margin::symmetric(12.0, 4.0)),
                )
                .show(ctx, |ui| {
                    actions.extend(ui::player::render_streams(ui, &theme, &stream_rows));
                });
        }

        egui::SidePanel::left("filter-sidebar")
            .exact_width(ui::SIDEBAR_WIDTH)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme.bg_sidebar))
            .show(ctx, |ui| {
                actions.extend(ui::sidebar::render(ui, &theme, &mut self.ui, &stats));
            });

        let playlist_station = self
            .selected_station
            .and_then(|id| self.stations.iter().find(|station| station.id == id));
        let provider_playlist = self.selected_station.and_then(|id| self.playlists.get(&id));
        let merged = merged_playlist(self.selected_station, &self.history, provider_playlist);
        let playlist = if merged.entries.is_empty() && merged.next.is_none() {
            None
        } else {
            Some(&merged)
        };
        let playlist_error = if playlist.is_some() {
            None
        } else {
            self.selected_station
                .and_then(|id| self.playlist_errors.get(&id))
        };
        let playlist_loading =
            self.playlist_fetch.is_some() && playlist.is_none() && playlist_error.is_none();
        // The panel describes the selected station, so it must not borrow the
        // title of a different stream that happens to be playing.
        let selected_now_playing =
            selected_now_playing(engine.active.as_ref(), self.selected_station);
        egui::SidePanel::right("now-playing")
            .resizable(true)
            .default_width(280.0)
            .width_range(220.0..=420.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.bg_sidebar)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui::now_playing::render(
                    ui,
                    &theme,
                    playlist_station,
                    selected_now_playing,
                    playlist,
                    playlist_error.map(String::as_str),
                    playlist_loading,
                    &mut actions,
                );
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme.bg_app)
                    .inner_margin(egui::Margin::symmetric(12.0, 8.0)),
            )
            .show(ctx, |ui| {
                actions.extend(ui::table::render(
                    ui,
                    &theme,
                    &mut self.ui,
                    &self.stations,
                    &self.view_indices,
                    self.selected_station,
                    table_active_station,
                    &live_stations,
                ));
            });

        actions.extend(self.render_add_dialog(ctx, &theme));
        self.render_remove_dialog(ctx);
        actions.extend(self.render_schedules_window(ctx, &theme));
        self.render_about_window(ctx);
        self.render_help_window(ctx);

        // An immediate viewport is rendered during the root's update pass. The
        // app requests a repaint every 500 ms, so the mini keeps updating while
        // the main window is minimised or behind another window.
        if self.mini_open {
            let viewport_id = egui::ViewportId::from_hash_of("viberadio-mini");
            let builder = egui::ViewportBuilder::default()
                .with_title("VibeRadio")
                .with_inner_size([320.0, 132.0])
                .with_min_inner_size([300.0, 120.0])
                .with_resizable(false)
                .with_always_on_top();
            let mini_theme = Theme::for_mode(self.ui.theme);
            let mini_station = self.active_station(&engine).cloned();
            let mini_volume = engine
                .active
                .as_ref()
                .map(|stream| stream.volume)
                .unwrap_or(self.settings.volume);
            let selected = self.selected_station;
            let mut close = false;
            let mini_actions = ctx.show_viewport_immediate(viewport_id, builder, |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                ui::mini::render(
                    ctx,
                    &mini_theme,
                    &engine,
                    mini_station.as_ref(),
                    display_track.as_ref(),
                    selected,
                    mini_volume,
                )
            });
            if close {
                ctx.send_viewport_cmd_to(viewport_id, egui::ViewportCommand::Close);
                self.mini_open = false;
            }
            actions.extend(mini_actions);
        }

        for action in actions {
            self.apply_action(action);
        }
        if focus_search {
            ctx.memory_mut(|memory| memory.request_focus(ui::table::search_id()));
        }
        if self.scheduler.pending_close {
            self.scheduler.pending_close = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // A floating toast, so transient status text never shifts table rows.
        let mut dismiss = false;
        if let Some(message) = &self.last_message {
            egui::Area::new(egui::Id::new("status-toast"))
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::none()
                        .fill(theme.bg_player)
                        .stroke(egui::Stroke::new(1.0, theme.border))
                        .rounding(4.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(message)
                                        .size(12.0)
                                        .color(theme.text_primary),
                                );
                                if ui.small_button("×").clicked() {
                                    dismiss = true;
                                }
                            });
                        });
                });
        }
        if dismiss {
            self.last_message = None;
        }

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl Drop for VibeRadioApp {
    fn drop(&mut self) {
        // Close and join every recording on exit; a blocking join here is fine
        // and prevents a truncated file behind a scheduled close.
        let recording_ids: Vec<PlaybackId> = self
            .player
            .snapshot()
            .streams
            .iter()
            .map(|stream| stream.id)
            .collect();
        for id in recording_ids {
            let _ = self.stop_recording_on(id);
        }
        for item in self.finalising.drain(..) {
            let _ = item.handle.join();
        }
        let ids: Vec<PlaybackId> = self.attempts.keys().copied().collect();
        for id in ids {
            self.finalise_attempt(id, "ended", Some("app closed"));
        }
        self.player.stop_all();
        if let Some(db) = &self.db {
            if let Err(e) = db.clear_heartbeat() {
                log::warn!("could not clear the library heartbeat: {e}");
            }
        }
    }
}

/// Position after `delta` in a ring, or `None` when there is nothing to play.
fn next_index(len: usize, current: Option<usize>, delta: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match current {
        Some(position) => Some((position as i32 + delta).rem_euclid(len as i32) as usize),
        None if delta < 0 => Some(len - 1),
        None => Some(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn station(name: &str, genre: &str, bitrate: Option<u32>, favorite: bool) -> Station {
        let mut station = Station::new(name, "http://example.invalid/stream");
        station.genre = Some(genre.to_string());
        station.bitrate = bitrate;
        station.is_favorite = favorite;
        station
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("viberadio-app-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn next_index_wraps_and_handles_an_empty_view() {
        assert_eq!(next_index(0, None, 1), None);
        assert_eq!(next_index(0, Some(0), 1), None);
        assert_eq!(next_index(3, None, 1), Some(0));
        assert_eq!(next_index(3, None, -1), Some(2));
        assert_eq!(next_index(3, Some(0), 1), Some(1));
        assert_eq!(next_index(3, Some(2), 1), Some(0));
        assert_eq!(next_index(3, Some(0), -1), Some(2));
        assert_eq!(next_index(3, Some(1), 5), Some(0));
        assert_eq!(next_index(3, Some(1), -5), Some(2));
    }

    #[test]
    fn pointer_redirects_the_data_dir() {
        let default = temp_dir("pointer-default");
        let custom = temp_dir("pointer-custom");
        std::fs::write(custom.join("viberadio.db"), b"").unwrap();
        set_data_dir_pointer(&default, Some(&custom)).unwrap();
        assert_eq!(resolve_data_dir(default.clone(), None).unwrap(), custom);
        set_data_dir_pointer(&default, None).unwrap();
        assert_eq!(resolve_data_dir(default.clone(), None).unwrap(), default);
        let _ = std::fs::remove_dir_all(&default);
        let _ = std::fs::remove_dir_all(&custom);
    }

    #[test]
    fn an_env_override_beats_the_pointer() {
        let default = temp_dir("pointer-env-default");
        let custom = temp_dir("pointer-env-custom");
        let explicit = temp_dir("pointer-env-explicit");
        std::fs::write(custom.join("viberadio.db"), b"").unwrap();
        set_data_dir_pointer(&default, Some(&custom)).unwrap();
        assert_eq!(
            resolve_data_dir(default.clone(), Some(explicit.clone())).unwrap(),
            explicit
        );
        let _ = std::fs::remove_dir_all(&default);
        let _ = std::fs::remove_dir_all(&custom);
        let _ = std::fs::remove_dir_all(&explicit);
    }

    #[test]
    fn an_empty_pointer_falls_back_to_the_default() {
        let default = temp_dir("pointer-empty");
        std::fs::write(default.join("location"), "   ").unwrap();
        assert_eq!(resolve_data_dir(default.clone(), None).unwrap(), default);
        let _ = std::fs::remove_dir_all(&default);
    }

    #[test]
    fn a_pointer_to_a_missing_folder_falls_back() {
        let default = temp_dir("pointer-missing");
        let missing = default.join("gone");
        std::fs::write(default.join("location"), missing.display().to_string()).unwrap();
        assert_eq!(resolve_data_dir(default.clone(), None).unwrap(), default);
        let _ = std::fs::remove_dir_all(&default);
    }

    #[test]
    fn removing_a_missing_pointer_is_ok() {
        let default = temp_dir("pointer-remove-missing");
        set_data_dir_pointer(&default, None).unwrap();
        let _ = std::fs::remove_dir_all(&default);
    }

    #[test]
    fn a_relative_pointer_is_ignored() {
        let default = temp_dir("pointer-relative");
        std::fs::write(default.join("location"), "relative/folder").unwrap();
        assert_eq!(resolve_data_dir(default.clone(), None).unwrap(), default);
        let _ = std::fs::remove_dir_all(&default);
    }

    #[test]
    fn help_parser_joins_hard_wrapped_lines() {
        let blocks =
            parse_help("# Title\n\nA wrapped\nparagraph.\n\n- A wrapped\n  bullet.\n| a | b |\n");
        assert!(matches!(&blocks[0], HelpBlock::Heading(text) if text == "Title"));
        assert!(matches!(&blocks[1], HelpBlock::Paragraph(text) if text == "A wrapped paragraph."));
        assert!(matches!(&blocks[2], HelpBlock::Bullet(text) if text == "A wrapped bullet."));
        assert!(matches!(&blocks[3], HelpBlock::Table(text) if text.starts_with("| a")));
    }

    #[test]
    fn help_is_bundled_and_covers_the_core_surfaces() {
        for needle in [
            "Getting started",
            "Recording",
            "Schedules",
            "Dynamic Boost",
            "Backup",
            "Keyboard",
        ] {
            assert!(i18n::help().contains(needle), "help is missing {needle}");
        }
    }

    #[test]
    fn sleep_timer_cycles_and_wraps() {
        assert_eq!(next_sleep_minutes(0), 15);
        assert_eq!(next_sleep_minutes(15), 30);
        assert_eq!(next_sleep_minutes(30), 60);
        assert_eq!(next_sleep_minutes(60), 0);
        assert_eq!(next_sleep_minutes(7), 0);
    }

    #[test]
    fn search_matches_all_visible_fields() {
        let mut value = station("Jazz FM", "Jazz", Some(128), false);
        value.city = Some("London".to_string());
        assert!(search_matches(&value, "jazz"));
        assert!(search_matches(&value, "london"));
        assert!(!search_matches(&value, "rock"));
    }

    #[test]
    fn broken_stations_are_hidden_until_requested() {
        let good = station("Aaa Good", "Jazz", Some(128), false);
        let mut hidden = station("Bbb Broken", "Jazz", Some(64), false);
        hidden.broken = true;
        let stations = vec![good, hidden];
        let empty = HashSet::new();
        let visible = build_view(
            &stations,
            &empty,
            &LibraryFilter::All,
            "",
            SortKey::Name,
            true,
            false,
        );
        assert_eq!(visible, vec![0]);
        let all = build_view(
            &stations,
            &empty,
            &LibraryFilter::All,
            "",
            SortKey::Name,
            true,
            true,
        );
        assert_eq!(all, vec![0, 1]);
    }

    #[test]
    fn view_filters_and_sorts_the_real_builder() {
        let mut stations = vec![
            station("Beta", "Rock", Some(64), false),
            station("Alpha", "Jazz", Some(192), true),
            station("Gamma", "Jazz", Some(128), false),
        ];
        stations[2].city = Some("Paris".to_string());
        let recent = HashSet::from([stations[0].id]);

        let empty = HashSet::new();
        let all = build_view(
            &stations,
            &empty,
            &LibraryFilter::All,
            "",
            SortKey::Name,
            true,
            false,
        );
        assert_eq!(all, vec![1, 0, 2]);

        let favorites = build_view(
            &stations,
            &empty,
            &LibraryFilter::Favorites,
            "",
            SortKey::Name,
            true,
            false,
        );
        assert_eq!(favorites, vec![1]);

        let jazz = build_view(
            &stations,
            &empty,
            &LibraryFilter::Genre("Jazz".to_string()),
            "",
            SortKey::Name,
            true,
            false,
        );
        assert_eq!(jazz, vec![1, 2]);

        let recent_view = build_view(
            &stations,
            &recent,
            &LibraryFilter::RecentlyPlayed,
            "",
            SortKey::Name,
            true,
            false,
        );
        assert_eq!(recent_view, vec![0]);

        let searched = build_view(
            &stations,
            &empty,
            &LibraryFilter::All,
            "paris",
            SortKey::Name,
            true,
            false,
        );
        assert_eq!(searched, vec![2]);

        let bitrate_desc = build_view(
            &stations,
            &empty,
            &LibraryFilter::All,
            "",
            SortKey::Bitrate,
            false,
            false,
        );
        assert_eq!(bitrate_desc, vec![1, 2, 0]);
    }
}

#[cfg(test)]
mod switch_tests {
    use super::*;
    use crate::player::StreamSnapshot;

    fn snapshot(id: PlaybackId) -> StreamSnapshot {
        StreamSnapshot {
            id,
            station_id: None,
            url: String::new(),
            status: PlaybackStatus::Live,
            status_text: String::new(),
            now_playing: None,
            display_now_playing: None,
            error: None,
            volume: 1.0,
            samples_played: 0,
            recording: false,
            rms: 0.0,
            rms_serial: 0,
            record_only: false,
            watching: false,
            reported_bitrate: None,
        }
    }

    #[test]
    fn history_falls_back_for_stations_without_a_playlist() {
        fn row(station_id: Uuid, artist: &str, title: &str) -> crate::models::StationHistory {
            crate::models::StationHistory {
                id: Uuid::new_v4(),
                play_instance_id: None,
                station_id,
                station_name: "Test FM".into(),
                url: None,
                artist: Some(artist.into()),
                song_title: Some(title.into()),
                played_at: Utc::now(),
                duration_seconds: 0,
                status: "ended".into(),
            }
        }
        let station = Uuid::new_v4();
        let other = Uuid::new_v4();
        let rows = vec![
            row(station, "Artist One", "Song One"),
            row(station, "Artist Two", "Song Two"),
            row(other, "Artist Three", "Song Three"),
        ];
        let playlist = merged_playlist(Some(station), &rows, None);
        assert_eq!(playlist.entries.len(), 2);
        assert!(playlist.entries.iter().any(|entry| entry.artist == "Artist One"));
        assert!(playlist.entries.iter().any(|entry| entry.artist == "Artist Two"));
        assert!(merged_playlist(None, &rows, None).entries.is_empty());
    }

    #[test]
    fn the_panel_title_belongs_to_the_selected_station() {
        let station = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut stream = snapshot(1);
        stream.station_id = Some(station);
        stream.now_playing = Some("Title".into());
        assert_eq!(
            selected_now_playing(Some(&stream), Some(station)),
            Some("Title")
        );
        assert_eq!(selected_now_playing(Some(&stream), Some(other)), None);
        assert_eq!(selected_now_playing(None, Some(station)), None);
    }

    #[test]
    fn merged_playlist_puts_live_history_before_provider_rows() {
        fn row(station_id: Uuid, artist: &str, title: &str) -> crate::models::StationHistory {
            crate::models::StationHistory {
                id: Uuid::new_v4(),
                play_instance_id: None,
                station_id,
                station_name: "Test FM".into(),
                url: None,
                artist: Some(artist.into()),
                song_title: Some(title.into()),
                played_at: Utc::now(),
                duration_seconds: 0,
                status: "ended".into(),
            }
        }
        let station = Uuid::new_v4();
        let history = vec![row(station, "Live Artist", "Live Song")];
        let newer = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let provider = crate::station_playlist::StationPlaylist {
            entries: vec![
                crate::station_playlist::PlaylistEntry {
                    at: Some(newer),
                    artist: "Newer Artist".into(),
                    title: "Newer Song".into(),
                    image: None,
                },
                crate::station_playlist::PlaylistEntry {
                    at: None,
                    artist: "Live Artist".into(),
                    title: "Live Song".into(),
                    image: None,
                },
                crate::station_playlist::PlaylistEntry {
                    at: None,
                    artist: "Older Artist".into(),
                    title: "Older Song".into(),
                    image: None,
                },
            ],
            next: Some(crate::station_playlist::PlaylistEntry {
                at: None,
                artist: "Next Artist".into(),
                title: "Next Song".into(),
                image: None,
            }),
            ..Default::default()
        };

        let merged = merged_playlist(Some(station), &history, Some(&provider));
        assert_eq!(merged.entries.len(), 3);
        assert_eq!(merged.entries[0].artist, "Newer Artist");
        assert_eq!(merged.entries[1].artist, "Live Artist");
        assert_eq!(merged.entries[2].artist, "Older Artist");
        assert!(merged.next.is_some());
    }

    #[test]
    fn recording_quality_filter_checks_bitrate_and_codec() {
        assert!(recording_quality_allowed(Some(192), Some("MP3"), 0, "any"));
        assert!(!recording_quality_allowed(Some(64), Some("MP3"), 128, "any"));
        assert!(!recording_quality_allowed(None, Some("MP3"), 128, "any"));
        assert!(recording_quality_allowed(Some(192), Some("MP3"), 128, "mp3"));
        assert!(!recording_quality_allowed(Some(192), Some("MP3"), 0, "aac"));
        assert!(recording_quality_allowed(Some(192), Some("AAC"), 0, "aac"));
        assert!(!recording_quality_allowed(Some(192), None, 0, "mp3"));
    }

    #[test]
    fn finds_the_background_recording_for_a_station() {
        let station = Uuid::new_v4();
        let mut stream = snapshot(7);
        stream.station_id = Some(station);
        stream.recording = true;
        assert_eq!(
            background_recording_target(&[stream.clone()], station),
            Some(7)
        );
        assert_eq!(background_recording_target(&[stream], Uuid::new_v4()), None);
    }

    #[test]
    fn a_user_play_stops_other_audible_streams_but_keeps_recordings() {
        let streams = vec![snapshot(1), snapshot(2), snapshot(3)];
        let mut muted = HashSet::new();
        muted.insert(3);
        assert_eq!(streams_to_replace(&streams, &muted, None), vec![1, 2]);
        assert_eq!(streams_to_replace(&streams, &muted, Some(2)), vec![1]);
        assert_eq!(streams_to_replace(&streams, &muted, Some(1)), vec![2]);
        assert_eq!(streams_to_replace(&streams, &muted, Some(3)), vec![1, 2]);
    }

    #[test]
    fn a_record_only_stream_survives_a_play() {
        let mut stream = snapshot(4);
        stream.record_only = true;
        assert!(streams_to_replace(&[stream], &HashSet::new(), None).is_empty());
    }
}
