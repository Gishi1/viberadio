//! SQLite persistence for the station library, streams, playback attempts and
//! history.
//!
//! The schema is versioned in `settings(schema_version)` and every migration is
//! additive: an older library opened by this build gains the new columns/tables
//! without losing rows (FR-D4).
//!
//! Two identifiers matter:
//! * `stations.source_id` is the legacy station id, so re-importing the same
//!   directory replaces rows instead of duplicating them (FR-D2);
//! * `streams.id` is derived from `(station source id, URL)`, so a station can
//!   keep several streams and a re-import still matches them.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Result};
use uuid::Uuid;

use crate::metadata::{CompareMode, WishEntry};
use crate::models::{
    PlayInstance, SessionSummary, Station, StationHistory, StationSeed, Stream, PLAY_STATUS_ENDED,
    PLAY_STATUS_ERROR, PLAY_STATUS_PLAYING,
};
use crate::scheduler::{FinishAction, Recurrence, Schedule, ScheduleAction};

/// Fixed namespace for station ids created from a playlist URL.
const PLAYLIST_NAMESPACE: Uuid = Uuid::from_bytes([
    0x56, 0x49, 0x42, 0x45, 0x52, 0x41, 0x44, 0x49, 0x4f, 0x50, 0x4c, 0x41, 0x59, 0x4c, 0x49, 0x53,
]);

/// Schema version written into `settings`; bumped when the shape changes.
///
/// * v2 added `stations.source_id` and the rule/session tables.
/// * v3 adds the full station taxonomy, `streams`, `play_instances`, `schedules`
///   and the history columns that link a row to its playback attempt.
/// * v4 adds the per-attempt `volume`, `recording` and `error` state (FR-D5).
/// * v5 adds `schedules.last_fired_at`, so a recurring occurrence fires at most
///   once even across a restart (FR-S3).
/// * v6 adds `stations.gain_db`, the per-station gain trim for Dynamic Boost.
pub(crate) const SCHEMA_VERSION: i64 = 8;

/// Deterministic id for a station created from a playlist entry.
fn stable_playlist_station_id(url: &str) -> Uuid {
    Uuid::new_v5(&PLAYLIST_NAMESPACE, url.as_bytes())
}

pub struct DbManager {
    conn: Connection,
}

impl DbManager {
    pub fn new(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(Self { conn })
    }

    /// Create every table and bring an older database up to `SCHEMA_VERSION`.
    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS stations (
                id TEXT PRIMARY KEY,
                source_id TEXT UNIQUE,
                name TEXT NOT NULL,
                url TEXT NOT NULL,
                genre TEXT,
                country TEXT,
                country_state TEXT,
                logo TEXT,
                comment TEXT,
                website TEXT,
                city TEXT,
                tags TEXT,
                bitrate INTEGER,
                codec TEXT,
                stream_type INTEGER,
                is_favorite INTEGER NOT NULL DEFAULT 0,
                rating INTEGER,
                play_count INTEGER NOT NULL DEFAULT 0,
                date_added TEXT,
                date_last_played TEXT,
                volume REAL NOT NULL DEFAULT 1.0,
                external INTEGER NOT NULL DEFAULT 0,
                gain_db REAL NOT NULL DEFAULT 0.0
            );

            -- Several stream URLs per station (FR-P8). `stations.url` mirrors the
            -- current row for simple callers.
            CREATE TABLE IF NOT EXISTS streams (
                id TEXT PRIMARY KEY,
                station_id TEXT NOT NULL,
                url TEXT NOT NULL,
                listen_url TEXT,
                label TEXT,
                bitrate INTEGER,
                metadata INTEGER NOT NULL DEFAULT 0,
                stream_type INTEGER,
                is_current INTEGER NOT NULL DEFAULT 0,
                custom_tag TEXT,
                open_as_website INTEGER NOT NULL DEFAULT 0,
                source_stream_id TEXT,
                UNIQUE (station_id, url)
            );

            -- One row per playback attempt: the state that belongs to the attempt,
            -- not to the station (FR-D5).
            CREATE TABLE IF NOT EXISTS play_instances (
                id TEXT PRIMARY KEY,
                station_id TEXT NOT NULL,
                stream_id TEXT,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                duration_seconds INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'playing',
                end_reason TEXT,
                volume REAL NOT NULL DEFAULT 1.0,
                recording INTEGER NOT NULL DEFAULT 0,
                error TEXT
            );

