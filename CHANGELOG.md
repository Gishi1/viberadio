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

## [0.1.2] - 2026-11-16

### Added
- Fullscreen media-centre display mode (F11 or the ⛶ button; Esc exits), with
  scaled artwork, transport, volume and recent tracks.
- Station info window with genre, country, city, codec, bitrate, source,
  added and last-played dates, play count, tags, comment, a website link and
  stream URL copy.
- A background coordinator runs schedules and wish captures independently of
  the window, so recordings start and stop while minimised or hidden.
- Per-file recording gap reporting: a file that lost audio is kept, marked
  incomplete and excluded from fingerprinting and the `verified/` tree.
- Editable watch buffer size.

### Changed
- The player bar, sidebar, table and now-playing panel use a shared type and
  spacing scale; rows show a playing dot and a clearer selection bar.
- SQLite runs in WAL mode with `synchronous=NORMAL`; history is pruned to 365
  days at startup.
- The table caches lowercase sort and search keys and no longer rebuilds on
  every ICY title change outside the recently-played filter.

### Fixed
- A stalled output device no longer blocks the network reader or the recorder;
  playback bytes are dropped and the status shows "Audio stalled".
- Scheduled recordings respect the disk-space floor and the recording quality
  filters, and deleting or disabling a schedule stops its running window.
- The "Library actions" footer no longer collides with the player bar.
- The bitrate and REC pills no longer overlap.
- The row-wide stream URL tooltip was removed because it covered the next row.

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

[0.1.2]: https://github.com/Gishi1/viberadio/releases/tag/v0.1.2
[0.1.1]: https://github.com/Gishi1/viberadio/releases/tag/v0.1.1
[0.1.0]: https://github.com/Gishi1/viberadio/releases/tag/v0.1.0
