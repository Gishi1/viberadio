//! Multi-stream audio playback for live radio.
//!
//! ## Why this is not a `Sink::append(Decoder::new(response))` one-liner
//!
//! 1. `Response::bytes()` never returns on an endless stream, so the body is
//!    consumed as it arrives (`bytes_stream`).
//! 2. `rodio::Decoder` needs `Read + Seek`, but a network stream is neither
//!    seekable nor complete when playback should start. `ProgressiveBuffer`
//!    implements `Read + Seek` over a growing, sliding network buffer.
//! 3. **The decoder must not run in the audio callback.** rodio's mixer is pulled
//!    by cpal's callback, and a `Sink`'s queue is one of its sources. Decoding
//!    inside a `Source::next` would therefore stall the callback (and every other
//!    stream) on a slow network read.
//!
//! So each stream has:
//!
//! ```text
//! download thread -> ProgressiveBuffer -> decoder worker -> bounded sample ring
//!                                                                    |
//!                                                       StreamSource (non-blocking)
//!                                                                    |
//!                                                                  Sink
//! ```
//!
//! The worker owns the sink and the decoder; the callback only pops samples from
//! the ring and returns silence on underrun. One worker per stream means one
//! stalled stream cannot silence another.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::metadata::IcyDemuxer;
use crate::recording::{CompletedFile, RecordedSummary, Recording, RecordingConfig};
use cpal::traits::{DeviceTrait, HostTrait};
use reqwest::Client;
use rodio::{OutputStream, OutputStreamHandle, Sink, Source};
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::decode;
use crate::dsp::{Dsp, DspPreset};
use crate::hls;
use crate::playlist;

/// How many decoded samples a stream may buffer ahead (`f32`, interleaved) with
/// the default setting. 4 s at 48 kHz stereo; the byte buffer in front is
/// separate and much smaller. A stream sizes its ring from its own rate and
/// channel count once the decoder opens.
const RING_SAMPLES: usize = 48_000 * 2 * 4;

/// Default decoded-audio buffer, in seconds.
pub const DEFAULT_STREAM_BUFFER_SECS: u32 = 4;

/// Bounds for the stream buffer setting.
pub const MIN_STREAM_BUFFER_SECS: u32 = 1;
pub const MAX_STREAM_BUFFER_SECS: u32 = 30;

/// Samples required before a stream is reported as `Live` rather than `Buffering`.
const PREBUFFER_SAMPLES: usize = 2_048;

/// How many samples `StreamSource` takes from the shared ring per lock.
const SOURCE_CHUNK: usize = 4_096;

/// How many decoded samples a worker pushes per lock.
const DECODE_BATCH: usize = 4_096;

/// Upper bound on the *byte* buffer in front of the decoder. It slides, so this is
/// a backpressure limit, not an end-of-stream cap (FR-P12).
const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;
/// How much compressed audio a watched station keeps for a wish trigger.
const WATCH_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Keep this much already-consumed data available for decoder rewinds, then slide.
const BUFFER_REWIND_BYTES: usize = 64 * 1024;

/// How long a single network read may stall before the stream is abandoned.
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the connection to be established.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// User agent sent to stations that reject unknown clients.
const USER_AGENT: &str = "VibeRadio/0.1";

/// Redirect hops followed before a fetch gives up.
const MAX_REDIRECTS: u32 = 5;

/// Nested playlists followed before a fetch gives up.
const MAX_PLAYLIST_HOPS: u32 = 3;

/// A stream instance handle. Scoped to one `RadioPlayer` (process) and never
/// reused, so a UI row cannot confuse a stopped stream with a new one.
pub type PlaybackId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Connecting,
    Buffering,
    Live,
    Error,
    Stopped,
}

impl PlaybackStatus {
    /// Whether the stream is still trying to play or playing.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            PlaybackStatus::Connecting | PlaybackStatus::Buffering | PlaybackStatus::Live
        )
    }
}

/// UI-facing view of one stream instance.
#[derive(Debug, Clone)]
pub struct StreamSnapshot {
    pub id: PlaybackId,
    pub station_id: Option<Uuid>,
    pub url: String,
    pub status: PlaybackStatus,
    pub status_text: String,
    /// Latest `StreamTitle` from ICY metadata, if the server sends it.
    pub now_playing: Option<String>,
    /// The same title once playback reaches the audio it describes. UI labels
    /// use this so they change with the speakers, not with the download.
    pub display_now_playing: Option<String>,
    /// Last error of this attempt, if any; the app records it on the history row.
    pub error: Option<String>,
    /// Live gain of this stream; the player bar reads it for the active stream.
    pub volume: f32,
    /// Increments for every real decoded sample the callback consumed. Silence
    /// inserted on underrun is not counted, so this is evidence of decoding.
    /// Read by the isolation tests today; the M3 buffer meter will use it.
    #[allow(dead_code)]
    pub samples_played: u64,
    /// Whether a raw recording is attached to this stream.
    pub recording: bool,
    /// Latest 0.5-second RMS window from the decoder, for live end detection.
    pub rms: f32,
    /// Increments for every new RMS window.
    pub rms_serial: u64,
    /// The stream records without decoding or playing audio.
    pub record_only: bool,
    /// The stream keeps a rolling wish buffer.
    pub watching: bool,
    /// Bitrate the server reported in `icy-br`, when it sent one.
    pub reported_bitrate: Option<u32>,
}

/// One coherent view of the engine for the UI. Built once per frame; the revision
/// changes whenever a status, volume or stream membership changes, so the app can
/// request an immediate repaint instead of polling blindly.
#[derive(Debug, Clone)]
pub struct EngineSnapshot {
    pub revision: u64,
    pub active: Option<StreamSnapshot>,
    pub streams: Vec<StreamSnapshot>,
}

/// Automatic reconnect policy after a dropped stream (FR-P11).
#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Request to start one stream instance.
#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub url: String,
    pub station_id: Option<Uuid>,
    pub stream_id: Option<Uuid>,
    pub volume: f32,
    /// Per-station gain trim in dB, applied by the DSP chain after decoding.
    pub gain_db: f32,
    /// `None` means a clean end is final (local files/tests); `Some` reconnects
    /// after a drop, which is what a live station wants.
    pub reconnect: Option<ReconnectPolicy>,
    /// Record the stream without decoding or playing it.
    pub record_only: bool,
    /// Keep a rolling audio buffer so a wish can be recorded from the start of
    /// the current song.
    pub watch: bool,
    /// Bytes of compressed audio kept per watched station.
    pub watch_buffer_bytes: usize,
}

impl PlayRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            station_id: None,
            stream_id: None,
            volume: 1.0,
            gain_db: 0.0,
            reconnect: None,
            record_only: false,
            watch: false,
            watch_buffer_bytes: WATCH_BUFFER_BYTES,
        }
    }
}

enum AudioCommand {
    Start {
        id: PlaybackId,
        request: PlayRequest,
        shared: Arc<StreamShared>,
    },
    /// Open a different output device; `None` means the system default.
    ///
    /// The reply is `Ok(None)` when the requested output opened,
    /// `Ok(Some(warning))` when it fell back to the system default, and `Err`
    /// when nothing could be opened.
    SetDevice {
        name: Option<String>,
        reply: Sender<Result<Option<String>, String>>,
    },
    Shutdown,
}

/// What happened when a requested output was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputChoice {
    /// The requested device (or the default, when none was requested) opened.
    Opened,
    /// The named device failed, but the system default opened instead.
    FellBack(String),
    /// Nothing opened.
    Failed(String),
}

/// Try the requested output, falling back to the system default only when a
/// named device was asked for. A `None` request has nothing to fall back to, so
/// a failure is reported once instead of being retried against itself.
fn choose_output<T>(
    requested: Option<&str>,
    mut open: impl FnMut(Option<&str>) -> Result<T, String>,
) -> (Option<T>, OutputChoice) {
    match open(requested) {
        Ok(output) => (Some(output), OutputChoice::Opened),
        Err(e) => match requested {
            Some(_) => match open(None) {
                Ok(output) => (Some(output), OutputChoice::FellBack(e)),
                Err(default_error) => (
                    None,
                    OutputChoice::Failed(format!("{e}; {default_error}")),
                ),
            },
            None => (None, OutputChoice::Failed(e)),
        },
    }
}

/// Open an output stream on the named device, or on the system default.
fn open_output(name: Option<&str>) -> Result<(OutputStream, OutputStreamHandle), String> {
    let host = cpal::default_host();
    let requested = name.filter(|name| !name.is_empty());
    let device = match requested {
        Some(name) => host
            .output_devices()
            .map_err(|e| format!("cannot list output devices: {e}"))?
            .find(|device| device.name().ok().as_deref() == Some(name))
            .ok_or_else(|| format!("output device \"{name}\" is no longer available"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| "no audio output device".to_string())?,
    };
    OutputStream::try_from_device(&device).map_err(|e| match requested {
        Some(name) => format!("cannot open output device \"{name}\": {e}"),
        None => format!("cannot open the default output device: {e}"),
    })
}

/// Handle to the playback engine. Cheap to clone? No: one handle owns the
/// controller thread; the app keeps it in a single field.
pub struct RadioPlayer {
    tx: Arc<Mutex<Sender<AudioCommand>>>,
    streams: Arc<Mutex<BTreeMap<PlaybackId, Arc<StreamShared>>>>,
    next_id: AtomicU64,
    active: AtomicU64,
    revision: Arc<AtomicU64>,
    device_error: Arc<Mutex<Option<String>>>,
    /// Non-fatal startup message, e.g. a remembered device that disappeared.
    device_notice: Arc<Mutex<Option<String>>>,
    /// Decoded-audio buffer in seconds for streams opened from now on.
    stream_seconds: Arc<AtomicU32>,
    controller: Mutex<Option<JoinHandle<()>>>,
    /// Global Dynamic Boost preset, shared by every stream worker.
    dsp_preset: Arc<AtomicU32>,
}

impl RadioPlayer {
    pub fn new() -> Self {
        Self::with_device(None)
    }

    /// Create the engine on a remembered output device; `None` uses the system
    /// default. A missing device falls back to the default and is reported by
    /// [`Self::take_device_notice`].
    pub fn with_device(initial_device: Option<String>) -> Self {
        let (tx, rx) = mpsc::channel::<AudioCommand>();
        let streams: Arc<Mutex<BTreeMap<PlaybackId, Arc<StreamShared>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let device_error = Arc::new(Mutex::new(None));
        let device_notice = Arc::new(Mutex::new(None));
        let revision = Arc::new(AtomicU64::new(0));

        let controller_streams = Arc::clone(&streams);
        let controller_error = Arc::clone(&device_error);
        let controller_notice = Arc::clone(&device_notice);
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let controller = thread::Builder::new()
            .name("viberadio-audio".to_string())
            .spawn(move || {
                audio_controller(
                    rx,
                    controller_streams,
                    controller_error,
                    controller_notice,
                    initial_device,
                    ready_tx,
                )
            })
            .expect("failed to spawn audio controller thread");
        // Wait for the initial device open so a startup notice is not missed.
        // A device that takes longer than this keeps opening in the background.
        let _ = ready_rx.recv_timeout(Duration::from_secs(3));

        Self {
            tx: Arc::new(Mutex::new(tx)),
            streams,
            next_id: AtomicU64::new(1),
            active: AtomicU64::new(0),
            revision,
            device_error,
            device_notice,
            stream_seconds: Arc::new(AtomicU32::new(DEFAULT_STREAM_BUFFER_SECS)),
            controller: Mutex::new(Some(controller)),
            dsp_preset: Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        }
    }

    /// Start a stream instance and return its handle.
    pub fn play_stream(&self, request: PlayRequest) -> PlaybackId {
        if let Some(stream_id) = request.stream_id {
            log::debug!("starting library stream {stream_id}");
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let shared = Arc::new(StreamShared::new_with_buffer(
            id,
            &request,
            self.device_error.lock().unwrap().clone(),
            Arc::clone(&self.revision),
            Arc::clone(&self.dsp_preset),
            Arc::clone(&self.stream_seconds),
        ));
        self.streams.lock().unwrap().insert(id, Arc::clone(&shared));
        if !request.record_only {
            self.active.store(id, Ordering::SeqCst);
        }
        self.bump();

        let sent = self.tx.lock().map(|tx| {
            tx.send(AudioCommand::Start {
                id,
                request,
                shared: Arc::clone(&shared),
            })
        });
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(e)) => shared.finish_error(format!("audio controller is not running: {e}")),
            Err(_) => shared.finish_error("audio controller lock is poisoned".to_string()),
        }
        id
    }

    /// Stop one stream instance. The entry disappears from [`Self::streams`]
    /// immediately; the worker drops its sink and cancels the download.
    pub fn stop_stream(&self, id: PlaybackId) {
        if let Some(shared) = self.streams.lock().unwrap().remove(&id) {
            shared.cancel();
        }
        self.repick_active();
        self.bump();
    }

    pub fn stop_all(&self) {
        let ids: Vec<PlaybackId> = self.streams.lock().unwrap().keys().copied().collect();
        for id in &ids {
            self.stop_stream(*id);
        }
        self.active.store(0, Ordering::SeqCst);
    }

    /// Enumerate the host's output devices, for the Settings list.
    pub fn output_devices() -> Vec<String> {
        match cpal::default_host().output_devices() {
            Ok(devices) => devices
                .filter_map(|device| device.name().ok())
                .collect::<Vec<_>>(),
            Err(e) => {
                log::warn!("cannot list output devices: {e}");
                Vec::new()
            }
        }
    }

    /// Switch the output device; `None` means the system default.
    ///
    /// The caller must stop its streams first: sinks belong to the old output
    /// stream and go silent when it is dropped. Returns `Ok(None)` when the
    /// request opened, `Ok(Some(warning))` when it fell back to the system
    /// default, and `Err` when nothing opened. If the controller does not answer
    /// within three seconds the caller must assume the old device is still in
    /// use and the switch may complete later.
    pub fn set_output_device(&self, name: Option<String>) -> Result<Option<String>, String> {
        let (reply, reply_rx) = mpsc::channel();
        let sent = self.tx.lock().map(|tx| {
            tx.send(AudioCommand::SetDevice {
                name,
                reply,
            })
        });
        match sent {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("audio controller is not running: {e}")),
            Err(_) => return Err("audio controller lock is poisoned".to_string()),
        }
        match reply_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => result,
            Err(_) => Err("audio controller did not answer the device switch".to_string()),
        }
    }

    /// A non-fatal startup message, such as a remembered device that vanished.
    pub fn take_device_notice(&self) -> Option<String> {
        self.device_notice
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Decoded-audio buffer for streams opened from now on.
    pub fn set_stream_buffer(&self, seconds: u32) {
        let seconds = seconds.clamp(MIN_STREAM_BUFFER_SECS, MAX_STREAM_BUFFER_SECS);
        self.stream_seconds.store(seconds, Ordering::Relaxed);
    }

    fn repick_active(&self) {
        let current = self.active.load(Ordering::SeqCst);
        let streams = self.streams.lock().unwrap();
        if streams.contains_key(&current) {
            return;
        }
        let next = streams
            .iter()
            .rev()
            .find(|(_, shared)| !shared.record_only())
            .map(|(id, _)| *id)
            .unwrap_or(0);
        self.active.store(next, Ordering::SeqCst);
    }

    pub fn set_stream_volume(&self, id: PlaybackId, volume: f32) {
        let volume = volume.clamp(0.0, 1.0);
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.set_volume(volume);
        }
    }

    pub fn set_stream_gain(&self, id: PlaybackId, gain_db: f32) {
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.set_gain_db(gain_db);
        }
    }

    pub fn set_dsp_preset(&self, preset: DspPreset) {
        self.dsp_preset.store(preset as u32, Ordering::Relaxed);
        self.bump();
    }

    pub fn start_recording(
        &self,
        id: PlaybackId,
        config: RecordingConfig,
        station: String,
    ) -> std::io::Result<()> {
        let shared = self
            .streams
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "stream is not open")
            })?;
        shared.start_recording(config, station)
    }

    /// Start a recording from a watched stream's rolling buffer, so the file
    /// includes the song from its title mark.
    pub fn promote_watch(
        &self,
        id: PlaybackId,
        config: crate::recording::RecordingConfig,
        station: String,
        artist: Option<&str>,
        title: Option<&str>,
    ) -> std::io::Result<bool> {
        let shared = self
            .streams
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "stream is not open")
            })?;
        shared.promote_watch(config, station, artist, title)
    }

    /// Resize the watch buffer of a running watched stream.
    pub fn set_watch_capacity(&self, id: PlaybackId, bytes: usize) {
        if let Some(shared) = self.streams.lock().unwrap().get(&id).cloned() {
            shared.set_watch_capacity(bytes);
        }
    }

    /// Close the recording and return a join handle, so the UI can poll instead
    /// of waiting for a slow disk.
    pub fn stop_recording_async(&self, id: PlaybackId) -> Option<JoinHandle<RecordedSummary>> {        let shared = self.streams.lock().unwrap().get(&id).cloned()?;
        shared.stop_recording_async()
    }

    /// Summary of a recording closed by the stream's terminal path.
    pub fn take_finished_recording(&self, id: PlaybackId) -> Option<RecordedSummary> {
        let shared = self.streams.lock().unwrap().get(&id).cloned()?;
        shared.take_finished_recording()
    }

    pub fn set_recording_track(
        &self,
        id: PlaybackId,
        artist: Option<&str>,
        title: Option<&str>,
    ) {
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.set_recording_track(artist, title);
        }
    }

    /// Correct the open split file after a catalog lookup recovered the artist.
    pub fn retitle_recording(&self, id: PlaybackId, from: &str, artist: &str, title: &str) {
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.retitle_recording(from, artist, title);
        }
    }

    /// Close the current split file at the expected end of its track.
    pub fn set_recording_boundary(&self, id: PlaybackId) {
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.recording_boundary();
        }
    }

    /// Files that closed while the recording kept running.
    pub fn take_completed_recordings(&self, id: PlaybackId) -> Vec<CompletedFile> {
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.take_completed_recordings()
        } else {
            Vec::new()
        }
    }

    /// Path of the split file being written, if the stream is recording.
    pub fn current_recording_path(&self, id: PlaybackId) -> Option<std::path::PathBuf> {
        if let Some(shared) = self.streams.lock().unwrap().get(&id) {
            shared.current_recording_path()
        } else {
            None
        }
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    /// Cheap coherent view for one UI frame.
    pub fn snapshot(&self) -> EngineSnapshot {
        // Read membership and the active pointer under one registry lock, so a
        // frame cannot show "Idle" next to a non-empty stream list.
        let (streams, active_id) = {
            let registry = self.streams.lock().unwrap();
            let active_id = self.active.load(Ordering::SeqCst);
            let streams: Vec<StreamSnapshot> =
                registry.values().map(|stream| stream.snapshot()).collect();
            (streams, active_id)
        };
        let active = streams.iter().find(|stream| stream.id == active_id).cloned();
        let revision = self.revision.load(Ordering::SeqCst);
        EngineSnapshot {
            revision,
            active,
            streams,
        }
    }

    /// Make `id` the active stream. Returns false when the stream is gone.
    pub fn set_active(&self, id: PlaybackId) -> bool {
        let streams = self.streams.lock().unwrap();
        if let Some(shared) = streams.get(&id) {
            if shared.record_only() {
                return false;
            }
            self.active.store(id, Ordering::SeqCst);
            drop(streams);
            self.bump();
            true
        } else {
            false
        }
    }

}