            -- Track-level history, linked to the playback attempt (FR-D6).
            CREATE TABLE IF NOT EXISTS history (
                id TEXT PRIMARY KEY,
                play_instance_id TEXT,
                station_id TEXT NOT NULL,
                station_name TEXT NOT NULL,
                url TEXT,
                artist TEXT,
                song_title TEXT,
                played_at TEXT NOT NULL,
                duration_seconds INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'ended'
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_stations (
                session_id TEXT NOT NULL,
                station_id TEXT NOT NULL,
                PRIMARY KEY (session_id, station_id)
            );

            CREATE TABLE IF NOT EXISTS wishlist (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                artist TEXT,
                song_title TEXT,
                action TEXT NOT NULL DEFAULT 'record',
                compare TEXT NOT NULL DEFAULT 'contains_text'
            );
            CREATE TABLE IF NOT EXISTS ignore_list (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                artist TEXT,
                song_title TEXT
            );
            CREATE TABLE IF NOT EXISTS custom_tags (
                station_id TEXT PRIMARY KEY,
                tag_format TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS schedules (
                id TEXT PRIMARY KEY,
                station_id TEXT NOT NULL,
                action TEXT NOT NULL,
                recurrence TEXT NOT NULL,
                start_at TEXT,
                end_at TEXT,
                finish_action TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_fired_at TEXT
            );

            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_streams_station ON streams(station_id);
            CREATE INDEX IF NOT EXISTS idx_history_played_at ON history(played_at DESC);
            CREATE INDEX IF NOT EXISTS idx_play_instances_station
                ON play_instances(station_id);
            -- Import matching scans by URL; without these a 24k-station library
            -- turns one import into hundreds of millions of row comparisons.
            CREATE INDEX IF NOT EXISTS idx_stations_url ON stations(url);
            CREATE INDEX IF NOT EXISTS idx_streams_url ON streams(url);",
        )?;

        self.migrate()?;

        // `source_id` is declared UNIQUE on fresh databases, but a v1 database
        // gains it through ALTER TABLE, so enforce uniqueness with an index too:
        // re-imports rely on it to find an existing station.
        if let Err(e) = self.conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_stations_source ON stations(source_id)",
            [],
        ) {
            // A database written by an older build can contain duplicate ids. That
            // must not stop the app from opening; imports still match by scan.
            log::warn!("could not enforce unique stations.source_id: {e}");
        }
        Ok(())
    }

    /// Additive migrations, keyed by the stored schema version.
    fn migrate(&self) -> Result<()> {
        let version = self
            .get_setting("schema_version")?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(1);

        if version < 2 {
            // v1 -> v2: stations gained `source_id`.
            self.add_column_if_missing("stations", "source_id", "TEXT")?;
        }

        if version < 3 {
            // The full original `Stations` taxonomy.
            for (column, decl) in [
                ("country_state", "TEXT"),
                ("logo", "TEXT"),
                ("comment", "TEXT"),
                ("website", "TEXT"),
                ("city", "TEXT"),
                ("tags", "TEXT"),
                ("stream_type", "INTEGER"),
                ("rating", "INTEGER"),
                ("play_count", "INTEGER NOT NULL DEFAULT 0"),
                ("date_added", "TEXT"),
                ("date_last_played", "TEXT"),
                ("volume", "REAL NOT NULL DEFAULT 1.0"),
                ("external", "INTEGER NOT NULL DEFAULT 0"),
            ] {
                self.add_column_if_missing("stations", column, decl)?;
            }

            // v1/v2 called the free-text field `description`; v3 matches the
            // UI by calling it `comment`.
            if self.has_column("stations", "description")? {
                self.conn.execute(
                    "UPDATE stations SET comment = description
                     WHERE (comment IS NULL OR comment = '') AND description IS NOT NULL",
                    [],
                )?;
            }

            for (column, decl) in [
                ("play_instance_id", "TEXT"),
                ("url", "TEXT"),
                ("artist", "TEXT"),
                ("song_title", "TEXT"),
                ("status", "TEXT NOT NULL DEFAULT 'ended'"),
            ] {
                self.add_column_if_missing("history", column, decl)?;
            }
        }

        if version < 4 {
            // Per-attempt playback state: volume is live state, recording and
            // error are written by M5/M4 but the columns belong with the attempt.
            for (column, decl) in [
                ("volume", "REAL NOT NULL DEFAULT 1.0"),
                ("recording", "INTEGER NOT NULL DEFAULT 0"),
                ("error", "TEXT"),
            ] {
                self.add_column_if_missing("play_instances", column, decl)?;
            }
        }

        if version < 5 {
            // The occurrence that last fired, so a restart cannot re-run it.
            self.add_column_if_missing("schedules", "last_fired_at", "TEXT")?;
        }

        if version < 6 {
            // Per-station gain trim for Dynamic Boost (M2b).
            self.add_column_if_missing("stations", "gain_db", "REAL NOT NULL DEFAULT 0.0")?;
        }

        if version < 7 {
            // Streams that failed to play are hidden from the list, and the
            // consecutive-failure count decides when to mark one broken.
            for (column, decl) in [
                ("broken", "INTEGER NOT NULL DEFAULT 0"),
                ("broken_at", "TEXT"),
                ("failure_count", "INTEGER NOT NULL DEFAULT 0"),
            ] {
                self.add_column_if_missing("stations", column, decl)?;
            }
        }

        if version < 8 {
            // A watched station keeps a background stream and a rolling audio
            // buffer so a wish can be recorded from the start of the song.
            self.add_column_if_missing("stations", "watched", "INTEGER NOT NULL DEFAULT 0")?;
        }

        if version > SCHEMA_VERSION {
            log::warn!(
                "database schema version {version} is newer than this build's {SCHEMA_VERSION}; \
                 leaving it untouched"
            );
        } else if version < SCHEMA_VERSION {
            self.set_setting("schema_version", &SCHEMA_VERSION.to_string())?;
        }
        Ok(())
    }

    fn table_columns(&self, table: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect()
    }

    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        Ok(self.table_columns(table)?.iter().any(|c| c == column))
    }

    fn add_column_if_missing(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        if !self.has_column(table, column)? {
            self.conn.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
                [],
            )?;
            log::info!("migration: added {table}.{column}");
        }
        Ok(())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM settings WHERE key = ?1")?;
        let mut rows = stmt.query_map(params![key], |row| row.get::<_, String>(0))?;
        match rows.next() {
            Some(v) => Ok(Some(v?)),
            None => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // Stations
    // -----------------------------------------------------------------------

    /// Insert or update a user-edited station, keeping every field as supplied.
    pub fn save_station(&self, station: &Station) -> Result<()> {
        self.conn.execute(
            "INSERT INTO stations (
                id, source_id, name, url, genre, country, country_state, logo, comment,
                website, city, tags, bitrate, codec, stream_type, is_favorite, rating,
                play_count, date_added, date_last_played, volume, external, gain_db
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23
             )
             ON CONFLICT(id) DO UPDATE SET
                source_id = excluded.source_id,
                name = excluded.name,
                url = excluded.url,
                genre = excluded.genre,
                country = excluded.country,
                country_state = excluded.country_state,
                logo = excluded.logo,
                comment = excluded.comment,
                website = excluded.website,
                city = excluded.city,
                tags = excluded.tags,
                bitrate = excluded.bitrate,
                codec = excluded.codec,
                stream_type = excluded.stream_type,
                is_favorite = excluded.is_favorite,
                rating = excluded.rating,
                play_count = excluded.play_count,
                date_added = excluded.date_added,
                date_last_played = excluded.date_last_played,
                volume = excluded.volume,
                external = excluded.external,
                gain_db = excluded.gain_db",
            params![
                station.id.to_string(),
                station.source_id.as_deref(),
                &station.name,
                &station.url,
                station.genre.as_deref(),
                station.country.as_deref(),
                station.country_state.as_deref(),
                station.logo.as_deref(),
                station.comment.as_deref(),
                station.website.as_deref(),
                station.city.as_deref(),
                station.tags.as_deref(),
                station.bitrate,
                station.codec.as_deref(),
                station.stream_type,
                station.is_favorite as i32,
                station.rating,
                station.play_count,
                station.date_added.as_deref(),
                station.date_last_played.as_deref(),
                station.volume as f64,
                station.external as i32,
                station.gain_db.clamp(-12.0, 12.0) as f64,
            ],
        )?;
        Ok(())
    }

    /// Count one playback failure and mark the station broken at two in a row.
    /// Returns the new count and whether the station is now hidden.
    pub fn record_station_failure(&self, id: Uuid) -> Result<(u32, bool)> {
        let current: Option<i64> = self
            .conn
            .query_row(
                "SELECT failure_count FROM stations WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Ok((0, false));
        };
        let failures = current.saturating_add(1);
        let broken = failures >= 2;
        self.conn.execute(
            "UPDATE stations
             SET failure_count = ?2, broken = ?3,
                 broken_at = CASE WHEN ?3 THEN ?4 ELSE broken_at END
             WHERE id = ?1",
            params![
                id.to_string(),
                failures,
                broken as i32,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok((failures as u32, broken))
    }

    /// A successful play clears the failure count and the hidden flag.
    pub fn clear_station_failure(&self, id: Uuid) -> Result<()> {        self.conn.execute(
            "UPDATE stations
             SET failure_count = 0, broken = 0, broken_at = NULL
             WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    /// Fill in a bitrate the stream reported at playback time. An existing
    /// value is never overwritten.
    pub fn set_station_bitrate(&self, id: Uuid, bitrate: u32) -> Result<()> {
        self.set_station_bitrates(&[(id, bitrate)])
    }

    /// Turn the background watch stream on or off for a station.
    pub fn set_station_watched(&self, id: Uuid, watched: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE stations SET watched = ?2 WHERE id = ?1",
            params![id.to_string(), watched as i32],
        )?;
        Ok(())
    }

    /// Batch form of [`Self::set_station_bitrate`], for a probe pass.
    pub fn set_station_bitrates(&self, values: &[(Uuid, u32)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (id, bitrate) in values {
            if *bitrate == 0 {
                continue;
            }
            tx.execute(
                "UPDATE stations SET bitrate = ?2
                 WHERE id = ?1 AND (bitrate IS NULL OR bitrate = 0)",
                params![id.to_string(), i64::from(*bitrate)],
            )?;
            tx.execute(
                "UPDATE streams SET bitrate = ?2
                 WHERE station_id = ?1 AND (bitrate IS NULL OR bitrate = 0)",
                params![id.to_string(), i64::from(*bitrate)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove stations and the rows that belong to them. History is kept as a
    /// record of what played.
    pub fn delete_stations(&self, ids: &[Uuid]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut removed = 0usize;
        for id in ids {
            let id = id.to_string();
            for table in [
                "streams",
                "session_stations",
                "schedules",
                "custom_tags",
                "play_instances",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE station_id = ?1"),
                    params![&id],
                )?;
            }
            removed += tx.execute("DELETE FROM stations WHERE id = ?1", params![&id])?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Load every readable station, sorted by name.
    ///
    /// A row whose stored id is not a UUID is skipped with a warning instead of
    /// failing the whole list: one corrupt row must not hide the library.
    pub fn load_stations(&self) -> Result<Vec<Station>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, name, url, genre, country, country_state, logo, comment,
                    website, city, tags, bitrate, codec, stream_type, is_favorite, rating,
                    play_count, date_added, date_last_played, volume, external, gain_db,
                    broken, broken_at, failure_count, watched
             FROM stations ORDER BY name COLLATE NOCASE",
        )?;

        let rows = stmt.query_map([], station_from_row)?;

        let mut stations = Vec::new();
        for row in rows {
            match row {
                Ok(station) => stations.push(station),
                Err(e) => log::warn!("skipping unreadable station row: {e}"),
            }
        }
        Ok(stations)
    }

    /// Update only the persisted default volume, so a slider drag cannot clobber
    /// `play_count`/`date_last_played` that `begin_play` already advanced.
    pub fn set_station_volume(&self, id: Uuid, volume: f32) -> Result<()> {
        self.conn.execute(
            "UPDATE stations SET volume = ?2 WHERE id = ?1",
            params![id.to_string(), volume.clamp(0.0, 1.0) as f64],
        )?;
        Ok(())
    }

    /// Update only the persisted per-station gain trim.
    pub fn set_station_gain(&self, id: Uuid, gain_db: f32) -> Result<()> {
        self.conn.execute(
            "UPDATE stations SET gain_db = ?2 WHERE id = ?1",
            params![id.to_string(), gain_db.clamp(-12.0, 12.0) as f64],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Backup and restore
    // -----------------------------------------------------------------------

    /// Write a consistent copy of the live database with SQLite's `VACUUM INTO`.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        if path.exists() {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "backup target already exists: {}",
                path.display()
            )));
        }
        let path_text = path.to_str().ok_or_else(|| {
            rusqlite::Error::InvalidParameterName("backup path is not UTF-8".to_string())
        })?;
        self.conn.execute("VACUUM INTO ?1", params![path_text])?;
        Ok(())
    }

    /// Check that a file is a readable VibeRadio database and return its schema
    /// version. An older backup is fine (it migrates); a newer one is rejected
    /// by the caller.
    pub fn validate_backup(path: &Path) -> io::Result<i64> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(io_error)?;
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(io_error)?;
        if check != "ok" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("integrity check failed: {check}"),
            ));
        }
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        match version {
            Some(text) => text.parse::<i64>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid schema_version {text:?}"),
                )
            }),
            None => Ok(1),
        }
    }

    /// Apply a restore requested by the Settings window. The marker holds only a
    /// backup file name in the data folder; the file is copied to a temporary
    /// sibling and renamed over the live database, so a failed copy cannot
    /// truncate the live file.
    pub fn apply_pending_restore(dir: &Path) -> Option<String> {
        let marker = dir.join("viberadio-restore.pending");
        let source_name = std::fs::read_to_string(&marker).ok()?.trim().to_string();
        let source = match safe_backup_name(dir, &source_name) {
            Ok(path) => path,
            Err(message) => {
                let _ = std::fs::remove_file(&marker);
                return Some(format!("Restore failed: {message}"));
            }
        };
        let live = dir.join("viberadio.db");
        let temp = dir.join("viberadio.db.restore-tmp");
        let result = (|| -> io::Result<String> {
            let source_canon = std::fs::canonicalize(&source).unwrap_or_else(|_| source.clone());
            let live_canon = std::fs::canonicalize(&live).unwrap_or_else(|_| live.clone());
            if source_canon == live_canon {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot restore the live database over itself",
                ));
            }
            let version = DbManager::validate_backup(&source)?;
            if version > SCHEMA_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "backup schema v{version} is newer than this build's v{SCHEMA_VERSION}"
                    ),
                ));
            }
            if live.exists() {
                let before = unique_restore_path(dir);
                // A corrupt live file must not block the restore. Prefer a
                // consistent VACUUM snapshot, but fall back to moving the file.
                let snapshot = live
                    .to_str()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "live database path is not UTF-8",
                        )
                    })
                    .and_then(|live_text| {
                        DbManager::new(live_text)
                            .and_then(|db| db.backup_to(&before))
                            .map_err(io_error)
                    });
                if snapshot.is_err() {
                    let _ = std::fs::remove_file(&before);
                    std::fs::rename(&live, &before)?;
                }
            }
            for sidecar in [
                "viberadio.db-wal",
                "viberadio.db-shm",
                "viberadio.db-journal",
            ] {
                let _ = std::fs::remove_file(dir.join(sidecar));
            }
            let _ = std::fs::remove_file(&temp);
            std::fs::copy(&source, &temp)?;
            // `FlushFileBuffers` needs write access on Windows, so open for
            // writing rather than using `File::open`.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&temp)?
                .sync_all()?;
            std::fs::rename(&temp, &live)?;
            std::fs::remove_file(&marker)?;
            prune_prefixed(dir, "viberadio-before-restore-", ".db", 5);
            Ok(format!("Restored database from {}", source.display()))
        })();
        match result {
            Ok(message) => Some(message),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                let _ = std::fs::remove_file(&marker);
                Some(format!("Restore failed: {e}"))
            }
        }
    }

    /// Copy the library to a new data folder before the next start. The marker
    /// is written in the default data folder by the Settings window; doing the
    /// copy here means the newest library is copied, not the snapshot from when
    /// the button was clicked.
    pub fn apply_pending_copy(default_dir: &Path) -> Option<String> {
        let marker = default_dir.join("viberadio-copy.pending");
        let text = std::fs::read_to_string(&marker).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let source = PathBuf::from(value.get("source")?.as_str()?);
        let target = PathBuf::from(value.get("target")?.as_str()?);
        let result = (|| -> io::Result<String> {
            if target.exists() {
                let _ = std::fs::remove_file(&marker);
                return Ok(format!(
                    "The target database already exists; keeping {}",
                    target.display()
                ));
            }
            let source_db = DbManager::new(source.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source database path is not UTF-8",
                )
            })?)
            .map_err(io_error)?;
            source_db.backup_to(&target).map_err(io_error)?;
            std::fs::remove_file(&marker)?;
            Ok(format!("Library copied to {}", target.display()))
        })();
        match result {
            Ok(message) => Some(message),
            Err(e) => {
                // Keep the app on the old folder if the copy failed.
                if let Some(source_dir) = source.parent() {
                    let default_canon = std::fs::canonicalize(default_dir)
                        .unwrap_or_else(|_| default_dir.to_path_buf());
                    let source_canon = std::fs::canonicalize(source_dir)
                        .unwrap_or_else(|_| source_dir.to_path_buf());
                    if source_canon == default_canon {
                        let _ = std::fs::remove_file(default_dir.join("location"));
                    } else {
                        let _ = std::fs::write(
                            default_dir.join("location"),
                            source_dir.display().to_string(),
                        );
                    }
                }
                let _ = std::fs::remove_file(&marker);
                Some(format!("Data folder copy failed: {e}"))
            }
        }
    }

    /// Newest `viberadio-backup-*.db` in the data folder. UTC file names sort
    /// correctly; one unreadable entry is skipped rather than hiding the rest.
    pub fn latest_backup(dir: &Path) -> Option<PathBuf> {
        let mut best: Option<PathBuf> = None;
        for entry in std::fs::read_dir(dir).ok()? {
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with("viberadio-backup-") && name.ends_with(".db") {
                if best.as_ref().map(|current| path > *current).unwrap_or(true) {
                    best = Some(path);
                }
            }
        }
        best
    }

    /// Keep only the newest `keep` backups; older ones are deleted.
    pub fn prune_backups(dir: &Path, keep: usize) {
        prune_prefixed(dir, "viberadio-backup-", ".db", keep);
    }

    /// Keep the open attempt's live volume in step with the slider, so a crash
    /// still leaves the volume that was actually in use.
    pub fn update_play_instance_volume(&self, id: Uuid, volume: f32) -> Result<()> {
        self.conn.execute(
            "UPDATE play_instances SET volume = ?2 WHERE id = ?1 AND ended_at IS NULL",
            params![id.to_string(), volume.clamp(0.0, 1.0) as f64],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Streams
    // -----------------------------------------------------------------------

    pub fn save_stream(&self, stream: &Stream) -> Result<()> {
        upsert_stream(&self.conn, stream)
    }

    /// All streams of one station, current first.
    pub fn load_streams_for_station(&self, station_id: Uuid) -> Result<Vec<Stream>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, station_id, url, listen_url, label, bitrate, metadata, stream_type,
                    is_current, custom_tag, open_as_website, source_stream_id
             FROM streams WHERE station_id = ?1
             ORDER BY is_current DESC, url COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![station_id.to_string()], stream_from_row)?;
        collect_streams(rows)
    }

    /// Every stream in the library (used by the playback layer and by tests).
    #[allow(dead_code)] // inventory helper; the UI reads one station at a time
    pub fn load_all_streams(&self) -> Result<Vec<Stream>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, station_id, url, listen_url, label, bitrate, metadata, stream_type,
                    is_current, custom_tag, open_as_website, source_stream_id
             FROM streams ORDER BY station_id, is_current DESC, url COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], stream_from_row)?;
        collect_streams(rows)
    }

    /// The stream a station should play, if any.
    pub fn current_stream_for_station(&self, station_id: Uuid) -> Result<Option<Stream>> {
        Ok(self
            .load_streams_for_station(station_id)?
            .into_iter()
            .next())
    }

    /// Guarantee a playable stream row for a station (used for user-added
    /// stations and as a fallback for imported ones).
    pub fn ensure_stream_for_station(&self, station: &Station) -> Result<Stream> {
        if let Some(stream) = self.current_stream_for_station(station.id)? {
            return Ok(stream);
        }
        let stream = Stream::new(station.id, station.url.clone());
        self.save_stream(&stream)?;
        Ok(stream)
    }

    // -----------------------------------------------------------------------
    // Play attempts and history
    // -----------------------------------------------------------------------

    /// Open a playback attempt and its linked history row.
    ///
    /// The history row records the URL of the stream that was actually played, not
    /// the station's default URL. Also bumps the station's `play_count` and
    /// `date_last_played`; the attempt itself keeps duration and status (FR-D5,
    /// FR-D6).
    pub fn begin_play(
        &self,
        station: &Station,
        stream: Option<&Stream>,
        volume: f32,
    ) -> Result<(PlayInstance, StationHistory)> {
        let now = Utc::now();
        let instance = PlayInstance {
            id: Uuid::new_v4(),
            station_id: station.id,
            stream_id: stream.map(|s| s.id),
            started_at: now,
            ended_at: None,
            duration_seconds: 0,
            status: PLAY_STATUS_PLAYING.to_string(),
            end_reason: None,
            volume: volume.clamp(0.0, 1.0),
            recording: false,
            error: None,
        };
        let history = StationHistory {
            id: Uuid::new_v4(),
            play_instance_id: Some(instance.id),
            station_id: station.id,
            station_name: station.name.clone(),
            url: Some(
                stream
                    .map(|s| s.url.clone())
                    .unwrap_or_else(|| station.url.clone()),
            ),
            artist: None,
            song_title: None,
            played_at: now,
            duration_seconds: 0,
            status: PLAY_STATUS_PLAYING.to_string(),
        };

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO play_instances
                (id, station_id, stream_id, started_at, ended_at, duration_seconds, status,
                 end_reason, volume, recording, error)
             VALUES (?1, ?2, ?3, ?4, NULL, 0, ?5, NULL, ?6, 0, NULL)",
            params![
                instance.id.to_string(),
                instance.station_id.to_string(),
                instance.stream_id.map(|id| id.to_string()),
                instance.started_at.to_rfc3339(),
                &instance.status,
                instance.volume as f64,
            ],
        )?;
        tx.execute(
            "INSERT INTO history
                (id, play_instance_id, station_id, station_name, url, artist, song_title,
                 played_at, duration_seconds, status)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, 0, ?7)",
            params![
                history.id.to_string(),
                history.play_instance_id.map(|id| id.to_string()),
                history.station_id.to_string(),
                &history.station_name,
                history.url.as_deref(),
                history.played_at.to_rfc3339(),
                &history.status,
            ],
        )?;
        tx.execute(
            "UPDATE stations
             SET play_count = play_count + 1, date_last_played = ?2
             WHERE id = ?1",
            params![station.id.to_string(), now.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok((instance, history))
    }

    /// Close a playback attempt and its history row.
    ///
    /// Only an open attempt is closed, so a double close (explicit stop, then the
    /// app's drop handler) cannot overwrite the real duration or end reason.
    pub fn end_play(
        &self,
        play_instance_id: Uuid,
        history_id: Uuid,
        duration_seconds: i64,
        status: &str,
        end_reason: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let ended_at = Utc::now();
        let tx = self.conn.unchecked_transaction()?;
        let closed = tx.execute(
            "UPDATE play_instances
             SET ended_at = ?2, duration_seconds = ?3, status = ?4, end_reason = ?5,
                 error = ?6
             WHERE id = ?1 AND ended_at IS NULL",
            params![
                play_instance_id.to_string(),
                ended_at.to_rfc3339(),
                duration_seconds,
                status,
                end_reason,
                error,
            ],
        )?;
        if closed == 0 {
            log::warn!("play instance {play_instance_id} was already closed; keeping its record");
        } else {
            tx.execute(
                "UPDATE history SET duration_seconds = ?2, status = ?3 WHERE id = ?1",
                params![history_id.to_string(), duration_seconds, status],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert one track-level history row linked to the playback attempt that
    /// produced it. Returns the row so the app can close it on the next title or
    /// when the stream stops.
    #[allow(clippy::too_many_arguments)]
    pub fn start_track(
        &self,
        play_instance_id: Uuid,
        station_id: Uuid,
        station_name: &str,
        url: Option<&str>,
        artist: Option<&str>,
        song_title: Option<&str>,
    ) -> Result<StationHistory> {
        let entry = StationHistory {
            id: Uuid::new_v4(),
            play_instance_id: Some(play_instance_id),
            station_id,
            station_name: station_name.to_string(),
            url: url.map(str::to_string),
            artist: artist.map(str::to_string).filter(|value| !value.is_empty()),
            song_title: song_title
                .map(str::to_string)
                .filter(|value| !value.is_empty()),
            played_at: Utc::now(),
            duration_seconds: 0,
            status: PLAY_STATUS_PLAYING.to_string(),
        };
        self.conn.execute(
            "INSERT INTO history
                (id, play_instance_id, station_id, station_name, url, artist, song_title,
                 played_at, duration_seconds, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9)",
            params![
                entry.id.to_string(),
                play_instance_id.to_string(),
                station_id.to_string(),
                &entry.station_name,
                entry.url.as_deref(),
                entry.artist.as_deref(),
                entry.song_title.as_deref(),
                entry.played_at.to_rfc3339(),
                &entry.status,
            ],
        )?;
        Ok(entry)
    }

    /// Replace the artist and title of an open history row after a catalog
    /// lookup recovered them from an uploader-style label.
    pub fn update_track_metadata(&self, id: Uuid, artist: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE history SET artist = ?2, song_title = ?3
             WHERE id = ?1 AND status = ?4",
            params![id.to_string(), artist, title, PLAY_STATUS_PLAYING],
        )?;
        Ok(())
    }

    /// Close a track row. A second close leaves the real duration/status intact.
    pub fn end_track(&self, id: Uuid, duration_seconds: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE history SET duration_seconds = ?2, status = ?3
             WHERE id = ?1 AND status = ?4",
            params![
                id.to_string(),
                duration_seconds.max(0),
                status,
                PLAY_STATUS_PLAYING
            ],
        )?;
        Ok(())
    }

    /// Track rows for one attempt, in play order (used by the history UI).
    #[allow(dead_code)]
    pub fn load_tracks_for_instance(&self, play_instance_id: Uuid) -> Result<Vec<StationHistory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, play_instance_id, station_id, station_name, url, artist, song_title,
                    played_at, duration_seconds, status
             FROM history
             WHERE play_instance_id = ?1 AND (artist IS NOT NULL OR song_title IS NOT NULL)
             ORDER BY played_at ASC",
        )?;
        let rows = stmt.query_map(params![play_instance_id.to_string()], |row| {
            Ok(StationHistory {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                play_instance_id: row
                    .get::<_, Option<String>>(1)?
                    .and_then(|value| Uuid::parse_str(&value).ok()),
                station_id: Uuid::parse_str(&row.get::<_, String>(2)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
                station_name: row.get(3)?,
                url: row.get(4)?,
                artist: row.get(5)?,
                song_title: row.get(6)?,
                played_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(7)?)
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?
                    .with_timezone(&Utc),
                duration_seconds: row.get(8)?,
                status: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// Ignore-list entries are `(artist, song_title)`; either side may be empty.
    pub fn load_ignore_list(&self) -> Result<Vec<(Option<String>, Option<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT artist, song_title FROM ignore_list ORDER BY id")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    /// Wish-list rules, including their action and compare mode.
    pub fn load_wishlist(&self) -> Result<Vec<WishEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, artist, song_title, action, compare FROM wishlist ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            let action: String = row.get(3)?;
            let compare: String = row.get(4)?;
            Ok(WishEntry {
                id: row.get(0)?,
                artist: row.get(1)?,
                song_title: row.get(2)?,
                action: if action.trim().is_empty() {
                    "record".to_string()
                } else {
                    action
                },
                compare: CompareMode::parse(&compare),
            })
        })?;
        rows.collect()
    }

    pub fn add_wishlist_entry(&self, entry: &WishEntry) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO wishlist (artist, song_title, action, compare)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                entry.artist.as_deref(),
                entry.song_title.as_deref(),
                entry.action,
                entry.compare.as_str()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_wishlist_entry(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM wishlist WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Per-station custom tag format, if the imported database or user set one.
    pub fn custom_tag_format(&self, station_id: Uuid) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tag_format FROM custom_tags WHERE station_id = ?1")?;
        let mut rows = stmt.query(params![station_id.to_string()])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Set or clear a station's custom ICY tag format.
    pub fn set_custom_tag_format(&self, station_id: Uuid, format: Option<&str>) -> Result<()> {
        match format.map(str::trim).filter(|format| !format.is_empty()) {
            Some(format) => {
                self.conn.execute(
                    "INSERT INTO custom_tags (station_id, tag_format) VALUES (?1, ?2)
                     ON CONFLICT(station_id) DO UPDATE SET tag_format = excluded.tag_format",
                    params![station_id.to_string(), format],
                )?;
            }
            None => {
                self.conn.execute(
                    "DELETE FROM custom_tags WHERE station_id = ?1",
                    params![station_id.to_string()],
                )?;
            }
        }
        Ok(())
    }

    /// Persist whether a recording was attached to this attempt (FR-D5).
    pub fn set_play_instance_recording(&self, id: Uuid, recording: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE play_instances SET recording = ?2 WHERE id = ?1",
            params![id.to_string(), i64::from(recording)],
        )?;
        Ok(())
    }

    /// Mark attempts left open by a previous run (crash or forced kill) as failed,
    /// so the history does not claim a stream is still playing forever.
    ///
    /// `ended_at` is stamped with the sweep time: the real end is unknown, but the
    /// attempt must not look open to any `ended_at IS NULL` query afterwards.
    pub fn close_orphaned_play_instances(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let count = tx.execute(
            "UPDATE play_instances
             SET status = ?1,
                 ended_at = COALESCE(ended_at, ?2),
                 end_reason = 'app exited before the attempt finished',
                 error = COALESCE(error, 'app exited before the attempt finished')
             WHERE ended_at IS NULL AND status = ?3",
            params![PLAY_STATUS_ERROR, now, PLAY_STATUS_PLAYING],
        )?;
        tx.execute(
            "UPDATE history SET status = ?1 WHERE status = ?2",
            params![PLAY_STATUS_ERROR, PLAY_STATUS_PLAYING],
        )?;
        tx.commit()?;
        Ok(count)
    }

    /// Force-close an attempt that `end_play` could not close (for example because
    /// the database was momentarily busy). Used before the app opens a new attempt,
    /// which would otherwise overwrite the only ids that could retry it.
    pub fn abandon_play_instance(&self, play_instance_id: Uuid, history_id: Uuid) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE play_instances
             SET status = ?3,
                 ended_at = COALESCE(ended_at, ?2),
                 end_reason = COALESCE(end_reason, 'could not be closed when playback switched'),
                 error = COALESCE(error, 'could not be closed when playback switched')
             WHERE id = ?1 AND ended_at IS NULL",
            params![play_instance_id.to_string(), now, PLAY_STATUS_ERROR],
        )?;
        tx.execute(
            "UPDATE history SET status = ?2 WHERE id = ?1 AND status = 'playing'",
            params![history_id.to_string(), PLAY_STATUS_ERROR],
        )?;
        tx.execute(
            "UPDATE history SET status = ?2
             WHERE play_instance_id = ?1 AND status = 'playing'
               AND (artist IS NOT NULL OR song_title IS NOT NULL)",
            params![play_instance_id.to_string(), PLAY_STATUS_ERROR],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// True when no heartbeat was written recently, i.e. no other VibeRadio
    /// process is using this library. The startup orphan sweep uses this so a
    /// second window does not mark a live window's attempts as failed.
    pub fn should_sweep_orphans(&self, stale_after_seconds: i64) -> Result<bool> {
        let Some(text) = self.get_setting("heartbeat_at")? else {
            return Ok(true);
        };
        let Ok(last) = DateTime::parse_from_rfc3339(&text) else {
            return Ok(true);
        };
        Ok((Utc::now() - last.with_timezone(&Utc)).num_seconds() > stale_after_seconds)
    }

    /// Record that this process owns the library. Cheap enough to call every few
    /// seconds; it is a single-row upsert.
    pub fn write_heartbeat(&self) -> Result<()> {
        self.set_setting("heartbeat_at", &Utc::now().to_rfc3339())
    }

    pub fn clear_heartbeat(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM settings WHERE key = 'heartbeat_at'", [])?;
        Ok(())
    }

    pub fn load_play_instance(&self, id: Uuid) -> Result<Option<PlayInstance>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, station_id, stream_id, started_at, ended_at, duration_seconds, status,
                    end_reason, volume, recording, error
             FROM play_instances WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id.to_string()], play_instance_from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Load the history, skipping rows that cannot be parsed.
    pub fn load_history(&self) -> Result<Vec<StationHistory>> {
        self.load_history_limit(None)
    }

    /// The newest `limit` history rows, for the in-memory view. The export
    /// still uses the unlimited query.
    pub fn load_recent_history(&self, limit: usize) -> Result<Vec<StationHistory>> {
        self.load_history_limit(Some(limit))
    }

    fn load_history_limit(&self, limit: Option<usize>) -> Result<Vec<StationHistory>> {
        let sql = if limit.is_some() {
            "SELECT id, play_instance_id, station_id, station_name, url, artist, song_title,
                    played_at, duration_seconds, status
             FROM history ORDER BY played_at DESC LIMIT ?1"
        } else {
            "SELECT id, play_instance_id, station_id, station_name, url, artist, song_title,
                    played_at, duration_seconds, status
             FROM history ORDER BY played_at DESC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let params: Vec<i64> = limit.map(|limit| vec![limit as i64]).unwrap_or_default();

        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })?;

        let mut history = Vec::new();
        for row in rows {
            let (
                id_str,
                play_instance_str,
                station_id_str,
                station_name,
                url,
                artist,
                song_title,
                played_at_str,
                duration,
                status,
            ) = row?;
            let (Ok(id), Ok(station_id)) =
                (Uuid::parse_str(&id_str), Uuid::parse_str(&station_id_str))
            else {
                log::warn!("skipping history row with unparsable id {id_str:?}");
                continue;
            };
            let play_instance_id = match play_instance_str {
                Some(s) => match Uuid::parse_str(&s) {
                    Ok(id) => Some(id),
                    Err(_) => {
                        log::warn!("history row {id_str:?} has an unparsable play instance id");
                        None
                    }
                },
                None => None,
            };
            let Ok(played_at) = DateTime::parse_from_rfc3339(&played_at_str) else {
                log::warn!("skipping history row with unparsable timestamp {played_at_str:?}");
                continue;
            };
            history.push(StationHistory {
                id,
                play_instance_id,
                station_id,
                station_name,
                url,
                artist,
                song_title,
                played_at: played_at.with_timezone(&Utc),
                duration_seconds: duration.unwrap_or(0),
                status: status.unwrap_or_else(|| PLAY_STATUS_ENDED.to_string()),
            });
        }

        Ok(history)
    }

    // -----------------------------------------------------------------------
    // Sessions and playlists
    // -----------------------------------------------------------------------

    pub fn save_session(&self, name: &str, station_ids: &[Uuid]) -> Result<Uuid> {
        let name = name.trim();
        if name.is_empty() {
            return Err(rusqlite::Error::InvalidParameterName(
                "session name is empty".to_string(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM sessions WHERE name = ?1 LIMIT 1",
                params![name],
                |row| row.get(0),
            )
            .optional()?;
        let id = existing
            .and_then(|value| Uuid::parse_str(&value).ok())
            .unwrap_or_else(Uuid::new_v4);
        tx.execute(
            "INSERT OR REPLACE INTO sessions (id, name) VALUES (?1, ?2)",
            params![id.to_string(), name],
        )?;
        tx.execute(
            "DELETE FROM session_stations WHERE session_id = ?1",
            params![id.to_string()],
        )?;
        for station_id in station_ids {
            tx.execute(
                "INSERT OR IGNORE INTO session_stations (session_id, station_id) VALUES (?1, ?2)",
                params![id.to_string(), station_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn load_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.name, COUNT(ss.station_id)
             FROM sessions s LEFT JOIN session_stations ss ON ss.session_id = s.id
             GROUP BY s.id, s.name ORDER BY s.name",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok((id, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            let (id, name, count) = row?;
            let Ok(id) = Uuid::parse_str(&id) else {
                continue;
            };
            sessions.push(SessionSummary {
                id,
                name,
                station_count: count.max(0) as usize,
            });
        }
        Ok(sessions)
    }

    pub fn load_session_stations(&self, session_id: Uuid) -> Result<Vec<Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT station_id FROM session_stations WHERE session_id = ?1 ORDER BY station_id",
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut stations = Vec::new();
        for row in rows {
            if let Ok(id) = Uuid::parse_str(&row?) {
                stations.push(id);
            }
        }
        Ok(stations)
    }

    pub fn delete_session(&self, session_id: Uuid) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM session_stations WHERE session_id = ?1",
            params![session_id.to_string()],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![session_id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Schedules (F37/F38, FR-S1–S3)
    // -----------------------------------------------------------------------

    pub fn load_schedules(&self) -> Result<Vec<Schedule>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, station_id, action, recurrence, start_at, end_at,
                    finish_action, enabled, last_fired_at
             FROM schedules
             ORDER BY start_at, id",
        )?;
        let rows = stmt.query_map([], schedule_from_row)?;
        let mut schedules = Vec::new();
        for row in rows {
            match row {
                Ok(schedule) => schedules.push(schedule),
                Err(e) => log::warn!("skipping unreadable schedule row: {e}"),
            }
        }
        Ok(schedules)
    }

    pub fn save_schedule(&self, schedule: &Schedule) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO schedules
                (id, station_id, action, recurrence, start_at, end_at,
                 finish_action, enabled, last_fired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                schedule.id.to_string(),
                schedule.station_id.to_string(),
                schedule.action.as_str(),
                schedule.recurrence.as_str(),
                schedule.start_at.format("%Y-%m-%dT%H:%M").to_string(),
                schedule
                    .end_at
                    .map(|end| end.format("%Y-%m-%dT%H:%M").to_string()),
                schedule.finish_action.as_str(),
                schedule.enabled as i32,
                schedule.last_fired_at.map(|at| at.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn delete_schedule(&self, schedule_id: Uuid) -> Result<()> {
        self.conn.execute(
            "DELETE FROM schedules WHERE id = ?1",
            params![schedule_id.to_string()],
        )?;
        Ok(())
    }

    /// Remember the occurrence that fired (or was deliberately skipped), so a
    /// restart cannot run it twice.
    pub fn mark_schedule_fired(&self, schedule_id: Uuid, at: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE schedules SET last_fired_at = ?2 WHERE id = ?1",
            params![schedule_id.to_string(), at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn import_seeds(&self, seeds: &[StationSeed]) -> io::Result<usize> {
        let tx = self.conn.unchecked_transaction().map_err(io_error)?;
        let mut imported = 0usize;
        for seed in seeds {
            let url = normalize_playlist_url(&seed.url);
            let Ok(parsed) = url::Url::parse(&url) else {
                continue;
            };
            if parsed.host_str().is_none()
                || !matches!(parsed.scheme(), "http" | "https")
                || url.len() > 2048
            {
                continue;
            }
            let id = stable_playlist_station_id(&url);
            if self.station_exists(id).map_err(io_error)?
                || self.url_in_library(&url).map_err(io_error)?
            {
                // A station imported before the radio.net id was kept still
                // needs it for its homepage and playlist.
                if let Some(source_id) = seed.source_id.as_deref() {
                    self.backfill_source_id(&url, source_id).map_err(io_error)?;
                }
                if seed.bitrate.is_some() {
                    self.backfill_bitrate(&url, seed.bitrate).map_err(io_error)?;
                }
                continue;
            }
            let mut station = Station::new(seed.name.clone(), url.clone());
            station.id = id;
            station.source_id = seed.source_id.clone();
            station.genre = seed.genre.clone();
            station.country = seed.country.clone();
            station.city = seed.city.clone();
            station.logo = seed.logo.clone();
            station.website = seed.website.clone();
            station.tags = seed.tags.clone();
            station.codec = seed.codec.clone();
            station.bitrate = seed.bitrate;
            self.save_station(&station).map_err(io_error)?;
            let mut stream = Stream::new(station.id, station.url.clone());
            stream.bitrate = seed.bitrate;
            self.save_stream(&stream).map_err(io_error)?;
            imported += 1;
        }
        tx.commit().map_err(io_error)?;
        Ok(imported)
    }

    /// Save the library as an M3U playlist.
    ///
    /// One entry per station, using its current stream URL. M3U has no way to
    /// express a station's alternate streams, so those are deliberately not
    /// exported; the library itself keeps them.
    pub fn export_m3u(&self, path: impl AsRef<Path>) -> io::Result<usize> {
        let stations = self
            .load_stations()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut out = String::from("#EXTM3U\n");
        let mut written = 0usize;
        for station in &stations {
            if station.url.trim().is_empty() {
                continue;
            }
            // A newline inside a title would split the #EXTINF line.
            let name = station.name.replace(['\r', '\n'], " ");
            out.push_str(&format!("#EXTINF:-1,{name}\n{}\n", station.url));
            written += 1;
        }
        std::fs::write(path, out)?;
        Ok(written)
    }

    /// Export attempt and track history as a self-contained HTML table.
    pub fn export_history_html(&self, path: impl AsRef<Path>) -> io::Result<usize> {
        let rows = self
            .load_history()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let mut out = String::from(
            "<!doctype html>\n<meta charset=\"utf-8\">\n<title>VibeRadio history</title>\n             <style>body{font-family:system-ui,sans-serif;margin:2rem;background:#f7f8fa;color:#16181c}             table{border-collapse:collapse;width:100%;background:#fff}th,td{padding:.45rem .6rem;             border-bottom:1px solid #e3e8f0;text-align:left;font-size:13px}th{background:#eef1f6}             td.num{text-align:right;font-variant-numeric:tabular-nums}</style>\n             <h1>VibeRadio history</h1>\n<table>\n             <thead><tr><th>Played (UTC)</th><th>Station</th><th>Artist</th><th>Song</th>             <th>Duration</th><th>Status</th></tr></thead>\n<tbody>\n",
        );
        for row in &rows {
            let artist = row.artist.as_deref().unwrap_or("—");
            let song = row.song_title.as_deref().unwrap_or("—");
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"num\">{}s</td><td>{}</td></tr>\n",
                escape_html(&row.played_at.to_rfc3339()),
                escape_html(&row.station_name),
                escape_html(artist),
                escape_html(song),
                row.duration_seconds,
                escape_html(&row.status),
            ));
        }
        out.push_str("</tbody>\n</table>\n");
        std::fs::write(path, out)?;
        Ok(rows.len())
    }

    /// Import an M3U playlist, skipping URLs that are already in the library.
    ///
    /// Ids are derived from the URL, so importing the same playlist twice is a
    /// no-op rather than a duplicate set of stations (FR-D2). A skipped entry also
    /// clears any pending `#EXTINF` title, so a stray line cannot attach its name
    /// to the next URL.
    pub fn import_m3u(&self, path: impl AsRef<Path>) -> io::Result<usize> {
        // Playlists in the wild are not always UTF-8; never fail the whole import
        // over one non-ASCII byte.
        let bytes = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes);
        // A UTF-8 BOM would otherwise make the first `#EXTM3U` unrecognisable.
        let text = text.trim_start_matches('\u{feff}');

        let tx = self.conn.unchecked_transaction().map_err(io_error)?;
        let mut imported = 0usize;
        let mut pending_name: Option<String> = None;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("#EXTINF:") {
                pending_name = extinf_title(rest);
                continue;
            }
            if line.starts_with('#') {
                continue;
            }

            let lower = line.to_ascii_lowercase();
            if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                // A non-playlist line breaks the association with a previous
                // #EXTINF; do not let that title leak onto the next URL.
                pending_name = None;
                continue;
            }
            // The scheme is case-insensitive; store the canonical lowercase form
            // so playback's scheme check accepts it.
            let url = normalize_playlist_url(line);

            let id = stable_playlist_station_id(&url);
            if self.station_exists(id).map_err(io_error)?
                || self.url_in_library(&url).map_err(io_error)?
            {
                pending_name = None;
                continue;
            }

            let name = pending_name.take().unwrap_or_else(|| url.clone());
            let mut station = Station::new(name, url.clone());
            station.id = id;
            self.save_station(&station).map_err(io_error)?;
            self.save_stream(&Stream::new(station.id, station.url.clone()))
                .map_err(io_error)?;
            imported += 1;
        }

        tx.commit().map_err(io_error)?;
        Ok(imported)
    }

    fn station_exists(&self, id: Uuid) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM stations WHERE id = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![id.to_string()])?;
        Ok(rows.next()?.is_some())
    }

    /// A URL is already known if it is a station's current URL or any stream's URL.
    /// Give an existing station its radio.net id without touching anything else.
    fn backfill_source_id(&self, url: &str, source_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE stations SET source_id = ?1
             WHERE url = ?2 AND (source_id IS NULL OR source_id = '')",
            params![source_id, url],
        )?;
        Ok(())
    }

    /// Fill in a bitrate that an earlier import did not know about.
    fn backfill_bitrate(&self, url: &str, bitrate: Option<u32>) -> Result<()> {
        let Some(bitrate) = bitrate else {
            return Ok(());
        };
        self.conn.execute(
            "UPDATE stations SET bitrate = ?1
             WHERE url = ?2 AND (bitrate IS NULL OR bitrate = 0)",
            params![i64::from(bitrate), url],
        )?;
        Ok(())
    }

    fn url_in_library(&self, url: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT 1 FROM stations WHERE url = ?1
             UNION
             SELECT 1 FROM streams WHERE url = ?1
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![url])?;
        Ok(rows.next()?.is_some())
    }
}

