//! VibeRadio — a Rust/egui internet radio client
//! An internet radio streaming application with egui GUI

mod app;
mod bitrate;
mod database;
mod decode;
mod desktop;
mod disk;
mod dsp;
mod duration;
mod fingerprint;
mod fonts;
mod hls;
mod i18n;
mod icon;
mod levels;
mod logging;
mod media;
mod models;
mod mpegts;
mod player;
mod playlist;
mod radiobrowser;
mod radionet;
mod recording;
mod scheduler;
mod station_playlist;
mod system;
mod tray;
mod ui;
mod verified;

fn main() -> eframe::Result<()> {
    logging::init();
    desktop::install();

    // Use dirs crate for proper data directory
    // This will use:
    // - Linux: ~/.local/share/viberadio
    // - Windows: %APPDATA%\VibeRadio
    // - macOS: ~/Library/Application Support/VibeRadio

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_app_id("viberadio")
            .with_icon(std::sync::Arc::new(eframe::egui::IconData {
                rgba: icon::rgba(128),
                width: 128,
                height: 128,
            })),
        ..Default::default()
    };

    eframe::run_native(
        "VibeRadio",
        options,
        Box::new(|cc| Ok(Box::new(app::VibeRadioApp::new(cc)))),
    )
}
mod metadata;
