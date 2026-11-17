//! Background coordinator.
//!
//! The egui frame callback stops running when the window is minimised or
//! hidden, but scheduled recordings and wish captures must still happen. This
//! thread owns the scheduler runtime and applies start/stop effects through a
//! [`PlayerControl`], independent of the UI. It reports what it did so the UI
//! can persist marks and update its maps when it next runs.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::database::DbManager;
use crate::metadata::{self, WishEntry};
use crate::player::{PlayRequest, PlaybackId, PlayerControl, ReconnectPolicy};
use crate::recording::{RecordedSummary, RecordingConfig, RecordingMode};
use crate::scheduler::{self, FinishAction, Schedule, SchedulerEffect, SchedulerRuntime};

/// The station fields the coordinator needs to open a stream and match wishes.
#[derive(Debug, Clone)]
pub struct CoordStation {
    pub id: Uuid,
    pub name: String,
    pub url: String,
    pub watched: bool,
    pub bitrate: Option<u32>,
    pub codec: Option<String>,
}

/// State the UI keeps in sync. The station list and id set are shared behind
/// `Arc`, so cloning this once per tick does not copy a large directory.
#[derive(Debug, Clone, Default)]
pub struct CoordinatorState {
    pub schedules: Vec<Schedule>,
    pub stations: Arc<Vec<CoordStation>>,
    pub known: Arc<HashSet<Uuid>>,
    pub recording: Option<RecordingConfig>,
    pub wishlist: Vec<WishEntry>,
    pub ignore_list: Vec<(Option<String>, Option<String>)>,
    /// Per-station tag format used to parse an ICY label for wishes.
    pub tag_formats: HashMap<Uuid, String>,
    /// A wishlist-only setting forces split files and keeps only wishes.
    pub wishlist_only: bool,
    pub min_free_mb: u32,
    pub min_kbps: u32,
    pub codec: String,
    /// Schedule ids whose active window the UI deleted, disabled or edited.
    pub cancel: Vec<Uuid>,
    /// Windows the coordinator currently holds, for the schedules window.
    pub active_count: usize,
    pub enabled: bool,
}

/// What the coordinator did, for the UI to persist and display.
#[derive(Debug)]
pub enum CoordinatorEvent {
    ScheduleFired {
        id: Uuid,
        at: DateTime<Utc>,
    },
    Started {
        station_id: Uuid,
        playback_id: PlaybackId,
        recording: bool,
        split: bool,
        audible: bool,
    },
    Stopped {
        playback_id: Option<PlaybackId>,
        recording: bool,
    },
    RecordingFinished(RecordedSummary),
    WishHit {
        station: Uuid,
        playback_id: PlaybackId,
        title: String,
    },
    Finish(FinishAction),
    Status(String),
}

pub struct Coordinator {
    state: Arc<Mutex<CoordinatorState>>,
    events: Receiver<CoordinatorEvent>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    /// Recording writers closing on their own thread; joined on shutdown so a
    /// file is never truncated by process exit.
    finishers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
}