/// Accept only a plain backup file name in `dir`; a marker cannot point at an
/// arbitrary path or escape the data folder.
fn safe_backup_name(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(name).components();
    let Some(std::path::Component::Normal(component)) = components.next() else {
        return Err("the pending restore must name a file in the data folder".to_string());
    };
    if components.next().is_some() {
        return Err("the pending restore must name a file in the data folder".to_string());
    }
    let name = component.to_string_lossy();
    if !name.starts_with("viberadio-backup-") || !name.ends_with(".db") {
        return Err("the pending restore must name a viberadio-backup-*.db file".to_string());
    }
    Ok(dir.join(component))
}

fn prune_prefixed(dir: &Path, prefix: &str, suffix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with(prefix) && name.ends_with(suffix))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();
    let remove = paths.len().saturating_sub(keep);
    for path in paths.into_iter().take(remove) {
        let _ = std::fs::remove_file(path);
    }
}

fn unique_restore_path(dir: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
    let mut path = dir.join(format!("viberadio-before-restore-{stamp}.db"));
    let mut counter = 2;
    while path.exists() {
        path = dir.join(format!("viberadio-before-restore-{stamp}-{counter}.db"));
        counter += 1;
    }
    path
}

fn io_error(e: rusqlite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

/// Insert a stream or update the existing row for the same id *or* the same
/// `(station_id, url)` key.
///
/// The two-step lookup matters because `ensure_stream_for_station` can create a
/// row with a generated id before the same stream is imported with its
/// deterministic id; a plain upsert on `id` would then violate the
/// `UNIQUE(station_id, url)` constraint.
fn upsert_stream(conn: &Connection, stream: &Stream) -> Result<()> {
    // Identity (`station_id`, `url`) is not part of the by-id update: changing a
    // URL on an existing id could collide with the unique `(station_id, url)` key.
    // A different URL is a different stream; callers create a new row for it.
    let updated = conn.execute(
        "UPDATE streams SET
            listen_url = COALESCE(?2, listen_url),
            label = COALESCE(?3, label),
            bitrate = COALESCE(?4, bitrate),
            metadata = ?5,
            stream_type = COALESCE(?6, stream_type),
            is_current = ?7,
            custom_tag = COALESCE(?8, custom_tag),
            open_as_website = ?9,
            source_stream_id = COALESCE(?10, source_stream_id)
         WHERE id = ?1",
        params![
            stream.id.to_string(),
            stream.listen_url.as_deref(),
            stream.label.as_deref(),
            stream.bitrate,
            stream.metadata as i32,
            stream.stream_type,
            stream.is_current as i32,
            stream.custom_tag.as_deref(),
            stream.open_as_website as i32,
            stream.source_stream_id.as_deref(),
        ],
    )?;
    if updated > 0 {
        return Ok(());
    }

    let updated = conn.execute(
        "UPDATE streams SET
            listen_url = COALESCE(?3, listen_url),
            label = COALESCE(?4, label),
            bitrate = COALESCE(?5, bitrate),
            metadata = ?6,
            stream_type = COALESCE(?7, stream_type),
            is_current = ?8,
            custom_tag = COALESCE(?9, custom_tag),
            open_as_website = ?10,
            source_stream_id = COALESCE(?11, source_stream_id)
         WHERE station_id = ?1 AND url = ?2",
        params![
            stream.station_id.to_string(),
            &stream.url,
            stream.listen_url.as_deref(),
            stream.label.as_deref(),
            stream.bitrate,
            stream.metadata as i32,
            stream.stream_type,
            stream.is_current as i32,
            stream.custom_tag.as_deref(),
            stream.open_as_website as i32,
            stream.source_stream_id.as_deref(),
        ],
    )?;
    if updated > 0 {
        return Ok(());
    }

    conn.execute(
        "INSERT INTO streams (
            id, station_id, url, listen_url, label, bitrate, metadata, stream_type,
            is_current, custom_tag, open_as_website, source_stream_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            stream.id.to_string(),
            stream.station_id.to_string(),
            &stream.url,
            stream.listen_url.as_deref(),
            stream.label.as_deref(),
            stream.bitrate,
            stream.metadata as i32,
            stream.stream_type,
            stream.is_current as i32,
            stream.custom_tag.as_deref(),
            stream.open_as_website as i32,
            stream.source_stream_id.as_deref(),
        ],
    )?;
    Ok(())
}

