//! Raw stream recording: write the compressed bytes exactly as received.
//!
//! The engine never performs file I/O on the decoder thread. A bounded channel
//! feeds one writer thread; if the disk cannot keep up, audio is dropped and the
//! recording is flagged rather than stalling playback (FR-R5).

use crate::metadata::{self, WishEntry};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingMode {
    Continuous,
    SplitPerTrack,
}

#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub dir: PathBuf,
    pub template: String,
    pub mode: RecordingMode,
    /// Container extension of the raw stream, usually `mp3` or `aac`.
    pub extension: String,
    /// Test hook: simulate a slow disk in the writer thread.
    pub write_delay: Option<Duration>,
    /// When non-empty, split mode keeps only files whose track matches a
    /// `record` wish.
    pub wishlist: Vec<WishEntry>,
    /// Station tag format used to parse the ICY label for wish matching.
    pub tag_format: Option<String>,
    /// Discard split fragments shorter than this; 0 keeps every fragment.
    pub min_track_seconds: u32,
    /// Remove split files shorter than this when they close, before
    /// fingerprinting; 0 keeps every file.
    pub min_fingerprint_seconds: u32,
    /// Keep split files even when they are shorter than
    /// `min_fingerprint_seconds`. Set for explicit wish captures, where the
    /// user asked for the track and a partial recording is still wanted.
    pub keep_short: bool,
}

impl RecordingConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            template: "{station} - {date} {time}".to_string(),
            mode: RecordingMode::Continuous,
            extension: "mp3".to_string(),
            write_delay: None,
            wishlist: Vec::new(),
            tag_format: None,
            min_track_seconds: 0,
            min_fingerprint_seconds: 0,
            keep_short: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecordedSummary {
    pub files: usize,
    pub bytes: u64,
    pub dropped: bool,
    pub error: Option<String>,
    /// Files that were kept (wishlist mode deletes the rest).
    pub paths: Vec<PathBuf>,
    /// Kept files with no track label, which still need fingerprinting before
    /// the wish list can accept or reject them.
    pub unverified: Vec<PathBuf>,
}

/// One split file that closed while the recording kept running.
#[derive(Debug, Clone)]
pub struct CompletedFile {
    pub path: PathBuf,
    /// The file has no track label yet and needs wishlist verification.
    pub unverified: bool,
}

/// One ordered writer event. Audio and titles share a queue so a title is
/// never reordered ahead of the audio it describes.
enum Event {
    Track {
        artist: Option<String>,
        title: Option<String>,
    },
    /// Correct the label of the file that is already open, after a catalog
    /// lookup recovered the real artist and title.
    Retitle {
        from: String,
        artist: String,
        title: String,
    },
    /// Close the current split file at a scheduled track end. Audio after this
    /// event is dropped until the next title, so an ad break is not recorded.
    Boundary,
    Audio(Vec<u8>),
}

/// The result of one queue wait. `Closed` means closed *and* drained, decided
/// under the same lock as the pop, so a push cannot slip in between.
enum Pop {
    Event(Event),
    Timeout,
    Closed,
}

/// The audio queue is bounded; titles are not counted against it, so a full
/// audio queue can never drop a title. The queue itself is ordered, unlike two
/// independent channels.
const AUDIO_QUEUE_CAPACITY: usize = 32;

struct EventQueue {
    events: VecDeque<Event>,
    audio_count: usize,
    closed: bool,
}

struct SharedQueue {
    inner: Mutex<EventQueue>,
    cond: Condvar,
    dropped: AtomicBool,
}

