//! OS media keys and now-playing metadata.
//!
//! `souvlaki` exposes the platform's media controls: MPRIS on Linux (over zbus,
//! so no libdbus build dependency), System Media Transport Controls on Windows
//! and Now Playing on macOS. The desktop then sends play/pause/next/previous and
//! volume to the app, and shows the station and current title. Windows is
//! disabled until the app can pass its window handle: souvlaki's backend panics
//! without one.
//!
//! The handler runs on souvlaki's own thread; it only pushes an event and wakes
//! the UI, which drains it once per frame.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// What the OS media controls asked the app to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaEvent {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Volume(f32),
    /// Bring the main window forward.
    Raise,
    Quit,
}

/// What the desktop shows for the current playback.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaState {
    pub station: String,
    pub title: Option<String>,
    pub playing: bool,
    pub volume: f32,
}

#[derive(Clone)]
struct EventSink {
    tx: Sender<MediaEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    fn send(&self, event: MediaEvent) {
        let _ = self.tx.send(event);
        (self.wake)();
    }
}

pub struct Media {
    controls: souvlaki::MediaControls,
    events: Receiver<MediaEvent>,
    /// Set after the first failed property update, so a dead service is logged
    /// once instead of every frame.
    warned: bool,
}

impl Media {
    /// Create the platform media controls. `wake` is called after each event so
    /// the app repaints without waiting for its periodic tick.
    pub fn start(state: MediaState, wake: impl Fn() + Send + Sync + 'static) -> Option<Media> {
        // souvlaki's Windows backend panics without a window handle, and this
        // app does not pass one yet. Disable it there rather than crash.
        #[cfg(target_os = "windows")]
        {
            log::info!("media controls are not wired on Windows yet");
            return None;
        }

        #[cfg(target_os = "linux")]
        if !session_bus_available() {
            log::info!("media keys unavailable: no D-Bus session bus");
            return None;
        }

        // souvlaki's MPRIS thread unwraps the name request, so a second window
        // would panic it. Leave media keys to the window that already owns them.
        #[cfg(target_os = "linux")]
        if mpris_name_taken() {
            log::info!("media keys left to the VibeRadio window already running");
            return None;
        }

        let mut controls = match souvlaki::MediaControls::new(souvlaki::PlatformConfig {
            display_name: "VibeRadio",
            dbus_name: "viberadio",
            hwnd: None,
        }) {
            Ok(controls) => controls,
            Err(e) => {
                log::warn!("media controls unavailable: {e}");
                return None;
            }
        };
        let (tx, events) = mpsc::channel();
        let sink = EventSink {
            tx,
            wake: Arc::new(wake),
        };
        if let Err(e) = controls.attach(move |event| {
            if let Some(event) = map_event(event) {
                sink.send(event);
            }
        }) {
            log::warn!("cannot attach media controls: {e}");
            return None;
        }
        let mut media = Media {
            controls,
            events,
            warned: false,
        };
        media.update(&state);
        Some(media)
    }

    /// Publish the current station, title, playback and volume. Call only when
    /// the state changes; each call sends a D-Bus property update.
    pub fn update(&mut self, state: &MediaState) {
        use souvlaki::{MediaMetadata, MediaPlayback};

        let (title, artist) = labels(state);
        if let Err(e) = self.controls.set_metadata(MediaMetadata {
            title,
            artist,
            album: None,
            cover_url: None,
            duration: None,
        }) {
            self.warn(e);
        }
        let playback = if state.playing {
            MediaPlayback::Playing { progress: None }
        } else {
            MediaPlayback::Stopped
        };
        if let Err(e) = self.controls.set_playback(playback) {
            self.warn(e);
        }
        // Only the MPRIS backend exposes a volume property.
        #[cfg(target_os = "linux")]
        if let Err(e) = self
            .controls
            .set_volume(state.volume.clamp(0.0, 1.0) as f64)
        {
            self.warn(e);
        }
    }

    fn warn(&mut self, error: souvlaki::Error) {
        if !self.warned {
            log::warn!("media controls stopped: {error}");
            self.warned = true;
        }
    }

    pub fn poll(&self) -> Option<MediaEvent> {
        self.events.try_recv().ok()
    }
}

/// Title and artist for the desktop widget. When there is no track title the
/// station is the title and the artist is left empty, so it is not shown twice.
fn labels(state: &MediaState) -> (Option<&str>, Option<&str>) {
    let explicit = state
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty());
    let station = (!state.station.is_empty()).then_some(state.station.as_str());
    let title = explicit.or(station);
    let artist = if explicit.is_some() { station } else { None };
    (title, artist)
}