/// The title of an `#EXTINF:` line: the text after the last comma that is not
/// inside a quoted attribute (`tvg-name="a,b"`).
fn extinf_title(rest: &str) -> Option<String> {
    let mut in_quotes = false;
    let mut last_comma = None;
    for (idx, ch) in rest.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => last_comma = Some(idx),
            _ => {}
        }
    }
    let title = last_comma
        .map(|idx| rest[idx + 1..].trim())
        .unwrap_or_default();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// Canonicalise an HTTP(S) URL's scheme to lowercase, leaving the rest untouched.
fn normalize_playlist_url(line: &str) -> String {
    let bytes = line.as_bytes();
    if bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        format!("http://{}", &line[7..])
    } else if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        format!("https://{}", &line[8..])
    } else {
        line.to_string()
    }
}

fn station_from_row(row: &rusqlite::Row<'_>) -> Result<Station> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let bitrate: Option<i64> = row.get(12)?;
    let volume: Option<f64> = row.get(20)?;
    let gain_db: Option<f64> = row.get(22)?;
    Ok(Station {
        id,
        source_id: row.get(1)?,
        name: row.get(2)?,
        url: row.get(3)?,
        genre: row.get(4)?,
        country: row.get(5)?,
        country_state: row.get(6)?,
        logo: row.get(7)?,
        comment: row.get(8)?,
        website: row.get(9)?,
        city: row.get(10)?,
        tags: row.get(11)?,
        bitrate: bitrate.and_then(|b| u32::try_from(b).ok()),
        codec: row.get(13)?,
        stream_type: row.get(14)?,
        is_favorite: row.get::<_, Option<i64>>(15)?.unwrap_or(0) != 0,
        rating: row.get(16)?,
        play_count: row.get::<_, Option<i64>>(17)?.unwrap_or(0),
        date_added: row.get(18)?,
        date_last_played: row.get(19)?,
        volume: volume.unwrap_or(1.0) as f32,
        gain_db: gain_db.unwrap_or(0.0) as f32,
        external: row.get::<_, Option<i64>>(21)?.unwrap_or(0) != 0,
        broken: row.get::<_, Option<i64>>(23)?.unwrap_or(0) != 0,
        broken_at: row.get(24)?,
        failure_count: row
            .get::<_, Option<i64>>(25)?
            .unwrap_or(0)
            .clamp(0, i64::from(u32::MAX)) as u32,
        watched: row.get::<_, Option<i64>>(26)?.unwrap_or(0) != 0,
    })
}

