//! Message catalog and lookup for the UI.
//!
//! Strings are addressed by stable keys, so a second language is another table
//! rather than a rewrite of the call sites. `t` takes a `&'static str` (always a
//! literal at the call site) and returns a static message; an unknown key is
//! logged once and returned as-is, which keeps a typo visible instead of blank.
//!
//! Coverage today is the app chrome: sidebar, player bar, status labels, the
//! Settings window and the empty states. Long-form text (help, About,
//! diagnostics) and the less-visited dialogs still carry inline English.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

/// A UI language. Only English ships today; the enum and the setting exist so
/// adding a table is all a second language needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    English = 0,
}

impl Lang {
    pub const ALL: [Lang; 1] = [Lang::English];

    pub fn code(self) -> &'static str {
        match self {
            Lang::English => "en",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Lang::English => "English",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "en" => Some(Lang::English),
            _ => None,
        }
    }

    fn from_code(value: u8) -> Option<Self> {
        match value {
            0 => Some(Lang::English),
            _ => None,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Lang::English as u8);

pub fn set_language(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn language() -> Lang {
    Lang::from_code(CURRENT.load(Ordering::Relaxed)).unwrap_or(Lang::English)
}

/// The bundled help document for the current language.
pub fn help() -> &'static str {
    match language() {
        Lang::English => include_str!("../assets/help.en.md"),
    }
}

/// Look up a message. Unknown keys are logged once and returned unchanged.
pub fn t(key: &'static str) -> &'static str {
    match lookup(key) {
        Some(value) => value,
        None => {
            report_missing(key);
            key
        }
    }
}

/// Look up a message and replace each `{}` with the next argument.
pub fn tf(key: &'static str, args: &[&str]) -> String {
    let out = fill(t(key), args);
    debug_assert!(
        !out.contains("{}"),
        "missing arguments for translation key {key}"
    );
    out
}

/// Replace `{}` placeholders left to right. Search resumes after each inserted
/// argument, so an argument that itself contains `{}` is not consumed again.
fn fill(template: &str, args: &[&str]) -> String {
    let mut out = template.to_string();
    let mut from = 0;
    for arg in args {
        match out[from..].find("{}") {
            Some(offset) => {
                let position = from + offset;
                out.replace_range(position..position + 2, arg);
                from = position + arg.len();
            }
            None => break,
        }
    }
    out
}

fn lookup(key: &str) -> Option<&'static str> {
    let table = match language() {
        Lang::English => EN,
    };
    table
        .iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, value)| *value)
}

fn report_missing(key: &'static str) {
    static MISSING: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut missing = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    if !missing.contains(&key) {
        log::warn!("missing translation key: {key}");
        missing.push(key);
    }
}

