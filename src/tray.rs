//! Tray icon and its menu.
//!
//! On Linux the icon is a StatusNotifierItem served over D-Bus (ksni), so no
//! GTK or AppIndicator build dependency is needed. The service runs on its own
//! thread: menu callbacks only push an event and wake the UI, and state updates
//! are queued so a slow bus can never block a frame.
//!
//! Other platforms get a stub until a native implementation exists; the app
//! treats a missing tray as normal.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// What the tray menu asked the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Bring the main window forward.
    Show,
    /// Open or close the compact player window.
    ToggleMini,
    /// Stop when playing, otherwise play the selected station.
    TogglePlayback,
    Quit,
    /// The tray service stopped; the app should drop its handle.
    Closed,
}

/// What the tray displays. Compared by the app so D-Bus is only touched when
/// something visible changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayState {
    pub station: String,
    pub now_playing: Option<String>,
    pub playing: bool,
    /// The UI language, so a language change refreshes the menu labels.
    pub language: u8,
}

/// Wakes the UI after a menu callback, so an event never waits for the periodic
/// repaint (a minimised window may not be redrawn by the compositor).
#[derive(Clone)]
struct EventSink {
    tx: Sender<TrayEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    fn send(&self, event: TrayEvent) {
        let _ = self.tx.send(event);
        (self.wake)();
    }
}

/// A running tray service.
pub struct Tray {
    updates: Sender<TrayState>,
    events: Receiver<TrayEvent>,
}

impl Tray {
    /// Start the tray. `wake` is called after each menu event so the app
    /// repaints promptly; the service itself runs on its own thread and never
    /// blocks the UI with D-Bus calls.
    pub fn start(state: TrayState, wake: impl Fn() + Send + Sync + 'static) -> Option<Tray> {
        // The Linux service consumes both receivers; other platforms drop them,
        // so the element types must be explicit to stay inferable.
        let (updates, update_rx) = mpsc::channel::<TrayState>();
        let (event_tx, events) = mpsc::channel::<TrayEvent>();
        let sink = EventSink {
            tx: event_tx,
            wake: Arc::new(wake),
        };
        #[cfg(target_os = "linux")]
        {
            std::thread::Builder::new()
                .name("viberadio-tray".to_string())
                .spawn(move || imp::run(state, update_rx, sink))
                .ok()?;
            Some(Tray { updates, events })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (state, update_rx, sink, updates, events);
            None
        }
    }

    /// Queue a state change. Never blocks: the tray thread applies it.
    pub fn update(&self, state: TrayState) {
        let _ = self.updates.send(state);
    }

    pub fn poll(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use ksni::blocking::{Handle, TrayMethods};
    use ksni::menu::StandardItem;
    use ksni::{Icon, MenuItem, ToolTip, Tray as KsniTray};

    use super::{EventSink, TrayEvent, TrayState};
    use crate::i18n::t;

    /// The VibeRadio mark as ARGB32, which is what ksni expects.
    fn icon(size: i32) -> Icon {
        let rgba = crate::icon::rgba(size.max(1) as u32);
        let mut data = Vec::with_capacity(rgba.len());
        for pixel in rgba.chunks_exact(4) {
            data.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
        }
        Icon {
            width: size,
            height: size,
            data,
        }
    }

    struct VibeTray {
        events: EventSink,
        state: TrayState,
    }

    impl KsniTray for VibeTray {
        fn id(&self) -> String {
            "viberadio".to_string()
        }

        fn title(&self) -> String {
            "VibeRadio".to_string()
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            vec![icon(32)]
        }

        fn tool_tip(&self) -> ToolTip {
            let mut description = self.state.station.clone();
            if let Some(title) = &self.state.now_playing {
                if !description.is_empty() {
                    description.push_str(" — ");
                }
                description.push_str(title);
            }
            ToolTip {
                icon_name: String::new(),
                icon_pixmap: vec![icon(32)],
                title: "VibeRadio".to_string(),
                description,
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.events.send(TrayEvent::ToggleMini);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let show = self.events.clone();
            let mini = self.events.clone();
            let playback = self.events.clone();
            let quit = self.events.clone();
            let playback_label = if self.state.playing {
                t("tray.stop")
            } else {
                t("tray.play")
            };
            vec![
                StandardItem {
                    label: t("tray.show").to_string(),
                    activate: Box::new(move |_| show.send(TrayEvent::Show)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: t("tray.mini").to_string(),
                    activate: Box::new(move |_| mini.send(TrayEvent::ToggleMini)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: playback_label.to_string(),
                    activate: Box::new(move |_| playback.send(TrayEvent::TogglePlayback)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: t("tray.quit").to_string(),
                    activate: Box::new(move |_| quit.send(TrayEvent::Quit)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Own the ksni service on the tray thread. Every D-Bus call happens here,
    /// never on the UI thread.
    pub fn run(state: TrayState, updates: Receiver<TrayState>, sink: EventSink) {
        let tray = VibeTray {
            events: sink.clone(),
            state,
        };
        // A desktop without a StatusNotifierWatcher is normal (headless,
        // minimal WM); keep running so the icon appears if one starts.
        let handle: Handle<VibeTray> = match tray.assume_sni_available(true).spawn() {
            Ok(handle) => handle,
            Err(e) => {
                log::warn!("tray icon unavailable: {e}");
                sink.send(TrayEvent::Closed);
                return;
            }
        };
        loop {
            match updates.recv_timeout(Duration::from_secs(5)) {
                Ok(mut state) => {
                    // Only the newest state matters; a busy bus must not build
                    // a backlog of stale titles.
                    while let Ok(next) = updates.try_recv() {
                        state = next;
                    }
                    if handle.update(|tray| tray.state = state).is_none() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if handle.is_closed() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        sink.send(TrayEvent::Closed);
        handle.shutdown().wait();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn icon_is_argb_with_a_teal_centre_and_transparent_corner() {
            let icon = icon(32);
            assert_eq!(icon.width, 32);
            assert_eq!(icon.height, 32);
            assert_eq!(icon.data.len(), 32 * 32 * 4);
            let pixel = |x: usize, y: usize| &icon.data[(y * 32 + x) * 4..][..4];
            let center = pixel(16, 16);
            assert_eq!(center[0], 255);
            assert!(center[1] > 40 && center[2] > 150 && center[3] > 130);
            assert_eq!(pixel(0, 0), [0, 0, 0, 0]);
        }
    }
}