impl Coordinator {
    pub fn start(
        control: PlayerControl,
        state: CoordinatorState,
        db_path: Option<String>,
    ) -> Self {
        let state = Arc::new(Mutex::new(state));
        let worker_state = Arc::clone(&state);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let finishers = Arc::new(Mutex::new(Vec::new()));
        let worker_finishers = Arc::clone(&finishers);
        let (tx, events) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("viberadio-coordinator".to_string())
            .spawn(move || {
                let mut scheduler = SchedulerRuntime::new();
                let mut recordings: HashMap<PlaybackId, Uuid> = HashMap::new();
                let db = db_path.and_then(|path| match DbManager::new(&path) {
                    Ok(db) => Some(db),
                    Err(e) => {
                        log::warn!("coordinator database unavailable: {e}");
                        None
                    }
                });
                while !worker_stop.load(Ordering::Relaxed) {
                    let snapshot = worker_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if snapshot.enabled {
                        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            tick(
                                &control,
                                &worker_state,
                                &mut scheduler,
                                &mut recordings,
                                &worker_finishers,
                                &tx,
                                &snapshot,
                                db.as_ref(),
                            );
                        }));
                        if outcome.is_err() {
                            log::error!("coordinator tick panicked; continuing");
                            let _ = tx.send(CoordinatorEvent::Status(
                                "Coordinator error; scheduled recordings may be paused".to_string(),
                            ));
                            thread::sleep(Duration::from_secs(1));
                        }
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            })
            .expect("failed to spawn the coordinator thread");
        Self {
            state,
            events,
            stop,
            handle: Some(handle),
            finishers,
        }
    }

    /// Replace part of the coordinator's state.
    pub fn update(&self, f: impl FnOnce(&mut CoordinatorState)) {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn try_recv(&self) -> Option<CoordinatorEvent> {
        self.events.try_recv().ok()
    }

    pub fn active_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_count
    }

    /// True when the worker thread has stopped, so the UI can fall back.
    pub fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(true)
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let finishers = std::mem::take(
            &mut *self
                .finishers
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for handle in finishers {
            let _ = handle.join();
        }
    }
}

