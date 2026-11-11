# VibeRadio help

VibeRadio is a native desktop internet-radio client. This help is bundled with
the application and works offline.

## Getting started

- Use "+ Add station" in the sidebar to add a stream by name and URL.
- "Import radio.net" fetches the default directory; "Import radio-browser" adds
  the community directory's most-clicked stations.
- Double-click a station to play it. The player bar shows the current title,
  station, status, bitrate, transport and volume.
- Space starts or stops playback. The left and right arrows change the volume.
  "/" focuses the search field.

## Library

- Filters: All, Favourites, Recently played and each genre with a count.
- Search matches name, URL, genre, country, state, city and tags as you type.
- Right-click a station for Play, Schedule and favourite.
- "Export M3U" writes the library as a playlist; "Import M3U" adds one back.
- "Export history" writes a self-contained HTML table.

## Playback

- Several streams can play at once. The Streams button shows every open stream
  with its own status, volume and Stop.
- Ctrl+Left and Ctrl+Right switch the active stream.
- Status is one of connecting, buffering, live or error. Stop cancels a blocked
  connect immediately.
- MP3, OGG Vorbis, FLAC and WAV use rodio. AAC-LC uses a symphonia fallback.
  HE-AAC is expected to fail or play degraded. Opus needs libopus. HLS is not
  supported yet.

## Metadata and history

- ICY StreamTitle drives the now-playing line.
- Ctrl+D opens the metadata debug view: raw title, station tag format, parsed
  artist and song, ignore and wish-list decisions.
- Ctrl+H opens track history. Each row is linked to the play instance that
  produced it, with duration and status.
- The ignore list suppresses a title. The wish list matches exact, contains and
  starts-with rules and reports a hit.

## Recording

- Ctrl+R or the REC button toggles recording of the active stream.
- Split mode writes one file per ICY title. Continuous mode writes one file and
  a JSONL metadata log that can be split later.
- File name tokens: {station}, {artist}, {title}, {date}, {time}.
- Settings (Ctrl+,) sets the folder, template and mode, and can split the last
  continuous recording.
- Recordings keep the station's encoded bytes. Dynamic Boost is not applied to
  recordings.

## Schedules

- The Schedules window creates play, play+record and record-only schedules with
  once, daily or weekly recurrence.
- A start/end window may cross midnight. A finish action needs an end time.
- Finish actions: none, close the app, or run a command.
- A schedule resumes an open window after a restart. A finish action more than
  five minutes late is skipped. Deleting or disabling a schedule cancels its
  active window.

## Dynamic Boost

- Presets: Off, Light (+6 dB max), Medium (+12 dB max), Strong (+18 dB max).
- A slow RMS AGC targets -18 dBFS with a -60 dBFS gate. A per-station trim is
  adjustable by +/-12 dB. A peak limiter holds -1 dBFS with a hard clamp.
- This is not EBU R128 or ReplayGain.

## Backup, data folder and logs

- Settings > Backup: "Backup now" writes a timestamped copy. "Restore latest on
  restart" validates the copy and swaps it in on the next start.
- Settings > Data folder: copy the library to a new folder and switch on the
  next restart. Recordings, logs and backups stay in the old folder.
- The data folder contains viberadio.log and viberadio-crash.log.
- About (sidebar) lists deliberately dropped features and copies diagnostics.

## Keyboard

| Key             | Action                         |
| --------------- | ------------------------------ |
| Space           | Play or stop                   |
| Left / Right    | Volume down / up               |
| Ctrl+Left/Right | Previous / next stream         |
| /               | Focus search                   |
| Esc             | Close the Add station dialog   |
| Ctrl+D          | Metadata debug view            |
| Ctrl+H          | History                        |
| Ctrl+R          | Toggle recording               |
| Ctrl+M          | Mini player                    |
| Ctrl+,          | Settings                       |