impl Default for RadioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RadioPlayer {
    fn drop(&mut self) {
        self.stop_all();
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(AudioCommand::Shutdown);
        }
        if let Some(controller) = self.controller.lock().unwrap().take() {
            let _ = controller.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Engine state shared with the UI
// ---------------------------------------------------------------------------

struct SharedState {
    status: PlaybackStatus,
    error: Option<String>,
    now_playing: Option<String>,
    /// Title the listener is actually hearing. It trails `now_playing` by the
    /// audio still queued in the ring, so the label changes when the new song
    /// reaches the speakers rather than when the download thread reads it.
    display_now_playing: Option<String>,
    /// Title waiting for playback to reach `samples_pushed` at the time it
    /// arrived.
    pending_now_playing: Option<(u64, String)>,
    ring: VecDeque<f32>,
    producer_done: bool,
    last_push: Instant,
}

struct StreamShared {
    id: PlaybackId,
    station_id: Option<Uuid>,
    url: String,
    record_only: bool,
    state: Mutex<SharedState>,
    cond: Condvar,
    cancelled: AtomicBool,
    /// Gain applied by `StreamSource` in the callback, so a volume change is heard
    /// immediately even while the worker is blocked in a stalled read.
    volume: AtomicU32,
    samples_played: AtomicU64,
    samples_pushed: AtomicU64,
    /// Latest 0.5-second RMS window, stored as f32 bits.
    level_rms: AtomicU32,
    /// Increments for every new RMS window.
    level_serial: AtomicU64,
    /// Bitrate the server reported in `icy-br`; 0 means unknown.
    reported_bitrate: AtomicU32,
    /// Incremented for every connection attempt. A `StreamSource` from an older
    /// attempt returns `None` immediately, so it cannot hold the sink queue open
    /// in front of the reconnected source.
    attempt: AtomicU64,
    /// Shared engine revision, bumped on every user-visible state change.
    revision: Arc<AtomicU64>,
    /// Notify for the current download task (set per attempt).
    wake: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Byte buffer of the current attempt, so Stop can unblock a decoder probe.
    buffer: Mutex<Option<Shared>>,
    /// Cancel flag of the current download task, so Stop can unblock a producer
    /// waiting for room in a full byte buffer.
    fetch_cancel: Mutex<Option<Arc<AtomicBool>>>,
    /// Raw recorder attached to this stream. Its writer thread is independent of
    /// decoding, so a slow disk cannot stall playback.
    recording: Mutex<Option<Recording>>,
    /// Summary produced when a recording was closed by the stream's terminal
    /// path. The app drains it so a stream that dies mid-recording still shows
    /// dropped-audio or error flags.
    finished_recording: Mutex<Option<RecordedSummary>>,
    /// Per-station gain trim, read by the worker for every batch.
    gain_db: AtomicU32,
    /// Global Dynamic Boost preset, shared with the engine.
    dsp_preset: Arc<AtomicU32>,
    /// Decoded-audio buffer setting, shared with the engine.
    stream_seconds: Arc<AtomicU32>,
    /// Samples the ring may hold, derived from the format and the setting.
    ring_limit: AtomicUsize,
    /// Rolling buffer for a watched station, when enabled.
    watch: Mutex<Option<WatchBuffer>>,
    /// Whether this stream is a watch stream.
    watching: bool,
}

/// Rolling compressed-audio buffer for a watched station. A mark records where
/// a song's title arrived, so a wish can start recording from that point.
struct WatchBuffer {
    data: VecDeque<u8>,
    /// Absolute byte offset of `data.front()`.
    base: u64,
    /// Absolute byte offset of the next byte to push.
    end: u64,
    marks: VecDeque<(u64, String)>,
    cap: usize,
}

impl WatchBuffer {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_capacity(WATCH_BUFFER_BYTES)
    }

    fn with_capacity(cap: usize) -> Self {
        Self {
            data: VecDeque::new(),
            base: 0,
            end: 0,
            marks: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Resize the rolling window, dropping the oldest audio and any marks that
    /// fall outside it.
    fn set_capacity(&mut self, cap: usize) {
        self.cap = cap.max(1);
        if self.data.len() > self.cap {
            let drop = self.data.len() - self.cap;
            self.data.drain(..drop);
            self.base += drop as u64;
        }
        while let Some((at, _)) = self.marks.front() {
            if *at < self.base {
                self.marks.pop_front();
            } else {
                break;
            }
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.data.extend(bytes.iter().copied());
        self.end += bytes.len() as u64;
        if self.data.len() > self.cap {
            let drop = self.data.len() - self.cap;
            self.data.drain(..drop);
            self.base += drop as u64;
        }
        while let Some((at, _)) = self.marks.front() {
            if *at < self.base {
                self.marks.pop_front();
            } else {
                break;
            }
        }
    }

    fn mark(&mut self, label: String) {
        self.marks.push_back((self.end, label));
        while let Some((at, _)) = self.marks.front() {
            if *at < self.base {
                self.marks.pop_front();
            } else {
                break;
            }
        }
    }

    /// Audio from the latest title mark to now, plus that title.
    fn take_since_mark(&mut self) -> (Vec<u8>, Option<String>) {
        let (start, label) = match self.marks.back() {
            Some((at, label)) => (*at, Some(label.clone())),
            None => (self.base, None),
        };
        let start = start.max(self.base);
        let offset = (start - self.base) as usize;
        let audio = self.data.iter().skip(offset).copied().collect();
        (audio, label)
    }
}

impl StreamShared {
    #[cfg(test)]
    fn new(
        id: PlaybackId,
        request: &PlayRequest,
        device_error: Option<String>,
        revision: Arc<AtomicU64>,
        dsp_preset: Arc<AtomicU32>,
    ) -> Self {
        Self::new_with_buffer(
            id,
            request,
            device_error,
            revision,
            dsp_preset,
            Arc::new(AtomicU32::new(DEFAULT_STREAM_BUFFER_SECS)),
        )
    }

    fn new_with_buffer(
        id: PlaybackId,
        request: &PlayRequest,
        device_error: Option<String>,
        revision: Arc<AtomicU64>,
        dsp_preset: Arc<AtomicU32>,
        stream_seconds: Arc<AtomicU32>,
    ) -> Self {
        let initial_status = if device_error.is_some() {
            PlaybackStatus::Error
        } else {
            PlaybackStatus::Connecting
        };
        Self {
            id,
            station_id: request.station_id,
            url: request.url.clone(),
            record_only: request.record_only,
            state: Mutex::new(SharedState {
                status: initial_status,
                error: device_error,
                now_playing: None,
                display_now_playing: None,
                pending_now_playing: None,
                ring: VecDeque::with_capacity(RING_SAMPLES),
                producer_done: false,
                last_push: Instant::now(),
            }),
            cond: Condvar::new(),
            cancelled: AtomicBool::new(false),
            volume: AtomicU32::new(request.volume.clamp(0.0, 1.0).to_bits()),
            samples_played: AtomicU64::new(0),
            samples_pushed: AtomicU64::new(0),
            level_rms: AtomicU32::new(0.0f32.to_bits()),
            level_serial: AtomicU64::new(0),
            reported_bitrate: AtomicU32::new(0),
            attempt: AtomicU64::new(0),
            revision,
            wake: Mutex::new(None),
            buffer: Mutex::new(None),
            fetch_cancel: Mutex::new(None),
            recording: Mutex::new(None),
            finished_recording: Mutex::new(None),
            gain_db: AtomicU32::new(request.gain_db.clamp(-12.0, 12.0).to_bits()),
            dsp_preset,
            stream_seconds,
            ring_limit: AtomicUsize::new(RING_SAMPLES),
            watch: Mutex::new(
                request
                    .watch
                    .then(|| WatchBuffer::with_capacity(request.watch_buffer_bytes)),
            ),
            watching: request.watch,
        }
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    fn set_status(&self, status: PlaybackStatus) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.status == status {
            return;
        }
        state.status = status;
        drop(state);
        self.bump();
    }

    fn set_reconnecting(&self, error: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.status = PlaybackStatus::Connecting;
        state.error = Some(error.to_string());
        drop(state);
        self.bump();
    }

    fn finish(&self, status: PlaybackStatus, error: Option<String>) {
        // Take the recorder first and release the mutex before joining: the UI
        // reads `recording` in `snapshot`, and a slow disk must not freeze it.
        let recording = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(recording) = recording {
            let summary = recording.finish();
            *self
                .finished_recording
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(summary);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.status = status;
        state.error = error;
        state.producer_done = true;
        drop(state);
        self.bump();
    }

    fn take_finished_recording(&self) -> Option<RecordedSummary> {
        self.finished_recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn finish_error(&self, error: String) {
        self.finish(PlaybackStatus::Error, Some(error));
    }

    fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.bump();
    }

    fn gain_db(&self) -> f32 {
        f32::from_bits(self.gain_db.load(Ordering::Relaxed))
    }

    fn set_gain_db(&self, gain_db: f32) {
        self.gain_db.store(
            gain_db.clamp(-12.0, 12.0).to_bits(),
            Ordering::Relaxed,
        );
        self.bump();
    }

    fn dsp_preset(&self) -> DspPreset {
        match self.dsp_preset.load(Ordering::Relaxed) {
            1 => DspPreset::Light,
            2 => DspPreset::Medium,
            3 => DspPreset::Strong,
            _ => DspPreset::Off,
        }
    }

    /// Size the decoded ring from this stream's format and the user setting.
    ///
    /// A zero rate or channel count falls back to the default ring rather than
    /// sizing from garbage, and the result is capped so a hostile header cannot
    /// ask for an enormous allocation.
    fn set_ring_limit(&self, sample_rate: u32, channels: u16) {
        let limit = ring_limit_for(
            sample_rate,
            channels,
            self.stream_seconds.load(Ordering::Relaxed),
        );
        self.ring_limit.store(limit, Ordering::Relaxed);
    }

    fn ring_limit(&self) -> usize {
        self.ring_limit.load(Ordering::Relaxed)
    }

    /// Drop the oldest buffered samples beyond one second. Called when the
    /// preset changes, so a stale envelope is not heard for the whole ring.
    fn flush_ring_if_long(&self, sample_rate: u32, channels: u16) {
        let keep = (sample_rate as usize).saturating_mul(channels as usize).max(1);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let excess = state.ring.len().saturating_sub(keep);
        if excess == 0 {
            return;
        }
        state.ring.drain(..excess);
        drop(state);
        self.samples_pushed
            .fetch_sub(excess as u64, Ordering::Relaxed);
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Cancel the worker, the decoder probe and the download task at once.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(wake) = self.wake.lock().unwrap().as_ref() {
            wake.notify_one();
        }
        if let Some(buffer) = self.buffer.lock().unwrap().as_ref() {
            cancel_buffer(buffer);
        }
        if let Some(cancel) = self.fetch_cancel.lock().unwrap().as_ref() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    fn register_attempt(
        &self,
        wake: Arc<tokio::sync::Notify>,
        buffer: Shared,
        fetch_cancel: Arc<AtomicBool>,
    ) {
        *self.wake.lock().unwrap() = Some(wake);
        *self.buffer.lock().unwrap() = Some(buffer);
        *self.fetch_cancel.lock().unwrap() = Some(fetch_cancel);
    }

    fn unregister_attempt(&self) {
        *self.wake.lock().unwrap() = None;
        *self.buffer.lock().unwrap() = None;
        *self.fetch_cancel.lock().unwrap() = None;
    }

    /// Start a new connection attempt: clears the ring and invalidates the
    /// previous attempt's `StreamSource`.
    fn begin_attempt(&self) -> u64 {
        let attempt = self.attempt.fetch_add(1, Ordering::SeqCst) + 1;
        // Any samples pushed by a previous attempt that never played must not make
        // the next "played out" wait compare against a permanently stale count.
        self.samples_pushed
            .store(self.samples_played.load(Ordering::Relaxed), Ordering::Relaxed);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.ring.clear();
        state.producer_done = false;
        state.last_push = Instant::now();
        state.now_playing = None;
        state.display_now_playing = None;
        state.pending_now_playing = None;
        drop(state);
        self.bump();
        attempt
    }

    /// No more samples will be produced for this attempt. When the ring is also
    /// empty, the sink has played everything that was decoded.
    fn mark_producer_done(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .producer_done = true;
    }

    /// Wait until the callback has consumed every sample that was pushed (the
    /// ring can be empty while `StreamSource` still holds a local chunk).
    fn wait_played_out(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.samples_played.load(Ordering::Relaxed)
                >= self.samples_pushed.load(Ordering::Relaxed)
            {
                return true;
            }
            if self.is_cancelled() || Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Push a batch of decoded samples, blocking while the ring is full so the
    /// decoder (and therefore the network) is back-pressured instead of buffering
    /// a multi-hour stream in memory.
    fn push_batch(&self, samples: &[f32]) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for sample in samples {
            while state.ring.len() >= self.ring_limit() {
                if self.is_cancelled() {
                    return false;
                }
                let (next, _) = match self.cond.wait_timeout(state, Duration::from_millis(50)) {
                    Ok(pair) => pair,
                    Err(e) => e.into_inner(),
                };
                state = next;
            }
            state.ring.push_back(*sample);
        }
        state.last_push = Instant::now();
        let became_live = state.status != PlaybackStatus::Live
            && state.ring.len() >= PREBUFFER_SAMPLES;
        if became_live {
            state.status = PlaybackStatus::Live;
            state.error = None;
        }
        drop(state);
        if became_live {
            self.bump();
        }
        self.samples_pushed
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
        true
    }

    /// Record a new ICY title for the current attempt. A late download thread
    /// from a previous attempt cannot overwrite the new connection's title.
    fn set_now_playing(&self, attempt: u64, title: String) {
        if title.is_empty() || self.attempt.load(Ordering::SeqCst) != attempt {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.now_playing.as_deref() == Some(title.as_str()) {
            return;
        }
        state.now_playing = Some(title.clone());
        // The title's audio starts at the current write position. Hold it back
        // until the callback has played everything queued before it, so the UI
        // and the speakers change together.
        let at = self.samples_pushed.load(Ordering::Relaxed);
        state.pending_now_playing = Some((at, title.clone()));
        drop(state);
        // Split recording follows the same ICY title that the UI shows. The
        // default format is used here; the app can still override via
        // `set_recording_track` before the first audio arrives.
        let format = crate::metadata::default_tag_format(&title);
        let tag = crate::metadata::split_title(&title, format);
        self.set_recording_track(tag.artist.as_deref(), tag.title.as_deref());
        self.bump();
    }

    fn start_recording(&self, config: RecordingConfig, station: String) -> std::io::Result<()> {
        let mut slot = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "already recording this stream",
            ));
        }
        *slot = Some(Recording::start(config, station, chrono::Local::now())?);
        drop(slot);
        self.bump();
        Ok(())
    }

    fn stop_recording_async(&self) -> Option<JoinHandle<RecordedSummary>> {
        let recording = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let handle = recording.map(Recording::finish_async);
        if handle.is_some() {
            self.bump();
        }
        handle
    }

    #[cfg(test)]
    fn stop_recording(&self) -> Option<RecordedSummary> {
        self.stop_recording_async()?.join().ok()
    }

    fn set_recording_track(&self, artist: Option<&str>, title: Option<&str>) {
        if let Some(recording) = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            recording.set_track(artist, title);
        }
    }

    fn retitle_recording(&self, from: &str, artist: &str, title: &str) {
        if let Some(recording) = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            recording.retitle(from, artist, title);
        }
    }

    fn recording_boundary(&self) {
        if let Some(recording) = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            recording.boundary();
        }
    }

    fn take_completed_recordings(&self) -> Vec<CompletedFile> {
        if let Some(recording) = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            recording.take_completed()
        } else {
            Vec::new()
        }
    }

    fn current_recording_path(&self) -> Option<std::path::PathBuf> {
        self.recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|recording| recording.current_path())
    }

    fn record_audio(&self, audio: &[u8]) {
        let recording = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = recording.as_ref() {
            recording.write(audio);
        }
        if let Some(watch) = watch.as_mut() {
            watch.push(audio);
        }
    }

    fn watch_title(&self, title: &str) {
        if let Some(watch) = self
            .watch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            watch.mark(title.to_string());
        }
    }

    /// Start a recording from the buffered audio of the current song. The
    /// pre-roll is written before any live audio, so the file starts at the
    /// title mark rather than at the trigger.
    fn promote_watch(
        &self,
        config: crate::recording::RecordingConfig,
        station: String,
        artist: Option<&str>,
        title: Option<&str>,
    ) -> std::io::Result<bool> {
        let mut recording = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if recording.is_some() {
            return Ok(false);
        }
        let handle =
            crate::recording::Recording::start(config, station, chrono::Local::now())?;
        handle.set_track(artist, title);
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(watch) = watch.as_mut() {
            let (pre_roll, _) = watch.take_since_mark();
            if !pre_roll.is_empty() {
                handle.write(&pre_roll);
            }
        }
        *recording = Some(handle);
        Ok(true)
    }

    /// Resize the rolling buffer of a watched stream. Existing audio is kept
    /// up to the new limit; the oldest bytes are dropped when it shrinks.
    fn set_watch_capacity(&self, bytes: usize) {
        let mut watch = self.watch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(buffer) = watch.as_mut() {
            buffer.set_capacity(bytes);
        }
    }

    fn snapshot(&self) -> StreamSnapshot {
        let played = self.samples_played.load(Ordering::Relaxed);
        let (rms, rms_serial) = self.level();
        let (status, error, now_playing, display_now_playing) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let due = state
                .pending_now_playing
                .as_ref()
                .map(|(at, _)| played >= *at)
                .unwrap_or(false);
            if due {
                if let Some((_, title)) = state.pending_now_playing.take() {
                    state.display_now_playing = Some(title);
                }
            }
            (
                state.status,
                state.error.clone(),
                state.now_playing.clone(),
                state.display_now_playing.clone(),
            )
        };
        let recording = self
            .recording
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false);
        StreamSnapshot {
            id: self.id,
            station_id: self.station_id,
            url: self.url.clone(),
            status,
            status_text: status_text(&self.url, status, error.as_deref()),
            now_playing,
            display_now_playing,
            error,
            volume: self.volume(),
            samples_played: self.samples_played.load(Ordering::Relaxed),
            recording,
            rms,
            rms_serial,
            record_only: self.record_only,
            watching: self.watching,
            reported_bitrate: self.reported_bitrate(),
        }
    }

    fn set_level(&self, rms: f32) {
        self.level_rms.store(rms.to_bits(), Ordering::Relaxed);
        self.level_serial.fetch_add(1, Ordering::Relaxed);
    }

    fn level(&self) -> (f32, u64) {
        (
            f32::from_bits(self.level_rms.load(Ordering::Relaxed)),
            self.level_serial.load(Ordering::Relaxed),
        )
    }

    fn set_reported_bitrate(&self, bitrate: u32) {
        if bitrate > 0 {
            self.reported_bitrate.store(bitrate, Ordering::Relaxed);
        }
    }

    fn reported_bitrate(&self) -> Option<u32> {
        match self.reported_bitrate.load(Ordering::Relaxed) {
            0 => None,
            bitrate => Some(bitrate),
        }
    }

    fn record_only(&self) -> bool {
        self.record_only
    }
}