impl SharedQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(EventQueue {
                events: VecDeque::new(),
                audio_count: 0,
                closed: false,
            }),
            cond: Condvar::new(),
            dropped: AtomicBool::new(false),
        }
    }

    fn push_track(&self, artist: Option<&str>, title: Option<&str>) {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return;
        }
        queue.events.push_back(Event::Track {
            artist: artist.map(str::to_string),
            title: title.map(str::to_string),
        });
        self.cond.notify_one();
    }

    fn push_boundary(&self) {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return;
        }
        queue.events.push_back(Event::Boundary);
        self.cond.notify_one();
    }

    fn push_retitle(&self, from: &str, artist: &str, title: &str) {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return;
        }
        queue.events.push_back(Event::Retitle {
            from: from.to_string(),
            artist: artist.to_string(),
            title: title.to_string(),
        });
        self.cond.notify_one();
    }

    fn push_audio(&self, audio: Vec<u8>) {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return;
        }
        if queue.audio_count >= AUDIO_QUEUE_CAPACITY {
            self.dropped.store(true, Ordering::Relaxed);
            return;
        }
        queue.events.push_back(Event::Audio(audio));
        queue.audio_count += 1;
        self.cond.notify_one();
    }

    fn pop_timeout(&self, timeout: Duration) -> Pop {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if queue.events.is_empty() && !queue.closed {
            let (next, _) = self
                .cond
                .wait_timeout(queue, timeout)
                .unwrap_or_else(|e| e.into_inner());
            queue = next;
        }
        if let Some(event) = queue.events.pop_front() {
            if matches!(event, Event::Audio(_)) {
                queue.audio_count = queue.audio_count.saturating_sub(1);
            }
            return Pop::Event(event);
        }
        if queue.closed {
            Pop::Closed
        } else {
            Pop::Timeout
        }
    }

    fn close(&self) {
        let mut queue = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        queue.closed = true;
        self.cond.notify_all();
    }

    fn dropped(&self) -> bool {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A running recording. Dropping it without `finish` still stops the writer.
pub struct Recording {
    queue: Arc<SharedQueue>,
    summary: Arc<Mutex<RecordedSummary>>,
    /// Files closed so far, drained by the app while recording continues.
    completed: Arc<Mutex<Vec<CompletedFile>>>,
    /// Path of the split file currently being written.
    current_path: Arc<Mutex<Option<PathBuf>>>,
    handle: Option<JoinHandle<()>>,
    /// `finish_async` hands the worker join to a separate thread; `Drop` must
    /// not join it a second time.
    detached: bool,
}

impl Recording {
    pub fn start(
        config: RecordingConfig,
        station: String,
        started: DateTime<Local>,
    ) -> io::Result<Self> {
        fs::create_dir_all(&config.dir)?;
        let queue = Arc::new(SharedQueue::new());
        let summary = Arc::new(Mutex::new(RecordedSummary::default()));
        let completed = Arc::new(Mutex::new(Vec::new()));
        let current_path = Arc::new(Mutex::new(None));
        let worker_queue = Arc::clone(&queue);
        let worker_summary = Arc::clone(&summary);
        let worker_completed = Arc::clone(&completed);
        let worker_path = Arc::clone(&current_path);
        let handle = thread::Builder::new()
            .name("viberadio-recorder".to_string())
            .spawn(move || {
                let mut writer =
                    Writer::new(config, station, started, worker_completed, worker_path);
                loop {
                    match worker_queue.pop_timeout(Duration::from_millis(50)) {
                        Pop::Event(Event::Track { artist, title }) => {
                            if let Err(e) = writer.track(artist.as_deref(), title.as_deref()) {
                                writer.error = Some(e.to_string());
                            }
                        }
                        Pop::Event(Event::Retitle {
                            from,
                            artist,
                            title,
                        }) => {
                            if let Err(e) = writer.retitle(&from, &artist, &title) {
                                log::warn!("could not retitle the recording: {e}");
                            }
                        }
                        Pop::Event(Event::Boundary) => {
                            if let Err(e) = writer.boundary() {
                                writer.error = Some(e.to_string());
                            }
                        }
                        Pop::Event(Event::Audio(bytes)) => {
                            if let Err(e) = writer.write(&bytes) {
                                writer.error = Some(e.to_string());
                            }
                        }
                        Pop::Timeout => {}
                        Pop::Closed => break,
                    }
                }
                let result = writer.finish();
                if let Ok(mut summary) = worker_summary.lock() {
                    *summary = result;
                    summary.dropped = worker_queue.dropped();
                }
            })?;
        Ok(Self {
            queue,
            summary,
            completed,
            current_path,
            handle: Some(handle),
            detached: false,
        })
    }

    /// Drain the files that have closed since the last call.
    pub fn take_completed(&self) -> Vec<CompletedFile> {
        self.completed
            .lock()
            .map(|mut files| std::mem::take(&mut *files))
            .unwrap_or_default()
    }

    /// Path of the split file being written, if any.
    pub fn current_path(&self) -> Option<PathBuf> {
        self.current_path
            .lock()
            .map(|path| path.clone())
            .unwrap_or_default()
    }

    /// Tell the writer about the current ICY track. Split mode starts a new file.
    pub fn set_track(&self, artist: Option<&str>, title: Option<&str>) {
        self.queue.push_track(artist, title);
    }

    /// Correct the open split file's name and label after a catalog lookup.
    pub fn retitle(&self, from: &str, artist: &str, title: &str) {
        self.queue.push_retitle(from, artist, title);
    }

    /// Close the current split file at the expected end of the track. The next
    /// title opens a new file; audio in between is discarded.
    pub fn boundary(&self) {
        self.queue.push_boundary();
    }

    /// Offer audio bytes. When the bounded audio queue is full the bytes are
    /// dropped and `dropped` is set; the caller never blocks on the disk.
    pub fn write(&self, audio: &[u8]) {
        if audio.is_empty() {
            return;
        }
        self.queue.push_audio(audio.to_vec());
    }

    /// Close the queue and return a handle that joins the writer off the caller's
    /// thread. The app polls `is_finished()` so a slow disk cannot freeze the UI.
    pub fn finish_async(mut self) -> JoinHandle<RecordedSummary> {
        self.queue.close();
        self.detached = true;
        let handle = self.handle.take();
        let summary = Arc::clone(&self.summary);
        thread::Builder::new()
            .name("viberadio-recorder-join".to_string())
            .spawn(move || {
                if let Some(handle) = handle {
                    if handle.join().is_err() {
                        if let Ok(mut summary) = summary.lock() {
                            summary.error = Some("recorder thread panicked".to_string());
                        }
                    }
                }
                summary
                    .lock()
                    .map(|summary| summary.clone())
                    .unwrap_or_default()
            })
            .expect("failed to spawn the recording joiner")
    }

    pub fn finish(self) -> RecordedSummary {
        self.finish_async().join().unwrap_or_default()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.queue.close();
        if !self.detached {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

struct Writer {
    config: RecordingConfig,
    station: String,
    started: DateTime<Local>,
    current: Option<(File, PathBuf)>,
    files: usize,
    bytes: u64,
    error: Option<String>,
    current_track: String,
    metadata_log: Option<File>,
    metadata_log_pending: Vec<String>,
    kept_paths: Vec<PathBuf>,
    unverified: Vec<PathBuf>,
    current_started: Option<Instant>,
    /// Split mode buffers the bytes since the last title; a title arrives after
    /// its first ICY interval, so those bytes belong to the new track.
    pending: Vec<u8>,
    /// After a scheduled boundary, audio is dropped until the next title so an
    /// ad or a talking segment is not written into the next song.
    discard_until_title: bool,
    /// Files closed so far, shared with the app.
    completed: Arc<Mutex<Vec<CompletedFile>>>,
    /// Path of the split file currently being written.
    current_path: Arc<Mutex<Option<PathBuf>>>,
}

impl Writer {
    fn new(
        config: RecordingConfig,
        station: String,
        started: DateTime<Local>,
        completed: Arc<Mutex<Vec<CompletedFile>>>,
        current_path: Arc<Mutex<Option<PathBuf>>>,
    ) -> Self {
        Self {
            config,
            station,
            started,
            current: None,
            files: 0,
            bytes: 0,
            error: None,
            current_track: String::new(),
            metadata_log: None,
            metadata_log_pending: Vec::new(),
            pending: Vec::new(),
            discard_until_title: false,
            completed,
            current_path,
            kept_paths: Vec::new(),
            unverified: Vec::new(),
            current_started: None,
        }
    }

    fn track(&mut self, artist: Option<&str>, title: Option<&str>) -> io::Result<()> {
        // An empty metadata event must not end the discard window after a
        // scheduled boundary; only a real title starts the next file.
        if artist.is_none() && title.is_none() {
            return Ok(());
        }
        self.discard_until_title = false;
        let label = metadata::label(artist, title);
        if self.config.mode == RecordingMode::Continuous {
            let file = self.current.as_ref().and_then(|(_, path)| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            });
            let line = serde_json::json!({
                "offset": self.bytes,
                "artist": artist,
                "title": title,
                "station": self.station,
                "file": file,
                "at": chrono::Utc::now().to_rfc3339(),
            })
            .to_string();
            self.append_metadata(line)?;
        }
        if self.config.mode == RecordingMode::SplitPerTrack && label != self.current_track {
            // A title change moments after the previous one is usually a
            // metadata blip, not a real track. Discard that fragment.
            let too_short = self.config.min_track_seconds > 0
                && self.current.is_some()
                && self
                    .current_started
                    .map(|started| {
                        started.elapsed()
                            < Duration::from_secs(u64::from(self.config.min_track_seconds))
                    })
                    .unwrap_or(false);
            if too_short {
                if let Some((file, path)) = self.current.take() {
                    let _ = file.sync_all();
                    let _ = std::fs::remove_file(&path);
                    self.files = self.files.saturating_sub(1);
                }
                self.current_started = None;
                self.current_track = label;
                // The buffer is the discarded fragment's tail.
                self.pending.clear();
                self.open_current()?;
                return Ok(());
            }
            // Audio after a title is written straight to the current file, so
            // there is no old-track tail left in the buffer here.
            self.close_current();
            self.current_track = label;
            self.open_current()?;
            // The first title has no earlier file, so its buffered audio is the
            // best content we have for this track.
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Correct the open split file after a catalog lookup found the real
    /// artist and title. Only the file that is still on `from` is renamed.
    fn retitle(&mut self, from: &str, artist: &str, title: &str) -> io::Result<()> {
        if self.config.mode != RecordingMode::SplitPerTrack || self.current_track != from {
            return Ok(());
        }
        let label = metadata::label(Some(artist), Some(title));
        if label.is_empty() || label == self.current_track {
            return Ok(());
        }
        self.current_track = label;
        let Some((file, old_path)) = self.current.take() else {
            return Ok(());
        };
        let new_path = self.next_path();
        match fs::rename(&old_path, &new_path) {
            Ok(()) => {
                if let Ok(mut current) = self.current_path.lock() {
                    *current = Some(new_path.clone());
                }
                self.current = Some((file, new_path));
                Ok(())
            }
            Err(e) => {
                // Windows cannot rename an open file; keep the old name and let
                // the verified copy carry the corrected tags.
                self.current = Some((file, old_path));
                Err(e)
            }
        }
    }

    /// Close the current file at the expected end of its track. The bytes that
    /// arrived before this event still belong to the ending track.
    fn boundary(&mut self) -> io::Result<()> {
        if self.config.mode != RecordingMode::SplitPerTrack {
            return Ok(());
        }
        if self.current_track.is_empty() {
            self.pending.clear();
        } else {
            self.close_current();
            self.current_track.clear();
        }
        self.discard_until_title = true;
        Ok(())
    }

    fn append_metadata(&mut self, line: String) -> io::Result<()> {
        if self.metadata_log.is_none() {
            let Some((_, path)) = self.current.as_ref() else {
                self.metadata_log_pending.push(line);
                return Ok(());
            };
            let path = path.with_extension("jsonl");
            self.metadata_log = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?,
            );
        }
        if let Some(log) = &mut self.metadata_log {
            writeln!(log, "{line}")?;
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(delay) = self.config.write_delay {
            std::thread::sleep(delay);
        }
        if self.config.mode == RecordingMode::SplitPerTrack {
            if self.discard_until_title {
                return Ok(());
            }
            if self.current.is_some() {
                self.write_current(bytes)?;
            } else {
                self.pending.extend_from_slice(bytes);
                // If a server never sends metadata, bound memory by starting an
                // untitled file after 1 MiB.
                if self.pending.len() > 1024 * 1024 {
                    self.flush_pending()?;
                }
            }
            return Ok(());
        }
        if self.current.is_none() {
            self.open_current()?;
        }
        if let Some((file, _)) = &mut self.current {
            file.write_all(bytes)?;
            self.bytes += bytes.len() as u64;
        }
        Ok(())
    }

    /// Write buffered audio to the current file, opening it first if needed.
    fn flush_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        self.write_current(&pending)
    }

    fn write_current(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.current.is_none() {
            self.open_current()?;
        }
        if let Some((file, _)) = &mut self.current {
            file.write_all(bytes)?;
            self.bytes += bytes.len() as u64;
        }
        Ok(())
    }

    fn open_current(&mut self) -> io::Result<()> {
        let path = self.next_path();
        let file = File::create(&path)?;
        self.files += 1;
        self.current_started = Some(Instant::now());
        if let Ok(mut current) = self.current_path.lock() {
            *current = Some(path.clone());
        }
        self.current = Some((file, path));
        if self.config.mode == RecordingMode::Continuous && !self.metadata_log_pending.is_empty() {
            let pending = std::mem::take(&mut self.metadata_log_pending);
            for line in pending {
                self.append_metadata(line)?;
            }
        }
        Ok(())
    }

    fn keep_current(&self) -> bool {
        if self.config.wishlist.is_empty() {
            return true;
        }
        metadata::wish_matches_with_format(
            &self.config.wishlist,
            &self.current_track,
            self.config.tag_format.as_deref(),
        )
        .iter()
        .any(|entry| entry.action.eq_ignore_ascii_case("record"))
    }

    fn close_current(&mut self) {
        let elapsed = self
            .current_started
            .map(|started| started.elapsed())
            .unwrap_or_default();
        self.current_started = None;
        if let Ok(mut current) = self.current_path.lock() {
            *current = None;
        }
        if let Some((file, path)) = self.current.take() {
            let _ = file.sync_all();
            let empty = std::fs::metadata(&path)
                .map(|metadata| metadata.len() == 0)
                .unwrap_or(false);
            if empty {
                let _ = std::fs::remove_file(&path);
                self.files = self.files.saturating_sub(1);
                if let Some(log) = self.metadata_log.take() {
                    let _ = log.sync_all();
                }
                return;
            }
            if self.config.mode == RecordingMode::SplitPerTrack
                && !self.config.keep_short
                && self.config.min_fingerprint_seconds > 0
                && elapsed < Duration::from_secs(u64::from(self.config.min_fingerprint_seconds))
            {
                log::info!(
                    "discarding {} after {}s; below the {}s fingerprint minimum",
                    path.display(),
                    elapsed.as_secs(),
                    self.config.min_fingerprint_seconds
                );
                let _ = std::fs::remove_file(&path);
                self.files = self.files.saturating_sub(1);
                if let Some(log) = self.metadata_log.take() {
                    let _ = log.sync_all();
                }
                return;
            }
            // With no title there is nothing to match against yet. Keep the
            // file so the app can fingerprint it and decide afterwards.
            let unverified = self.config.mode == RecordingMode::SplitPerTrack
                && !self.config.wishlist.is_empty()
                && self.current_track.trim().is_empty();
            if self.config.mode == RecordingMode::SplitPerTrack
                && !self.config.wishlist.is_empty()
                && !unverified
                && !self.keep_current()
            {
                // Wishlist mode records continuously but only keeps wanted
                // songs; everything else is discarded at the track boundary.
                log::info!(
                    "discarding {}; the track does not match a record wish",
                    path.display()
                );
                let _ = std::fs::remove_file(&path);
                self.files = self.files.saturating_sub(1);
            } else {
                if unverified {
                    self.unverified.push(path.clone());
                }
                self.kept_paths.push(path.clone());
                if let Ok(mut files) = self.completed.lock() {
                    files.push(CompletedFile { path, unverified });
                }
            }
        }
        if let Some(log) = self.metadata_log.take() {
            let _ = log.sync_all();
        }
    }

    fn next_path(&self) -> PathBuf {
        let base = render_name(
            &self.config.template,
            &self.station,
            &self.current_track,
            self.started,
        );
        let mut path = self
            .config
            .dir
            .join(format!("{base}.{}", self.config.extension));
        let mut counter = 2;
        while path.exists() {
            path = self
                .config
                .dir
                .join(format!("{base} ({counter}).{}", self.config.extension));
            counter += 1;
        }
        path
    }

    fn finish(mut self) -> RecordedSummary {
        if self.config.mode == RecordingMode::SplitPerTrack && !self.pending.is_empty() {
            if let Err(e) = self.flush_pending() {
                self.error = Some(e.to_string());
            }
        }
        self.close_current();
        RecordedSummary {
            files: self.files,
            bytes: self.bytes,
            dropped: false,
            error: self.error,
            paths: self.kept_paths,
            unverified: self.unverified,
        }
    }
}

/// Render `{station}`, `{artist}`, `{title}`, `{date}` and `{time}` and make the
/// result safe on Linux, macOS and Windows.
pub fn render_name(template: &str, station: &str, track: &str, started: DateTime<Local>) -> String {
    let (artist, title) = match track.split_once(" - ") {
        Some((artist, title)) => (artist, title),
        None => ("", track),
    };
    let mut name = template
        .replace("{station}", station)
        .replace("{artist}", artist)
        .replace("{title}", title)
        .replace("{date}", &started.format("%Y-%m-%d").to_string())
        .replace("{time}", &started.format("%H-%M-%S").to_string());
    if name.trim().is_empty() {
        name = "recording".to_string();
    }
    truncate_name(&sanitize(&name), 200)
}

/// Keep a rendered name inside the 255-byte filename limit on every supported
/// filesystem, cutting on a character boundary.
fn truncate_name(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim_end().to_string()
}

/// Template tokens that [`render_name`] does not replace.
pub fn unknown_tokens(template: &str) -> Vec<String> {
    const KNOWN: &[&str] = &["station", "artist", "title", "date", "time"];
    let mut unknown = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            break;
        };
        let token = after[..end].trim();
        if !token.is_empty()
            && !KNOWN.contains(&token)
            && !unknown.iter().any(|known| known == token)
        {
            unknown.push(token.to_string());
        }
        rest = &after[end + 1..];
    }
    unknown
}

pub(crate) fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            out.push('_');
        } else {
            out.push(character);
        }
    }
    let out = out.trim().trim_matches('.').trim().to_string();
    if out.is_empty() {
        "recording".to_string()
    } else {
        out
    }
}