fn map_event(event: souvlaki::MediaControlEvent) -> Option<MediaEvent> {
    use souvlaki::MediaControlEvent as Event;
    match event {
        Event::Play => Some(MediaEvent::Play),
        Event::Pause => Some(MediaEvent::Pause),
        Event::Toggle => Some(MediaEvent::Toggle),
        Event::Stop => Some(MediaEvent::Stop),
        Event::Next => Some(MediaEvent::Next),
        Event::Previous => Some(MediaEvent::Previous),
        Event::SetVolume(volume) if volume.is_finite() => {
            Some(MediaEvent::Volume(volume as f32))
        }
        Event::SetVolume(_) => None,
        Event::Raise => Some(MediaEvent::Raise),
        Event::Quit => Some(MediaEvent::Quit),
        // Live radio has nothing to seek or open by URI.
        _ => None,
    }
}

/// Whether another process already owns the MPRIS name this app registers.
#[cfg(target_os = "linux")]
fn mpris_name_taken() -> bool {
    const NAME: &str = "org.mpris.MediaPlayer2.viberadio";
    let Ok(connection) = zbus::blocking::Connection::session() else {
        return false;
    };
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(NAME,),
        )
        .ok()
        .and_then(|reply| reply.body::<bool>().ok())
        .unwrap_or(false)
}

/// Whether a session bus is reachable. Without one, souvlaki's service thread
/// would panic, so the app simply runs without media controls.
#[cfg(target_os = "linux")]
fn session_bus_available() -> bool {
    if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        let address = address.to_string_lossy();
        for part in address.split(';') {
            if let Some(path) = part.strip_prefix("unix:path=") {
                // A stale socket file passes an existence check but not a connect.
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return true;
                }
            } else if let Some(rest) = part.strip_prefix("tcp:host=") {
                if let Some((host, port)) = rest.split_once(",port=") {
                    if let Ok(port) = port.parse::<u16>() {
                        if std::net::TcpStream::connect((host, port)).is_ok() {
                            return true;
                        }
                    }
                }
            }
        }
        // Abstract sockets and unusual transports: assume the bus exists rather
        // than disabling media keys on a working desktop.
        if !address.contains("unix:path=") && !address.contains("tcp:host=") {
            return true;
        }
        return false;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| std::path::Path::new(&dir).join("bus").exists())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_events_map_from_the_platform() {
        use souvlaki::MediaControlEvent as Event;
        assert_eq!(map_event(Event::Play), Some(MediaEvent::Play));
        assert_eq!(map_event(Event::Pause), Some(MediaEvent::Pause));
        assert_eq!(map_event(Event::Toggle), Some(MediaEvent::Toggle));
        assert_eq!(map_event(Event::Stop), Some(MediaEvent::Stop));
        assert_eq!(map_event(Event::Next), Some(MediaEvent::Next));
        assert_eq!(map_event(Event::Previous), Some(MediaEvent::Previous));
        assert_eq!(map_event(Event::Raise), Some(MediaEvent::Raise));
        assert_eq!(map_event(Event::Quit), Some(MediaEvent::Quit));
        assert_eq!(
            map_event(Event::SetVolume(0.5)),
            Some(MediaEvent::Volume(0.5))
        );
        assert_eq!(
            map_event(Event::Seek(souvlaki::SeekDirection::Forward)),
            None
        );
        assert_eq!(
            map_event(Event::SeekBy(souvlaki::SeekDirection::Backward, std::time::Duration::from_secs(5))),
            None
        );
        assert_eq!(map_event(Event::OpenUri("http://a".into())), None);
        assert_eq!(map_event(Event::SetVolume(f64::NAN)), None);
    }

    #[test]
    fn labels_do_not_repeat_the_station_as_artist_without_a_title() {
        let state = MediaState {
            station: "Radio One".into(),
            title: None,
            playing: true,
            volume: 0.5,
        };
        assert_eq!(labels(&state), (Some("Radio One"), None));

        let state = MediaState {
            title: Some("Song".into()),
            ..state
        };
        assert_eq!(labels(&state), (Some("Song"), Some("Radio One")));

        let blank = MediaState {
            station: String::new(),
            title: Some("   ".into()),
            ..state
        };
        assert_eq!(labels(&blank), (None, None));
    }
}