fn status_text(url: &str, status: PlaybackStatus, error: Option<&str>) -> String {
    match status {
        PlaybackStatus::Connecting => match error {
            Some(error) => format!("Reconnecting after {error}"),
            None => "Connecting…".to_string(),
        },
        PlaybackStatus::Buffering => format!("Buffering {url}"),
        PlaybackStatus::Live => format!("Playing {url}"),
        PlaybackStatus::Error => format!(
            "Stopped: {}",
            error.unwrap_or("unknown error")
        ),
        PlaybackStatus::Stopped => "Stopped (stream ended)".to_string(),
    }
}

/// The audio callback side of one stream: non-blocking reads from the shared ring.
/// Samples the decoded ring may hold for a format and a buffer setting.
fn ring_limit_for(sample_rate: u32, channels: u16, seconds: u32) -> usize {
    if sample_rate == 0 || channels == 0 {
        return RING_SAMPLES;
    }
    let seconds = seconds.clamp(MIN_STREAM_BUFFER_SECS, MAX_STREAM_BUFFER_SECS) as usize;
    (sample_rate as usize)
        .saturating_mul(channels as usize)
        .saturating_mul(seconds)
        .clamp(PREBUFFER_SAMPLES, RING_SAMPLES * 8)
}

struct StreamSource {
    shared: Arc<StreamShared>,
    channels: u16,
    sample_rate: u32,
    local: VecDeque<f32>,
    attempt: u64,
    last_stall_check: Instant,
    /// How many silence samples have been emitted since the ring was last seen
    /// empty. Avoids taking the state lock on every sample during an underrun.
    silent_run: u32,
}

impl StreamSource {
    fn new(shared: Arc<StreamShared>, channels: u16, sample_rate: u32, attempt: u64) -> Self {
        Self {
            shared,
            channels: channels.max(1),
            sample_rate: sample_rate.max(1),
            local: VecDeque::with_capacity(SOURCE_CHUNK),
            attempt,
            last_stall_check: Instant::now(),
            silent_run: 0,
        }
    }
}

impl Iterator for StreamSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        // An atomic generation check: a reconnect must invalidate stale buffered
        // samples immediately, not after the local chunk drains.
        if self.shared.attempt.load(Ordering::Relaxed) != self.attempt {
            self.local.clear();
            return None;
        }

        let volume = self.shared.volume();

        if let Some(sample) = self.local.pop_front() {
            self.shared.samples_played.fetch_add(1, Ordering::Relaxed);
            return Some(sample * volume);
        }

        // While starved, re-check the ring only every 1024 samples; otherwise the
        // callback would lock the shared state 48k-96k times a second.
        if self.silent_run > 0 && self.silent_run < 1024 {
            self.silent_run += 1;
            return Some(0.0);
        }
        self.silent_run = 0;

        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.ring.is_empty() {
            if state.producer_done {
                return None;
            }
            // Underrun: the callback must never wait, so emit silence. Report a
            // stall at most once a second, and never hold the state lock while
            // taking it again in `set_status`.
            let stalled = state.last_push.elapsed() > Duration::from_secs(1);
            drop(state);
            if stalled && self.last_stall_check.elapsed() > Duration::from_secs(1) {
                self.last_stall_check = Instant::now();
                self.shared.set_status(PlaybackStatus::Buffering);
            }
            self.silent_run = 1;
            return Some(0.0);
        }

        let take = state.ring.len().min(SOURCE_CHUNK);
        self.local.extend(state.ring.drain(..take));
        drop(state);
        // A producer blocked on a full ring waits on this condvar.
        self.shared.cond.notify_one();

        let sample = self.local.pop_front()?;
        self.shared.samples_played.fetch_add(1, Ordering::Relaxed);
        Some(sample * volume)
    }
}

impl Source for StreamSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

// ---------------------------------------------------------------------------
// Audio controller thread
// ---------------------------------------------------------------------------

fn audio_controller(
    rx: Receiver<AudioCommand>,
    streams: Arc<Mutex<BTreeMap<PlaybackId, Arc<StreamShared>>>>,
    device_error: Arc<Mutex<Option<String>>>,
    device_notice: Arc<Mutex<Option<String>>>,
    initial_device: Option<String>,
    ready: Sender<()>,
) {
    // The OutputStream is `!Send`, so it lives here for the process lifetime.
    // Sinks created from its handle are `Send` and are moved to stream workers.
    let requested = initial_device
        .as_deref()
        .filter(|name| !name.is_empty());
    let (mut output, choice) = choose_output(requested, open_output);
    match choice {
        // A remembered device can disappear (unplugged headset, new login).
        // Keep the app usable on the system default and report the reason.
        OutputChoice::Opened => {}
        OutputChoice::FellBack(e) => {
            log::warn!("audio: {e}; using the system default");
            *device_notice.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
        }
        OutputChoice::Failed(message) => {
            log::error!("audio: {message}");
            *device_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(message);
            output = None;
        }
    }
    // Tell the engine constructor that the initial device is settled, so the
    // startup notice can be read without racing the open.
    let _ = ready.send(());

    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while let Ok(command) = rx.recv() {
        match command {
            AudioCommand::Start {
                id,
                request,
                shared,
            } => {
                if request.record_only {
                    let worker = thread::Builder::new()
                        .name(format!("viberadio-record-{id}"))
                        .spawn(move || record_worker(id, request, shared));
                    match worker {
                        Ok(worker) => workers.push(worker),
                        Err(e) => log::error!("could not spawn record worker: {e}"),
                    }
                    workers.retain(|worker| !worker.is_finished());
                    continue;
                }
                if let Some(error) = device_error.lock().unwrap_or_else(|p| p.into_inner()).clone() {
                    shared.finish_error(error);
                    continue;
                }
                let Some((_, handle)) = output.as_ref() else {
                    shared.finish_error("no audio output device".to_string());
                    continue;
                };
                let sink = match Sink::try_new(handle) {
                    Ok(sink) => sink,
                    Err(e) => {
                        shared.finish_error(format!("cannot open audio sink: {e}"));
                        continue;
                    }
                };
                let worker_streams = Arc::clone(&streams);
                match thread::Builder::new()
                    .name(format!("viberadio-stream-{id}"))
                    .spawn(move || stream_worker(sink, id, request, shared, worker_streams))
                {
                    Ok(worker) => workers.push(worker),
                    Err(e) => {
                        log::error!("could not spawn stream worker: {e}");
                    }
                }
                workers.retain(|worker| !worker.is_finished());
            }
            AudioCommand::SetDevice { name, reply } => {
                // Drop the old stream first. Sinks made from its handle go
                // silent, which is why the caller stops its streams first.
                drop(output.take());
                let requested = name.as_deref().filter(|name| !name.is_empty());
                let (next, choice) = choose_output(requested, open_output);
                let result = match choice {
                    OutputChoice::Opened => Ok(None),
                    OutputChoice::FellBack(e) => Ok(Some(e)),
                    OutputChoice::Failed(message) => {
                        *device_error.lock().unwrap_or_else(|p| p.into_inner()) =
                            Some(message.clone());
                        Err(message)
                    }
                };
                output = next;
                // Any working output clears a previous fatal device error, even
                // when the request fell back to the system default.
                if output.is_some() {
                    *device_error.lock().unwrap_or_else(|p| p.into_inner()) = None;
                }
                let _ = reply.send(result);
            }
            AudioCommand::Shutdown => break,
        }
    }

    // Stop any worker that did not get a Stop command (e.g. the engine was
    // dropped without stop_all). Cancelling makes the workers return promptly.
    for shared in streams.lock().unwrap().values() {
        shared.cancel();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    for worker in workers {
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            // Never block shutdown on a worker that ignored cancellation; the
            // process is exiting anyway.
            log::warn!("stream worker did not stop within 2s; detaching it");
        }
    }
}

// ---------------------------------------------------------------------------
// Per-stream worker
// ---------------------------------------------------------------------------

enum AttemptEnd {
    Cancelled,
    Ended { samples: u64 },
    Failed { error: String, samples: u64 },
}

fn stream_worker(
    sink: Sink,
    _id: PlaybackId,
    request: PlayRequest,
    shared: Arc<StreamShared>,
    _streams: Arc<Mutex<BTreeMap<PlaybackId, Arc<StreamShared>>>>,
) {
    let mut attempt = 0u32;

    loop {
        if shared.is_cancelled() {
            shared.finish(PlaybackStatus::Stopped, None);
            return;
        }
        let attempt_id = shared.begin_attempt();

        let end = run_attempt(&sink, &request, &shared, attempt_id);

        match end {
            AttemptEnd::Cancelled => {
                sink.pause();
                shared.finish(PlaybackStatus::Stopped, None);
                return;
            }
            AttemptEnd::Ended { samples } => {
                if samples > 0 {
                    // A stream that actually played resets the outage budget, so a
                    // station dropping once every few hours never exhausts it.
                    attempt = 0;
                }
                let Some(policy) = request.reconnect else {
                    // Let the sink play out the samples already decoded; closing
                    // the worker now would cut off the end of a finite stream.
                    shared.mark_producer_done();
                    shared.wait_played_out(Duration::from_secs(30));
                    thread::sleep(Duration::from_millis(20));
                    sink.pause();
                    shared.finish(PlaybackStatus::Stopped, None);
                    return;
                };
                // Preserve the tail of the dropped stream before reconnecting.
                shared.mark_producer_done();
                shared.wait_played_out(Duration::from_secs(10));
                thread::sleep(Duration::from_millis(20));
                sink.pause();
                if shared.is_cancelled() {
                    shared.finish(PlaybackStatus::Stopped, None);
                    return;
                }
                if attempt >= policy.max_attempts {
                    shared.finish(
                        PlaybackStatus::Error,
                        Some("stream kept ending; reconnect attempts exhausted".to_string()),
                    );
                    return;
                }
                attempt += 1;
                shared.set_reconnecting("stream ended");
                if !sleep_cancellable(&shared, backoff(&policy, attempt)) {
                    shared.finish(PlaybackStatus::Stopped, None);
                    return;
                }
            }
            AttemptEnd::Failed { error, samples } => {
                if samples > 0 {
                    attempt = 0;
                }
                // Samples already decoded are valid even when the connection
                // failed, so play them out before going quiet.
                shared.mark_producer_done();
                shared.wait_played_out(Duration::from_secs(10));
                thread::sleep(Duration::from_millis(20));
                let Some(policy) = request.reconnect else {
                    sink.pause();
                    shared.finish(PlaybackStatus::Error, Some(error));
                    return;
                };
                sink.pause();
                if shared.is_cancelled() {
                    shared.finish(PlaybackStatus::Stopped, None);
                    return;
                }
                if attempt >= policy.max_attempts {
                    shared.finish(
                        PlaybackStatus::Error,
                        Some(format!("{error} (reconnect attempts exhausted)")),
                    );
                    return;
                }
                attempt += 1;
                shared.set_reconnecting(&error);
                if !sleep_cancellable(&shared, backoff(&policy, attempt)) {
                    shared.finish(PlaybackStatus::Stopped, None);
                    return;
                }
            }
        }
    }
}

/// Record a stream without decoding or playing it. The download thread feeds
/// the recorder and updates ICY titles; the reconnect policy matches playback.
fn record_worker(_id: PlaybackId, request: PlayRequest, shared: Arc<StreamShared>) {
    let mut attempt = 0u32;
    loop {
        if shared.is_cancelled() {
            shared.finish(PlaybackStatus::Stopped, None);
            return;
        }
        let attempt_id = shared.begin_attempt();
        let buffer = new_shared();
        let wake = Arc::new(tokio::sync::Notify::new());
        let cancel_fetch = Arc::new(AtomicBool::new(false));
        shared.register_attempt(
            Arc::clone(&wake),
            Arc::clone(&buffer),
            Arc::clone(&cancel_fetch),
        );
        if shared.is_cancelled() {
            cancel_fetch.store(true, Ordering::SeqCst);
            wake.notify_one();
            cancel_buffer(&buffer);
            shared.unregister_attempt();
            shared.finish(PlaybackStatus::Stopped, None);
            return;
        }
        let download = {
            let owner = Arc::clone(&shared);
            let buffer = Arc::clone(&buffer);
            let wake = Arc::clone(&wake);
            let cancel = Arc::clone(&cancel_fetch);
            let url = request.url.clone();
            thread::Builder::new()
                .name("viberadio-record-download".to_string())
                .spawn(move || download_into(owner, buffer, url, cancel, wake, attempt_id))
        };
        let download = match download {
            Ok(handle) => handle,
            Err(e) => {
                shared.unregister_attempt();
                shared.finish(
                    PlaybackStatus::Error,
                    Some(format!("cannot start download: {e}")),
                );
                return;
            }
        };
        let _ = download.join();
        shared.unregister_attempt();
        if shared.is_cancelled() {
            shared.finish(PlaybackStatus::Stopped, None);
            return;
        }
        let error = buffer_error_text(&buffer);
        let Some(policy) = request.reconnect else {
            shared.finish(
                if error.is_some() {
                    PlaybackStatus::Error
                } else {
                    PlaybackStatus::Stopped
                },
                error,
            );
            return;
        };
        if attempt >= policy.max_attempts {
            shared.finish(
                PlaybackStatus::Error,
                Some(error.unwrap_or_else(|| "stream kept ending".to_string())),
            );
            return;
        }
        attempt += 1;
        shared.set_reconnecting(error.as_deref().unwrap_or("stream ended"));
        if !sleep_cancellable(&shared, backoff(&policy, attempt)) {
            shared.finish(PlaybackStatus::Stopped, None);
            return;
        }
    }
}

fn backoff(policy: &ReconnectPolicy, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.saturating_sub(1).min(20)).unwrap_or(u32::MAX);
    policy
        .initial_delay
        .saturating_mul(factor)
        .min(policy.max_delay)
}

fn sleep_cancellable(shared: &StreamShared, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if shared.is_cancelled() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        thread::sleep(left.min(Duration::from_millis(50)));
    }
    !shared.is_cancelled()
}

/// Open a decoder with a bounded format probe.
///
/// The probe budget starts at the first byte and is cleared once a backend is
/// open, so an unrecognised stream fails in seconds while a slow connect or a
/// normal playback read keeps the full stall timeout.
fn open_decoder(shared: &Shared) -> Result<Box<dyn decode::StreamDecoder>, String> {
    let decoded = decode::open_with(|budget| {
        Box::new(ProgressiveBuffer::probing(Arc::clone(shared), budget))
    });
    clear_probe_deadline(shared);
    decoded
}