/// Split a continuous raw recording into per-track files using its JSONL log.
/// Offsets are byte positions in the recording; the final segment runs to EOF.
#[allow(dead_code)] // UI wiring is the next M5b task
pub fn split_continuous_recording(
    audio_path: &Path,
    log_path: &Path,
    out_dir: &Path,
    station: &str,
    template: &str,
) -> io::Result<Vec<PathBuf>> {
    #[derive(Debug)]
    struct Entry {
        offset: u64,
        artist: Option<String>,
        title: Option<String>,
        station: Option<String>,
        at: DateTime<Local>,
    }

    fs::create_dir_all(out_dir)?;
    let log = fs::read_to_string(log_path)?;
    let mut entries = Vec::new();
    for line in log.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A crash or an in-progress recording can leave a truncated line.
            continue;
        };
        let Some(offset) = value.get("offset").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let at = value
            .get("at")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.with_timezone(&Local))
            .unwrap_or_else(Local::now);
        entries.push(Entry {
            offset,
            artist: value
                .get("artist")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            title: value
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            station: value
                .get("station")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            at,
        });
    }
    if entries.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata log has no usable entries",
        ));
    }
    entries.sort_by_key(|entry| entry.offset);

    let length = fs::metadata(audio_path)?.len();
    let extension = audio_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let mut audio = File::open(audio_path)?;
    let mut written = Vec::new();
    let mut start = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        let end = if index + 1 == entries.len() {
            length
        } else {
            entry.offset.min(length)
        };
        if end > start {
            let label = match (entry.artist.as_deref(), entry.title.as_deref()) {
                (Some(artist), Some(title)) => format!("{artist} - {title}"),
                (Some(artist), None) => artist.to_string(),
                (None, Some(title)) => title.to_string(),
                (None, None) => format!("track {}", index + 1),
            };
            let station_name = entry.station.as_deref().unwrap_or(station);
            let base = render_name(template, station_name, &label, entry.at);
            let path = unique_path(out_dir, &base, extension);
            let mut out = File::create(&path)?;
            audio.seek(std::io::SeekFrom::Start(start))?;
            let mut remaining = end - start;
            let mut buffer = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let take = remaining.min(buffer.len() as u64) as usize;
                let read = Read::read(&mut audio, &mut buffer[..take])?;
                if read == 0 {
                    break;
                }
                out.write_all(&buffer[..read])?;
                remaining -= read as u64;
            }
            out.sync_all()?;
            written.push(path);
        }
        start = end;
    }
    // The final logged track owns everything after its offset to EOF.
    Ok(written)
}