fn stream_from_row(row: &rusqlite::Row<'_>) -> Result<Stream> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let station_id_str: String = row.get(1)?;
    let station_id = Uuid::parse_str(&station_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let bitrate: Option<i64> = row.get(5)?;
    Ok(Stream {
        id,
        station_id,
        url: row.get(2)?,
        listen_url: row.get(3)?,
        label: row.get(4)?,
        bitrate: bitrate.and_then(|b| u32::try_from(b).ok()),
        metadata: row.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
        stream_type: row.get(7)?,
        is_current: row.get::<_, Option<i64>>(8)?.unwrap_or(0) != 0,
        custom_tag: row.get(9)?,
        open_as_website: row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0,
        source_stream_id: row.get(11)?,
    })
}

fn collect_streams<I>(rows: I) -> Result<Vec<Stream>>
where
    I: Iterator<Item = Result<Stream>>,
{
    let mut streams = Vec::new();
    for row in rows {
        match row {
            Ok(stream) => streams.push(stream),
            Err(e) => log::warn!("skipping unreadable stream row: {e}"),
        }
    }
    Ok(streams)
}

fn play_instance_from_row(row: &rusqlite::Row<'_>) -> Result<PlayInstance> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let station_id_str: String = row.get(1)?;
    let station_id = Uuid::parse_str(&station_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let stream_id = match row.get::<_, Option<String>>(2)? {
        Some(s) => Uuid::parse_str(&s).ok(),
        None => None,
    };
    let started_at = parse_utc(row.get::<_, String>(3)?.as_str())?;
    let ended_at = match row.get::<_, Option<String>>(4)? {
        Some(s) => Some(parse_utc(&s)?),
        None => None,
    };
    Ok(PlayInstance {
        id,
        station_id,
        stream_id,
        started_at,
        ended_at,
        duration_seconds: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        status: row.get::<_, String>(6)?,
        end_reason: row.get(7)?,
        volume: row.get::<_, Option<f64>>(8)?.unwrap_or(1.0) as f32,
        recording: row.get::<_, Option<i64>>(9)?.unwrap_or(0) != 0,
        error: row.get(10)?,
    })
}

fn parse_utc(text: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn schedule_from_row(row: &rusqlite::Row<'_>) -> Result<Schedule> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let station_str: String = row.get(1)?;
    let station_id = Uuid::parse_str(&station_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let action_text: String = row.get(2)?;
    let action = ScheduleAction::parse(&action_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown schedule action {action_text:?}"),
            )),
        )
    })?;
    let recurrence_text: String = row.get(3)?;
    let recurrence = Recurrence::parse(&recurrence_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown recurrence {recurrence_text:?}"),
            )),
        )
    })?;
    let start_text: String = row.get(4)?;
    let start_at = parse_local(&start_text)?;
    let end_at = match row.get::<_, Option<String>>(5)? {
        Some(text) => Some(parse_local(&text)?),
        None => None,
    };
    let finish_action = FinishAction::parse(&row.get::<_, Option<String>>(6)?.unwrap_or_default());
    let enabled = row.get::<_, Option<i64>>(7)?.unwrap_or(1) != 0;
    let last_fired_at = match row.get::<_, Option<String>>(8)? {
        Some(text) => Some(parse_utc(&text)?),
        None => None,
    };
    Ok(Schedule {
        id,
        station_id,
        action,
        recurrence,
        start_at,
        end_at,
        finish_action,
        enabled,
        last_fired_at,
    })
}