fn tick(
    control: &PlayerControl,
    state: &Arc<Mutex<CoordinatorState>>,
    scheduler: &mut SchedulerRuntime,
    recordings: &mut HashMap<PlaybackId, Uuid>,
    finishers: &Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    tx: &Sender<CoordinatorEvent>,
    snapshot: &CoordinatorState,
    db: Option<&DbManager>,
) {
    // The UI deleted, disabled or edited a schedule; stop its window now.
    let cancel = {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut guard.cancel)
    };
    for id in cancel {
        if let Some(SchedulerEffect::Stop {
            playback_id,
            owns_stream,
            owns_recording,
            ..
        }) = scheduler.cancel(id)
        {
            stop(
                control,
                recordings,
                finishers,
                tx,
                playback_id,
                owns_stream,
                owns_recording,
            );
        }
    }
    for effect in scheduler.retain_valid(&snapshot.schedules, &snapshot.known) {
        if let SchedulerEffect::Stop {
            playback_id,
            owns_stream,
            owns_recording,
            ..
        } = effect
        {
            stop(
                control,
                recordings,
                finishers,
                tx,
                playback_id,
                owns_stream,
                owns_recording,
            );
        }
    }

    for event in control.take_titles() {
        let Some(station) = snapshot
            .stations
            .iter()
            .find(|station| Some(station.id) == event.station_id)
        else {
            continue;
        };
        let tag_format = snapshot.tag_formats.get(&station.id).map(String::as_str);

        // Split a scheduled recording at each title, even while hidden.
        for (id, station_id) in recordings.iter() {
            if *station_id != station.id {
                continue;
            }
            let format = tag_format
                .map(str::to_string)
                .unwrap_or_else(|| metadata::default_tag_format(&event.title).to_string());
            let tag = metadata::split_title(&event.title, &format);
            control.set_recording_track(*id, tag.artist.as_deref(), tag.title.as_deref());
        }

        // Match wishes against a watched station's title.
        if !station.watched || snapshot.wishlist.is_empty() {
            continue;
        }
        if metadata::title_is_ignored_with_format(
            &snapshot.ignore_list,
            &event.title,
            tag_format,
        ) {
            continue;
        }
        let hits =
            metadata::wish_matches_with_format(&snapshot.wishlist, &event.title, tag_format);
        if !hits
            .iter()
            .any(|entry| entry.action.eq_ignore_ascii_case("record"))
        {
            continue;
        }
        let watch_id = control
            .snapshot()
            .streams
            .iter()
            .find(|stream| stream.id == event.playback_id && stream.watching)
            .map(|stream| stream.id);
        let (Some(watch_id), Some(base)) = (watch_id, snapshot.recording.as_ref()) else {
            continue;
        };
        let mut config = base.clone();
        config.mode = RecordingMode::SplitPerTrack;
        config.keep_short = true;
        config.wishlist = snapshot.wishlist.clone();
        config.tag_format = tag_format.map(str::to_string);
        config.one_shot = false;
        match control.promote_watch(
            watch_id,
            config,
            station.name.clone(),
            None,
            Some(&event.title),
        ) {
            Ok(true) => {
                recordings.insert(watch_id, station.id);
                let _ = tx.send(CoordinatorEvent::WishHit {
                    station: station.id,
                    playback_id: watch_id,
                    title: event.title.clone(),
                });
            }
            Ok(false) => {}
            Err(e) => {
                let _ = tx.send(CoordinatorEvent::Status(format!(
                    "Could not record {}: {e}",
                    event.title
                )));
            }
        }
    }

    let result = scheduler.tick(
        &snapshot.schedules,
        &snapshot.known,
        Utc::now(),
        &scheduler::resolve_local,
    );

    if !result.marks.is_empty() {
        let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
        for (id, at) in &result.marks {
            if let Some(schedule) = guard.schedules.iter_mut().find(|schedule| schedule.id == *id) {
                schedule.last_fired_at = Some(*at);
            }
        }
    }
    for (id, at) in &result.marks {
        if let Some(db) = db {
            if let Err(e) = db.mark_schedule_fired(*id, *at) {
                log::warn!("could not mark schedule {id} fired: {e}");
            }
        }
    }
    for (id, at) in result.marks {
        let _ = tx.send(CoordinatorEvent::ScheduleFired { id, at });
    }

    for effect in result.effects {
        match effect {
            SchedulerEffect::Start {
                schedule_id,
                station_id,
                action,
            } => {
                let Some(station) = snapshot
                    .stations
                    .iter()
                    .find(|station| station.id == station_id)
                else {
                    continue;
                };
                let record_only = !action.plays();
                // Adopt a stream the user (or another schedule) already has
                // open for this station instead of opening a duplicate.
                let existing = control.snapshot().streams.into_iter().find(|stream| {
                    stream.station_id == Some(station.id)
                        && if record_only {
                            stream.record_only
                        } else {
                            !stream.record_only
                        }
                });
                let (playback_id, owns_stream, already_recording) = match existing {
                    Some(stream) => (stream.id, false, stream.recording),
                    None => {
                        let mut request = PlayRequest::new(station.url.clone());
                        request.station_id = Some(station.id);
                        request.record_only = record_only;
                        request.volume = if record_only { 0.0 } else { 1.0 };
                        request.reconnect = Some(ReconnectPolicy::default());
                        (control.play_stream(request), true, false)
                    }
                };
                let mut recording = already_recording;
                let mut owns_recording = false;
                if already_recording {
                    recordings.insert(playback_id, station_id);
                }
                if action.records() && !already_recording {
                    if let Some(base) = &snapshot.recording {
                        if !crate::disk::has_space(&base.dir, snapshot.min_free_mb) {
                            let _ = tx.send(CoordinatorEvent::Status(format!(
                                "Not enough disk space to record {}",
                                station.name
                            )));
                        } else if !crate::app::recording_quality_allowed(
                            station.bitrate,
                            station.codec.as_deref(),
                            snapshot.min_kbps,
                            &snapshot.codec,
                        ) {
                            let _ = tx.send(CoordinatorEvent::Status(format!(
                                "{} does not meet the recording quality settings",
                                station.name
                            )));
                        } else {
                            let mut config = base.clone();
                            let split = action.split() || snapshot.wishlist_only;
                            config.mode = if split {
                                RecordingMode::SplitPerTrack
                            } else {
                                RecordingMode::Continuous
                            };
                            config.one_shot = false;
                            config.keep_short = snapshot.wishlist_only;
                            if !snapshot.wishlist_only {
                                config.wishlist.clear();
                            }
                            if control
                                .start_recording(playback_id, config, station.name.clone())
                                .is_ok()
                            {
                                recording = true;
                                owns_recording = true;
                                recordings.insert(playback_id, station_id);
                            }
                        }
                    }
                }
                scheduler.attach(
                    schedule_id,
                    Some(playback_id),
                    owns_stream,
                    owns_recording,
                );
                let _ = tx.send(CoordinatorEvent::Started {
                    station_id,
                    playback_id,
                    recording,
                    split: recording && (action.split() || snapshot.wishlist_only),
                    audible: !record_only,
                });
            }
            SchedulerEffect::Stop {
                playback_id,
                owns_stream,
                owns_recording,
                ..
            } => stop(
                control,
                recordings,
                finishers,
                tx,
                playback_id,
                owns_stream,
                owns_recording,
            ),
            SchedulerEffect::Finish { action, .. } => {
                let _ = tx.send(CoordinatorEvent::Finish(action));
            }
        }
    }

    if let Some(message) = scheduler.message.take() {
        let _ = tx.send(CoordinatorEvent::Status(message));
    }

    state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active_count = scheduler.active_count();
}