fn run_attempt(
    sink: &Sink,
    request: &PlayRequest,
    shared: &Arc<StreamShared>,
    attempt_id: u64,
) -> AttemptEnd {
    let buffer = new_shared();
    let wake = Arc::new(tokio::sync::Notify::new());
    let cancel_fetch = Arc::new(AtomicBool::new(false));
    shared.register_attempt(
        Arc::clone(&wake),
        Arc::clone(&buffer),
        Arc::clone(&cancel_fetch),
    );
    // Stop may have landed between the check in the worker loop and the
    // registration above; in that case no task is watching the cancel flag yet.
    if shared.is_cancelled() {
        cancel_fetch.store(true, Ordering::SeqCst);
        wake.notify_one();
        cancel_buffer(&buffer);
        shared.unregister_attempt();
        return AttemptEnd::Cancelled;
    }
    let download = {
        let owner = Arc::clone(shared);
        let buffer = Arc::clone(&buffer);
        let wake = Arc::clone(&wake);
        let cancel = Arc::clone(&cancel_fetch);
        let url = request.url.clone();
        thread::Builder::new()
            .name("viberadio-download".to_string())
            .spawn(move || download_into(owner, buffer, url, cancel, wake, attempt_id))
    };

    let download = match download {
        Ok(handle) => handle,
        Err(e) => {
            shared.unregister_attempt();
            return AttemptEnd::Failed {
                error: format!("cannot start download: {e}"),
                samples: 0,
            };
        }
    };

    // Blocks this worker (never the audio callback) until the container is
    // identified or the probe fails. Stop unblocks it through `cancel_buffer`.
    let decoder = match open_decoder(&buffer) {
        Ok(decoder) => decoder,
        Err(e) => {
            cancel_fetch.store(true, Ordering::SeqCst);
            wake.notify_one();
            cancel_buffer(&buffer);
            wait_for_download(download, Duration::from_secs(1));
            shared.unregister_attempt();
            if shared.is_cancelled() {
                return AttemptEnd::Cancelled;
            }
            let error = buffer_error_text(&buffer)
                .unwrap_or_else(|| format!("unsupported stream: {e}"));
            return AttemptEnd::Failed { error, samples: 0 };
        }
    };

    set_sliding(&buffer, true);
    shared.set_status(PlaybackStatus::Buffering);

    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    shared.set_ring_limit(sample_rate, channels);
    let mut dsp = Dsp::new(
        shared.dsp_preset(),
        shared.gain_db(),
        sample_rate,
        channels,
    );
    let source = StreamSource::new(Arc::clone(shared), channels, sample_rate, attempt_id);
    sink.append(source);
    sink.play();

    let mut samples = decoder;
    let mut samples_pushed = 0u64;
    let level_target = (sample_rate as u64 * channels as u64) / 2;
    let mut level_sum = 0.0f64;
    let mut level_count = 0u64;
    loop {
        if shared.is_cancelled() {
            break;
        }

        let mut batch = Vec::with_capacity(DECODE_BATCH);
        while batch.len() < DECODE_BATCH {
            match samples.next() {
                Some(sample) => batch.push(sample),
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        let preset = shared.dsp_preset();
        if preset != dsp.preset() {
            dsp.set_preset(preset);
            shared.flush_ring_if_long(sample_rate, channels);
        }
        let trim = shared.gain_db();
        if (trim - dsp.trim_db()).abs() > 0.001 {
            dsp.set_trim_db(trim);
            // The ring is upstream of the DSP, so a trim change would otherwise
            // be heard only after up to four seconds of already-processed audio.
            shared.flush_ring_if_long(sample_rate, channels);
        }
        for sample in &batch {
            level_sum += (*sample as f64) * (*sample as f64);
        }
        level_count += batch.len() as u64;
        if level_count >= level_target {
            shared.set_level((level_sum / level_count as f64).sqrt() as f32);
            level_sum = 0.0;
            level_count = 0;
        }
        dsp.process(&mut batch);
        if !shared.push_batch(&batch) {
            break;
        }
        samples_pushed += batch.len() as u64;
    }

    cancel_fetch.store(true, Ordering::SeqCst);
    wake.notify_one();
    cancel_buffer(&buffer);
    wait_for_download(download, Duration::from_secs(1));
    shared.unregister_attempt();

    if shared.is_cancelled() {
        return AttemptEnd::Cancelled;
    }
    match buffer_error_text(&buffer) {
        Some(error) => AttemptEnd::Failed {
            error,
            samples: samples_pushed,
        },
        None => AttemptEnd::Ended {
            samples: samples_pushed,
        },
    }
}

/// Wait briefly for a cancelled download thread to finish without blocking the
/// worker indefinitely on a DNS/connect attempt.
fn wait_for_download(handle: JoinHandle<()>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        log::warn!("download thread did not stop within {timeout:?}; detaching it");
    }
}

// ---------------------------------------------------------------------------
// Shared download buffer (sliding window)
// ---------------------------------------------------------------------------

struct BufferState {
    content_type: Option<String>,
    data: Vec<u8>,
    /// Probe budget for the current backend. It is armed by the first byte so
    /// connect, DNS and header latency do not count against the probe.
    probe_budget: Option<Duration>,
    /// Set when `probe_budget` is armed; a read fails once this instant passes.
    probe_deadline: Option<Instant>,
    /// Absolute stream offset of `data[0]`; lets a sliding buffer keep `pos` in
    /// absolute coordinates while dropping consumed bytes.
    base: u64,
    eof: bool,
    error: Option<String>,
    sliding: bool,
}

type Shared = Arc<(Mutex<BufferState>, Condvar)>;
fn new_shared() -> Shared {
    Arc::new((
        Mutex::new(BufferState {
            content_type: None,
            data: Vec::new(),
            probe_budget: None,
            probe_deadline: None,
            base: 0,
            eof: false,
            error: None,
            sliding: false,
        }),
        Condvar::new(),
    ))
}

/// Reader over a growing network buffer. `read` blocks until data is available,
/// the stream ends, the download fails, or the stream stalls.
struct ProgressiveBuffer {
    shared: Shared,
    pos: u64,
}

impl ProgressiveBuffer {
    fn new(shared: Shared) -> Self {
        Self { shared, pos: 0 }
    }

    /// A reader that gives up if the format is not identified within `budget`.
    ///
    /// The deadline lives in the shared buffer so the worker can clear it after
    /// `decode::open_with` returns; a fresh reader for the fallback backend
    /// resets it, so each probe gets its own window.
    fn probing(shared: Shared, budget: Duration) -> Self {
        {
            let (lock, _) = &*shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.probe_budget = Some(budget);
            // Data may have arrived before the worker opened a decoder; arm the
            // budget now so it still starts at the first byte.
            state.probe_deadline = if state.data.is_empty() {
                None
            } else {
                Some(Instant::now() + budget)
            };
        }
        Self::new(shared)
    }
}

/// Stop enforcing the format-probe deadline; playback reads may wait again.
fn clear_probe_deadline(shared: &Shared) {
    let (lock, _) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    state.probe_budget = None;
    state.probe_deadline = None;
}

impl decode::ReadSeekSend for ProgressiveBuffer {
    fn is_seekable(&self) -> bool {
        false
    }

    fn content_type(&self) -> Option<String> {
        let (lock, _) = &*self.shared;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .content_type
            .clone()
    }
}

impl Read for ProgressiveBuffer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if self.pos < state.base {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "read behind the sliding buffer window",
                ));
            }
            // Checked before serving data as well: a stream that trickles one
            // byte at a time must not extend a probe indefinitely.
            if let Some(deadline) = state.probe_deadline {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "stream format was not recognised in time",
                    ));
                }
            }
            let start = (self.pos - state.base) as usize;
            if start < state.data.len() {
                let n = std::cmp::min(buf.len(), state.data.len() - start);
                buf[..n].copy_from_slice(&state.data[start..start + n]);
                self.pos += n as u64;
                slide_consumed(&mut state, self.pos);
                return Ok(n);
            }
            if let Some(err) = &state.error {
                return Err(io::Error::new(io::ErrorKind::Other, err.clone()));
            }
            if state.eof {
                return Ok(0);
            }
            // Never block forever: a server that accepts the connection and then
            // sends nothing must not wedge the decoder. During a probe the wait
            // is shortened to the probe deadline.
            let wait = match state.probe_deadline {
                Some(deadline) => deadline
                    .saturating_duration_since(Instant::now())
                    .min(READ_STALL_TIMEOUT),
                None => READ_STALL_TIMEOUT,
            };
            let (next, timeout) = match cvar.wait_timeout(state, wait) {
                Ok(pair) => pair,
                Err(e) => e.into_inner(),
            };
            state = next;
            if timeout.timed_out() && state.data.len() <= start && !state.eof {
                // During a probe the wait was shortened to the remaining
                // budget, so let the top of the loop decide. A probe timeout is
                // never stored on the buffer: the fallback backend gets a fresh
                // window and must still be able to try.
                if state.probe_deadline.is_some() {
                    continue;
                }
                let message = format!(
                    "stream stalled: no data for {}s",
                    READ_STALL_TIMEOUT.as_secs()
                );
                state.error = Some(message.clone());
                return Err(io::Error::new(io::ErrorKind::TimedOut, message));
            }
        }
    }
}

impl Seek for ProgressiveBuffer {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let (lock, _) = &*self.shared;
        let state = lock.lock().unwrap_or_else(|e| e.into_inner());
        let new_pos = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(delta) => match (self.pos as i64).checked_add(delta) {
                Some(v) if v >= 0 => v as u64,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "seek before start of stream",
                    ))
                }
            },
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot seek from the end of a live stream",
                ))
            }
        };
        if new_pos < state.base {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek behind the sliding buffer window",
            ));
        }
        self.pos = new_pos;
        Ok(new_pos)
    }
}

fn set_sliding(shared: &Shared, sliding: bool) {
    let (lock, cvar) = &**shared;
    lock.lock().unwrap_or_else(|e| e.into_inner()).sliding = sliding;
    cvar.notify_all();
}

/// Drop consumed bytes once the window is large, keeping a small rewind margin
/// for a decoder that seeks back a little (FR-P12).
fn slide_consumed(state: &mut BufferState, pos: u64) {
    if !state.sliding || pos < state.base {
        return;
    }
    let consumed = (pos - state.base) as usize;
    if consumed <= MAX_BUFFER_BYTES / 4 + BUFFER_REWIND_BYTES {
        return;
    }
    let drop_n = consumed.saturating_sub(BUFFER_REWIND_BYTES);
    if drop_n == 0 {
        return;
    }
    state.data.drain(..drop_n);
    state.base += drop_n as u64;
}

/// Append a chunk, blocking while the window is full so the network is
/// back-pressured instead of buffered without bound. Returns `false` on cancel.
fn append_chunk(shared: &Shared, chunk: &[u8], cancel: &AtomicBool) -> bool {
    let (lock, cvar) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    let mut written = 0usize;
    while written < chunk.len() {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        let room = MAX_BUFFER_BYTES.saturating_sub(state.data.len());
        if room == 0 {
            let (next, _) = match cvar.wait_timeout(state, Duration::from_millis(50)) {
                Ok(pair) => pair,
                Err(e) => e.into_inner(),
            };
            state = next;
            continue;
        }
        let take = room.min(chunk.len() - written);
        state
            .data
            .extend_from_slice(&chunk[written..written + take]);
        if state.probe_deadline.is_none() {
            if let Some(budget) = state.probe_budget {
                state.probe_deadline = Some(Instant::now() + budget);
            }
        }
        written += take;
        cvar.notify_all();
    }
    true
}

fn buffer_error(shared: &Shared, msg: String) {
    let (lock, cvar) = &**shared;
    lock.lock().unwrap_or_else(|e| e.into_inner()).error = Some(msg);
    cvar.notify_all();
}

fn buffer_error_text(shared: &Shared) -> Option<String> {
    let (lock, _) = &**shared;
    lock.lock().unwrap_or_else(|e| e.into_inner()).error.clone()
}

/// Unblock a decoder probe and any reader: clean EOF, no error.
fn cancel_buffer(shared: &Shared) {
    let (lock, cvar) = &**shared;
    lock.lock().unwrap_or_else(|e| e.into_inner()).eof = true;
    cvar.notify_all();
}

// ---------------------------------------------------------------------------
// HTTP/ICY fetch
// ---------------------------------------------------------------------------

/// What one plain-HTTP fetch produced.
#[derive(Debug)]
enum HttpOutcome {
    Finished(usize),
    Redirect(String),
    /// The response was a playlist; the body is bounded by
    /// [`playlist::MAX_BODY_BYTES`].
    Playlist(Vec<u8>),
    Cancelled,
}

/// What a whole fetch produced.
#[derive(Debug)]
enum FetchOutcome {
    Done(usize),
    /// The response was a playlist that still has to be resolved.
    Playlist(Vec<u8>),
    Cancelled,
}

/// Response body state while the start is classified as audio or a playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyMode {
    Undecided,
    Audio,
    Playlist,
}

/// Collects a response body while deciding whether it is a playlist.
///
/// A trusted hint (content type) means the body is a playlist and is read
/// directly. Otherwise the first bytes are held back until
/// [`playlist::classify`] is certain, so an audio stream is forwarded without
/// losing its first chunk. A weak hint (URL extension) only makes an unmarked
/// body that starts like a URL count as a playlist.
struct BodyReader {
    hint: Option<playlist::Hint>,
    mode: BodyMode,
    /// Bytes held back while the start is still ambiguous.
    pending: Vec<u8>,
    /// Playlist body collected so far.
    body: Vec<u8>,
    demux: IcyDemuxer,
    /// Applied to the buffer only once the response is known to be audio, so a
    /// playlist's content type never reaches the decoder.
    content_type: Option<String>,
    downloaded: usize,
    /// Bounds how long a slow server may spend sending a playlist body.
    started: Instant,
}

/// How many non-whitespace bytes are enough to decide that no marker will come.
const MAX_SNIFF_BYTES: usize = 4 * 1024;

/// A playlist is a few KiB; a server that cannot send it in this time is not
/// going to start the stream either.
const PLAYLIST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

impl BodyReader {
    fn new(hint: Option<playlist::Hint>, metaint: usize, content_type: Option<String>) -> Self {
        Self {
            hint,
            // A trusted content type means the body is a playlist even when it
            // has no marker line.
            mode: if hint.is_some_and(|hint| hint.trusted) {
                BodyMode::Playlist
            } else {
                BodyMode::Undecided
            },
            pending: Vec::new(),
            body: Vec::new(),
            demux: IcyDemuxer::new(metaint),
            content_type,
            downloaded: 0,
            started: Instant::now(),
        }
    }

    /// Consume one transport chunk. `Ok(false)` means the stream was cancelled.
    fn feed(
        &mut self,
        owner: &StreamShared,
        attempt: u64,
        shared: &Shared,
        chunk: &[u8],
        cancel: &AtomicBool,
    ) -> Result<bool, String> {
        match self.mode {
            BodyMode::Playlist => {
                if self.started.elapsed() > PLAYLIST_BODY_TIMEOUT {
                    return Err("playlist download timed out".to_string());
                }
                self.body.extend_from_slice(chunk);
                if self.body.len() > playlist::MAX_BODY_BYTES {
                    return Err(playlist::Error::TooLarge.to_string());
                }
                Ok(true)
            }
            BodyMode::Audio => Ok(self.forward(owner, attempt, shared, chunk, cancel)),
            BodyMode::Undecided => {
                self.pending.extend_from_slice(chunk);
                match playlist::classify(&self.pending) {
                    playlist::Sniff::Playlist(_) => {
                        self.start_playlist();
                        Ok(true)
                    }
                    playlist::Sniff::Audio if self.weak_hint_matches() => {
                        self.start_playlist();
                        Ok(true)
                    }
                    playlist::Sniff::Audio => {
                        self.mode = BodyMode::Audio;
                        let pending = std::mem::take(&mut self.pending);
                        Ok(self.forward(owner, attempt, shared, &pending, cancel))
                    }
                    playlist::Sniff::Undecided => {
                        // Keep holding only while a marker could still appear.
                        if sniff_len(&self.pending) > playlist::SNIFF_BYTES
                            || self.pending.len() > MAX_SNIFF_BYTES
                        {
                            self.mode = BodyMode::Audio;
                            let pending = std::mem::take(&mut self.pending);
                            Ok(self.forward(owner, attempt, shared, &pending, cancel))
                        } else {
                            Ok(true)
                        }
                    }
                }
            }
        }
    }

    /// Called at end of body: an undecided prefix is the start of a stream.
    fn finish(
        &mut self,
        owner: &StreamShared,
        attempt: u64,
        shared: &Shared,
        cancel: &AtomicBool,
    ) -> bool {
        if self.mode != BodyMode::Undecided || self.pending.is_empty() {
            return true;
        }
        self.mode = BodyMode::Audio;
        let pending = std::mem::take(&mut self.pending);
        self.forward(owner, attempt, shared, &pending, cancel)
    }

    fn start_playlist(&mut self) {
        self.mode = BodyMode::Playlist;
        self.body.append(&mut self.pending);
    }

    /// A weak hint plus a body that starts like a URL line is a bare-URL M3U.
    fn weak_hint_matches(&self) -> bool {
        let Some(hint) = self.hint else {
            return false;
        };
        if hint.trusted {
            return false;
        }
        let text = String::from_utf8_lossy(&self.pending);
        let text = text.trim_start_matches(char::is_whitespace);
        text.starts_with("http://") || text.starts_with("https://") || text.starts_with('/')
    }

    fn forward(
        &mut self,
        owner: &StreamShared,
        attempt: u64,
        shared: &Shared,
        bytes: &[u8],
        cancel: &AtomicBool,
    ) -> bool {
        if let Some(content_type) = self.content_type.take() {
            let (lock, _) = &**shared;
            lock.lock()
                .unwrap_or_else(|e| e.into_inner())
                .content_type = Some(content_type);
        }
        self.downloaded += bytes.len();
        push_demuxed(owner, attempt, shared, &mut self.demux, bytes, cancel)
    }
}

/// Bytes left after leading whitespace, which [`playlist::classify`] ignores.
fn sniff_len(prefix: &[u8]) -> usize {
    let start = prefix
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(prefix.len());
    prefix.len() - start
}

/// Push one transport chunk through the ICY demuxer so the decoder never sees
/// metadata bytes. Returns false when the stream was cancelled.
fn push_demuxed(
    owner: &StreamShared,
    attempt: u64,
    shared: &Shared,
    demux: &mut IcyDemuxer,
    chunk: &[u8],
    cancel: &AtomicBool,
) -> bool {
    let mut ok = true;
    demux.feed_with(chunk, |audio, title| {
        if !ok {
            return;
        }
        if !audio.is_empty() {
            owner.record_audio(audio);
            if owner.record_only() {
                owner.set_status(PlaybackStatus::Live);
            } else if !append_chunk(shared, audio, cancel) {
                ok = false;
                return;
            }
        }
        if let Some(title) = title {
            owner.watch_title(title);
            owner.set_now_playing(attempt, title.to_string());
        }
    });
    ok
}

