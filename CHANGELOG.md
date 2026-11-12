# Changelog

All notable changes to VibeRadio are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.1] - 2026-11-11

### Fixed
- AcoustID lookups no longer overflow the request line; the compressed
  fingerprint is sent as a POST form.
- Long and multi-byte recording and verified filenames are cut on a character
  boundary so they stay within filesystem limits.
- HLS stations from radio.net and radio-browser are no longer discarded.
- A watched station's background stream shows a "Watching" label instead of a
  stop button that did nothing.
- The AcoustID key is saved when the settings window closes rather than on
  every keystroke.

## [0.1.0] - 2026-11-11

### Added
- Native Rust/egui radio client with a station library, favorites, sessions,
  schedules, mini player, tray icon and MPRIS media keys.
- radio.net and radio-browser import, M3U import/export and manual stations.
- MP3, AAC-LC, FLAC, Vorbis and WAV playback with ICY metadata and artwork.
- Split recording per song, scheduled and live boundaries, background
  record-only streams, disk-space guard and wishlist-only mode.
- Chromaprint fingerprinting, optional AcoustID identification, tagged
  `verified/<artist>/<album>/` copies and duplicate removal.
- Wishlist matching with record/activate rules, watched stations with a rolling
  pre-roll buffer, watch history and JSON import/export.
- Broken-station detection with bulk removal, multi-select and bitrate probing.
- Flatpak manifest, release script, GitHub Actions and GitLab CI packaging.

[0.1.1]: https://github.com/Gishi1/viberadio/releases/tag/v0.1.1
[0.1.0]: https://github.com/Gishi1/viberadio/releases/tag/v0.1.0
