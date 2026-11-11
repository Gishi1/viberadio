//! Install a user-level desktop entry and icon for portable builds.
//!
//! Packaged builds (Flatpak, deb, AppImage) install these system-wide. A binary
//! run from a tarball or a development path has no desktop integration, so KDE
//! and GNOME fall back to a generic window icon. Writing the entry and icon
//! into the user's data directory fixes the title bar, taskbar and launcher.
//!
//! The freedesktop entry format is Linux-only; Windows and macOS builds get
//! their icon from the executable and the packaging step.

#[cfg(target_os = "linux")]
pub use imp::install;

/// Other platforms have no freedesktop desktop entry to install.
#[cfg(not(target_os = "linux"))]
pub fn install() {}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use image::ImageEncoder;

    const DESKTOP: &str = include_str!("../packaging/viberadio.desktop");
    const ICON_SVG: &str = include_str!("../packaging/viberadio.svg");
    const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

    pub fn install() {
        // Flatpak and system packages already install the metadata under /app
        // or /usr; a user-level duplicate would be redundant.
        if std::env::var_os("FLATPAK_ID").is_some() {
            return;
        }
        let Some(data) = dirs::data_dir() else {
            return;
        };
        let apps = data.join("applications");
        if let Err(e) = std::fs::create_dir_all(&apps) {
            log::debug!("could not create {}: {e}", apps.display());
            return;
        }

        let mut wrote = false;

        let scalable = data.join("icons/hicolor/scalable/apps");
        if std::fs::create_dir_all(&scalable).is_ok()
            && write_if_changed(&scalable.join("viberadio.svg"), ICON_SVG.as_bytes())
        {
            wrote = true;
        }

        // Raster copies let every desktop size resolve without an SVG renderer.
        let mut largest = None;
        for size in ICON_SIZES {
            let dir = data.join(format!("icons/hicolor/{size}x{size}/apps"));
            if std::fs::create_dir_all(&dir).is_err() {
                continue;
            }
            let path = dir.join("viberadio.png");
            if write_png_if_changed(&path, size) {
                wrote = true;
            }
            if size == 256 {
                largest = Some(path);
            }
        }

        // Point the entry at the installed file. A portable install has no
        // theme index, so a bare `Icon=viberadio` can fail to resolve and the
        // taskbar falls back to a generic document icon.
        let icon_ref = largest
            .as_deref()
            .filter(|path| path.exists())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "viberadio".to_string());

        let exec = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("viberadio"));
        let desktop = DESKTOP
            .replace("Exec=viberadio", &format!("Exec=\"{}\"", exec.display()))
            .replace("TryExec=viberadio", &format!("TryExec={}", exec.display()))
            .replace("Icon=viberadio", &format!("Icon={icon_ref}"));
        if write_if_changed(&apps.join("viberadio.desktop"), desktop.as_bytes()) {
            wrote = true;
        }

        if wrote {
            // Refresh the KDE service cache so the updated entry is picked up
            // without a re-login. Other desktops watch the directories.
            let _ = Command::new("kbuildsycoca6")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn write_if_changed(path: &Path, bytes: &[u8]) -> bool {
        if std::fs::read(path).map(|old| old == bytes).unwrap_or(false) {
            return false;
        }
        match std::fs::write(path, bytes) {
            Ok(()) => true,
            Err(e) => {
                log::debug!("could not write {}: {e}", path.display());
                false
            }
        }
    }

    fn write_png_if_changed(path: &Path, size: u32) -> bool {
        let mut rgba = crate::icon::rgba(size);
        // `icon::rgba` is premultiplied for egui; PNG stores straight alpha.
        for pixel in rgba.chunks_exact_mut(4) {
            let alpha = u32::from(pixel[3]);
            if alpha == 0 {
                pixel[..3].fill(0);
            } else if alpha < 255 {
                for channel in &mut pixel[..3] {
                    *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        let Some(image) = image::RgbaImage::from_raw(size, size, rgba) else {
            return false;
        };
        let mut encoded = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut encoded);
        if encoder
            .write_image(image.as_raw(), size, size, image::ExtendedColorType::Rgba8)
            .is_err()
        {
            return false;
        }
        write_if_changed(path, &encoded)
    }
}