/// Read one plain-HTTP stream, tolerating the non-standard `ICY 200 OK` status
/// line that Shoutcast servers send and that hyper (hence reqwest) rejects.
///
/// The request is HTTP/1.0 with `Icy-MetaData: 1`; when the server answers with
/// `icy-metaint`, interleaved metadata is stripped before reaching the decoder.
async fn fetch_http_once(
    owner: &StreamShared,
    attempt: u64,
    url: &str,
    shared: &Shared,
    cancel: &AtomicBool,
    wake: &tokio::sync::Notify,
) -> Result<HttpOutcome, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let parsed = url::Url::parse(url).map_err(|e| format!("bad stream URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "stream URL has no host".to_string())?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    // `host_str()` keeps IPv6 brackets, which `TcpStream::connect` rejects.
    let connect_host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    // Virtual hosts behind a non-default port reject a bare-host `Host:` header.
    let host_header = if port == 80 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let path = if parsed.path().is_empty() {
        "/".to_string()
    } else {
        format!(
            "{}{}",
            parsed.path(),
            parsed
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default()
        )
    };

    if cancel.load(Ordering::SeqCst) {
        return Ok(HttpOutcome::Cancelled);
    }
    let mut socket = tokio::select! {
        _ = wake.notified() => return Ok(HttpOutcome::Cancelled),
        result = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((connect_host.as_str(), port)),
        ) => {
            match result {
                Err(_) => return Err(format!(
                    "cannot connect to {connect_host}:{port}: timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                )),
                Ok(Err(e)) => return Err(format!("cannot connect to {connect_host}:{port}: {e}")),
                Ok(Ok(socket)) => socket,
            }
        }
    };

    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {host_header}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\nIcy-MetaData: 1\r\nConnection: close\r\n\r\n"
    );
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("cannot send request: {e}"))?;

    // Read until the end of the response headers.
    let mut head = Vec::new();
    let mut scratch = [0u8; 4096];
    let header_end = loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(HttpOutcome::Cancelled);
        }
        if let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if head.len() > 64 * 1024 {
            return Err("response headers too large".to_string());
        }
        let n = tokio::select! {
            _ = wake.notified() => return Ok(HttpOutcome::Cancelled),
            result = tokio::time::timeout(READ_STALL_TIMEOUT, socket.read(&mut scratch)) => {
                match result {
                    Err(_) => return Err(format!(
                        "stream stalled: no data for {}s",
                        READ_STALL_TIMEOUT.as_secs()
                    )),
                    Ok(Err(e)) => return Err(format!("cannot read response: {e}")),
                    Ok(Ok(0)) => return Err("connection closed before response headers".to_string()),
                    Ok(Ok(n)) => n,
                }
            }
        };
        head.extend_from_slice(&scratch[..n]);
    };

    let head_text = String::from_utf8_lossy(&head[..header_end]).to_string();
    let mut lines = head_text.lines();
    let status_line = lines.next().unwrap_or("").trim().to_string();

    // "HTTP/1.0 200 OK" or the legacy "ICY 200 OK".
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();

    let headers: Vec<&str> = lines.collect();
    if status_code.starts_with('3') {
        let location = headers
            .iter()
            .find_map(|line| {
                let (k, v) = line.split_once(':')?;
                if k.trim().eq_ignore_ascii_case("location") {
                    Some(v.trim().to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("redirect without Location: {status_line}"))?;
        let target = parsed
            .join(&location)
            .map_err(|e| format!("bad redirect target: {e}"))?;
        return Ok(HttpOutcome::Redirect(target.to_string()));
    }

    if status_code != "200" {
        return Err(format!("server answered: {status_line}"));
    }

    if headers.iter().any(|line| {
        line.split_once(':')
            .map(|(key, value)| {
                key.trim().eq_ignore_ascii_case("transfer-encoding")
                    && value.to_ascii_lowercase().contains("chunked")
            })
            .unwrap_or(false)
    }) {
        return Err("chunked HTTP responses are not supported on the raw ICY path".to_string());
    }

    let content_type = headers.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case("content-type") {
            Some(value.trim().to_string())
        } else {
            None
        }
    });
    let metaint = headers
        .iter()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.trim().eq_ignore_ascii_case("icy-metaint") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    if let Some(bitrate) = headers.iter().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        crate::bitrate::from_header(key.trim(), value.trim())
    }) {
        owner.set_reported_bitrate(bitrate);
    }

    let hint = playlist::hint(url, content_type.as_deref());
    if hint.is_none() {
        // The content type describes audio, so the decoder can use it before the
        // first byte arrives (the AAC shortcut).
        if let Some(value) = &content_type {
            let (lock, _) = &**shared;
            lock.lock()
                .unwrap_or_else(|e| e.into_inner())
                .content_type = Some(value.clone());
        }
    }
    let mut reader = BodyReader::new(hint, metaint, content_type);

    // Bytes read past the header block are already body data.
    let leftover = &head[header_end + 4..];
    if !reader.feed(owner, attempt, shared, leftover, cancel)? {
        return Ok(HttpOutcome::Cancelled);
    }

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(HttpOutcome::Cancelled);
        }
        let n = tokio::select! {
            _ = wake.notified() => return Ok(HttpOutcome::Cancelled),
            result = tokio::time::timeout(READ_STALL_TIMEOUT, socket.read(&mut scratch)) => {
                match result {
                    Err(_) => return Err(format!(
                        "stream stalled: no data for {}s",
                        READ_STALL_TIMEOUT.as_secs()
                    )),
                    Ok(Err(e)) => return Err(format!("read failed: {e}")),
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => n,
                }
            }
        };
        if !reader.feed(owner, attempt, shared, &scratch[..n], cancel)? {
            return Ok(HttpOutcome::Cancelled);
        }
    }

    if reader.mode == BodyMode::Playlist {
        return Ok(HttpOutcome::Playlist(reader.body));
    }
    if !reader.finish(owner, attempt, shared, cancel) {
        return Ok(HttpOutcome::Cancelled);
    }
    Ok(HttpOutcome::Finished(reader.downloaded))
}

/// Read an `https://` stream through reqwest (TLS handled by rustls).
async fn fetch_reqwest(
    owner: &StreamShared,
    attempt: u64,
    url: &str,
    shared: &Shared,
    cancel: &AtomicBool,
    wake: &tokio::sync::Notify,
) -> Result<FetchOutcome, String> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = tokio::select! {
        _ = wake.notified() => return Ok(FetchOutcome::Cancelled),
        result = client.get(url).header("Icy-MetaData", "1").send() => {
            result.map_err(|e| format!("request failed: {e}"))?
        }
    };
    let response = response
        .error_for_status()
        .map_err(|e| format!("server answered: {e}"))?;

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let metaint = response
        .headers()
        .get("icy-metaint")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if let Some(bitrate) = response.headers().iter().find_map(|(name, value)| {
        crate::bitrate::from_header(name.as_str(), value.to_str().ok()?)
    }) {
        owner.set_reported_bitrate(bitrate);
    }
    let hint = playlist::hint(url, content_type.as_deref());
    if hint.is_none() {
        // The content type describes audio, so the decoder can use it before the
        // first byte arrives (the AAC shortcut).
        if let Some(value) = &content_type {
            let (lock, _) = &**shared;
            lock.lock()
                .unwrap_or_else(|e| e.into_inner())
                .content_type = Some(value.clone());
        }
    }
    let mut reader = BodyReader::new(hint, metaint, content_type);
    let mut stream = response.bytes_stream();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(FetchOutcome::Cancelled);
        }
        let next = tokio::select! {
            _ = wake.notified() => return Ok(FetchOutcome::Cancelled),
            result = tokio::time::timeout(READ_STALL_TIMEOUT, stream.next()) => result,
        };
        let chunk = match next {
            Err(_) => {
                return Err(format!(
                    "stream stalled: no data for {}s",
                    READ_STALL_TIMEOUT.as_secs()
                ))
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(format!("stream error: {e}")),
            Ok(Some(Ok(bytes))) => bytes,
        };
        if !reader.feed(owner, attempt, shared, &chunk, cancel)? {
            return Ok(FetchOutcome::Cancelled);
        }
    }
    if reader.mode == BodyMode::Playlist {
        return Ok(FetchOutcome::Playlist(reader.body));
    }
    if !reader.finish(owner, attempt, shared, cancel) {
        return Ok(FetchOutcome::Cancelled);
    }
    Ok(FetchOutcome::Done(reader.downloaded))
}

/// Fetch a stream, following redirects and playlist indirections and choosing
/// the transport by scheme.
async fn fetch_stream(
    owner: &StreamShared,
    shared: &Shared,
    start: &str,
    cancel: &AtomicBool,
    wake: &tokio::sync::Notify,
    attempt: u64,
) -> Result<FetchOutcome, String> {
    let mut url = start.to_string();
    let mut redirects = 0u32;
    let mut playlists = 0u32;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(FetchOutcome::Cancelled);
        }
        if url.starts_with("https://") {
            match fetch_reqwest(owner, attempt, &url, shared, cancel, wake).await? {
                FetchOutcome::Done(n) => return Ok(FetchOutcome::Done(n)),
                FetchOutcome::Cancelled => return Ok(FetchOutcome::Cancelled),
                FetchOutcome::Playlist(body) => {
                    playlists += 1;
                    if playlists > MAX_PLAYLIST_HOPS {
                        return Err("too many nested playlists".to_string());
                    }
                    if hls::is_hls(&body) {
                        log::info!("following an HLS playlist");
                        return fetch_hls(owner, attempt, &url, body, shared, cancel, wake).await;
                    }
                    url = playlist::next_stream(&body, &url)
                        .map_err(|e| format!("playlist: {e}"))?;
                    log::info!("resolved a playlist to its first stream");
                }
            }
            continue;
        }
        if !url.starts_with("http://") {
            return Err(format!("unsupported URL scheme: {url}"));
        }
        match fetch_http_once(owner, attempt, &url, shared, cancel, wake).await? {
            HttpOutcome::Finished(n) => return Ok(FetchOutcome::Done(n)),
            HttpOutcome::Cancelled => return Ok(FetchOutcome::Cancelled),
            HttpOutcome::Redirect(next) => {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err("too many redirects".to_string());
                }
                url = next;
            }
            HttpOutcome::Playlist(body) => {
                playlists += 1;
                if playlists > MAX_PLAYLIST_HOPS {
                    return Err("too many nested playlists".to_string());
                }
                if hls::is_hls(&body) {
                    log::info!("following an HLS playlist");
                    return fetch_hls(owner, attempt, &url, body, shared, cancel, wake).await;
                }
                url = playlist::next_stream(&body, &url)
                    .map_err(|e| format!("playlist: {e}"))?;
                log::info!("resolved a playlist to its first stream");
            }
        }
    }
}

/// Follow an HLS playlist: fetch its segments in order and push the audio into
/// the same buffer the decoder reads. Live playlists are reloaded until the
/// stream ends, the download is cancelled, or the playlist stops advancing.
async fn fetch_hls(
    owner: &StreamShared,
    attempt: u64,
    start_url: &str,
    first_body: Vec<u8>,
    shared: &Shared,
    cancel: &AtomicBool,
    wake: &tokio::sync::Notify,
) -> Result<FetchOutcome, String> {
    let client = Client::builder()
        .timeout(READ_STALL_TIMEOUT)
        .build()
        .map_err(|e| format!("cannot start HLS client: {e}"))?;

    // The first body may be a master playlist that lists variants.
    let (media_url, mut body) = match hls::master_variant(&first_body, start_url)? {
        Some(variant) => {
            let Some(bytes) =
                hls_get(&client, &variant, hls::MAX_PLAYLIST_BYTES, cancel, wake).await?
            else {
                return Ok(FetchOutcome::Cancelled);
            };
            (variant, bytes)
        }
        None => (start_url.to_string(), first_body),
    };

    let mut next_sequence: Option<u64> = None;
    let mut init_pushed = false;
    let mut demux = IcyDemuxer::new(0);
    let mut ts = crate::mpegts::TsDemuxer::new();
    let mut total = 0usize;
    let mut idle_reloads = 0u32;

    loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(FetchOutcome::Cancelled);
        }
        let playlist = hls::media_playlist(&body, &media_url)?;

        if !init_pushed {
            if let Some(init) = &playlist.init {
                let Some(bytes) =
                    hls_get(&client, init, hls::MAX_SEGMENT_BYTES, cancel, wake).await?
                else {
                    return Ok(FetchOutcome::Cancelled);
                };
                total += bytes.len();
                if !push_demuxed(owner, attempt, shared, &mut demux, &bytes, cancel) {
                    return Ok(FetchOutcome::Cancelled);
                }
            }
            init_pushed = true;
        }

        let start = next_sequence.unwrap_or(playlist.media_sequence);
        let mut fetched = 0usize;
        for segment in playlist
            .segments
            .iter()
            .filter(|segment| segment.sequence >= start)
        {
            if cancel.load(Ordering::SeqCst) {
                return Ok(FetchOutcome::Cancelled);
            }
            let Some(bytes) =
                hls_get(&client, &segment.url, hls::MAX_SEGMENT_BYTES, cancel, wake).await?
            else {
                return Ok(FetchOutcome::Cancelled);
            };
            total += bytes.len();
            // MPEG-TS segments carry the audio in PES packets; raw AAC and MP3
            // segments are passed through unchanged.
            let mut elementary = Vec::new();
            let audio: &[u8] = if ts.demux(&bytes, &mut elementary) {
                &elementary
            } else {
                &bytes
            };
            if !audio.is_empty()
                && !push_demuxed(owner, attempt, shared, &mut demux, audio, cancel)
            {
                return Ok(FetchOutcome::Cancelled);
            }
            next_sequence = Some(segment.sequence + 1);
            fetched += 1;
        }

        if playlist.end_list {
            break;
        }
        idle_reloads = if fetched == 0 { idle_reloads + 1 } else { 0 };
        if idle_reloads > 60 {
            return Err("HLS playlist stopped producing segments".to_string());
        }

        // Live playlist: wait about half a target duration, then reload.
        let wait = Duration::from_millis(u64::from(playlist.target_duration.max(1)) * 500);
        if !sleep_hls(wait, cancel, wake).await {
            return Ok(FetchOutcome::Cancelled);
        }
        let Some(bytes) =
            hls_get(&client, &media_url, hls::MAX_PLAYLIST_BYTES, cancel, wake).await?
        else {
            return Ok(FetchOutcome::Cancelled);
        };
        body = bytes;
    }

    Ok(FetchOutcome::Done(total))
}

/// GET a playlist or segment, returning `None` when cancelled.
async fn hls_get(
    client: &Client,
    url: &str,
    limit: usize,
    cancel: &AtomicBool,
    wake: &tokio::sync::Notify,
) -> Result<Option<Vec<u8>>, String> {
    if cancel.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let response = tokio::select! {
        _ = wake.notified() => return Ok(None),
        result = client.get(url).send() => result.map_err(|e| format!("HLS request failed: {e}"))?,
    };
    if !response.status().is_success() {
        return Err(format!("HLS request returned {}", response.status()));
    }
    let body = tokio::select! {
        _ = wake.notified() => return Ok(None),
        result = response.bytes() => result.map_err(|e| format!("HLS body failed: {e}"))?,
    };
    if body.len() > limit {
        return Err(format!("HLS response exceeded {limit} bytes"));
    }
    Ok(Some(body.to_vec()))
}

/// Sleep, returning false when the download was cancelled.
async fn sleep_hls(duration: Duration, cancel: &AtomicBool, wake: &tokio::sync::Notify) -> bool {
    if cancel.load(Ordering::SeqCst) {
        return false;
    }
    tokio::select! {
        _ = wake.notified() => false,
        _ = tokio::time::sleep(duration) => !cancel.load(Ordering::SeqCst),
    }
}