/// The English table. Keys are namespaced by surface.
const EN: &[(&str, &str)] = &[
    ("app.name", "VibeRadio"),
    // Sidebar.
    ("sidebar.library", "LIBRARY"),
    ("sidebar.genres", "GENRES"),
    ("sidebar.all", "All"),
    ("sidebar.favourites", "Favourites"),
    ("sidebar.recently_played", "Recently played"),
    ("sidebar.watched", "Watched"),
    ("sidebar.sessions", "Sessions"),
    ("sidebar.schedules", "Schedules"),
    ("sidebar.wishlist", "Wishlist"),
    ("sidebar.watch_history", "Watch history"),
    ("sidebar.settings", "Settings"),
    ("sidebar.about", "About"),
    ("sidebar.help", "Help"),
    ("sidebar.no_genres", "No genres yet"),
    ("sidebar.add_station", "+  Add station"),
    ("sidebar.import_radionet", "Import radio.net"),
    ("sidebar.import_radiobrowser", "Import radio-browser"),
    ("sidebar.export_m3u", "Export M3U"),
    ("sidebar.import_m3u", "Import M3U"),
    ("sidebar.export_history", "Export history"),
    ("sidebar.theme_light", "Light"),
    ("sidebar.theme_dark", "Dark"),
    ("sidebar.theme_toggle", "Switch theme"),
    // Player bar and stream list.
    ("player.nothing", "Nothing playing"),
    ("player.select_station", "Select a station to begin"),
    ("player.internet_radio", "Internet radio"),
    ("player.idle", "Idle"),
    ("player.error", "Error"),
    ("player.streams", "Streams"),
    ("player.streams_count", "{} ({})"),
    ("player.active_streams", "ACTIVE STREAMS"),
    ("player.no_streams", "No open streams"),
    ("player.stop_all", "Stop playback"),
    ("player.stop_all_tooltip", "Stop playback; recordings keep running"),
    ("player.stop_stream", "Stop this stream"),
    ("player.activate_stream", "Make this stream active"),
    ("player.play_selected", "Play the selected station"),
    ("player.restart_selected", "Restart the selected station"),
    ("player.record", "REC"),
    (
        "player.record_tooltip",
        "Record this station in the background (Ctrl+R; settings Ctrl+,)",
    ),
    (
        "player.record_stop_tooltip",
        "Stop the background recording (Ctrl+R; settings Ctrl+,)",
    ),
    ("player.sleep", "Sleep"),
    ("player.sleep_minutes", "Sleep {}m"),
    ("player.sleep_tooltip", "Cycle sleep timer: 15 / 30 / 60 / off"),
    ("player.show_streams", "Show every open stream"),
    ("player.mini", "Mini player"),
    ("mini.open", "Open"),
    // Tray menu.
    ("tray.show", "Show VibeRadio"),
    ("tray.mini", "Mini player"),
    ("tray.play", "Play"),
    ("tray.stop", "Stop"),
    ("tray.quit", "Quit"),
    // Playback state.
    ("status.connecting", "Connecting…"),
    ("status.buffering", "Buffering"),
    ("status.live", "Live"),
    ("status.error", "Error"),
    ("status.stopped", "Stopped"),
    // Settings.
    ("settings.title", "Settings"),
    ("settings.language", "Language"),
    ("settings.audio", "Audio"),
    ("settings.output_device", "Output device"),
    ("settings.system_default", "System default"),
    ("settings.refresh", "Refresh"),
    ("settings.stream_buffer", "Stream buffer"),
    ("settings.seconds_value", "{} s"),
    (
        "settings.device_note",
        "Changing the device stops all streams. The buffer applies to streams started afterwards.",
    ),
    ("settings.dynamic_boost", "Dynamic Boost"),
    ("settings.station_gain", "Station gain"),
    ("settings.select_station_gain", "Select a station to set its gain."),
    (
        "settings.gain_note",
        "Dynamic Boost targets -18 dBFS with a -1 dBFS limiter; it is not EBU R128 or ReplayGain.",
    ),
    (
        "settings.gain_applies",
        "Applies to {}. Recordings keep the station's encoded audio.",
    ),
    ("settings.recording", "Recording"),
    ("settings.folder", "Folder"),
    ("settings.template_preview", "Preview: {}"),
    ("settings.template_unknown", "Unknown template token(s): {}"),
    ("settings.template", "File name template"),
    ("settings.split", "Split one file per track"),
    ("settings.sleep", "Sleep timer"),
    ("settings.sleep_shutdown", "Shut down the computer when the timer ends"),
    ("settings.sleep_shutdown_note", "Playback stops first, then the system is asked to power off."),
    ("settings.open_folder", "Open"),
    ("msg.open_folder_failed", "Could not open the folder: {}"),
    ("settings.min_track", "Discard splits shorter than:"),
    ("settings.min_track_note", "Fragments shorter than this are deleted when the title changes again. 0 keeps every split."),
    ("settings.min_fingerprint", "Remove recordings shorter than:"),
    ("settings.min_fingerprint_note", "Deleted when the file closes, before fingerprinting. 0 keeps every recording."),
    ("settings.min_bitrate", "Minimum bitrate:"),
    ("settings.codec", "Codec:"),
    ("settings.codec_any", "Any"),
    ("settings.codec_mp3", "MP3"),
    ("settings.codec_aac", "AAC"),
    ("settings.min_bitrate_note", "Recordings are skipped when the station is below these values. 0 kbps and Any disable the filter."),
    ("settings.keep_best", "Keep only the best copy of repeated songs"),
    ("settings.keep_best_note", "After fingerprinting, airings with the same AcoustID recording id keep the largest file."),
    ("settings.min_free_space", "Keep free:"),
    ("settings.min_free_space_note", "Recording stops when the disk has less free space than this. 0 disables the check."),
    ("settings.wishlist_only", "Record only wished songs"),
    ("settings.fingerprinting", "Fingerprinting"),
    ("settings.acoustid_key", "AcoustID API key (optional)"),
    (
        "settings.acoustid_note",
        "A free application key from acoustid.org. Register an application at acoustid.org/new-application. Without one, only local Chromaprint fingerprints are stored.",
    ),
    ("settings.fingerprint_now", "Fingerprint recordings"),
    (
        "settings.tokens",
        "Tokens: {station} {artist} {title} {date} {time}",
    ),
    ("settings.save", "Save"),
    ("settings.split_last", "Split last recording"),
    ("settings.backup", "Backup"),
    ("settings.backups_written", "Backups are written to {}"),
    ("settings.latest", "Latest: {}"),
    ("settings.unavailable", "unavailable"),
    ("settings.backup_now", "Backup now"),
    ("settings.restore_latest", "Restore latest on restart"),
    (
        "settings.close_other",
        "Close any other VibeRadio instance before restoring.",
    ),
    (
        "settings.restore_pending",
        "A restore is pending for the next start.",
    ),
    ("settings.cancel_restore", "Cancel pending restore"),
    ("settings.data_folder", "Data folder"),
    (
        "settings.data_dir_override",
        "VIBERADIO_DATA_DIR overrides this setting.",
    ),
    (
        "settings.restart_required",
        "Restart required. The latest library is copied at startup; recordings, logs and backups stay in the old folder.",
    ),
    (
        "settings.copy_and_switch",
        "Copy library and switch on restart",
    ),
    // Dynamic Boost presets.
    ("dsp.off", "Off"),
    ("dsp.light", "Light (+6 dB max)"),
    ("dsp.medium", "Medium (+12 dB max)"),
    ("dsp.strong", "Strong (+18 dB max)"),
    // Library table and context menu.
    ("table.name", "NAME"),
    ("table.genre", "GENRE"),
    ("table.bitrate", "BITRATE"),
    ("table.show_broken", "Show broken"),
    ("table.remove_broken", "Remove broken ({})"),
    ("table.remove_selected", "Remove selected ({})"),
    ("table.clear_selection", "Clear selection"),
    ("table.fetch_bitrates", "Fetch bitrates ({})"),
    ("table.stop_bitrates", "Stop"),
    ("table.bitrate_progress", "Fetching bitrates… {}/{}"),
    ("table.country", "COUNTRY"),
    ("topbar.search", "Search stations"),
    ("topbar.station_one", "{} station"),
    ("topbar.station_many", "{} stations"),
    // Right-hand now-playing / playlist panel.
    ("panel.now_playing", "Now playing"),
    ("panel.up_next", "Up next"),
    ("panel.recently_played", "Recently played"),
    ("panel.select_station", "Select a station to see its playlist."),
    ("panel.playlist_loading", "Loading playlist…"),
    ("panel.playlist_empty", "This station does not publish a playlist."),
    ("panel.playlist_failed", "Could not load the playlist: {}"),
    ("panel.playlist_source", "Playlist source"),
    ("panel.want_this", "Want this"),
    ("panel.refresh", "Refresh"),
    // Wishlist.
    ("wishlist.title", "Wishlist"),
    ("wishlist.note", "Rules for songs to keep. A record rule makes the wishlist recorder save that song; ignore rules only label it."),
    ("wishlist.artist", "Artist"),
    ("wishlist.song", "Song"),
    ("wishlist.add", "Add"),
    ("wishlist.remove", "Remove"),
    ("wishlist.empty", "No wishes yet."),
    ("wishlist.no_matches", "No matching wishes."),
    ("wishlist.search", "Search"),
    ("wishlist.import", "Import"),
    ("wishlist.export", "Export"),
    ("wishlist.need_field", "Enter an artist or a song."),
    ("wishlist.exists", "That wish is already in the list."),
    ("wishlist.action_record", "record"),
    ("wishlist.action_activate", "activate"),
    ("wishlist.action_record_activate", "record + activate"),
    ("wishlist.action_ignore", "ignore"),
    ("wishlist.compare_contains", "contains"),
    ("wishlist.compare_exact", "exact"),
    ("wishlist.compare_starts", "starts with"),
    ("msg.wish_added", "Added to the wishlist"),
    ("msg.wishlist_imported", "Imported {} wishes ({} skipped)"),
    ("msg.wishlist_exported", "Exported {} wishes to {}"),
    ("msg.wishlist_import_failed", "Could not import wishes: {}"),
    ("msg.wishlist_export_failed", "Could not export wishes: {}"),
    ("msg.background_recording", "Recording {} in the background"),
    ("msg.background_stopped", "Background recording stopped"),
    ("msg.wish_removed", "Removed from the wishlist"),
    ("msg.duplicates_removed", "Removed {} duplicate recording(s)"),
    ("msg.wish_verify_running", "Checking recordings against the wish list..."),
    ("msg.wish_verify_done", "Wish-list check: kept {}, removed {}, unverified {}"),
    ("msg.wish_verify_failed", "Wish-list check failed: {}"),
    ("msg.recording_quality", "Recording skipped: {} does not meet the quality filter"),
    ("msg.recording_low_space", "Not enough free space to record (need {} MB)"),
    ("msg.recording_stopped_low_space", "Recording stopped: the disk is low on free space"),
    ("msg.format_saved", "Tag format saved for this station"),
    ("msg.format_reset", "Tag format reset to the default"),
    ("msg.format_save_failed", "Could not save the tag format: {}"),
    ("msg.no_track", "No track is playing"),
    ("msg.wish_failed", "Could not save the wish: {}"),
    ("msg.fingerprinting", "Fingerprinting recordings…"),
    ("msg.fingerprinted_issues", "Fingerprinted {} recording(s); {} incomplete, {} interrupted"),
    ("msg.fingerprinted", "Fingerprinted {} recordings"),
    ("msg.fingerprint_failed", "Fingerprint scan failed: {}"),
    ("msg.fingerprinting_busy", "A fingerprint scan is already running"),
    ("menu.play", "Play"),
    ("menu.schedule", "Schedule…"),
    ("menu.record_background", "Record in background"),
    ("menu.add_favourite", "Add favourite"),
    ("menu.remove_favourite", "Remove favourite"),
    ("menu.remove_station", "Remove station"),
    ("menu.watch_station", "Watch for wishes"),
    ("menu.unwatch_station", "Stop watching"),
    ("dialog.remove_stations", "Remove stations"),
    ("dialog.remove_stations_body", "Remove {} station(s)? This cannot be undone."),
    ("dialog.remove", "Remove"),
    ("dialog.cancel", "Cancel"),
    // Empty states.
    ("empty.title", "No stations yet"),
    (
        "empty.body",
        "Import stations from radio.net, or add a station by URL.",
    ),
    ("empty.import", "Import from radio.net"),
    ("empty.add", "Add station"),
    ("empty.no_match", "No stations match"),
    ("empty.filter_empty", "This filter is empty."),
    ("empty.try_search", "Try a different search or filter."),
    ("empty.clear_search", "Clear search"),
    ("empty.show_all", "Show all stations"),
    // Dialogs and secondary windows.
    ("common.cancel", "Cancel"),
    ("common.close", "Close"),
    ("add.title", "Add station"),
    ("add.name", "Station name"),
    ("add.url", "Stream URL"),
    ("add.submit", "Add and play"),
    ("about.title", "About VibeRadio"),
    ("about.heading", "VibeRadio"),
    ("about.version", "Version {}"),
    (
        "about.tagline",
        "A native Rust/egui internet-radio client for Linux.",
    ),
    ("about.formats", "Formats"),
    (
        "about.formats_text",
        "MP3, OGG Vorbis, FLAC and WAV via rodio; AAC-LC and Ogg Opus via symphonia and a pure-Rust decoder. HE-AAC is expected to fail or play degraded. Unencrypted HLS playlists are followed.",
    ),
    ("about.boost", "Dynamic Boost"),
    (
        "about.boost_text",
        "A fixed slow-RMS AGC, per-station trim and -1 dBFS peak limiter. It is not EBU R128 or ReplayGain.",
    ),
    ("about.data", "Data folder: {}"),
    ("about.database", "Database: {}"),
    ("about.schema", "Schema: {}"),
    ("about.copy", "Copy diagnostics"),
    ("help.title", "Help"),
    ("schedules.title", "Schedules"),
    ("schedules.new", "New schedule"),
    ("schedules.count_one", "{} schedule"),
    ("schedules.count_many", "{} schedules"),
    ("schedules.running", "{} running"),
    (
        "schedules.note",
        "A schedule starts its station and stops it when the window ends. A finish action needs an end time.",
    ),
    ("schedules.empty", "No schedules yet."),
    ("schedules.station_missing", "(station missing)"),
    ("schedules.next", "Next: {}"),
    ("schedules.enabled", "Enabled"),
    ("schedules.edit", "Edit"),
    ("schedules.delete", "Delete"),
    ("schedules.edit_title", "Edit schedule"),
    ("schedules.new_title", "New schedule"),
    ("schedules.back", "Back"),
    ("schedules.station", "Station"),
    ("schedules.pick_station", "(pick a station)"),
    ("schedules.action", "Action"),
    ("schedules.repeat", "Repeat"),
    ("schedules.start", "Start"),
    ("schedules.end", "End"),
    ("schedules.date_hint", "YYYY-MM-DD"),
    ("schedules.time_hint", "HH:MM"),
    (
        "schedules.end_note",
        "Blank end time means start only. An end at or before the start crosses midnight (equal means 24 hours).",
    ),
    ("schedules.finish", "Finish"),
    ("schedules.command_hint", "command"),
    ("schedules.save", "Save"),
    ("sessions.title", "Sessions"),
    ("sessions.save", "Save current"),
    (
        "sessions.note",
        "Saves the open streams (or the selected station).",
    ),
    ("sessions.empty", "No saved sessions yet."),
    ("sessions.row", "{} ({} stations)"),
    ("sessions.open", "Open"),
    ("sessions.delete", "Delete"),
    ("history.title", "History"),
    ("history.empty", "No history yet."),
    (
        "history.count",
        "{} rows (track rows and station attempts)",
    ),
    ("history.played", "Played"),
    ("history.station", "Station"),
    ("history.artist", "Artist"),
    ("history.song", "Song"),
    ("history.duration", "Duration"),
    ("history.status", "Status"),
    ("debug.title", "Metadata debug"),
    ("debug.station", "Station: {}"),
    ("debug.status", "Status: {}"),
    ("debug.raw", "Raw StreamTitle: {}"),
    ("debug.format", "Tag format: {}"),
    ("debug.format_edit", "Edit tag format:"),
    ("debug.save_format", "Save"),
    ("debug.reset_format", "Reset"),
    ("debug.artist_song", "Artist: {}    Song: {}"),
    ("debug.ignored", "Ignored: {}    Wish-list hits: {}"),
    ("debug.url", "URL: {}"),
    ("debug.hint", "Ctrl+D closes this window."),
    ("debug.none", "(none)"),
    ("debug.idle", "Idle"),
    ("debug.yes", "yes"),
    ("debug.no", "no"),
    // Schedule vocabulary.
    ("schedule.action.play", "Play only"),
    ("schedule.action.play_record_split", "Play + record (split)"),
    (
        "schedule.action.play_record_continuous",
        "Play + record (continuous)",
    ),
    ("schedule.action.record_split", "Record only (split)"),
    (
        "schedule.action.record_continuous",
        "Record only (continuous)",
    ),
    ("schedule.finish.none", "None"),
    ("schedule.finish.close", "Close the app"),
    ("schedule.finish.command", "Run a command"),
    ("schedule.recurrence.once", "Once"),
    ("schedule.recurrence.daily", "Daily"),
    ("schedule.recurrence.weekly", "Weekly: {}"),
    ("weekday.mon", "Mon"),
    ("weekday.tue", "Tue"),
    ("weekday.wed", "Wed"),
    ("weekday.thu", "Thu"),
    ("weekday.fri", "Fri"),
    ("weekday.sat", "Sat"),
    ("weekday.sun", "Sun"),
    // Status and error messages.
    ("msg.audio_notice", "Audio: {}"),
    ("msg.radionet_fetching", "Fetching stations from radio.net…"),
    ("msg.radionet_busy", "Already fetching stations from radio.net"),
    ("msg.radionet_empty", "radio.net returned no playable stations"),
    ("msg.radionet_imported", "Imported {} stations from radio.net"),
    ("msg.radionet_failed", "Could not fetch radio.net stations: {}"),
    ("msg.radiobrowser_fetching", "Fetching stations from radio-browser…"),
    ("msg.radiobrowser_busy", "Already fetching stations from radio-browser"),
    ("msg.radiobrowser_empty", "radio-browser returned no playable stations"),
    ("msg.radiobrowser_imported", "Imported {} stations from radio-browser"),
    ("msg.radiobrowser_failed", "Could not fetch radio-browser stations: {}"),
    ("msg.no_stream_url", "{} has no stream URL"),
    ("msg.active", "Active: {}"),
    ("msg.connecting", "Connecting to {}"),
    ("msg.reload_failed", "Could not reload the library: {}"),
    ("msg.stopped", "Stopped"),
    ("msg.gain_save_failed", "Could not save the gain: {}"),
    ("msg.no_data_folder", "No data folder is available"),
    ("msg.no_database", "No local database is open"),
    ("msg.no_broken", "No broken stations"),
    ("msg.watched", "Watching this station for wishes"),
    ("msg.unwatched", "Stopped watching this station"),
    ("watch_history.title", "Watch history"),
    ("watch_history.note", "Songs heard on watched stations. Add any of them as a wish."),
    ("watch_history.empty", "No songs have played on a watched station yet."),
    ("watch_history.wish", "Wish"),
    ("msg.no_bitrate_targets", "Every station already has a bitrate"),
    ("msg.bitrates_done", "Checked {} station bitrates"),
    ("msg.bitrate_probe_failed", "Could not start the bitrate probe: {}"),
    ("msg.stations_removed", "Removed {} station(s)"),
    ("msg.remove_failed", "Could not remove stations: {}"),
    ("msg.backup_saved", "Backup saved to {}"),
    ("msg.backup_failed", "Backup failed: {}"),
    ("msg.no_backup", "No backup found; make one first"),
    ("msg.restore_schedule_failed", "Could not schedule the restore: {}"),
    ("msg.restore_cancelled", "Pending restore cancelled"),
    ("msg.no_pending_restore", "No pending restore"),
    ("msg.restore_cancel_failed", "Could not cancel the restore: {}"),
    ("msg.data_folder_empty", "Enter a data folder path"),
    ("msg.data_folder_absolute", "Use an absolute data folder path"),
    ("msg.data_folder_failed", "Could not use that folder: {}"),
    ("msg.data_folder_same", "Already using this data folder"),
    ("msg.no_default_data_folder", "No default data folder is available"),
    ("msg.data_copy_failed", "Could not schedule the data folder copy: {}"),
    (
        "msg.data_restart",
        "Restart VibeRadio to copy the latest library and switch to {}",
    ),
    ("msg.data_pointer_failed", "Could not set the data folder pointer: {}"),
    ("msg.dsp", "Dynamic Boost: {}"),
    ("msg.favorite_failed", "Could not save favorite: {}"),
    ("msg.export_failed", "Export failed: {}"),
    ("msg.no_writable_dir", "No writable data directory"),
    ("msg.sleep_off", "Sleep timer off"),
    ("msg.sleep_set", "Sleep timer: {} min"),
    ("msg.play_or_select", "Play or select a station first"),
    ("msg.session_saved", "Saved session {}"),
    ("msg.session_save_failed", "Could not save session: {}"),
    ("msg.session_opened", "Opened session ({} stations)"),
    ("msg.session_open_failed", "Could not open session: {}"),
    ("msg.session_delete_failed", "Could not delete session: {}"),
    ("msg.session_deleted", "Session deleted"),
    ("msg.schedule_saved", "Schedule saved"),
    ("msg.schedule_delete_failed", "Could not delete schedule: {}"),
    ("msg.schedule_deleted", "Schedule deleted"),
    ("msg.schedule_save_failed", "Could not save the schedule: {}"),
    ("msg.recording_dir_missing", "Recording folder does not exist"),
    ("msg.no_recording_log", "No continuous recording log found"),
    ("msg.recording_log_no_name", "Recording log has no file name"),
    (
        "msg.recording_not_found",
        "Could not find the recording next to its log",
    ),
    ("msg.split_failed", "Split failed: {}"),
    ("msg.select_play", "Select and play a station first"),
    ("msg.recording_already_stopped", "Recording had already stopped"),
    ("msg.recording_writer_failed", "Recording writer failed"),
    ("msg.recording_to", "Recording to {}"),
    ("msg.recording_start_failed", "Could not start recording: {}"),
    ("msg.history_export_failed", "History export failed: {}"),
    ("msg.import_failed", "Import failed: {}"),
    ("msg.station_fields", "Station name and URL are both required"),
    ("msg.station_save_failed", "Could not save station: {}"),
    ("msg.shutdown_requested", "Sleep timer finished; requesting shutdown"),
    ("msg.shutdown_failed", "Could not request shutdown: {}"),
    ("msg.sleep_stopped", "Sleep timer: playback stopped"),
    (
        "msg.recording_dir_unwritable",
        "Recording folder is not writable: {}",
    ),
    ("msg.output_device", "Output device: {}"),
    (
        "msg.output_fallback",
        "Could not open the requested output ({}); using the system default",
    ),
    ("msg.output_switch_failed", "Could not switch output: {}"),
    ("msg.audio_saved", "Audio settings saved"),
    ("msg.wish_hit", "Wish-list hit: {}"),
    ("msg.imported_n", "Imported {} new stations from {}"),
    (
        "msg.imported_reload_failed",
        "Imported, but the library could not be reloaded",
    ),
    (
        "msg.recording_stopped",
        "Recording stopped: {} file(s), {} bytes{}{}",
    ),
    ("msg.recording_dropped", " (some audio was dropped)"),
    ("msg.recording_error", " — {}"),
    ("msg.exported", "Exported {} stations to {}"),
    (
        "msg.schedule_finish_skipped",
        "Scheduled finish skipped: the window ended while the app was not running",
    ),
    ("msg.schedule_command_empty", "Scheduled finish command is empty"),
    ("msg.schedule_command_ran", "Ran scheduled command: {}"),
    ("msg.schedule_command_failed", "Scheduled command failed: {}"),
    ("schedule.countdown.days", "{}d {}h"),
    ("schedule.countdown.hours", "{}h {}m"),
    ("schedule.countdown.minutes", "{}m"),
    ("diag.data", "data: {}"),
    ("diag.database", "database: {}"),
    ("diag.schema", "schema: {}"),
    ("diag.preset", "preset: {}"),
    ("diag.recording", "recording: {}"),
    (
        "msg.connecting_history",
        "Connecting to {} (history not saved: {})",
    ),
    ("msg.restore_restart", "Restart VibeRadio to restore the latest backup"),
    (
        "msg.data_env_override",
        "VIBERADIO_DATA_DIR is set and overrides the data folder",
    ),
    (
        "msg.data_folder_exists",
        "That folder already contains a viberadio.db",
    ),
    ("msg.split_done", "Split into {} files in {}"),
    ("msg.history_exported", "Exported {} history rows to {}"),
    ("schedule.error.station", "Pick a station"),
    ("schedule.error.start_date", "Start date must be YYYY-MM-DD"),
    ("schedule.error.start_time", "Start time must be HH:MM"),
    ("schedule.error.end_time", "End time must be HH:MM"),
    (
        "schedule.error.finish_needs_end",
        "A finish action needs an end time",
    ),
    ("schedule.error.weekday", "Pick at least one weekday"),
    (
        "schedule.error.passed",
        "That one-time schedule has already passed",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_unique_and_has_a_value() {
        let mut seen = std::collections::HashSet::new();
        for (key, value) in EN {
            assert!(seen.insert(*key), "duplicate key: {key}");
            assert!(!key.trim().is_empty(), "empty key");
            assert!(!value.trim().is_empty(), "empty value for {key}");
        }
    }

    #[test]
    fn known_keys_resolve() {
        assert_eq!(t("app.name"), "VibeRadio");
        assert_eq!(t("status.live"), "Live");
        assert_eq!(t("settings.output_device"), "Output device");
    }

    #[test]
    fn unknown_keys_are_returned_unchanged() {
        let unknown: &'static str = "no.such.key";
        assert_eq!(t(unknown), "no.such.key");
    }

    #[test]
    fn formatting_replaces_each_placeholder() {
        assert_eq!(tf("player.sleep_minutes", &["15"]), "Sleep 15m");
        assert_eq!(
            tf("settings.gain_applies", &["Radio One"]),
            "Applies to Radio One. Recordings keep the station's encoded audio."
        );
        assert_eq!(
            tf("settings.tokens", &[]),
            "Tokens: {station} {artist} {title} {date} {time}"
        );
        assert_eq!(tf("app.name", &["ignored"]), "VibeRadio");
        assert_eq!(fill("{} and {}", &["A{}B", "C"]), "A{}B and C");
    }

    #[test]
    fn every_call_site_key_exists() {
        fn scan(dir: &std::path::Path, missing: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    scan(&path, missing);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                    let source = std::fs::read_to_string(&path).expect("read source");
                    for key in call_keys(&source) {
                        if !EN.iter().any(|(candidate, _)| *candidate == key) {
                            missing.push(format!("{}: {key}", path.display()));
                        }
                    }
                }
            }
        }

        let mut missing = Vec::new();
        scan(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut missing,
        );
        assert!(missing.is_empty(), "unknown translation keys: {missing:?}");
    }

    /// Keys passed to `t`/`tf` as literals, ignoring names like `expect(`.
    fn call_keys(source: &str) -> Vec<&str> {
        let bytes = source.as_bytes();
        let mut keys = Vec::new();
        for marker in ["t(", "tf("] {
            for (index, _) in source.match_indices(marker) {
                let boundary = index == 0 || {
                    let previous = bytes[index - 1];
                    previous != b'"'
                        && previous != b'\''
                        && !(previous as char).is_alphanumeric()
                        && previous != b'_'
                };
                if !boundary {
                    continue;
                }
                let mut cursor = index + marker.len();
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                if bytes.get(cursor) != Some(&b'"') {
                    continue;
                }
                let start = cursor + 1;
                if let Some(end) = source[start..].find('"') {
                    keys.push(&source[start..start + end]);
                }
            }
        }
        keys
    }

    #[test]
    fn language_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::parse(lang.code()), Some(lang));
            assert!(!lang.label().is_empty());
        }
        assert_eq!(Lang::parse("xx"), None);
    }
}