/// Schedule times are stored as naive local text, with a tolerant reader for
/// rows written by hand or by an earlier build.
fn parse_local(text: &str) -> Result<NaiveDateTime> {
    for format in [
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(text, format) {
            return Ok(value);
        }
    }
    Err(rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid local time {text:?}"),
        )),
    ))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "viberadio-test-{}-{}.sqlite",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "viberadio-test-dir-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fresh_db(name: &str) -> (std::path::PathBuf, DbManager) {
        let path = temp_path(name);
        let db = DbManager::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();
        (path, db)
    }

    /// The real `Stations`/`Streams` layout observed in the shipped
    /// `StationRadios.sqlite`, populated with a small fixture.
    /// A v2 database (the previous build's shape) must migrate in place: rows
    /// kept, `description` folded into `comment`, new columns/tables available and
    /// the version recorded.
    #[test]
    fn migration_v2_to_latest_keeps_rows() {
        let path = temp_path("migrate");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE stations (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL,
                    genre TEXT, country TEXT, bitrate INTEGER, codec TEXT,
                    description TEXT, is_favorite BOOLEAN DEFAULT 0, source_id TEXT
                );
                CREATE TABLE history (
                    id TEXT PRIMARY KEY, station_id TEXT NOT NULL,
                    station_name TEXT NOT NULL, played_at TEXT NOT NULL,
                    duration_seconds INTEGER NOT NULL
                );
                CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO settings VALUES ('schema_version', '2');
                INSERT INTO stations VALUES
                    ('11111111-1111-1111-1111-111111111111', 'Old', 'http://x/y',
                     'Jazz', 'UK', 128, 'MP3', 'legacy comment', 1, 'orig-1');
                INSERT INTO stations VALUES
                    ('not-a-uuid', 'Corrupt', 'http://x/z',
                     NULL, NULL, NULL, NULL, NULL, 0, NULL);
                INSERT INTO history VALUES
                    ('22222222-2222-2222-2222-222222222222',
                     '11111111-1111-1111-1111-111111111111',
                     'Old', '2026-11-01T09:01:00+00:00', 12);",
            )
            .unwrap();
        }

        let db = DbManager::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        let stations = db.load_stations().unwrap();
        assert_eq!(
            stations.len(),
            1,
            "migration must not drop valid rows, and one corrupt row is skipped"
        );
        assert_eq!(stations[0].comment.as_deref(), Some("legacy comment"));
        assert_eq!(stations[0].play_count, 0);
        assert!((stations[0].volume - 1.0).abs() < f32::EPSILON);
        assert!(stations[0].is_favorite);

        let history = db.load_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].duration_seconds, 12);
        assert_eq!(history[0].status, PLAY_STATUS_ENDED);
        assert_eq!(history[0].play_instance_id, None);

        // The v3 tables exist and accept the new shapes.
        assert_eq!(db.load_all_streams().unwrap().len(), 0);
        let station = &db.load_stations().unwrap()[0];
        let (instance, entry) = db.begin_play(station, None, 1.0).unwrap();
        assert!(db.load_play_instance(instance.id).unwrap().is_some());
        assert!(entry.play_instance_id.is_some());

        assert_eq!(
            db.get_setting("schema_version").unwrap().as_deref(),
            Some("8")
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A v1 database (no `source_id`, no `settings` table at all) must also migrate:
    /// version detection falls back to 1 and every later column is added.
    #[test]
    fn migration_v1_to_latest_adds_source_id_and_version() {
        let path = temp_path("migrate-v1");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE stations (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL,
                    genre TEXT, country TEXT, bitrate INTEGER, codec TEXT,
                    description TEXT, is_favorite BOOLEAN DEFAULT 0
                );
                INSERT INTO stations VALUES
                    ('11111111-1111-1111-1111-111111111111', 'Very Old',
                     'http://x/old', NULL, NULL, NULL, NULL, NULL, 0);",
            )
            .unwrap();
        }

        let db = DbManager::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        assert!(db.has_column("stations", "source_id").unwrap());
        assert!(db.has_column("stations", "country_state").unwrap());
        assert!(db.has_column("history", "play_instance_id").unwrap());
        let stations = db.load_stations().unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].source_id, None);
        assert_eq!(
            db.get_setting("schema_version").unwrap().as_deref(),
            Some("8")
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A v3 database has the play-instances table without per-attempt state. The
    /// v4 migration must add it and keep existing rows.
    #[test]
    fn migration_v3_to_v4_adds_attempt_state() {
        let path = temp_path("migrate-v4");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO settings VALUES ('schema_version', '3');
                 CREATE TABLE play_instances (
                    id TEXT PRIMARY KEY, station_id TEXT NOT NULL, stream_id TEXT,
                    started_at TEXT NOT NULL, ended_at TEXT,
                    duration_seconds INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL DEFAULT 'playing', end_reason TEXT
                 );
                 INSERT INTO play_instances
                    (id, station_id, started_at, ended_at, duration_seconds, status)
                 VALUES ('11111111-1111-1111-1111-111111111111',
                         '22222222-2222-2222-2222-222222222222',
                         '2026-11-01T09:00:00+00:00', '2026-11-01T09:01:00+00:00',
                         60, 'ended');",
            )
            .unwrap();
        }

        let db = DbManager::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        assert!(db.has_column("play_instances", "volume").unwrap());
        assert!(db.has_column("play_instances", "recording").unwrap());
        assert!(db.has_column("play_instances", "error").unwrap());

        let id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let instance = db.load_play_instance(id).unwrap().unwrap();
        assert_eq!(instance.duration_seconds, 60);
        assert!((instance.volume - 1.0).abs() < f32::EPSILON);
        assert!(!instance.recording);
        assert!(instance.error.is_none());
        assert_eq!(
            db.get_setting("schema_version").unwrap().as_deref(),
            Some("8")
        );

        // The migrated table must accept the new writes, not only report columns.
        let station = Station::new("After migration", "http://example.invalid/mig");
        db.save_station(&station).unwrap();
        let (fresh, fresh_history) = db.begin_play(&station, None, 0.35).unwrap();
        let stored = db.load_play_instance(fresh.id).unwrap().unwrap();
        assert!((stored.volume - 0.35).abs() < f32::EPSILON);
        db.update_play_instance_volume(fresh.id, 0.15).unwrap();
        db.end_play(
            fresh.id,
            fresh_history.id,
            9,
            "error",
            Some("boom"),
            Some("boom"),
        )
        .unwrap();
        let closed = db.load_play_instance(fresh.id).unwrap().unwrap();
        assert!((closed.volume - 0.15).abs() < f32::EPSILON);
        assert_eq!(closed.error.as_deref(), Some("boom"));

        // `recording` is a M5 placeholder, but the column must round-trip now.
        db.conn
            .execute(
                "UPDATE play_instances SET recording = 1 WHERE id = ?1",
                params![fresh.id.to_string()],
            )
            .unwrap();
        assert!(db.load_play_instance(fresh.id).unwrap().unwrap().recording);

        let _ = std::fs::remove_file(&path);
    }

    /// A v4 database has the schedules table without `last_fired_at`. The v5
    /// migration must add it and keep the existing row.
    #[test]
    fn migration_v4_to_v5_adds_schedule_last_fired() {
        let path = temp_path("migrate-v5");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO settings VALUES ('schema_version', '4');
                 CREATE TABLE schedules (
                    id TEXT PRIMARY KEY, station_id TEXT NOT NULL, action TEXT NOT NULL,
                    recurrence TEXT NOT NULL, start_at TEXT, end_at TEXT,
                    finish_action TEXT, enabled INTEGER NOT NULL DEFAULT 1
                 );
                 INSERT INTO schedules
                    (id, station_id, action, recurrence, start_at, end_at, finish_action, enabled)
                 VALUES ('11111111-1111-1111-1111-111111111111',
                         '22222222-2222-2222-2222-222222222222',
                         'play', 'daily', '2026-11-09T07:00', '2026-11-09T08:00',
                         'run:notify-send done', 1);",
            )
            .unwrap();
        }

        let db = DbManager::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        assert!(db.has_column("schedules", "last_fired_at").unwrap());
        let schedules = db.load_schedules().unwrap();
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].recurrence, Recurrence::Daily);
        assert_eq!(schedules[0].action, ScheduleAction::Play);
        assert_eq!(
            schedules[0].end_at,
            Some(
                chrono::NaiveDate::from_ymd_opt(2026, 11, 9)
                    .unwrap()
                    .and_hms_opt(8, 0, 0)
                    .unwrap()
            )
        );
        assert_eq!(
            schedules[0].finish_action,
            FinishAction::RunCommand("notify-send done".to_string())
        );
        assert!(schedules[0].last_fired_at.is_none());
        assert_eq!(
            db.get_setting("schema_version").unwrap().as_deref(),
            Some("8")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_v5_to_v6_adds_station_gain() {
        let (path, db) = fresh_db("migrate-v6");
        db.conn
            .execute("ALTER TABLE stations DROP COLUMN gain_db", [])
            .unwrap();
        db.set_setting("schema_version", "5").unwrap();
        db.conn
            .execute(
                "INSERT INTO stations (id, name, url)
                 VALUES ('11111111-1111-1111-1111-111111111111', 'Old FM', 'http://x/old')",
                [],
            )
            .unwrap();
        drop(db);

        let db = DbManager::new(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        assert!(db.has_column("stations", "gain_db").unwrap());
        let stations = db.load_stations().unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "Old FM");
        assert_eq!(stations[0].gain_db, 0.0);
        assert_eq!(
            db.get_setting("schema_version").unwrap().as_deref(),
            Some("8")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn station_gain_round_trips_and_updates_in_place() {
        let (path, db) = fresh_db("station-gain");
        let mut station = Station::new("Gain FM", "http://example.invalid/gain");
        station.gain_db = 6.5;
        db.save_station(&station).unwrap();
        assert_eq!(db.load_stations().unwrap()[0].gain_db, 6.5);

        db.set_station_gain(station.id, -3.5).unwrap();
        assert_eq!(db.load_stations().unwrap()[0].gain_db, -3.5);

        db.set_station_gain(station.id, 99.0).unwrap();
        assert_eq!(db.load_stations().unwrap()[0].gain_db, 12.0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backup_to_creates_a_valid_copy() {
        let (path, db) = fresh_db("backup-source");
        let station = Station::new("Backup FM", "http://example.invalid/backup");
        db.save_station(&station).unwrap();
        let backup = temp_path("backup-copy");
        db.backup_to(&backup).unwrap();
        assert_eq!(DbManager::validate_backup(&backup).unwrap(), SCHEMA_VERSION);
        let copy = DbManager::new(backup.to_str().unwrap()).unwrap();
        let stations = copy.load_stations().unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "Backup FM");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn validate_backup_rejects_a_non_database() {
        let path = temp_path("bad-backup");
        std::fs::write(&path, b"not a database").unwrap();
        assert!(DbManager::validate_backup(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pending_restore_replaces_the_live_database() {
        let dir = temp_dir("pending-restore");
        let live_path = dir.join("viberadio.db");
        let live = DbManager::new(live_path.to_str().unwrap()).unwrap();
        live.init_schema().unwrap();
        live.save_station(&Station::new("Old FM", "http://x/old"))
            .unwrap();
        drop(live);

        let backup_path = dir.join("viberadio-backup-20260101-000000.db");
        let backup = DbManager::new(backup_path.to_str().unwrap()).unwrap();
        backup.init_schema().unwrap();
        backup
            .save_station(&Station::new("New FM", "http://x/new"))
            .unwrap();
        drop(backup);

        std::fs::write(
            dir.join("viberadio-restore.pending"),
            "viberadio-backup-20260101-000000.db",
        )
        .unwrap();
        let message = DbManager::apply_pending_restore(&dir).expect("restore result");
        assert!(message.contains("Restored"), "{message}");
        assert!(!dir.join("viberadio-restore.pending").exists());

        let restored = DbManager::new(live_path.to_str().unwrap()).unwrap();
        let stations = restored.load_stations().unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "New FM");
        let before = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("viberadio-before-restore-")
            });
        assert!(
            before,
            "the live database must be preserved before the swap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_backups_keeps_the_newest() {
        let dir = temp_dir("prune-backups");
        for index in 0..12 {
            std::fs::write(
                dir.join(format!("viberadio-backup-202601{index:02}-000000.db")),
                b"x",
            )
            .unwrap();
        }
        DbManager::prune_backups(&dir, 10);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names.len(), 10);
        assert!(!names.contains(&"viberadio-backup-20260100-000000.db".to_string()));
        assert!(!names.contains(&"viberadio-backup-20260101-000000.db".to_string()));
        assert!(names.contains(&"viberadio-backup-20260111-000000.db".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_copy_copies_the_library() {
        let default = temp_dir("copy-default");
        let source_dir = temp_dir("copy-source");
        let source_db = source_dir.join("viberadio.db");
        let source = DbManager::new(source_db.to_str().unwrap()).unwrap();
        source.init_schema().unwrap();
        source
            .save_station(&Station::new("Copied FM", "http://x/copied"))
            .unwrap();
        drop(source);

        let target_dir = temp_dir("copy-target");
        let target_db = target_dir.join("viberadio.db");
        let marker = default.join("viberadio-copy.pending");
        std::fs::write(
            &marker,
            serde_json::json!({
                "source": source_db.display().to_string(),
                "target": target_db.display().to_string(),
            })
            .to_string(),
        )
        .unwrap();

        let message = DbManager::apply_pending_copy(&default).expect("copy result");
        assert!(message.contains("copied"), "{message}");
        assert!(!marker.exists());
        let target = DbManager::new(target_db.to_str().unwrap()).unwrap();
        let stations = target.load_stations().unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "Copied FM");

        let _ = std::fs::remove_dir_all(&default);
        let _ = std::fs::remove_dir_all(&source_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
    }

    #[test]
    fn pending_copy_failure_restores_the_pointer() {
        let default = temp_dir("copy-fail-default");
        let source_dir = temp_dir("copy-fail-source");
        let source_db = source_dir.join("viberadio.db");
        let source = DbManager::new(source_db.to_str().unwrap()).unwrap();
        source.init_schema().unwrap();
        source
            .save_station(&Station::new("Source FM", "http://x/source"))
            .unwrap();
        drop(source);

        let target_dir = temp_dir("copy-fail-target");
        // The parent of the target database does not exist, so VACUUM INTO fails.
        let target_db = target_dir.join("missing").join("viberadio.db");
        let marker = default.join("viberadio-copy.pending");
        std::fs::write(
            &marker,
            serde_json::json!({
                "source": source_db.display().to_string(),
                "target": target_db.display().to_string(),
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(default.join("location"), target_dir.display().to_string()).unwrap();

        let message = DbManager::apply_pending_copy(&default).expect("copy result");
        assert!(message.contains("failed"), "{message}");
        assert!(!marker.exists());
        let pointer = std::fs::read_to_string(default.join("location")).unwrap();
        assert_eq!(pointer.trim(), source_dir.display().to_string());

        let _ = std::fs::remove_dir_all(&default);
        let _ = std::fs::remove_dir_all(&source_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
    }

    #[test]
    fn latest_backup_picks_the_newest() {
        let dir = temp_dir("latest-backup");
        for name in [
            "viberadio-backup-20260101-000000.db",
            "viberadio-backup-20260202-000000.db",
            "other.db",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        let latest = DbManager::latest_backup(&dir).expect("latest backup");
        assert_eq!(
            latest.file_name().unwrap().to_string_lossy(),
            "viberadio-backup-20260202-000000.db"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schedules_round_trip_and_mark_fired() {
        let (path, db) = fresh_db("schedules");
        let station = Station::new("Jazz FM", "http://example.invalid/jazz");
        db.save_station(&station).unwrap();

        let mut schedule = Schedule::new(
            station.id,
            chrono::NaiveDate::from_ymd_opt(2026, 11, 8)
                .unwrap()
                .and_hms_opt(6, 0, 0)
                .unwrap(),
        );
        schedule.action = ScheduleAction::PlayRecordSplit;
        schedule.recurrence = Recurrence::Weekly(vec![chrono::Weekday::Mon, chrono::Weekday::Fri]);
        schedule.finish_action = FinishAction::RunCommand("notify-send done".to_string());
        schedule.enabled = false;
        db.save_schedule(&schedule).unwrap();

        let loaded = db.load_schedules().unwrap();
        assert_eq!(loaded.len(), 1);
        let stored = &loaded[0];
        assert_eq!(stored.id, schedule.id);
        assert_eq!(stored.station_id, station.id);
        assert_eq!(stored.action, ScheduleAction::PlayRecordSplit);
        assert_eq!(stored.recurrence, schedule.recurrence);
        assert_eq!(stored.start_at, schedule.start_at);
        assert_eq!(stored.end_at, schedule.end_at);
        assert_eq!(stored.finish_action, schedule.finish_action);
        assert!(!stored.enabled);
        assert!(stored.last_fired_at.is_none());

        let fired_at = Utc.with_ymd_and_hms(2026, 11, 8, 7, 0, 0).single().unwrap();
        db.mark_schedule_fired(schedule.id, fired_at).unwrap();
        let loaded = db.load_schedules().unwrap();
        assert_eq!(loaded[0].last_fired_at, Some(fired_at));

        db.delete_schedule(schedule.id).unwrap();
        assert!(db.load_schedules().unwrap().is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_or_unwritable_history_export_is_an_error() {
        let (path, db) = fresh_db("history-html-empty");
        let out = temp_path("history-html-empty-out").with_extension("html");
        assert_eq!(db.export_history_html(&out).unwrap(), 0);
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.contains("<tbody>\n</tbody>"));
        let _ = std::fs::remove_file(&out);

        let bad = std::path::Path::new("/definitely/not/a/directory/history.html");
        assert!(db.export_history_html(bad).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn history_exports_as_html() {
        let (path, db) = fresh_db("history-html");
        let station = Station::new("Track <FM>", "http://example.invalid/live");
        db.save_station(&station).unwrap();
        let (instance, attempt) = db.begin_play(&station, None, 1.0).unwrap();
        let track = db
            .start_track(
                instance.id,
                station.id,
                &station.name,
                Some(&station.url),
                Some("Artist & Co"),
                Some("Song <One>"),
            )
            .unwrap();
        db.end_track(track.id, 42, "ended").unwrap();

        let out = temp_path("history-html-out").with_extension("html");
        assert_eq!(db.export_history_html(&out).unwrap(), 2);
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.contains("Track &lt;FM&gt;"));
        assert!(html.contains("Artist &amp; Co"));
        assert!(html.contains("Song &lt;One&gt;"));
        assert!(html.contains("42s"));
        assert!(html.contains(&attempt.played_at.to_rfc3339()));
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sessions_save_replace_and_load() {
        let (path, db) = fresh_db("sessions");
        let one = Station::new("One", "http://example.invalid/1");
        let two = Station::new("Two", "http://example.invalid/2");
        db.save_station(&one).unwrap();
        db.save_station(&two).unwrap();
        let id = db.save_session("Morning", &[one.id, two.id]).unwrap();
        let sessions = db.load_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].station_count, 2);
        assert_eq!(db.load_session_stations(id).unwrap().len(), 2);

        // Saving the same name replaces the station list rather than duplicating.
        let same = db.save_session("Morning", &[two.id]).unwrap();
        assert_eq!(same, id);
        let sessions = db.load_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].station_count, 1);
        assert_eq!(db.load_session_stations(id).unwrap(), vec![two.id]);

        db.delete_session(id).unwrap();
        assert!(db.load_sessions().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wishlist_entries_keep_action_and_compare_mode() {
        let (path, db) = fresh_db("wishlist");
        db.conn
            .execute(
                "INSERT INTO wishlist (artist, song_title, action, compare) VALUES
                    ('Miles Davis', 'Blue in Green', 'record', 'exact'),
                    (NULL, 'Jingle', 'notify', 'starts_with'),
                    (NULL, 'Ad', '', '')",
                [],
            )
            .unwrap();
        let list = db.load_wishlist().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].action, "record");
        assert_eq!(list[0].compare, CompareMode::Exact);
        assert_eq!(list[1].compare, CompareMode::StartsWith);
        assert_eq!(list[2].action, "record");
        assert_eq!(list[2].compare, CompareMode::Contains);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_open_track_accepts_corrected_metadata() {
        let (path, db) = fresh_db("track-retitle");
        let station = Station::new("Retitle FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();
        let (instance, _) = db.begin_play(&station, None, 1.0).unwrap();
        let track = db
            .start_track(
                instance.id,
                station.id,
                &station.name,
                Some(&station.url),
                Some("DJ PinkGesia"),
                Some("CeCe Peniston - Finally"),
            )
            .unwrap();
        db.update_track_metadata(track.id, "CeCe Peniston", "Finally")
            .unwrap();
        let tracks = db.load_tracks_for_instance(instance.id).unwrap();
        let updated = tracks.iter().find(|row| row.id == track.id).unwrap();
        assert_eq!(updated.artist.as_deref(), Some("CeCe Peniston"));
        assert_eq!(updated.song_title.as_deref(), Some("Finally"));

        // A closed row keeps the metadata it was closed with.
        db.end_track(track.id, 200, "ended").unwrap();
        db.update_track_metadata(track.id, "Ignored", "Ignored")
            .unwrap();
        let tracks = db.load_tracks_for_instance(instance.id).unwrap();
        let closed = tracks.iter().find(|row| row.id == track.id).unwrap();
        assert_eq!(closed.artist.as_deref(), Some("CeCe Peniston"));
        assert_eq!(closed.song_title.as_deref(), Some("Finally"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_station_is_hidden_after_two_failures_and_clears_on_success() {
        let (path, db) = fresh_db("station-broken");
        let station = Station::new("Broken FM", "http://example.invalid/broken");
        db.save_station(&station).unwrap();
        assert_eq!(db.record_station_failure(station.id).unwrap(), (1, false));
        assert_eq!(db.record_station_failure(station.id).unwrap(), (2, true));
        let loaded = db
            .load_stations()
            .unwrap()
            .into_iter()
            .find(|row| row.id == station.id)
            .unwrap();
        assert!(loaded.broken);
        assert_eq!(loaded.failure_count, 2);
        assert!(loaded.broken_at.is_some());

        db.clear_station_failure(station.id).unwrap();
        let loaded = db
            .load_stations()
            .unwrap()
            .into_iter()
            .find(|row| row.id == station.id)
            .unwrap();
        assert!(!loaded.broken);
        assert_eq!(loaded.failure_count, 0);
        assert!(loaded.broken_at.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reported_bitrate_fills_only_an_empty_station() {
        let (path, db) = fresh_db("station-bitrate");
        let station = Station::new("Radio Net FM", "http://example.invalid/rn");
        db.save_station(&station).unwrap();
        db.save_stream(&Stream::new(station.id, station.url.clone()))
            .unwrap();

        db.set_station_bitrate(station.id, 128).unwrap();
        let loaded = db
            .load_stations()
            .unwrap()
            .into_iter()
            .find(|row| row.id == station.id)
            .unwrap();
        assert_eq!(loaded.bitrate, Some(128));
        assert_eq!(
            db.load_streams_for_station(station.id).unwrap()[0].bitrate,
            Some(128)
        );

        // A later stream report must not overwrite a known bitrate.
        db.set_station_bitrate(station.id, 320).unwrap();
        let loaded = db
            .load_stations()
            .unwrap()
            .into_iter()
            .find(|row| row.id == station.id)
            .unwrap();
        assert_eq!(loaded.bitrate, Some(128));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleting_stations_removes_their_rows_but_keeps_history() {
        let (path, db) = fresh_db("station-delete");
        let station = Station::new("Delete FM", "http://example.invalid/delete");
        db.save_station(&station).unwrap();
        db.save_stream(&Stream::new(station.id, station.url.clone()))
            .unwrap();
        let (instance, _) = db.begin_play(&station, None, 1.0).unwrap();
        db.start_track(
            instance.id,
            station.id,
            &station.name,
            Some(&station.url),
            Some("Artist"),
            Some("Song"),
        )
        .unwrap();

        assert_eq!(db.delete_stations(&[station.id]).unwrap(), 1);
        assert!(db.load_stations().unwrap().is_empty());
        assert!(db.load_streams_for_station(station.id).unwrap().is_empty());
        assert!(!db.load_history().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_rows_link_to_attempt_and_close_once() {
        let (path, db) = fresh_db("track-history");
        let station = Station::new("Track FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();
        let (instance, station_row) = db.begin_play(&station, None, 1.0).unwrap();

        db.set_play_instance_recording(instance.id, true).unwrap();
        assert!(
            db.load_play_instance(instance.id)
                .unwrap()
                .unwrap()
                .recording
        );
        db.set_play_instance_recording(instance.id, false).unwrap();
        assert!(
            !db.load_play_instance(instance.id)
                .unwrap()
                .unwrap()
                .recording
        );

        let first = db
            .start_track(
                instance.id,
                station.id,
                &station.name,
                Some(&station.url),
                Some("Artist One"),
                Some("Song One"),
            )
            .unwrap();
        let second = db
            .start_track(
                instance.id,
                station.id,
                &station.name,
                Some(&station.url),
                Some("Artist Two"),
                Some("Song Two"),
            )
            .unwrap();
        assert_eq!(first.play_instance_id, Some(instance.id));
        assert_eq!(first.artist.as_deref(), Some("Artist One"));
        assert_eq!(first.song_title.as_deref(), Some("Song One"));

        db.end_track(first.id, 12, "ended").unwrap();
        db.end_track(first.id, 99, "error").unwrap();
        let tracks = db.load_tracks_for_instance(instance.id).unwrap();
        assert_eq!(tracks.len(), 2);
        let closed = tracks.iter().find(|track| track.id == first.id).unwrap();
        assert_eq!(closed.duration_seconds, 12);
        assert_eq!(closed.status, "ended");
        assert_eq!(
            tracks
                .iter()
                .find(|track| track.id == second.id)
                .unwrap()
                .status,
            "playing"
        );

        // The station-attempt row is still present and separate from track rows.
        let all = db.load_history().unwrap();
        assert!(all.iter().any(|row| row.id == station_row.id));
        assert_eq!(all.len(), 3);
        let attempts: Vec<_> = all
            .iter()
            .filter(|row| row.artist.is_none() && row.song_title.is_none())
            .collect();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, station_row.id);

        // Abandoning the attempt must also close its still-open track.
        db.abandon_play_instance(instance.id, station_row.id)
            .unwrap();
        let tracks = db.load_tracks_for_instance(instance.id).unwrap();
        assert_eq!(
            tracks
                .iter()
                .find(|track| track.id == second.id)
                .unwrap()
                .status,
            "error"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// FR-D6: each play attempt gets its own row and duration, and history points
    /// at the attempt.
    #[test]
    fn play_instances_keep_independent_history() {
        let (path, db) = fresh_db("play-instances");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();
        let stream = db.ensure_stream_for_station(&station).unwrap();

        let (first, first_history) = db.begin_play(&station, Some(&stream), 0.8).unwrap();
        let (second, second_history) = db.begin_play(&station, Some(&stream), 0.8).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first_history.play_instance_id, Some(first.id));
        assert_eq!(second_history.play_instance_id, Some(second.id));
        assert_eq!(first.stream_id, Some(stream.id));
        assert!((first.volume - 0.8).abs() < f32::EPSILON);
        assert!(!first.recording);
        assert!(first.error.is_none());
        assert_eq!(
            first_history.url.as_deref(),
            Some(stream.url.as_str()),
            "history must record the stream URL, not only the station default"
        );

        db.end_play(
            first.id,
            first_history.id,
            42,
            "ended",
            Some("stopped"),
            None,
        )
        .unwrap();

        // Closing twice must not overwrite the real duration/status.
        db.end_play(
            first.id,
            first_history.id,
            999,
            "error",
            Some("again"),
            None,
        )
        .unwrap();
        let loaded = db.load_play_instance(first.id).unwrap().unwrap();
        assert_eq!(loaded.duration_seconds, 42);
        assert_eq!(loaded.status, "ended");
        assert_eq!(loaded.end_reason.as_deref(), Some("stopped"));
        assert!(loaded.ended_at.is_some());
        assert!(db
            .load_play_instance(second.id)
            .unwrap()
            .unwrap()
            .ended_at
            .is_none());

        let history = db.load_history().unwrap();
        assert_eq!(history.len(), 2);
        let closed = history
            .iter()
            .find(|h| h.play_instance_id == Some(first.id))
            .unwrap();
        assert_eq!(closed.duration_seconds, 42);
        assert_eq!(closed.status, "ended");

        // play_count is bumped by each attempt.
        let reloaded = db
            .load_stations()
            .unwrap()
            .into_iter()
            .find(|s| s.id == station.id)
            .unwrap();
        assert_eq!(reloaded.play_count, 2);
        assert!(reloaded.date_last_played.is_some());

        let _ = std::fs::remove_file(&path);
    }

    /// FR-D1: history is stored as UTC RFC3339, so text ordering is monotonic
    /// across the US DST fall-back (2026-11-01 09:00 UTC).
    #[test]
    fn history_orders_correctly_across_dst() {
        let (path, db) = fresh_db("dst");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();

        for played_at in [
            Utc.with_ymd_and_hms(2026, 11, 1, 8, 59, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 1, 9, 1, 0).unwrap(),
        ] {
            let (_, entry) = db.begin_play(&station, None, 1.0).unwrap();

            // The *production* timestamp (before the test rewrites it) must be UTC.
            let stored: String = db
                .conn
                .query_row(
                    "SELECT played_at FROM history WHERE id = ?1",
                    params![entry.id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                stored.ends_with("+00:00"),
                "begin_play must store UTC RFC3339: {stored}"
            );

            // Rewrite this row's timestamp to the fixed instant under test.
            db.conn
                .execute(
                    "UPDATE history SET played_at = ?2 WHERE id = ?1",
                    params![entry.id.to_string(), played_at.to_rfc3339()],
                )
                .unwrap();
        }

        let history = db.load_history().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].played_at,
            Utc.with_ymd_and_hms(2026, 11, 1, 9, 1, 0).unwrap()
        );
        assert_eq!(
            history[1].played_at,
            Utc.with_ymd_and_hms(2026, 11, 1, 8, 59, 0).unwrap()
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Attempts left open by a crash are swept on the next start; an already
    /// closed attempt is left alone.
    #[test]
    fn orphan_sweep_closes_only_open_attempts() {
        let (path, db) = fresh_db("orphans");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();

        let (open, open_history) = db.begin_play(&station, None, 1.0).unwrap();
        let (closed, closed_history) = db.begin_play(&station, None, 1.0).unwrap();
        db.end_play(
            closed.id,
            closed_history.id,
            10,
            "ended",
            Some("stopped"),
            None,
        )
        .unwrap();

        assert_eq!(db.close_orphaned_play_instances().unwrap(), 1);
        let swept = db.load_play_instance(open.id).unwrap().unwrap();
        assert_eq!(swept.status, PLAY_STATUS_ERROR);
        assert!(swept.end_reason.is_some());
        assert!(
            swept.ended_at.is_some(),
            "a swept attempt must not still look open"
        );

        let history = db.load_history().unwrap();
        let swept_history = history.iter().find(|h| h.id == open_history.id).unwrap();
        assert_eq!(swept_history.status, PLAY_STATUS_ERROR);
        let closed_history_row = history.iter().find(|h| h.id == closed_history.id).unwrap();
        assert_eq!(closed_history_row.status, "ended");

        // A second start has nothing left to sweep.
        assert_eq!(db.close_orphaned_play_instances().unwrap(), 0);

        let _ = std::fs::remove_file(&path);
    }

    /// A fresh heartbeat means another instance owns the library, so the startup
    /// sweep must not touch its live attempts.
    #[test]
    fn heartbeat_guards_the_orphan_sweep() {
        let (path, db) = fresh_db("heartbeat");
        assert!(db.should_sweep_orphans(30).unwrap());

        db.write_heartbeat().unwrap();
        assert!(!db.should_sweep_orphans(30).unwrap());

        let stale = (Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        db.set_setting("heartbeat_at", &stale).unwrap();
        assert!(db.should_sweep_orphans(30).unwrap());

        db.clear_heartbeat().unwrap();
        assert!(db.should_sweep_orphans(30).unwrap());

        let _ = std::fs::remove_file(&path);
    }

    /// `abandon_play_instance` closes an attempt that could not be closed normally,
    /// and a later `end_play` cannot overwrite that record.
    #[test]
    fn abandon_closes_an_attempt_once() {
        let (path, db) = fresh_db("abandon");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();

        let (instance, history) = db.begin_play(&station, None, 1.0).unwrap();
        db.abandon_play_instance(instance.id, history.id).unwrap();

        let abandoned = db.load_play_instance(instance.id).unwrap().unwrap();
        assert_eq!(abandoned.status, PLAY_STATUS_ERROR);
        assert!(abandoned.ended_at.is_some());

        // A late close must not rewrite the record.
        db.end_play(instance.id, history.id, 500, "ended", Some("late"), None)
            .unwrap();
        let still = db.load_play_instance(instance.id).unwrap().unwrap();
        assert_eq!(still.status, PLAY_STATUS_ERROR);
        assert_eq!(still.duration_seconds, 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Saving a stream under a new id but an existing `(station_id, url)` key must
    /// update the row in place instead of violating the unique key.
    #[test]
    fn stream_upsert_matches_on_station_and_url() {
        let (path, db) = fresh_db("stream-upsert");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();

        let first = Stream::new(station.id, "http://example.invalid/live");
        db.save_stream(&first).unwrap();

        let mut second = Stream::new(station.id, "http://example.invalid/live");
        second.bitrate = Some(320);
        db.save_stream(&second).unwrap();

        let streams = db.load_streams_for_station(station.id).unwrap();
        assert_eq!(streams.len(), 1, "same station+URL must stay one row");
        assert_eq!(streams[0].id, first.id, "the existing row id is kept");
        assert_eq!(streams[0].bitrate, Some(320));

        // Reusing an existing id with a different URL must not violate the unique
        // (station_id, url) key: identity stays with the existing row.
        let mut renamed = first.clone();
        renamed.url = "http://example.invalid/other".to_string();
        db.save_stream(&renamed).unwrap();
        let streams = db.load_streams_for_station(station.id).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].url, "http://example.invalid/live");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn m3u_import_handles_attributes_bom_and_duplicates() {
        let (path, db) = fresh_db("m3u-edge");
        let playlist = temp_path("playlist-edge").with_extension("m3u");
        std::fs::write(
            &playlist,
            concat!(
                "\u{feff}#EXTM3U\n",
                "#EXTINF:-1 tvg-name=\"Jazz, Smooth\",Smooth Jazz FM\n",
                "HTTP://example.invalid/a.mp3\n",
                "this is not a playlist line\n",
                "#EXTINF:-1,Leaked Title\n",
                "# a comment\n",
                "http://example.invalid/a.mp3\n",
                "#EXTINF:-1 tvg-name=\"x,y\",Rock Radio\n",
                "https://example.invalid/b.ogg\n",
            ),
        )
        .unwrap();

        let imported = db.import_m3u(&playlist).unwrap();
        assert_eq!(imported, 2, "duplicate URL and junk lines must be skipped");

        let stations = db.load_stations().unwrap();
        let names: Vec<&str> = stations.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Smooth Jazz FM"), "names: {names:?}");
        assert!(names.contains(&"Rock Radio"), "names: {names:?}");
        assert!(
            !names.contains(&"Leaked Title"),
            "a title must not leak past a skipped line"
        );
        let smooth = stations
            .iter()
            .find(|s| s.name == "Smooth Jazz FM")
            .unwrap();
        assert_eq!(
            smooth.url, "http://example.invalid/a.mp3",
            "the scheme must be canonicalised"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&playlist);
    }

    #[test]
    fn seeds_match_m3u_ids_and_skip_existing_and_non_http() {
        let (path, db) = fresh_db("seeds-parity");
        let playlist = path.with_extension("m3u");
        std::fs::write(
            &playlist,
            "#EXTM3U\n#EXTINF:-1,One\nhttps://a.example/one\n",
        )
        .unwrap();
        assert_eq!(db.import_m3u(&playlist).unwrap(), 1);

        let seed = |name: &str, url: &str| StationSeed {
            source_id: None,
            name: name.into(),
            url: url.into(),
            genre: None,
            country: None,
            city: None,
            logo: None,
            website: None,
            tags: None,
            codec: None,
            bitrate: None,
        };
        let seeds = vec![
            // Same URL with an upper-case scheme: the M3U id must be reused.
            seed("One again", "HTTPS://a.example/one"),
            seed("FTP", "ftp://a.example/x"),
            seed("No host", "http://"),
            seed("Two", "https://b.example/two"),
        ];
        assert_eq!(db.import_seeds(&seeds).unwrap(), 1);
        assert_eq!(db.load_stations().unwrap().len(), 2);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(playlist);
    }

    #[test]
    fn station_seeds_import_with_metadata_and_are_idempotent() {
        let (path, db) = fresh_db("seeds");
        let seeds = vec![
            StationSeed {
                source_id: Some("rn-one".into()),
                name: "One".into(),
                url: "https://a.example/one".into(),
                genre: Some("Jazz".into()),
                country: Some("USA".into()),
                city: Some("Richland".into()),
                logo: Some("https://img.example/one.png".into()),
                website: None,
                tags: Some("Talk".into()),
                codec: Some("MP3".into()),
                bitrate: Some(192),
            },
            StationSeed {
                source_id: None,
                name: "Duplicate".into(),
                url: "https://a.example/one".into(),
                genre: None,
                country: None,
                city: None,
                logo: None,
                website: None,
                tags: None,
                codec: None,
                bitrate: None,
            },
            StationSeed {
                source_id: None,
                name: "Two".into(),
                url: "http://b.example/two".into(),
                genre: None,
                country: None,
                city: None,
                logo: None,
                website: None,
                tags: None,
                codec: None,
                bitrate: None,
            },
        ];
        assert_eq!(db.import_seeds(&seeds).unwrap(), 2);
        let stations = db.load_stations().unwrap();
        let one = stations
            .iter()
            .find(|station| station.name == "One")
            .unwrap();
        assert_eq!(one.genre.as_deref(), Some("Jazz"));
        assert_eq!(one.country.as_deref(), Some("USA"));
        assert_eq!(one.city.as_deref(), Some("Richland"));
        assert_eq!(one.logo.as_deref(), Some("https://img.example/one.png"));
        assert_eq!(one.tags.as_deref(), Some("Talk"));
        assert_eq!(one.codec.as_deref(), Some("MP3"));
        assert_eq!(one.bitrate, Some(192));
        assert_eq!(one.source_id.as_deref(), Some("rn-one"));
        assert_eq!(db.import_seeds(&seeds).unwrap(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn seed_import_backfills_source_id() {
        let (path, db) = fresh_db("seed-backfill");
        let seed = |source_id: Option<&str>| StationSeed {
            source_id: source_id.map(str::to_string),
            name: "One".into(),
            url: "https://a.example/one".into(),
            genre: None,
            country: None,
            city: None,
            logo: None,
            website: None,
            tags: None,
            codec: None,
            bitrate: None,
        };
        assert_eq!(db.import_seeds(&[seed(None)]).unwrap(), 1);
        assert_eq!(db.import_seeds(&[seed(Some("rn-one"))]).unwrap(), 0);
        let stations = db.load_stations().unwrap();
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].source_id.as_deref(), Some("rn-one"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wishlist_entries_can_be_added_and_deleted() {
        let (path, db) = fresh_db("wishlist-add");
        let entry = WishEntry {
            id: 0,
            artist: Some("Artist".into()),
            song_title: Some("Song".into()),
            action: "record".into(),
            compare: CompareMode::Contains,
        };
        let id = db.add_wishlist_entry(&entry).unwrap();
        let list = db.load_wishlist().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].artist.as_deref(), Some("Artist"));
        assert_eq!(list[0].song_title.as_deref(), Some("Song"));
        db.delete_wishlist_entry(id).unwrap();
        assert!(db.load_wishlist().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn m3u_export_round_trips() {
        let (source_path, source) = fresh_db("m3u-source");
        let mut a = Station::new("Alpha Radio", "http://example.invalid/alpha.mp3");
        a.genre = Some("Pop".into());
        let b = Station::new("Beta FM", "http://example.invalid/beta.ogg");
        source.save_station(&a).unwrap();
        source.save_station(&b).unwrap();

        let playlist = temp_path("playlist").with_extension("m3u");
        let written = source.export_m3u(&playlist).unwrap();
        assert_eq!(written, 2);

        let (target_path, target) = fresh_db("m3u-target");
        let imported = target.import_m3u(&playlist).unwrap();
        assert_eq!(imported, 2);
        let mut names: Vec<String> = target
            .load_stations()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Alpha Radio".to_string(), "Beta FM".to_string()]
        );
        let urls: Vec<String> = target
            .load_stations()
            .unwrap()
            .into_iter()
            .map(|s| s.url)
            .collect();
        assert!(urls.contains(&"http://example.invalid/alpha.mp3".to_string()));
        assert!(urls.contains(&"http://example.invalid/beta.ogg".to_string()));

        // Re-importing the same playlist is a no-op.
        assert_eq!(target.import_m3u(&playlist).unwrap(), 0);
        assert_eq!(target.load_stations().unwrap().len(), 2);

        for p in [&source_path, &target_path, &playlist] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// An attempt closed with an error keeps both the error text and the live
    /// volume that was in effect.
    #[test]
    fn end_play_records_error_and_live_volume() {
        let (path, db) = fresh_db("attempt-error");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();

        let (instance, history) = db.begin_play(&station, None, 0.4).unwrap();
        db.update_play_instance_volume(instance.id, 0.2).unwrap();
        db.end_play(
            instance.id,
            history.id,
            5,
            "error",
            Some("stream stalled"),
            Some("stream stalled: no data for 30s"),
        )
        .unwrap();

        let closed = db.load_play_instance(instance.id).unwrap().unwrap();
        assert_eq!(closed.status, "error");
        assert_eq!(
            closed.error.as_deref(),
            Some("stream stalled: no data for 30s")
        );
        assert!((closed.volume - 0.2).abs() < f32::EPSILON);
        assert!(closed.ended_at.is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn station_roundtrip() {
        let (path, db) = fresh_db("roundtrip");
        let mut station = Station::new("Test", "http://example.invalid/stream");
        station.genre = Some("Pop".into());
        station.country = Some("UK".into());
        station.country_state = Some("England".into());
        station.bitrate = Some(192);
        station.codec = Some("MP3".into());
        station.rating = Some(5);
        station.volume = 0.5;
        station.play_count = 9;
        db.save_station(&station).unwrap();

        // A volume-only UPDATE must not disturb the counters begin_play owns.
        db.set_station_volume(station.id, 0.2).unwrap();
        let loaded = db.load_stations().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, station.id);
        assert_eq!(loaded[0].url, station.url);
        assert_eq!(loaded[0].country_state.as_deref(), Some("England"));
        assert_eq!(loaded[0].rating, Some(5));
        assert!((loaded[0].volume - 0.2).abs() < f32::EPSILON);
        assert_eq!(loaded[0].play_count, 9);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn history_roundtrip() {
        let (path, db) = fresh_db("history");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();
        let (_, entry) = db.begin_play(&station, None, 1.0).unwrap();

        let loaded = db.load_history().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].station_name, "Test FM");
        assert_eq!(loaded[0].id, entry.id);
        assert_eq!(loaded[0].play_instance_id, entry.play_instance_id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn recent_history_is_limited() {
        let (path, db) = fresh_db("recent-history");
        let station = Station::new("Test FM", "http://example.invalid/live");
        db.save_station(&station).unwrap();
        let (instance, _) = db.begin_play(&station, None, 1.0).unwrap();
        for (artist, title) in [("A", "One"), ("B", "Two")] {
            db.start_track(
                instance.id,
                station.id,
                &station.name,
                Some(&station.url),
                Some(artist),
                Some(title),
            )
            .unwrap();
        }

        assert_eq!(db.load_recent_history(2).unwrap().len(), 2);
        assert_eq!(db.load_recent_history(1).unwrap().len(), 1);
        assert_eq!(db.load_history().unwrap().len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn custom_tag_formats_round_trip_and_clear() {
        let (path, db) = fresh_db("custom-tags");
        let station = Uuid::new_v4();
        assert_eq!(db.custom_tag_format(station).unwrap(), None);

        db.set_custom_tag_format(station, Some("<Artist> - <Song>"))
            .unwrap();
        assert_eq!(
            db.custom_tag_format(station).unwrap().as_deref(),
            Some("<Artist> - <Song>")
        );

        db.set_custom_tag_format(station, Some("  ")).unwrap();
        assert_eq!(db.custom_tag_format(station).unwrap(), None);

        let _ = std::fs::remove_file(&path);
    }
}