/// Stop a stream/recording owned by a schedule. A summary already produced by
/// the stream's terminal path is taken before the stream is removed.
fn stop(
    control: &PlayerControl,
    recordings: &mut HashMap<PlaybackId, Uuid>,
    finishers: &Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    tx: &Sender<CoordinatorEvent>,
    playback_id: Option<PlaybackId>,
    owns_stream: bool,
    owns_recording: bool,
) {
    let Some(id) = playback_id else {
        return;
    };
    recordings.remove(&id);
    if owns_recording {
        if let Some(summary) = control.take_finished_recording(id) {
            let _ = tx.send(CoordinatorEvent::RecordingFinished(summary));
        } else if let Some(handle) = control.stop_recording_async(id) {
            let tx = tx.clone();
            let finisher = thread::spawn(move || {
                let summary = handle.join().unwrap_or_default();
                let _ = tx.send(CoordinatorEvent::RecordingFinished(summary));
            });
            finishers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(finisher);
        }
    }
    if owns_stream {
        control.stop_stream(id);
    }
    let _ = tx.send(CoordinatorEvent::Stopped {
        playback_id: Some(id),
        recording: owns_recording,
    });
}

/// Keep the later `last_fired_at` when the UI syncs its copy over the
/// coordinator's. The coordinator may have fired while the UI was hidden.
pub(crate) fn merge_marks(coordinator: &[Schedule], incoming: &mut [Schedule]) {
    for schedule in incoming {
        if let Some(existing) = coordinator.iter().find(|s| s.id == schedule.id) {
            if existing.last_fired_at > schedule.last_fired_at {
                schedule.last_fired_at = existing.last_fired_at;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{Recurrence, ScheduleAction};
    use chrono::TimeZone;

    fn schedule(at: DateTime<Utc>) -> Schedule {
        Schedule {
            id: Uuid::new_v4(),
            station_id: Uuid::new_v4(),
            action: ScheduleAction::Play,
            recurrence: Recurrence::Once,
            start_at: at.naive_utc(),
            end_at: None,
            finish_action: FinishAction::None,
            enabled: true,
            last_fired_at: Some(at),
        }
    }

    #[test]
    fn merge_marks_keeps_the_later_fire_time() {
        let now = Utc.with_ymd_and_hms(2026, 11, 16, 0, 0, 0).unwrap();
        let later = now + chrono::Duration::minutes(5);
        let id = Uuid::new_v4();
        let mut fired = schedule(later);
        fired.id = id;
        let mut stale = schedule(now);
        stale.id = id;
        let mut incoming = vec![stale];
        merge_marks(&[fired], &mut incoming);
        assert_eq!(incoming[0].last_fired_at, Some(later));
    }
}