#[allow(dead_code)]
fn unique_path(dir: &Path, base: &str, extension: &str) -> PathBuf {
    let mut path = dir.join(format!("{base}.{extension}"));
    let mut counter = 2;
    while path.exists() {
        path = dir.join(format!("{base} ({counter}).{extension}"));
        counter += 1;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("viberadio-recording-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn split_mode_writes_one_file_per_track() {
        let dir = temp_dir("split");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        // A title block marks the boundary: the bytes buffered before it are
        // the tail of the ending track, and the next bytes belong to the new
        // track.
        recording.write(b"first");
        recording.set_track(Some("Artist One"), Some("Song / One"));
        recording.write(b"second");
        recording.set_track(Some("Artist Two"), Some("Song Two"));
        recording.write(b"third");
        let summary = recording.finish();
        assert_eq!(summary.files, 2);
        assert!(!summary.dropped);
        assert!(summary.error.is_none());
        assert_eq!(
            fs::read(dir.join("Artist One - Song _ One.bin")).unwrap(),
            b"firstsecond"
        );
        assert_eq!(
            fs::read(dir.join("Artist Two - Song Two.bin")).unwrap(),
            b"third"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fallback_retitle_renames_the_open_file() {
        let dir = temp_dir("retitle");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.set_track(Some("DJ PinkGesia"), Some("CeCe Peniston - Finally"));
        recording.write(b"audio");
        recording.retitle(
            "DJ PinkGesia - CeCe Peniston - Finally",
            "CeCe Peniston",
            "Finally",
        );
        recording.write(b"more");
        let summary = recording.finish();
        assert_eq!(summary.files, 1);
        assert!(!dir
            .join("DJ PinkGesia - CeCe Peniston - Finally.bin")
            .exists());
        assert_eq!(
            fs::read(dir.join("CeCe Peniston - Finally.bin")).unwrap(),
            b"audiomore"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_split_file_is_removed() {
        let dir = temp_dir("empty-split");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"audio");
        recording.set_track(Some("Artist One"), Some("Song One"));
        recording.set_track(Some("Artist Two"), Some("Song Two"));
        let summary = recording.finish();
        assert_eq!(summary.files, 1);
        assert!(dir.join("Artist One - Song One.bin").is_file());
        assert!(!dir.join("Artist Two - Song Two.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_files_are_removed_before_fingerprinting() {
        let dir = temp_dir("short-fingerprint");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        config.min_fingerprint_seconds = 30;
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"audio");
        recording.set_track(Some("Artist"), Some("Song"));
        recording.write(b"more");
        let summary = recording.finish();
        assert_eq!(summary.files, 0);
        assert!(summary.paths.is_empty());
        assert!(!dir.join("Artist - Song.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wish_captures_keep_short_files() {
        let dir = temp_dir("wish-short");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        config.min_fingerprint_seconds = 30;
        config.keep_short = true;
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"audio");
        recording.set_track(Some("Artist"), Some("Song"));
        recording.write(b"more");
        let summary = recording.finish();
        assert_eq!(summary.files, 1);
        assert!(dir.join("Artist - Song.bin").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_files_are_reported_as_they_close() {
        let dir = temp_dir("completed");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"one");
        recording.set_track(Some("Artist One"), Some("Song One"));
        recording.write(b"two");
        recording.set_track(Some("Artist Two"), Some("Song Two"));
        recording.write(b"three");

        let mut completed = Vec::new();
        for _ in 0..100 {
            completed = recording.take_completed();
            if !completed.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].path,
            dir.join("Artist One - Song One.bin")
        );
        assert!(!completed[0].unverified);
        assert!(recording.take_completed().is_empty());

        let summary = recording.finish();
        assert_eq!(summary.files, 2);
        assert_eq!(summary.paths.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scheduled_boundary_closes_the_file_and_drops_the_gap() {
        let dir = temp_dir("boundary");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"before");
        recording.set_track(Some("Artist"), Some("Song"));
        recording.write(b"tail");
        recording.boundary();
        recording.write(b"ad");
        recording.set_track(Some("Artist Two"), Some("Song Two"));
        recording.write(b"next");
        let summary = recording.finish();
        assert_eq!(summary.files, 2);
        assert!(summary.error.is_none());
        assert_eq!(
            fs::read(dir.join("Artist - Song.bin")).unwrap(),
            b"beforetail"
        );
        assert_eq!(
            fs::read(dir.join("Artist Two - Song Two.bin")).unwrap(),
            b"next"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reports_unknown_template_tokens() {
        assert!(unknown_tokens("{station} - {artist} - {title} - {date} {time}").is_empty());
        assert_eq!(
            unknown_tokens("{station} - {album} - {album}"),
            vec!["album".to_string()]
        );
        assert!(unknown_tokens("{station").is_empty());
    }

    #[test]
    fn wishlist_split_keeps_only_matching_tracks() {
        let dir = temp_dir("wishlist-split");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        config.wishlist = vec![WishEntry {
            id: 0,
            artist: Some("Artist One".into()),
            song_title: Some("Song One".into()),
            action: "record".into(),
            compare: crate::metadata::CompareMode::Contains,
        }];
        config.tag_format = Some("<Artist> - <Song>".into());
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        // ICY titles arrive after the audio they describe.
        recording.write(b"one");
        recording.set_track(Some("Artist One"), Some("Song One"));
        recording.write(b"two");
        recording.set_track(Some("Artist Two"), Some("Song Two"));
        recording.write(b"three");
        recording.set_track(Some("Artist One"), Some("Song One"));
        recording.write(b"four");
        let summary = recording.finish();
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2);
        assert!(names.iter().all(|name| name.contains("Song One")));
        assert_eq!(summary.files, 2);
        assert!(!dir.join("Artist Two - Song Two.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_split_fragments_are_discarded() {
        let dir = temp_dir("short-split");
        let mut config = RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        config.min_track_seconds = 1;
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"a");
        recording.set_track(Some("Artist"), Some("Short"));
        recording.write(b"b");
        recording.set_track(Some("Artist"), Some("Long"));
        recording.write(b"c");
        let summary = recording.finish();
        assert!(!dir.join("Artist - Short.bin").exists());
        assert!(dir.join("Artist - Long.bin").exists());
        assert_eq!(summary.files, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wishlist_without_metadata_keeps_the_file_for_fingerprinting() {
        let dir = temp_dir("wishlist-unverified");
        let mut config = RecordingConfig::new(&dir);
        config.template = "untitled".to_string();
        config.mode = RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        config.wishlist = vec![WishEntry {
            id: 0,
            artist: Some("Artist One".into()),
            song_title: Some("Song One".into()),
            action: "record".into(),
            compare: crate::metadata::CompareMode::Contains,
        }];
        let recording = Recording::start(config, "Test FM".to_string(), Local::now()).unwrap();
        recording.write(b"audio without a title");
        let summary = recording.finish();
        assert_eq!(summary.files, 1);
        assert_eq!(summary.unverified.len(), 1);
        assert!(summary.unverified[0].is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuous_mode_ignores_track_changes_and_avoids_collisions() {
        let dir = temp_dir("continuous");
        fs::create_dir_all(&dir).unwrap();
        let mut config = RecordingConfig::new(&dir);
        config.template = "fixed".to_string();
        config.extension = "bin".to_string();
        let started = Local::now();
        let recording = Recording::start(config.clone(), "Test FM".to_string(), started).unwrap();
        recording.set_track(Some("A"), Some("One"));
        recording.write(b"a");
        let summary = recording.finish();
        assert_eq!(summary.files, 1);
        assert_eq!(fs::read(dir.join("fixed.bin")).unwrap(), b"a");

        let recording = Recording::start(config, "Test FM".to_string(), started).unwrap();
        recording.write(b"b");
        recording.finish();
        assert_eq!(fs::read(dir.join("fixed (2).bin")).unwrap(), b"b");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuous_mode_writes_a_metadata_log() {
        let dir = temp_dir("metadata-log");
        let mut config = RecordingConfig::new(&dir);
        config.template = "cont".to_string();
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Log FM".to_string(), Local::now()).unwrap();
        recording.write(b"AAAA");
        recording.set_track(Some("Artist One"), Some("Song One"));
        recording.write(b"BBBB");
        recording.set_track(Some("Artist Two"), Some("Song Two"));
        recording.finish();
        let log = fs::read_to_string(dir.join("cont.jsonl")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["offset"], 4);
        assert_eq!(first["artist"], "Artist One");
        assert_eq!(first["title"], "Song One");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["offset"], 8);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn slow_writer_never_blocks_the_caller() {
        let dir = temp_dir("slow-writer");
        let mut config = RecordingConfig::new(&dir);
        config.template = "slow".to_string();
        config.extension = "bin".to_string();
        config.write_delay = Some(Duration::from_millis(50));
        let recording = Recording::start(config, "Slow FM".to_string(), Local::now()).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..200 {
            recording.write(&[0u8; 1024]);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "writer call blocked for {elapsed:?}"
        );
        let summary = recording.finish();
        assert!(summary.dropped, "full queue must flag dropped audio");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_async_returns_before_the_writer_finishes() {
        let dir = temp_dir("async-finish");
        let mut config = RecordingConfig::new(&dir);
        config.template = "async".to_string();
        config.extension = "bin".to_string();
        config.write_delay = Some(Duration::from_millis(250));
        let recording = Recording::start(config, "Async FM".to_string(), Local::now()).unwrap();
        recording.write(&[7u8; 1024]);
        let started = std::time::Instant::now();
        let handle = recording.finish_async();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "finish_async blocked for {elapsed:?}"
        );
        let summary = handle.join().unwrap();
        assert!(summary.error.is_none());
        assert_eq!(summary.bytes, 1024);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_survives_a_full_audio_queue() {
        let dir = temp_dir("track-full");
        let mut config = RecordingConfig::new(&dir);
        config.template = "tracks".to_string();
        config.extension = "bin".to_string();
        config.write_delay = Some(Duration::from_millis(5));
        let recording = Recording::start(config, "Queue FM".to_string(), Local::now()).unwrap();
        for _ in 0..(AUDIO_QUEUE_CAPACITY * 3) {
            recording.write(&[1u8; 1024]);
        }
        recording.set_track(Some("Queue Artist"), Some("Queue Song"));
        let summary = recording.finish();
        assert!(summary.dropped, "the audio queue must overflow");
        let log = fs::read_to_string(dir.join("tracks.jsonl")).unwrap();
        let line: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(line["artist"], "Queue Artist");
        assert_eq!(line["title"], "Queue Song");
        let offset = line["offset"].as_u64().unwrap();
        assert_eq!(offset % 1024, 0, "offset must fall on a chunk boundary");
        assert!(
            offset >= (AUDIO_QUEUE_CAPACITY * 1024) as u64,
            "the title must follow the audio that was actually written"
        );
        assert!(
            offset < (AUDIO_QUEUE_CAPACITY * 3 * 1024) as u64,
            "a dropped chunk must not be counted in the offset"
        );
        assert_eq!(
            summary.bytes, offset,
            "the title is the last event, so the file ends at its offset"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn thirty_second_volume_capture_is_byte_exact() {
        let dir = temp_dir("thirty-seconds");
        let mut config = RecordingConfig::new(&dir);
        config.template = "thirty".to_string();
        config.extension = "bin".to_string();
        let recording = Recording::start(config, "Thirty FM".to_string(), Local::now()).unwrap();
        // 30 s at 128 kbps is 480 KB. A per-chunk counter detects reordering or
        // duplication; backpressure is covered by the slow-writer test.
        for index in 0..30u8 {
            recording.write(&vec![index; 16 * 1024]);
        }
        recording.set_track(Some("Thirty Artist"), Some("Thirty Song"));
        let summary = recording.finish();
        assert!(!summary.dropped);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.bytes, 480 * 1024);
        let bytes = fs::read(dir.join("thirty.bin")).unwrap();
        assert_eq!(bytes.len(), 480 * 1024);
        for index in 0..30usize {
            let start = index * 16 * 1024;
            let end = start + 16 * 1024;
            assert!(
                bytes[start..end].iter().all(|byte| *byte == index as u8),
                "chunk {index} was reordered or duplicated"
            );
        }
        let log = fs::read_to_string(dir.join("thirty.jsonl")).unwrap();
        assert!(log.contains("Thirty Artist"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuous_recording_splits_from_its_metadata_log() {
        let dir = temp_dir("split-log");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("cont.bin");
        fs::write(&audio, b"AAAABBBBCCCC").unwrap();
        let log = dir.join("cont.jsonl");
        fs::write(
            &log,
            "{\"offset\":4,\"artist\":\"Artist One\",\"title\":\"Song One\",\"station\":\"Log FM\",\"at\":\"2026-11-08T12:00:00Z\"}\n\
             {\"offset\":8,\"artist\":\"Artist Two\",\"title\":\"Song Two\",\"station\":\"Log FM\",\"at\":\"2026-11-08T12:01:00Z\"}\n\
             {truncated\n",
        )
        .unwrap();
        let out = dir.join("tracks");
        let files =
            split_continuous_recording(&audio, &log, &out, "Split FM", "{station} - {title}")
                .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            fs::read(out.join("Log FM - Song One.bin")).unwrap(),
            b"AAAA"
        );
        assert_eq!(
            fs::read(out.join("Log FM - Song Two.bin")).unwrap(),
            b"BBBBCCCC"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_sanitized() {
        let name = render_name("{artist} - {title}", "Station", "A/B - C?", Local::now());
        assert_eq!(name, "A_B - C_");
        assert!(!name.contains('/'));
    }

    #[test]
    fn long_names_are_shortened() {
        let track = format!("{} - {}", "A".repeat(300), "B".repeat(300));
        let name = render_name("{artist} - {title}", "Station", &track, Local::now());
        assert!(name.len() <= 200, "name is {} bytes", name.len());
    }

    #[test]
    fn multibyte_names_are_shortened_on_a_boundary() {
        let track = format!("{} - {}", "曲".repeat(200), "歌".repeat(200));
        let name = render_name("{artist} - {title}", "Station", &track, Local::now());
        assert!(name.len() <= 200);
        assert!(!name.is_empty());
    }
}