/// Download `url` into `shared`, notifying the reader as data arrives.
fn download_into(
    owner: Arc<StreamShared>,
    shared: Shared,
    url: String,
    cancel: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    attempt_id: u64,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            buffer_error(&shared, format!("cannot start network runtime: {e}"));
            return;
        }
    };

    let result = {
        let shared = Arc::clone(&shared);
        let cancel_dl = Arc::clone(&cancel);
        let wake = Arc::clone(&wake);
        rt.block_on(async move {
            fetch_stream(&owner, &shared, &url, &cancel_dl, &wake, attempt_id).await
        })
    };
    // A dropped current-thread runtime waits for blocking tasks (DNS lookups);
    // shut it down in the background so a cancelled download cannot linger.
    rt.shutdown_background();

    let (lock, cvar) = &*shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    let cancelled = cancel.load(Ordering::SeqCst) || matches!(result, Ok(FetchOutcome::Cancelled));
    if cancelled {
        // Cancelled: close the reader without reporting an error.
        state.eof = true;
    } else {
        match result {
            Ok(FetchOutcome::Done(0)) => {
                state.error = Some("server returned no audio data".to_string())
            }
            Ok(FetchOutcome::Done(_)) => state.eof = true,
            Ok(FetchOutcome::Cancelled) => state.eof = true,
            // `fetch_stream` resolves playlists before returning; reaching this
            // arm would mean an indirection was left dangling.
            Ok(FetchOutcome::Playlist(_)) => {
                state.error = Some("playlist was not resolved".to_string())
            }
            Err(e) => state.error = Some(e),
        }
    }
    drop(state);
    cvar.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// Only one test may hold the audio device at a time: cpal and the test
    /// harness run tests in parallel.
    fn audio_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn progressive_buffer_reads_all_chunks_in_order() {
        let shared = new_shared();
        let mut reader = ProgressiveBuffer::new(Arc::clone(&shared));

        let producer = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let (lock, cvar) = &*shared;
                for chunk in [b"hello ".as_slice(), b"world".as_slice()] {
                    {
                        let mut st = lock.lock().unwrap();
                        st.data.extend_from_slice(chunk);
                    }
                    cvar.notify_all();
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let (lock, cvar) = &*shared;
                lock.lock().unwrap().eof = true;
                cvar.notify_all();
            })
        };

        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        producer.join().unwrap();

        assert_eq!(out, b"hello world");
    }

    #[test]
    fn progressive_buffer_seek_to_start_replays_data() {
        let shared = new_shared();
        {
            let (lock, cvar) = &*shared;
            let mut st = lock.lock().unwrap();
            st.data.extend_from_slice(b"abcdef");
            st.eof = true;
            cvar.notify_all();
        }

        let mut reader = ProgressiveBuffer::new(Arc::clone(&shared));
        let mut first = [0u8; 6];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"abcdef");

        // Seeking back to the start must replay the same bytes (the decoder does
        // this while probing).
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut second = [0u8; 3];
        reader.read_exact(&mut second).unwrap();
        assert_eq!(&second, b"abc");
    }

    #[test]
    fn progressive_buffer_reports_download_error() {
        let shared = new_shared();
        {
            let (lock, cvar) = &*shared;
            let mut st = lock.lock().unwrap();
            st.error = Some("boom".to_string());
            cvar.notify_all();
        }
        let mut reader = ProgressiveBuffer::new(Arc::clone(&shared));
        let mut out = Vec::new();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    fn probe_deadline(shared: &Shared) -> Option<Instant> {
        let (lock, _) = &**shared;
        lock.lock().unwrap().probe_deadline
    }

    #[test]
    fn probe_budget_is_armed_by_the_first_byte() {
        let shared = new_shared();
        let _reader = ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(50));
        assert!(probe_deadline(&shared).is_none());
        append_chunk(&shared, b"x", &AtomicBool::new(false));
        assert!(probe_deadline(&shared).is_some());
    }

    #[test]
    fn probing_arms_immediately_when_data_already_arrived() {
        let shared = new_shared();
        append_chunk(&shared, b"x", &AtomicBool::new(false));
        let _reader = ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(50));
        assert!(probe_deadline(&shared).is_some());
    }

    #[test]
    fn probe_deadline_bounds_a_stalled_read() {
        let shared = new_shared();
        let mut reader = ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(50));
        append_chunk(&shared, b"x", &AtomicBool::new(false));
        assert_eq!(reader.read(&mut [0u8; 1]).unwrap(), 1);

        let start = Instant::now();
        let error = reader.read(&mut [0u8; 16]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "probe deadline did not fire: {:?}",
            start.elapsed()
        );
        // A probe timeout must not poison the buffer for the fallback backend.
        assert!(buffer_error_text(&shared).is_none());
    }

    #[test]
    fn probe_deadline_is_not_sticky_after_it_is_cleared() {
        let shared = new_shared();
        let mut reader = ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(20));
        append_chunk(&shared, b"x", &AtomicBool::new(false));
        assert_eq!(reader.read(&mut [0u8; 1]).unwrap(), 1);
        assert!(reader.read(&mut [0u8; 4]).is_err());
        assert!(buffer_error_text(&shared).is_none());

        clear_probe_deadline(&shared);
        append_chunk(&shared, b"abcd", &AtomicBool::new(false));
        let mut buffer = [0u8; 4];
        assert_eq!(reader.read(&mut buffer).unwrap(), 4);
        assert_eq!(&buffer, b"abcd");
    }

    /// The symphonia fallback opens a new reader; its budget must restart.
    #[test]
    fn a_second_probe_gets_a_fresh_budget() {
        let shared = new_shared();
        let mut first = ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(20));
        append_chunk(&shared, b"x", &AtomicBool::new(false));
        assert_eq!(first.read(&mut [0u8; 1]).unwrap(), 1);
        assert!(first.read(&mut [0u8; 4]).is_err());

        let mut second =
            ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(500));
        let mut buffer = [0u8; 1];
        assert_eq!(second.read(&mut buffer).unwrap(), 1);
        assert!(buffer_error_text(&shared).is_none());
    }

    /// A stream that trickles one byte at a time must not extend a probe.
    #[test]
    fn probe_deadline_bounds_a_trickling_stream() {
        let shared = new_shared();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let producer = {
            let shared = Arc::clone(&shared);
            let cancel = Arc::clone(&cancel);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    append_chunk(&shared, &[0u8], &cancel);
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };

        let mut reader = ProgressiveBuffer::probing(Arc::clone(&shared), Duration::from_millis(50));
        let start = Instant::now();
        let mut error = None;
        while start.elapsed() < Duration::from_secs(2) {
            match reader.read(&mut [0u8; 4]) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
        producer.join().unwrap();

        let error = error.expect("a trickling stream must not extend the probe");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "probe deadline did not fire: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn opening_a_decoder_clears_the_probe_deadline() {
        let shared = new_shared();
        append_chunk(&shared, &make_wav(1), &AtomicBool::new(false));
        {
            let (lock, cvar) = &*shared;
            lock.lock().unwrap().eof = true;
            cvar.notify_all();
        }

        let decoder = open_decoder(&shared).expect("the WAV must open");
        assert!(decoder.channels() > 0);
        assert!(probe_deadline(&shared).is_none());
    }

    #[test]
    fn watch_buffer_keeps_audio_from_the_latest_mark() {
        let mut buffer = WatchBuffer::new();
        buffer.push(b"old");
        buffer.mark("Old Song".into());
        buffer.push(b"new");
        buffer.mark("New Song".into());
        buffer.push(b"more");
        let (audio, label) = buffer.take_since_mark();
        assert_eq!(audio, b"more");
        assert_eq!(label.as_deref(), Some("New Song"));
    }

    #[test]
    fn watch_buffer_drops_the_oldest_audio() {
        let mut buffer = WatchBuffer::new();
        buffer.cap = 4;
        buffer.push(b"abc");
        buffer.mark("Dropped".into());
        buffer.push(b"def");
        assert_eq!(buffer.data.len(), 4);
        assert_eq!(buffer.base, 2);
        assert_eq!(buffer.marks.len(), 1);
        buffer.push(b"ghi");
        assert!(buffer.marks.is_empty());
        let (audio, label) = buffer.take_since_mark();
        assert_eq!(audio, b"fghi");
        assert!(label.is_none());
    }

    #[test]
    fn watch_buffer_capacity_can_shrink() {
        let mut buffer = WatchBuffer::new();
        buffer.push(&[1u8; 1024]);
        buffer.mark("Song".into());
        buffer.push(&[2u8; 4096]);
        buffer.set_capacity(2048);
        assert_eq!(buffer.data.len(), 2048);
        assert_eq!(buffer.base, 3072);
        assert!(buffer.marks.is_empty(), "the mark fell outside the window");
        let (audio, label) = buffer.take_since_mark();
        assert_eq!(audio.len(), 2048);
        assert!(label.is_none());
    }

    #[test]
    fn promoting_a_watch_writes_from_the_title_mark() {
        let dir = std::env::temp_dir().join(format!(
            "viberadio-watch-promote-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let request = PlayRequest {
            watch: true,
            ..PlayRequest::new("http://example.invalid/watch")
        };
        let shared = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(0)),
        );
        shared.record_audio(b"AAA");
        shared.watch_title("Artist - Song");
        shared.record_audio(b"BBB");

        let mut config = crate::recording::RecordingConfig::new(&dir);
        config.template = "{artist} - {title}".to_string();
        config.mode = crate::recording::RecordingMode::SplitPerTrack;
        config.extension = "bin".to_string();
        assert!(shared
            .promote_watch(
                config,
                "Test FM".to_string(),
                Some("Artist"),
                Some("Song")
            )
            .unwrap());

        shared.record_audio(b"CCC");
        let summary = shared
            .stop_recording_async()
            .expect("recording must stop")
            .join()
            .expect("writer must finish");
        assert_eq!(summary.files, 1);
        assert_eq!(
            std::fs::read(dir.join("Artist - Song.bin")).unwrap(),
            b"BBBCCC"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sliding_buffer_drops_consumed_bytes_and_keeps_reading() {
        let shared = new_shared();
        set_sliding(&shared, true);
        {
            let (lock, cvar) = &*shared;
            let mut st = lock.lock().unwrap();
            st.data
                .extend(std::iter::repeat(7u8).take(MAX_BUFFER_BYTES / 2));
            st.eof = true;
            cvar.notify_all();
        }
        let mut reader = ProgressiveBuffer::new(Arc::clone(&shared));
        let mut chunk = vec![0u8; MAX_BUFFER_BYTES / 4 + 1024 * 1024];
        reader.read_exact(&mut chunk).unwrap();
        let (lock, _) = &*shared;
        let st = lock.lock().unwrap();
        assert!(st.base > 0, "consumed bytes must slide out of the window");
        assert!(st.data.len() < MAX_BUFFER_BYTES / 2);
    }

    /// A session larger than `MAX_BUFFER_BYTES` must keep reading, not end as a
    /// clean EOF when the window fills.
    #[test]
    fn a_session_larger_than_the_buffer_cap_keeps_reading() {
        let shared = new_shared();
        set_sliding(&shared, true);
        let total = MAX_BUFFER_BYTES + 4 * 1024 * 1024;
        let chunk = vec![0x5a; 1024 * 1024];
        let producer = {
            let shared = Arc::clone(&shared);
            let cancel = AtomicBool::new(false);
            thread::spawn(move || {
                let mut sent = 0usize;
                while sent < total {
                    let take = (total - sent).min(chunk.len());
                    if !append_chunk(&shared, &chunk[..take], &cancel) {
                        return false;
                    }
                    sent += take;
                }
                let (lock, cvar) = &*shared;
                lock.lock().unwrap().eof = true;
                cvar.notify_all();
                true
            })
        };

        let mut reader = ProgressiveBuffer::new(Arc::clone(&shared));
        let mut buffer = vec![0u8; 256 * 1024];
        let mut read = 0usize;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) => panic!("read failed after {read} bytes: {e}"),
            }
        }
        assert!(producer.join().unwrap(), "the producer was cancelled");
        assert_eq!(read, total);
        let (lock, _) = &*shared;
        let state = lock.lock().unwrap();
        assert!(state.error.is_none());
        assert!(state.base > 0, "the window must have slid");
    }

    /// Cancelling while the downloader waits for the next byte must stop it
    /// promptly, not only between chunks.
    #[test]
    fn a_stalled_download_is_cancelled_while_waiting_for_data() {
        let addr = serve_stalled_wav();
        let buffer = new_shared();
        let request = PlayRequest::new(format!("http://{addr}/stall"));
        let owner = Arc::new(StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        ));
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let handle = {
            let owner = Arc::clone(&owner);
            let buffer = Arc::clone(&buffer);
            let cancel = Arc::clone(&cancel);
            let wake = Arc::clone(&wake);
            let url = request.url.clone();
            thread::spawn(move || download_into(owner, buffer, url, cancel, wake, 1))
        };

        thread::sleep(Duration::from_millis(300));
        assert!(!handle.is_finished(), "the server should still be stalling");

        cancel.store(true, Ordering::SeqCst);
        wake.notify_one();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished(), "the download did not stop while stalled");
        handle.join().unwrap();

        let (lock, _) = &*buffer;
        let state = lock.lock().unwrap();
        assert!(state.eof, "a cancelled download closes the buffer");
        assert!(state.error.is_none(), "cancellation is not an error");
    }

    /// Minimal 16-bit mono PCM WAV of `seconds` duration, decodable by rodio.
    fn make_wav(seconds: u32) -> Vec<u8> {
        let sample_rate = 44_100u32;
        let frames = sample_rate * seconds;
        let mut pcm = Vec::with_capacity((frames * 2) as usize);
        for i in 0..frames {
            let t = i as f32 / sample_rate as f32;
            let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 0.2 * i16::MAX as f32) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
        }

        let data_len = pcm.len() as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&pcm);
        wav
    }

    fn serve_file(bytes: &'static [u8], content_type: &'static str) -> std::net::SocketAddr {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let header = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = c.write_all(header.as_bytes());
                let _ = c.write_all(bytes);
                let _ = c.flush();
            }
        });
        addr
    }

    /// Answer every request with a 302 to `location`.
    fn serve_redirect(location: String) -> std::net::SocketAddr {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let response = format!(
                    "HTTP/1.0 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = c.write_all(response.as_bytes());
                let _ = c.flush();
            }
        });
        addr
    }

    fn wav_header(wav_len: usize) -> String {
        format!(
            "HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {wav_len}\r\nConnection: close\r\n\r\n"
        )
    }

    /// Serve one WAV per connection with the given status line.
    fn serve_wav(status_line: &'static str, seconds: u32) -> std::net::SocketAddr {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let wav = make_wav(seconds);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let header = wav_header(wav.len()).replace("HTTP/1.0 200 OK", status_line);
                let _ = c.write_all(header.as_bytes());
                let _ = c.write_all(&wav);
                let _ = c.flush();
            }
        });
        addr
    }

    /// Serve a WAV with ICY metadata interleaved every `metaint` audio bytes.
    fn serve_icy_wav(metaint: usize, title: &'static str) -> std::net::SocketAddr {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let wav = make_wav(3);
        let mut wire = Vec::new();
        for chunk in wav.chunks(metaint) {
            wire.extend_from_slice(chunk);
            if chunk.len() == metaint {
                let text = format!("StreamTitle='{title}';");
                let blocks = text.len().div_ceil(16);
                wire.push(blocks as u8);
                let mut block = vec![0u8; blocks * 16];
                block[..text.len()].copy_from_slice(text.as_bytes());
                wire.extend_from_slice(&block);
            }
        }
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let header = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: audio/wav\r\nicy-metaint: {metaint}\r\nicy-br: 128\r\nConnection: close\r\n\r\n"
                );
                let _ = c.write_all(header.as_bytes());
                let _ = c.write_all(&wire);
                let _ = c.flush();
            }
        });
        addr
    }

    /// Serve three ICY tracks, each with four audio bytes.
    fn serve_icy_three_tracks() -> std::net::SocketAddr {
        use std::net::TcpListener;

        const TRACKS: [(&str, &[u8]); 3] = [
            ("Artist One - Song One", b"AAAA"),
            ("Artist Two - Song Two", b"BBBB"),
            ("Artist Three - Song Three", b"CCCC"),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut wire = Vec::new();
        // A metadata block labels the audio that follows it. The first interval
        // is a pre-roll that the first title adopts.
        wire.extend_from_slice(b"PRE!");
        for (title, audio) in TRACKS {
            let text = format!("StreamTitle='{title}';");
            let blocks = text.len().div_ceil(16);
            wire.push(blocks as u8);
            let mut block = vec![0u8; blocks * 16];
            block[..text.len()].copy_from_slice(text.as_bytes());
            wire.extend_from_slice(&block);
            wire.extend_from_slice(audio);
        }
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut request = [0u8; 1024];
                let _ = c.read(&mut request);
                // The record-only test attaches its recorder just after
                // play_stream; a real stream cannot deliver audio before that.
                // Give the attach a moment so the test is not a race.
                thread::sleep(Duration::from_millis(100));
                let _ = c.write_all(
                    b"HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\nicy-metaint: 4\r\nConnection: close\r\n\r\n",
                );
                let _ = c.write_all(&wire);
                let _ = c.flush();
            }
        });
        addr
    }

    /// Serve raw bytes with ICY metadata; used by the transport-only test so it
    /// does not depend on an audio device.
    fn serve_icy_bytes(metaint: usize, title: &'static str, audio: &'static [u8]) -> std::net::SocketAddr {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut wire = Vec::new();
        for chunk in audio.chunks(metaint) {
            wire.extend_from_slice(chunk);
            if chunk.len() == metaint {
                let text = format!("StreamTitle='{title}';");
                let blocks = text.len().div_ceil(16);
                wire.push(blocks as u8);
                let mut block = vec![0u8; blocks * 16];
                block[..text.len()].copy_from_slice(text.as_bytes());
                wire.extend_from_slice(&block);
            }
        }
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut request = [0u8; 1024];
                let _ = c.read(&mut request);
                let header = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: audio/mpeg\r\nicy-metaint: {metaint}\r\nConnection: close\r\n\r\n"
                );
                let _ = c.write_all(header.as_bytes());
                let _ = c.write_all(&wire);
                let _ = c.flush();
            }
        });
        addr
    }

    /// Accept a connection, send a valid WAV header and a little audio, then hold
    /// the socket open without sending anything else.
    fn serve_stalled_wav() -> std::net::SocketAddr {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let wav = make_wav(5);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let header = wav_header(wav.len());
                let _ = c.write_all(header.as_bytes());
                let _ = c.write_all(&wav[..44 + 44_100 * 2 / 2]);
                let _ = c.flush();
                std::thread::sleep(Duration::from_secs(20));
            }
        });
        addr
    }

    /// Close after one short WAV and count how many connections were made.
    fn serve_dropping_wav() -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        use std::net::TcpListener;
        use std::sync::atomic::AtomicUsize;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let wav = make_wav(1);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let _ = c.write_all(wav_header(wav.len()).as_bytes());
                let _ = c.write_all(&wav);
                let _ = c.flush();
                drop(c);
            }
        });
        (addr, connections)
    }

    /// A server that accepts the connection, reads the request and never answers.
    /// It reports when the client closes the socket.
    fn serve_silent() -> (std::net::SocketAddr, Arc<AtomicBool>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = Arc::clone(&closed);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                // Wait until the client gives up and drops the connection.
                loop {
                    match c.read(&mut scratch) {
                        Ok(0) | Err(_) => {
                            closed_flag.store(true, Ordering::SeqCst);
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            }
        });
        (addr, closed)
    }

    fn has_audio_device() -> bool {
        // Hosted macOS and Windows runners expose an output device that never
        // consumes samples, so the sink never drains and the Windows device
        // path crashes. Linux runners have no device and already skip.
        if std::env::var_os("CI").is_some() && !cfg!(target_os = "linux") {
            eprintln!("skipping playback test on hosted CI");
            return false;
        }
        match OutputStream::try_default() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("no audio output device ({e}); skipping playback test");
                false
            }
        }
    }

    fn snapshot_streams(player: &RadioPlayer) -> Vec<StreamSnapshot> {
        player.snapshot().streams
    }

    fn active_status(player: &RadioPlayer) -> String {
        player
            .snapshot()
            .active
            .map(|stream| stream.status_text)
            .unwrap_or_else(|| "Idle".to_string())
    }

    fn any_playing(player: &RadioPlayer) -> bool {
        player
            .snapshot()
            .streams
            .iter()
            .any(|stream| stream.status.is_active())
    }

    fn stream_is_active(player: &RadioPlayer, id: PlaybackId) -> bool {
        player
            .snapshot().streams
            .into_iter()
            .find(|stream| stream.id == id)
            .map(|stream| stream.status.is_active())
            .unwrap_or(false)
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        condition()
    }

    /// Run the player against `url` and require it to reach `Playing`.
    ///
    /// Also requires the stream to stop afterwards, which only happens if the
    /// decoder actually produced samples and the sink drained.
    fn assert_plays(url: &str) {
        let player = RadioPlayer::new();
        player.play_stream(PlayRequest::new(url));

        let started = wait_until(Duration::from_secs(20), || {
            let status = active_status(&player);
            status.starts_with("Playing")
        });
        if !started {
            panic!("playback did not start; last status: {}", active_status(&player));
        }
        let leveled = wait_until(Duration::from_secs(10), || {
            player
                .snapshot()
                .streams
                .iter()
                .any(|stream| stream.rms_serial > 0 && stream.rms > 0.0)
        });
        assert!(leveled, "the decoder did not report an RMS window");

        let stopped = wait_until(Duration::from_secs(15), || active_status(&player).contains("Stopped"));
        if !stopped {
            panic!(
                "sink never drained; audio was probably not decoded: {}",
                active_status(&player)
            );
        }
        assert!(!any_playing(&player), "still playing after drain");
    }

    /// Full path: HTTP -> progressive buffer -> decoder worker -> ring -> sink.
    #[test]
    fn plays_local_wav_stream_end_to_end() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let addr = serve_wav("HTTP/1.0 200 OK", 1);
        assert_plays(&format!("http://{addr}/stream.wav"));
    }

    /// Fetch a URL to completion without an audio device and return the outcome
    /// plus everything the download wrote into the shared buffer.
    fn fetch_to_end(url: &str) -> (FetchOutcome, Vec<u8>, Option<String>) {
        let buffer = new_shared();
        let request = PlayRequest::new(url);
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(fetch_stream(&owner, &buffer, url, &cancel, &wake, attempt))
            .unwrap();
        let (lock, _) = &*buffer;
        let state = lock.lock().unwrap();
        (outcome, state.data.clone(), state.content_type.clone())
    }

    fn fetch_error(url: &str) -> String {
        let buffer = new_shared();
        let request = PlayRequest::new(url);
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(fetch_stream(&owner, &buffer, url, &cancel, &wake, attempt))
            .unwrap_err()
    }

    /// A station URL that returns an M3U must play the stream it points at.
    #[test]
    fn playlist_url_resolves_to_the_stream() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let body: &'static [u8] = Box::leak(
            format!("#EXTM3U\n#EXTINF:-1,Test\nhttp://{audio}/stream.wav\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let list = serve_file(body, "audio/x-mpegurl");

        let (outcome, data, content_type) = fetch_to_end(&format!("http://{list}/radio.m3u"));
        assert!(matches!(outcome, FetchOutcome::Done(n) if n == wav.len()));
        assert_eq!(data, wav);
        // The playlist's own content type must not reach the decoder.
        assert_eq!(content_type.as_deref(), Some("audio/wav"));
    }

    /// PLS is recognised by its content type and URL extension.
    #[test]
    fn pls_url_resolves_to_the_stream() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let body: &'static [u8] = Box::leak(
            format!("[playlist]\nNumberOfEntries=1\nFile1=http://{audio}/stream.wav\nTitle1=Test\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let list = serve_file(body, "audio/x-scpls");

        let (outcome, data, _) = fetch_to_end(&format!("http://{list}/radio.pls"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    /// A playlist served with a generic content type is still detected from its
    /// first bytes.
    #[test]
    fn playlist_body_is_detected_without_a_content_type() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let body: &'static [u8] = Box::leak(
            format!("#EXTM3U\nhttp://{audio}/stream.wav\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let list = serve_file(body, "application/octet-stream");

        let (outcome, data, _) = fetch_to_end(&format!("http://{list}/listen"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    /// Held-back bytes are forwarded unchanged when the response is audio, even
    /// when the content type is wrong.
    #[test]
    fn audio_body_is_forwarded_after_sniffing() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "text/plain");

        let (outcome, data, content_type) = fetch_to_end(&format!("http://{audio}/stream"));
        assert!(matches!(outcome, FetchOutcome::Done(n) if n == wav.len()));
        assert_eq!(data, wav);
        assert_eq!(content_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn nested_playlists_are_followed() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let inner: &'static [u8] = Box::leak(
            format!("#EXTM3U\nhttp://{audio}/stream.wav\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let inner_addr = serve_file(inner, "audio/x-mpegurl");
        let outer: &'static [u8] = Box::leak(
            format!("#EXTM3U\nhttp://{inner_addr}/inner.m3u\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let outer_addr = serve_file(outer, "audio/x-mpegurl");

        let (outcome, data, _) = fetch_to_end(&format!("http://{outer_addr}/outer.m3u"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    #[test]
    fn hls_live_playlist_without_endlist_keeps_reloading() {
        // A live playlist has no ENDLIST; the fetch must follow it rather than
        // reject it. A cancelled fetch stops promptly.
        let body: &'static [u8] = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg1.aac\n";
        let list = serve_file(body, "application/vnd.apple.mpegurl");

        let buffer = new_shared();
        let request = PlayRequest::new(format!("http://{list}/live.m3u8"));
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fetch = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                fetch_stream(&owner, &buffer, &request.url, &cancel, &wake, attempt),
            )
            .await
        });
        // Still running after two seconds means it is polling the live playlist.
        assert!(fetch.is_err(), "live HLS playlist ended unexpectedly");
        cancel.store(true, Ordering::SeqCst);
        wake.notify_one();
    }

    /// A playlist that points at itself must stop instead of looping forever.
    #[test]
    fn self_referential_playlist_stops() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = format!("#EXTM3U\nhttp://{addr}/again.m3u\n");
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { break };
                let mut scratch = [0u8; 1024];
                let _ = c.read(&mut scratch);
                let header = format!(
                    "HTTP/1.0 200 OK\r\nContent-Type: audio/x-mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(header.as_bytes());
                let _ = c.write_all(body.as_bytes());
            }
        });

        let error = fetch_error(&format!("http://{addr}/loop.m3u"));
        assert!(error.contains("too many nested playlists"), "{error}");
    }

    /// A PLS without its `[playlist]` header must still be parsed as PLS, not
    /// as an M3U whose entries are `File1=...` lines.
    #[test]
    fn pls_without_a_header_is_detected_by_its_file_key() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let body: &'static [u8] = Box::leak(
            format!("File1=http://{audio}/stream.wav\nTitle1=Test\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let list = serve_file(body, "application/octet-stream");

        let (outcome, data, _) = fetch_to_end(&format!("http://{list}/listen"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    /// An audio content type wins over a playlist-looking URL extension.
    #[test]
    fn audio_at_a_playlist_extension_is_streamed() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/mpeg");

        let (outcome, data, content_type) = fetch_to_end(&format!("http://{audio}/stream.m3u"));
        assert!(matches!(outcome, FetchOutcome::Done(n) if n == wav.len()));
        assert_eq!(data, wav);
        assert_eq!(content_type.as_deref(), Some("audio/mpeg"));
    }

    /// HLS is refused even when neither the content type nor the extension says
    /// so, because the body marker is enough.
    #[test]
    fn hls_is_detected_from_the_body_without_a_hint() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let segment = serve_file(wav, "audio/aac");
        let body: &'static [u8] = Box::leak(
            format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nhttp://{segment}/seg.aac\n#EXT-X-ENDLIST\n"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let list = serve_file(body, "application/octet-stream");

        let (outcome, data, _) = fetch_to_end(&format!("http://{list}/listen"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    #[test]
    fn playlist_after_a_redirect_resolves() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let body: &'static [u8] = Box::leak(
            format!("#EXTM3U\nhttp://{audio}/stream.wav\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let list = serve_file(body, "audio/x-mpegurl");
        let redirect = serve_redirect(format!("http://{list}/radio.m3u"));

        let (outcome, data, _) = fetch_to_end(&format!("http://{redirect}/start"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    /// A playlist entry may itself redirect; the hop counters are separate.
    #[test]
    fn a_playlist_entry_may_redirect() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let audio = serve_file(wav, "audio/wav");
        let redirect = serve_redirect(format!("http://{audio}/stream.wav"));
        let body: &'static [u8] = Box::leak(
            format!("#EXTM3U\nhttp://{redirect}/entry\n")
                .into_bytes()
                .into_boxed_slice(),
        );
        let list = serve_file(body, "audio/x-mpegurl");

        let (outcome, data, _) = fetch_to_end(&format!("http://{list}/radio.m3u"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    /// An HLS media playlist must play every segment in order.
    #[test]
    fn hls_media_playlist_plays_its_segments() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let segment = serve_file(wav, "audio/aac");
        let body: &'static [u8] = Box::leak(
            format!(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:9\n#EXTINF:4.0,\nhttp://{segment}/seg-9.aac\n#EXT-X-ENDLIST\n"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let list = serve_file(body, "application/vnd.apple.mpegurl");

        let (outcome, data, _) = fetch_to_end(&format!("http://{list}/live.m3u8"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    /// A master playlist must be reduced to one variant before segments load.
    #[test]
    fn hls_master_playlist_picks_a_variant() {
        let wav: &'static [u8] = Box::leak(make_wav(1).into_boxed_slice());
        let segment = serve_file(wav, "audio/aac");
        let media: &'static [u8] = Box::leak(
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nhttp://{segment}/seg.aac\n#EXT-X-ENDLIST\n"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let playlist = serve_file(media, "application/vnd.apple.mpegurl");
        let master: &'static [u8] = Box::leak(
            format!(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=64000\nhttp://{playlist}/low.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=128000\nhttp://{playlist}/high.m3u8\n"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let entry = serve_file(master, "application/vnd.apple.mpegurl");

        let (outcome, data, _) = fetch_to_end(&format!("http://{entry}/master.m3u8"));
        assert!(matches!(outcome, FetchOutcome::Done(_)));
        assert_eq!(data, wav);
    }

    fn body_reader_owner() -> StreamShared {
        let request = PlayRequest::new("http://a.example/radio.m3u");
        StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        )
    }

    fn buffer_data(shared: &Shared) -> (Vec<u8>, Option<String>) {
        let (lock, _) = &**shared;
        let state = lock.lock().unwrap();
        (state.data.clone(), state.content_type.clone())
    }

    #[test]
    fn body_reader_holds_a_marker_split_across_chunks() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let mut reader = BodyReader::new(None, 0, None);

        assert!(reader.feed(&owner, 1, &buffer, b"#EX", &cancel).unwrap());
        assert_eq!(reader.mode, BodyMode::Undecided);
        assert!(reader
            .feed(&owner, 1, &buffer, b"TM3U\nhttp://a/b\n", &cancel)
            .unwrap());
        assert_eq!(reader.mode, BodyMode::Playlist);
        assert_eq!(reader.body, b"#EXTM3U\nhttp://a/b\n");
        assert!(buffer_data(&buffer).0.is_empty());
    }

    #[test]
    fn body_reader_forwards_a_prefix_that_turns_out_to_be_audio() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let mut reader = BodyReader::new(None, 0, None);

        assert!(reader.feed(&owner, 1, &buffer, b"#EX", &cancel).unwrap());
        assert!(reader.feed(&owner, 1, &buffer, b"RIFFdata", &cancel).unwrap());
        assert_eq!(reader.mode, BodyMode::Audio);
        assert_eq!(buffer_data(&buffer).0, b"#EXRIFFdata");
    }

    #[test]
    fn body_reader_never_applies_a_playlist_content_type() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let hint = Some(playlist::Hint {
            kind: playlist::Kind::M3u,
            trusted: true,
        });
        let mut reader = BodyReader::new(hint, 0, Some("audio/x-mpegurl".to_string()));

        assert!(reader
            .feed(&owner, 1, &buffer, b"#EXTM3U\nhttp://a/b\n", &cancel)
            .unwrap());
        assert_eq!(reader.mode, BodyMode::Playlist);
        assert_eq!(buffer_data(&buffer).1, None);
    }

    #[test]
    fn trusted_hint_reads_a_bare_url_body_as_a_playlist() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let hint = Some(playlist::Hint {
            kind: playlist::Kind::M3u,
            trusted: true,
        });
        let mut reader = BodyReader::new(hint, 0, None);

        assert!(reader
            .feed(&owner, 1, &buffer, b"http://a/stream\n", &cancel)
            .unwrap());
        assert_eq!(reader.mode, BodyMode::Playlist);
        assert_eq!(reader.body, b"http://a/stream\n");
    }

    #[test]
    fn body_reader_flushes_an_undecided_prefix_at_end_of_body() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let mut reader = BodyReader::new(None, 0, None);

        assert!(reader.feed(&owner, 1, &buffer, b" ", &cancel).unwrap());
        assert_eq!(reader.mode, BodyMode::Undecided);
        assert!(reader.finish(&owner, 1, &buffer, &cancel));
        assert_eq!(reader.mode, BodyMode::Audio);
        assert_eq!(buffer_data(&buffer).0, b" ");
    }

    #[test]
    fn weak_hint_accepts_a_bare_url_body() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let hint = Some(playlist::Hint {
            kind: playlist::Kind::M3u,
            trusted: false,
        });
        let mut reader = BodyReader::new(hint, 0, None);

        assert!(reader
            .feed(&owner, 1, &buffer, b"http://a/stream\n", &cancel)
            .unwrap());
        assert_eq!(reader.mode, BodyMode::Playlist);
    }

    #[test]
    fn weak_hint_still_streams_audio() {
        let buffer = new_shared();
        let owner = body_reader_owner();
        let cancel = AtomicBool::new(false);
        let hint = Some(playlist::Hint {
            kind: playlist::Kind::M3u,
            trusted: false,
        });
        let mut reader = BodyReader::new(hint, 0, None);

        assert!(reader.feed(&owner, 1, &buffer, b"RIFFdata", &cancel).unwrap());
        assert_eq!(reader.mode, BodyMode::Audio);
        assert_eq!(buffer_data(&buffer).0, b"RIFFdata");
    }

    /// Legacy Shoutcast servers answer `ICY 200 OK`, which hyper/reqwest rejects;
    /// the raw-HTTP path must still play them.
    #[test]
    fn reconnect_clears_the_title_and_ignores_late_attempts() {
        let request = PlayRequest::new("http://127.0.0.1:9/icy");
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let first = owner.begin_attempt();
        owner.set_now_playing(first, "Old title".to_string());
        assert_eq!(owner.snapshot().now_playing.as_deref(), Some("Old title"));

        let second = owner.begin_attempt();
        assert_eq!(owner.snapshot().now_playing, None);
        owner.set_now_playing(first, "Late old title".to_string());
        assert_eq!(owner.snapshot().now_playing, None);
        owner.set_now_playing(second, "New title".to_string());
        assert_eq!(owner.snapshot().now_playing.as_deref(), Some("New title"));
    }

    /// The download thread reads ahead of the speakers. The label must follow
    /// the audio, not the download.
    #[test]
    fn the_display_title_waits_for_the_queued_audio_before_it() {
        let request = PlayRequest::new("http://127.0.0.1:9/icy");
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        owner.set_now_playing(attempt, "First".to_string());
        assert_eq!(
            owner.snapshot().display_now_playing.as_deref(),
            Some("First")
        );

        // Four seconds of audio are queued when the second title arrives.
        owner.samples_pushed.store(10_000, Ordering::Relaxed);
        owner.set_now_playing(attempt, "Second".to_string());
        assert_eq!(owner.snapshot().now_playing.as_deref(), Some("Second"));
        assert_eq!(
            owner.snapshot().display_now_playing.as_deref(),
            Some("First")
        );

        owner.samples_played.store(9_999, Ordering::Relaxed);
        assert_eq!(
            owner.snapshot().display_now_playing.as_deref(),
            Some("First")
        );

        owner.samples_played.store(10_000, Ordering::Relaxed);
        assert_eq!(
            owner.snapshot().display_now_playing.as_deref(),
            Some("Second")
        );
    }

    #[test]
    fn offline_icy_capture_splits_three_tracks() {
        let dir = std::env::temp_dir().join(format!(
            "viberadio-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let addr = serve_icy_three_tracks();
        let buffer = new_shared();
        let request = PlayRequest::new(format!("http://{addr}/icy"));
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let mut config = crate::recording::RecordingConfig::new(&dir);
        config.mode = crate::recording::RecordingMode::SplitPerTrack;
        config.template = "{artist} - {title}".to_string();
        config.extension = "bin".to_string();
        owner.start_recording(config, "Tracks FM".to_string()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(fetch_http_once(
                &owner,
                attempt,
                &request.url,
                &buffer,
                &cancel,
                &wake,
            ))
            .unwrap();
        let summary = owner.stop_recording().unwrap();
        assert_eq!(summary.files, 3);
        assert!(!summary.dropped);
        assert_eq!(
            std::fs::read(dir.join("Artist One - Song One.bin")).unwrap(),
            b"PRE!AAAA"
        );
        assert_eq!(
            std::fs::read(dir.join("Artist Two - Song Two.bin")).unwrap(),
            b"BBBB"
        );
        assert_eq!(
            std::fs::read(dir.join("Artist Three - Song Three.bin")).unwrap(),
            b"CCCC"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_only_stream_records_and_splits_without_playback() {
        let dir = std::env::temp_dir().join(format!(
            "viberadio-record-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let addr = serve_icy_three_tracks();
        let player = RadioPlayer::new();
        let mut config = crate::recording::RecordingConfig::new(&dir);
        config.mode = crate::recording::RecordingMode::SplitPerTrack;
        config.template = "{artist} - {title}".to_string();
        config.extension = "bin".to_string();
        let id = player.play_stream(PlayRequest {
            record_only: true,
            reconnect: None,
            ..PlayRequest::new(format!("http://{addr}/icy"))
        });
        player
            .start_recording(id, config, "Tracks FM".to_string())
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut summary = None;
        while Instant::now() < deadline {
            if let Some(result) = player.take_finished_recording(id) {
                summary = Some(result);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let summary = summary.expect("the record-only stream did not finish");
        assert_eq!(summary.files, 3, "summary: {summary:?}");
        assert_eq!(
            std::fs::read(dir.join("Artist One - Song One.bin")).unwrap(),
            b"PRE!AAAA"
        );
        assert_eq!(
            std::fs::read(dir.join("Artist Two - Song Two.bin")).unwrap(),
            b"BBBB"
        );
        assert_eq!(
            std::fs::read(dir.join("Artist Three - Song Three.bin")).unwrap(),
            b"CCCC"
        );
        assert!(player
            .snapshot()
            .streams
            .iter()
            .find(|stream| stream.id == id)
            .map(|stream| stream.record_only)
            .unwrap_or(false));
        player.stop_stream(id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offline_icy_capture_continuous_mode_writes_one_file() {
        let dir = std::env::temp_dir().join(format!(
            "viberadio-continuous-capture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let addr = serve_icy_three_tracks();
        let buffer = new_shared();
        let request = PlayRequest::new(format!("http://{addr}/icy"));
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let mut config = crate::recording::RecordingConfig::new(&dir);
        config.mode = crate::recording::RecordingMode::Continuous;
        config.template = "continuous".to_string();
        config.extension = "bin".to_string();
        owner.start_recording(config, "Tracks FM".to_string()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(fetch_http_once(
                &owner,
                attempt,
                &request.url,
                &buffer,
                &Arc::new(AtomicBool::new(false)),
                &Arc::new(tokio::sync::Notify::new()),
            ))
            .unwrap();
        let summary = owner.stop_recording().unwrap();
        assert_eq!(summary.files, 1);
        assert_eq!(
            std::fs::read(dir.join("continuous.bin")).unwrap(),
            b"PRE!AAAABBBBCCCC"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_tee_receives_audio_after_the_demuxer() {
        let dir = std::env::temp_dir().join(format!(
            "viberadio-tee-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let request = PlayRequest::new("http://127.0.0.1:9/tee");
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let mut config = crate::recording::RecordingConfig::new(&dir);
        config.mode = crate::recording::RecordingMode::Continuous;
        config.template = "tee".to_string();
        config.extension = "bin".to_string();
        owner
            .start_recording(config, "Tee FM".to_string())
            .unwrap();
        assert!(owner.snapshot().recording);
        let buffer = new_shared();
        let mut demux = IcyDemuxer::new(0);
        assert!(push_demuxed(
            &owner,
            attempt,
            &buffer,
            &mut demux,
            b"audio-only",
            &AtomicBool::new(false),
        ));
        let summary = owner.stop_recording().unwrap();
        assert_eq!(summary.files, 1);
        assert!(!summary.dropped);
        assert_eq!(std::fs::read(dir.join("tee.bin")).unwrap(), b"audio-only");
        assert!(!owner.snapshot().recording);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_ring_keeps_one_second_and_adjusts_pushed_count() {
        let request = PlayRequest::new("http://127.0.0.1:9/flush");
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(0)),
        );
        // Two seconds of stereo 48 kHz audio.
        let samples = vec![0.1f32; 48_000 * 2 * 2];
        assert!(owner.push_batch(&samples));
        assert_eq!(owner.state.lock().unwrap().ring.len(), 192_000);
        owner.flush_ring_if_long(48_000, 2);
        assert_eq!(owner.state.lock().unwrap().ring.len(), 96_000);
        assert_eq!(owner.samples_pushed.load(Ordering::Relaxed), 96_000);
    }

    fn shared_with_buffer(seconds: u32) -> (Arc<StreamShared>, Arc<AtomicU32>) {
        let setting = Arc::new(AtomicU32::new(seconds));
        let request = PlayRequest::new("http://127.0.0.1:9/test");
        let owner = Arc::new(StreamShared::new_with_buffer(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
            Arc::clone(&setting),
        ));
        (owner, setting)
    }

    #[test]
    fn ring_limit_follows_the_stream_buffer_setting() {
        let (owner, setting) = shared_with_buffer(2);
        owner.set_ring_limit(48_000, 2);
        assert_eq!(owner.ring_limit(), 48_000 * 2 * 2);

        setting.store(8, Ordering::Relaxed);
        owner.set_ring_limit(48_000, 2);
        assert_eq!(owner.ring_limit(), 48_000 * 2 * 8);
    }

    #[test]
    fn output_choice_falls_back_only_for_a_named_device() {
        let (output, choice) = choose_output(Some("Missing"), |name| match name {
            Some("Missing") => Err("missing".to_string()),
            None => Ok("default"),
            Some(other) => panic!("unexpected device {other}"),
        });
        assert_eq!(output, Some("default"));
        assert_eq!(choice, OutputChoice::FellBack("missing".to_string()));

        let (output, choice) = choose_output(None, |_| Err::<&str, _>("no device".to_string()));
        assert_eq!(output, None);
        assert_eq!(choice, OutputChoice::Failed("no device".to_string()));
    }

    #[test]
    fn ring_limit_handles_formats_and_bounds() {
        assert_eq!(ring_limit_for(44_100, 1, 4), 44_100 * 4);
        assert_eq!(ring_limit_for(48_000, 2, 2), 48_000 * 2 * 2);
        // A zero format falls back to the default ring.
        assert_eq!(ring_limit_for(0, 2, 4), RING_SAMPLES);
        assert_eq!(ring_limit_for(48_000, 0, 4), RING_SAMPLES);
        // Absurd formats are capped; tiny ones keep the prebuffer floor.
        assert_eq!(ring_limit_for(384_000, 8, 30), RING_SAMPLES * 8);
        assert_eq!(ring_limit_for(1, 1, 1), PREBUFFER_SAMPLES);
        // The setting is clamped to the supported range.
        assert_eq!(
            ring_limit_for(48_000, 2, 0),
            ring_limit_for(48_000, 2, MIN_STREAM_BUFFER_SECS)
        );
        assert_eq!(
            ring_limit_for(48_000, 2, 999),
            ring_limit_for(48_000, 2, MAX_STREAM_BUFFER_SECS)
        );
    }

    #[test]
    fn push_batch_stops_at_the_configured_ring_limit() {
        let (owner, _) = shared_with_buffer(1);
        owner.set_ring_limit(4_000, 1);
        assert_eq!(owner.ring_limit(), 4_000);
        assert!(owner.push_batch(&vec![0.0; 4_000]));

        let writer = {
            let owner = Arc::clone(&owner);
            thread::spawn(move || owner.push_batch(&[0.0]))
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!writer.is_finished(), "the ring limit was not enforced");

        owner
            .state
            .lock()
            .unwrap()
            .ring
            .pop_front();
        owner.cond.notify_all();
        writer.join().unwrap();
    }

    #[test]
    fn stream_stores_gain_and_reads_the_shared_preset() {
        let request = PlayRequest {
            gain_db: 6.0,
            ..PlayRequest::new("http://127.0.0.1:9/dsp")
        };
        let preset = Arc::new(AtomicU32::new(DspPreset::Medium as u32));
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&preset),
        );
        assert_eq!(owner.gain_db(), 6.0);
        assert_eq!(owner.dsp_preset(), DspPreset::Medium);
        owner.set_gain_db(-3.0);
        assert_eq!(owner.gain_db(), -3.0);
        preset.store(DspPreset::Strong as u32, Ordering::Relaxed);
        assert_eq!(owner.dsp_preset(), DspPreset::Strong);
    }

    #[test]
    fn terminal_stream_keeps_the_recording_summary() {
        let dir = std::env::temp_dir().join(format!(
            "viberadio-terminal-recording-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let request = PlayRequest::new("http://127.0.0.1:9/terminal");
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let mut config = crate::recording::RecordingConfig::new(&dir);
        config.template = "terminal".to_string();
        config.extension = "bin".to_string();
        owner
            .start_recording(config, "Terminal FM".to_string())
            .unwrap();
        owner.record_audio(b"tail");
        owner.finish(PlaybackStatus::Stopped, None);
        let summary = owner
            .take_finished_recording()
            .expect("terminal path must keep the summary");
        assert_eq!(summary.bytes, 4);
        assert_eq!(std::fs::read(dir.join("terminal.bin")).unwrap(), b"tail");
        assert!(!owner.snapshot().recording);
        assert!(owner.take_finished_recording().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn icy_transport_strips_metadata_without_an_audio_device() {
        const AUDIO: &[u8] = b"abcdefghij";
        let addr = serve_icy_bytes(4, "Transport Artist - Transport Song", AUDIO);
        let buffer = new_shared();
        let request = PlayRequest::new(format!("http://{addr}/icy"));
        let owner = StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        );
        let attempt = owner.begin_attempt();
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(fetch_http_once(
                &owner,
                attempt,
                &request.url,
                &buffer,
                &cancel,
                &wake,
            ))
            .unwrap();
        assert!(matches!(outcome, HttpOutcome::Finished(_)));
        let (lock, _) = &*buffer;
        let state = lock.lock().unwrap();
        assert_eq!(state.data, AUDIO, "metadata bytes leaked into the decoder input");
        drop(state);
        assert_eq!(
            owner.snapshot().now_playing.as_deref(),
            Some("Transport Artist - Transport Song")
        );
    }

    #[test]
    fn icy_metadata_is_stripped_and_reaches_the_snapshot() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let addr = serve_icy_wav(1024, "Test Artist - ICY Song");
        let player = RadioPlayer::new();
        let id = player.play_stream(PlayRequest::new(format!("http://{addr}/icy.wav")));
        assert!(
            wait_until(Duration::from_secs(20), || {
                let snapshot = player.snapshot();
                snapshot.active.as_ref().map(|stream| stream.status) == Some(PlaybackStatus::Live)
            }),
            "ICY stream never went live"
        );
        assert!(
            wait_until(Duration::from_secs(5), || {
                player
                    .snapshot()
                    .streams
                    .iter()
                    .find(|stream| stream.id == id)
                    .and_then(|stream| stream.now_playing.clone())
                    .as_deref()
                    == Some("Test Artist - ICY Song")
            }),
            "ICY StreamTitle did not reach the snapshot: {:?}",
            player
                .snapshot()
                .streams
                .iter()
                .find(|stream| stream.id == id)
                .and_then(|stream| stream.now_playing.clone())
        );
        assert_eq!(
            player
                .snapshot()
                .streams
                .iter()
                .find(|stream| stream.id == id)
                .and_then(|stream| stream.reported_bitrate),
            Some(128)
        );
        player.stop_all();
    }

    #[test]
    fn plays_local_aac_stream_end_to_end() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tone.aac"
        ))
        .unwrap();
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let addr = serve_file(bytes, "audio/aac");
        assert_plays(&format!("http://{addr}/tone.aac"));
    }

    #[test]
    fn plays_legacy_icy_status_line() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let addr = serve_wav("ICY 200 OK", 1);
        assert_plays(&format!("http://{addr}/stream.wav"));
    }

    /// FR-P8: two streams play at once, and each keeps decoding independently.
    #[test]
    fn two_streams_play_independently() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let a = serve_wav("HTTP/1.0 200 OK", 10);
        let b = serve_wav("HTTP/1.0 200 OK", 10);

        let player = RadioPlayer::new();
        let id_a = player.play_stream(PlayRequest::new(format!("http://{a}/a.wav")));
        let id_b = player.play_stream(PlayRequest::new(format!("http://{b}/b.wav")));

        assert!(
            wait_until(Duration::from_secs(15), || {
                let streams = snapshot_streams(&player);
                streams.len() == 2
                    && streams
                        .iter()
                        .all(|stream| stream.status == PlaybackStatus::Live)
            }),
            "both streams must go live: {:?}",
            snapshot_streams(&player)
        );

        let before: Vec<u64> = player
            .snapshot().streams
            .iter()
            .map(|stream| stream.samples_played)
            .collect();
        std::thread::sleep(Duration::from_millis(1_000));
        let after: Vec<u64> = player
            .snapshot().streams
            .iter()
            .map(|stream| stream.samples_played)
            .collect();
        assert!(
            after[0] > before[0] + 10_000 && after[1] > before[1] + 10_000,
            "both streams must keep decoding: {before:?} -> {after:?}"
        );

        assert!(stream_is_active(&player, id_a));
        assert!(stream_is_active(&player, id_b));
        player.stop_all();
        assert!(snapshot_streams(&player).is_empty());
    }

    /// The architecture's core promise: a stalled stream must not silence the
    /// other one. This is the falsifier the round-1 advisor asked for.
    #[test]
    fn stalled_stream_does_not_block_another() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        // Long enough that a slow machine cannot finish it during the test.
        let healthy = serve_wav("HTTP/1.0 200 OK", 30);
        let stalled = serve_stalled_wav();

        let player = RadioPlayer::new();
        let healthy_id = player.play_stream(PlayRequest::new(format!(
            "http://{healthy}/healthy.wav"
        )));
        let stalled_id =
            player.play_stream(PlayRequest::new(format!("http://{stalled}/stalled.wav")));

        assert!(
            wait_until(Duration::from_secs(15), || {
                stream_is_active(&player, healthy_id) && stream_is_active(&player, stalled_id)
            }),
            "both must start: {:?}",
            snapshot_streams(&player)
        );

        // Wait until the stalled stream has played its short burst and its
        // sample counter stops advancing for 500 ms.
        let mut stalled_before = 0;
        let mut stable_since = Instant::now();
        assert!(
            wait_until(Duration::from_secs(10), || {
                let now = player
                    .snapshot().streams
                    .into_iter()
                    .find(|stream| stream.id == stalled_id)
                    .unwrap()
                    .samples_played;
                if now != stalled_before {
                    stalled_before = now;
                    stable_since = Instant::now();
                    false
                } else {
                    stalled_before > 1_000 && stable_since.elapsed() > Duration::from_millis(500)
                }
            }),
            "the stalled stream never settled at {} samples",
            stalled_before
        );
        let healthy_before = player
            .snapshot().streams
            .into_iter()
            .find(|stream| stream.id == healthy_id)
            .unwrap()
            .samples_played;

        // Once the quiet producer is past the 1 s stall threshold, it must be
        // reported as buffering, and the healthy stream must have advanced.
        assert!(
            wait_until(Duration::from_secs(5), || {
                player
                    .snapshot().streams
                    .into_iter()
                    .find(|stream| stream.id == stalled_id)
                    .map(|stream| stream.status == PlaybackStatus::Buffering)
                    .unwrap_or(false)
            }),
            "a quiet producer must be reported as buffering: {:?}",
            snapshot_streams(&player)
        );
        std::thread::sleep(Duration::from_millis(1_000));
        let snapshots = snapshot_streams(&player);
        let stalled_now = snapshots
            .iter()
            .find(|stream| stream.id == stalled_id)
            .unwrap();
        let healthy_now = snapshots
            .iter()
            .find(|stream| stream.id == healthy_id)
            .unwrap();
        assert_eq!(
            stalled_now.samples_played, stalled_before,
            "the stalled stream must stop advancing"
        );
        assert!(
            healthy_now.samples_played > healthy_before + 10_000,
            "the healthy stream must keep playing while the other stalls: {} -> {}",
            healthy_before,
            healthy_now.samples_played
        );
        player.stop_all();
    }

    /// FR-P6: Stop during connect must cancel promptly, even while the decoder
    /// probe is blocked waiting for headers.
    #[test]
    fn stop_during_connect_cancels() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let (silent, client_closed) = serve_silent();
        let player = RadioPlayer::new();
        let id = player.play_stream(PlayRequest::new(format!("http://{silent}/never.mp3")));

        // Give the worker time to enter the blocked header read.
        std::thread::sleep(Duration::from_millis(300));
        player.stop_stream(id);
        assert!(
            snapshot_streams(&player).is_empty(),
            "stop must remove the stream immediately"
        );
        // The download task must really go away, not merely disappear from the UI.
        assert!(
            wait_until(Duration::from_secs(3), || client_closed.load(Ordering::SeqCst)),
            "the server never saw the client close the connection"
        );
        let dropped = Instant::now();
        drop(player);
        assert!(
            dropped.elapsed() < Duration::from_secs(3),
            "dropping the engine took {:?}",
            dropped.elapsed()
        );
    }

    /// FR-P11: a dropped stream reconnects with backoff instead of staying dead.
    #[test]
    fn dropped_stream_reconnects() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let (addr, connections) = serve_dropping_wav();
        let player = RadioPlayer::new();
        let policy = ReconnectPolicy {
            max_attempts: 5,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(400),
        };
        let mut request = PlayRequest::new(format!("http://{addr}/drop.wav"));
        request.reconnect = Some(policy);
        let id = player.play_stream(request);

        let reconnected = wait_until(Duration::from_secs(10), || {
            connections.load(Ordering::SeqCst) >= 2
        });
        assert!(
            reconnected,
            "expected at least two connections, saw {}",
            connections.load(Ordering::SeqCst)
        );

        // Audio must actually resume on the new connection, not merely reconnect.
        let after_second = player
            .snapshot().streams
            .into_iter()
            .find(|stream| stream.id == id)
            .map(|stream| stream.samples_played)
            .unwrap_or(0);
        assert!(
            wait_until(Duration::from_secs(8), || {
                player
                    .snapshot().streams
                    .into_iter()
                    .find(|stream| stream.id == id)
                    .map(|stream| stream.samples_played > after_second + 1_000)
                    .unwrap_or(false)
            }),
            "no audio was decoded after reconnecting"
        );
        player.stop_all();
    }

    #[test]
    fn cancel_during_connect_stops_download_thread() {
        // Start a connection to a black-holed address, then cancel from another
        // thread. The lost-wakeup race in the old implementation would leave the
        // task waiting for the connect timeout.
        let shared = new_shared();
        let request = PlayRequest::new("http://192.0.2.1:81/never");
        let owner = Arc::new(StreamShared::new(
            1,
            &request,
            None,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU32::new(DspPreset::Off as u32)),
        ));
        let cancel = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(tokio::sync::Notify::new());
        let handle = {
            let owner = Arc::clone(&owner);
            let shared = Arc::clone(&shared);
            let cancel = Arc::clone(&cancel);
            let wake = Arc::clone(&wake);
            std::thread::spawn(move || {
                download_into(
                    owner,
                    shared,
                    "http://192.0.2.1:81/never".to_string(),
                    cancel,
                    wake,
                    0,
                )
            })
        };

        // Cancel immediately: Notify stores a permit, so a wakeup that lands
        // before the fetch task waits must still be observed.
        cancel.store(true, Ordering::SeqCst);
        wake.notify_one();

        assert!(
            wait_until(Duration::from_secs(3), || handle.is_finished()),
            "cancelled download did not stop"
        );
        handle.join().unwrap();
        let (lock, _) = &*shared;
        let st = lock.lock().unwrap();
        // Depending on the environment the connect may already have failed; what
        // matters is that the task stopped and the buffer closed cleanly or with
        // the connect error, never by hanging.
        assert!(
            st.eof || st.error.is_some(),
            "cancelled download left the buffer open"
        );
    }

    /// Live HLS smoke test: pull one real MPEG-TS segment and demux its audio.
    ///
    /// Ignored by default because it needs outbound network. Run with
    /// `cargo test -- --ignored live_hls_segment_demuxes`.
    #[test]
    #[ignore = "needs outbound network"]
    fn live_hls_segment_demuxes() {
        const MASTER: &str = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap();
            let cancel = AtomicBool::new(false);
            let wake = tokio::sync::Notify::new();
            let master = hls_get(
                &client,
                MASTER,
                hls::MAX_PLAYLIST_BYTES,
                &cancel,
                &wake,
            )
            .await
            .unwrap()
            .unwrap();
            let variant = hls::master_variant(&master, MASTER).unwrap().unwrap();
            let media = hls_get(
                &client,
                &variant,
                hls::MAX_PLAYLIST_BYTES,
                &cancel,
                &wake,
            )
            .await
            .unwrap()
            .unwrap();
            let playlist = hls::media_playlist(&media, &variant).unwrap();
            let segment = hls_get(
                &client,
                &playlist.segments[0].url,
                hls::MAX_SEGMENT_BYTES,
                &cancel,
                &wake,
            )
            .await
            .unwrap()
            .unwrap();
            let mut demuxer = crate::mpegts::TsDemuxer::new();
            let mut audio = Vec::new();
            assert!(demuxer.demux(&segment, &mut audio), "not MPEG-TS");
            assert!(audio.len() > 1_000, "no audio extracted");
            assert_eq!(audio[0], 0xff, "audio does not start with ADTS sync");
            assert_eq!(audio[1] & 0xf0, 0xf0);
        });
    }

    /// Live-station smoke test: the one thing every fixture cannot prove.
    ///
    /// Ignored by default because it needs outbound network. Run with
    /// `VIBERADIO_LIVE_URL=... cargo test -- --ignored plays_a_live_station`.
    #[test]
    #[ignore = "needs outbound network"]
    fn plays_a_live_station() {
        let _guard = audio_test_lock();
        if !has_audio_device() {
            return;
        }
        let url = std::env::var("VIBERADIO_LIVE_URL")
            .unwrap_or_else(|_| "https://ice1.somafm.com/groovesalad-128-mp3".to_string());
        let player = RadioPlayer::new();
        player.play_stream(PlayRequest::new(url.as_str()));

        let started = wait_until(Duration::from_secs(30), || {
            active_status(&player).starts_with("Playing")
        });
        if !started {
            panic!(
                "live playback did not start for {url}; last status: {}",
                active_status(&player)
            );
        }

        // A live stream must keep playing, not drain after the first buffer.
        std::thread::sleep(Duration::from_secs(5));
        assert!(
            any_playing(&player),
            "live stream stopped after start: {}",
            active_status(&player)
        );

        let stopped = Instant::now();
        player.stop_all();
        assert!(!any_playing(&player), "Stop did not take effect");
        assert!(
            stopped.elapsed() < Duration::from_secs(2),
            "Stop took {:?}",
            stopped.elapsed()
        );
    }
}
