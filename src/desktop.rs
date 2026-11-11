//! Install a user-level desktop entry and icon for portable builds.
//!
//! Packaged builds (Flatpak, deb, AppImage) install these system-wide. A binary
//! run from a tarball or a development path has no desktop integration, so KDE
//! and GNOME fall back to a generic window icon. Writing the entry and icon
//! into the user's data directory fixes the title bar, taskbar and launcher.

use std::path::PathBuf;

const DESKTOP: &str = include_str!("../packaging/viberadio.desktop");
const ICON: &str = include_str!("../packaging/viberadio.svg");

pub fn install() {
    // Flatpak and system packages already install the metadata under /app or
    // /usr; a user-level duplicate would be redundant.
    if std::env::var_os("FLATPAK_ID").is_some() {
        return;
    }
    let Some(data) = dirs::data_dir() else {
        return;
    };
    let apps = data.join("applications");
    let icons = data.join("icons/hicolor/scalable/apps");
    if let Err(e) = std::fs::create_dir_all(&apps) {
        log::debug!("could not create {}: {e}", apps.display());
        return;
    }
    if let Err(e) = std::fs::create_dir_all(&icons) {
        log::debug!("could not create {}: {e}", icons.display());
        return;
    }

    let desktop_path = apps.join("viberadio.desktop");
    let icon_path = icons.join("viberadio.svg");
    let mut wrote = false;
    if !desktop_path.exists() {
        let exec = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("viberadio"));
        let desktop = DESKTOP
            .replace("Exec=viberadio", &format!("Exec=\"{}\"", exec.display()))
            .replace("TryExec=viberadio", &format!("TryExec={}", exec.display()));
        match std::fs::write(&desktop_path, desktop) {
            Ok(()) => wrote = true,
            Err(e) => log::debug!("could not write {}: {e}", desktop_path.display()),
        }
    }
    if !icon_path.exists() {
        match std::fs::write(&icon_path, ICON) {
            Ok(()) => wrote = true,
            Err(e) => log::debug!("could not write {}: {e}", icon_path.display()),
        }
    }
    if wrote {
        // Refresh the KDE service cache so the new icon is picked up without a
        // re-login. Other desktops watch the directories themselves.
        let _ = std::process::Command::new("kbuildsycoca6")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
