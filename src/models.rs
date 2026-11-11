//! Data models for VibeRadio.
//!
//! The shapes here follow the schema described in `docs/SPEC.md` §5. Two rules are
//! encoded in the types rather than left to callers:
//!
//! * timestamps are UTC (`DateTime<Utc>`), because local time makes stored text
//!   ordering wrong across a DST change (FR-D1);
//! * playback state belongs to a `PlayInstance`, not a `Station`, so the same
//!   station can be played twice (or two stations at once) and each attempt keeps
//!   its own duration and status (FR-D5, FR-D6).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Play-instance / history status values stored in the database.
pub const PLAY_STATUS_PLAYING: &str = "playing";
pub const PLAY_STATUS_ENDED: &str = "ended";
pub const PLAY_STATUS_ERROR: &str = "error";

/// A station fetched from an external directory, before it is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationSeed {
    /// radio.net station id, kept so the homepage and playlist can be fetched
    /// later without re-searching.
    pub source_id: Option<String>,
    pub name: String,
    pub url: String,
    pub genre: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    pub logo: Option<String>,
    /// Station homepage, when the directory supplied one.
    pub website: Option<String>,
    pub tags: Option<String>,
    pub codec: Option<String>,
    /// Stream bitrate in kbps, when the directory reports one.
    pub bitrate: Option<u32>,
}

/// A station in the library.
///
/// The fields mirror the legacy station schema, plus the fields VibeRadio adds.
/// `url` is the convenience "current stream" URL; the authoritative per-stream
/// rows live in [`Stream`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub id: Uuid,
    /// The legacy station id, when this station was imported.
    /// Kept so re-imports can match rows and later DBs (favourites, sessions,
    /// custom tags) can be joined back to the station.
    pub source_id: Option<String>,
    pub name: String,
    pub url: String,
    pub genre: Option<String>,
    pub country: Option<String>,
    /// US/Canada state.
    pub country_state: Option<String>,
    pub logo: Option<String>,
    pub comment: Option<String>,
    pub website: Option<String>,
    pub city: Option<String>,
    pub tags: Option<String>,
    pub bitrate: Option<u32>,
    pub codec: Option<String>,
    /// Legacy stream-type value, kept verbatim.
    pub stream_type: Option<i64>,
    pub is_favorite: bool,
    pub rating: Option<i64>,
    pub play_count: i64,
    pub date_added: Option<String>,
    pub date_last_played: Option<String>,
    /// Per-station *default* volume (FR-D5). The live volume lives on the
    /// play instance.
    pub volume: f32,
    /// Per-station gain trim in dB, clamped to ±12. This is a user preference
    /// on top of the optional Dynamic Boost AGC; recordings are unaffected.
    pub gain_db: f32,
    /// Original `External` flag (the station is not played by the built-in player).
    pub external: bool,
    /// The stream failed to play repeatedly and is hidden from the list.
    pub broken: bool,
    /// When the station was marked broken, UTC RFC3339.
    pub broken_at: Option<String>,
    /// Consecutive playback failures; cleared after a successful play.
    pub failure_count: u32,
    /// Keep a background stream and rolling buffer for wish auto-recording.
    pub watched: bool,
}

impl Station {
    /// A minimal, user-created station. Imported stations are built field by
    /// field from the original database instead.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            source_id: None,
            name: name.into(),
            url: url.into(),
            genre: None,
            country: None,
            country_state: None,
            logo: None,
            comment: None,
            website: None,
            city: None,
            tags: None,
            bitrate: None,
            codec: None,
            stream_type: None,
            is_favorite: false,
            rating: None,
            play_count: 0,
            date_added: None,
            date_last_played: None,
            volume: 1.0,
            gain_db: 0.0,
            external: false,
            broken: false,
            broken_at: None,
            failure_count: 0,
            watched: false,
        }
    }
}

/// One stream URL belonging to a station (FR-P8 supports several per station).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: Uuid,
    pub station_id: Uuid,
    pub url: String,
    pub listen_url: Option<String>,
    /// User-visible label; falls back to the URL when empty.
    pub label: Option<String>,
    pub bitrate: Option<u32>,
    /// Original `MetaData` flag (the stream advertises ICY metadata).
    pub metadata: bool,
    pub stream_type: Option<i64>,
    /// Whether this is the stream the station row points at.
    pub is_current: bool,
    pub custom_tag: Option<String>,
    pub open_as_website: bool,
    /// Original `StreamID`, kept for lossless re-import.
    pub source_stream_id: Option<String>,
}

impl Stream {
    pub fn new(station_id: Uuid, url: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            station_id,
            url: url.into(),
            listen_url: None,
            label: None,
            bitrate: None,
            metadata: false,
            stream_type: None,
            is_current: true,
            custom_tag: None,
            open_as_website: false,
            source_stream_id: None,
        }
    }
}

/// One playback attempt of one stream.
///
/// This is the row that owns duration/status/end reason, so a station played
/// twice produces two independent records (FR-D5, FR-D6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayInstance {
    pub id: Uuid,
    pub station_id: Uuid,
    pub stream_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub duration_seconds: i64,
    pub status: String,
    pub end_reason: Option<String>,
    /// Live gain of this attempt (FR-D5).
    pub volume: f32,
    /// Whether a recording tap was attached to this attempt (M5).
    pub recording: bool,
    /// Last error reported for the attempt, if any.
    pub error: Option<String>,
}

/// A track-level history entry, linked to the [`PlayInstance`] that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationHistory {
    pub id: Uuid,
    pub play_instance_id: Option<Uuid>,
    pub station_id: Uuid,
    pub station_name: String,
    pub url: Option<String>,
    pub artist: Option<String>,
    pub song_title: Option<String>,
    /// UTC, so text ordering is monotonic across a DST change (FR-D1).
    pub played_at: DateTime<Utc>,
    pub duration_seconds: i64,
    pub status: String,
}

/// A saved set of stations that can be reopened together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub name: String,
    pub station_count: usize,
}

/// Persisted preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub audio_buffer_size: usize,
    pub auto_connect: bool,
    pub volume: f32,
    pub language: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            audio_buffer_size: 8192,
            auto_connect: false,
            volume: 0.8,
            language: "en".to_string(),
        }
    }
}
