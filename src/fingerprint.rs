//! Recording fingerprinting: Chromaprint locally, AcoustID lookup optionally.
//!
//! MusicBrainz does not fingerprint audio itself; AcoustID is the open
//! fingerprint service and returns MusicBrainz recording ids. Without an
//! AcoustID key this module still computes and stores the Chromaprint
//! fingerprint, which is enough for local duplicate detection.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use serde::{Deserialize, Serialize};

use crate::decode;

const ACOUSTID_URL: &str = "https://api.acoustid.org/v2/lookup";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// Below this AcoustID confidence the match is too weak to trust for a length,
/// a verified path, or deduplication.
const MIN_ACOUSTID_SCORE: f32 = 0.5;
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "aac", "m4a", "mp4", "ogg", "oga", "opus", "flac", "wav", "aif", "aiff", "wma",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaveformProfile {
    /// RMS per `hop_seconds`, in the same 0..1 scale as the decoded samples.
    pub envelope: Vec<f32>,
    pub hop_seconds: f32,
    /// Longest run of near-silent windows away from the start and end.
    pub longest_gap_seconds: f32,
    /// Largest level change between adjacent windows, in dB.
    pub largest_step_db: f32,
    /// A gap or level jump inside the recording, which often marks an ad break
    /// or a hard cut.
    pub interrupted: bool,
    /// Strongest gradual fade-out found anywhere in the recording.
    pub fade_out: Option<Fade>,
    /// Strongest gradual fade-in found anywhere in the recording.
    pub fade_in: Option<Fade>,
}

/// A gradual, sustained level change. Radio songs usually fade at a natural
/// edge; an abrupt step or a silent gap is more likely an ad or a hard cut.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Fade {
    /// Length of the monotonic change, in seconds.
    pub seconds: f32,
    /// Total level change, in dB (always positive).
    pub db: f32,
    /// The fade reaches the recording's first or last audible window, so it
    /// looks like a natural edge rather than an interruption.
    pub at_edge: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fingerprint {
    pub duration_seconds: u32,
    /// Compressed Chromaprint fingerprint, base64.
    pub compressed: String,
    pub raw: Vec<u32>,
    /// RMS of the last half second divided by the whole-file RMS. A loud tail
    /// suggests an abrupt cut; a quiet one suggests a fade or natural ending.
    pub tail_rms_ratio: f32,
    /// Seconds between the first and last non-silent samples, used to align the
    /// recording's content with the canonical song.
    pub content_seconds: u32,
    /// Sliding-window RMS profile and its gap/step findings.
    pub waveform: WaveformProfile,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AcoustIdMatch {
    pub recording_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// MusicBrainz release group, used for cover art.
    pub release_group_id: Option<String>,
    pub year: Option<u32>,
    pub score: Option<f32>,
    /// Canonical recording length from MusicBrainz metadata, when AcoustID
    /// returns it.
    pub expected_duration_seconds: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sidecar {
    pub duration_seconds: u32,
    pub fingerprint: String,
    pub recording_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// MusicBrainz release group, used for cover art.
    pub release_group_id: Option<String>,
    pub year: Option<u32>,
    pub score: Option<f32>,
    /// Canonical length reported by AcoustID/MusicBrainz, when known.
    pub expected_duration_seconds: Option<u32>,
    /// Non-silent span of the recording, used for alignment.
    pub content_seconds: Option<u32>,
    /// Aligned content as a percentage of the canonical length.
    pub match_percent: Option<f32>,
    /// The aligned content is materially shorter than the canonical length.
    pub incomplete: bool,
    /// Last-half-second RMS divided by the whole-file RMS.
    pub tail_rms_ratio: Option<f32>,
    /// Sliding-window RMS profile, one value per `hop_seconds`.
    pub rms_envelope: Option<Vec<f32>>,
    pub hop_seconds: Option<f32>,
    pub longest_gap_seconds: Option<f32>,
    pub largest_step_db: Option<f32>,
    /// A gap or level jump inside the recording.
    pub interrupted: bool,
    /// Strongest gradual fade-out, when the envelope found one.
    pub fade_out: Option<Fade>,
    /// Strongest gradual fade-in, when the envelope found one.
    pub fade_in: Option<Fade>,
    /// The local fingerprint is saved, but AcoustID could not identify it.
    pub lookup_failed: bool,
}

/// `song.mp3` -> `song.mp3.fingerprint.json`, so it cannot collide with an
/// existing JSON file that shares the stem.
pub fn sidecar_path(audio: &Path) -> PathBuf {
    let mut name = audio.file_name().unwrap_or_default().to_os_string();
    name.push(".fingerprint.json");
    audio.with_file_name(name)
}

/// Decode a recording and compute its Chromaprint fingerprint.
pub fn fingerprint_file(path: &Path) -> Result<Fingerprint, String> {
    std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let path = path.to_path_buf();
    let decoder = decode::open_with(|_| {
        std::fs::File::open(&path)
            .map(|file| Box::new(file) as Box<dyn decode::ReadSeekSend>)
            .unwrap_or_else(|_| Box::new(std::io::Cursor::new(Vec::<u8>::new())))
    })?;
    let channels = decoder.channels().max(1) as usize;
    let sample_rate = decoder.sample_rate().max(1);
    let config = Configuration::default();
    let mut printer = Fingerprinter::new(&config);
    printer
        .start(sample_rate, channels as u32)
        .map_err(|e| e.to_string())?;

    let tail_limit = ((sample_rate as usize * channels) / 2).max(1);
    let mut tail: std::collections::VecDeque<f32> = std::collections::VecDeque::with_capacity(tail_limit);
    let mut total_sq = 0.0f64;
    let mut total_count = 0u64;
    let mut tail_sq = 0.0f64;
    let mut buffer: Vec<i16> = Vec::with_capacity(8192);
    let mut total_samples = 0u64;
    let mut first_loud: Option<u64> = None;
    let mut last_loud = 0u64;
    let mut sample_index = 0u64;
    let hop_samples = ((sample_rate as u64 * channels as u64) / 2).max(1);
    let mut hop_sq = 0.0f64;
    let mut hop_count = 0u64;
    let mut envelope: Vec<f32> = Vec::new();
    for sample in decoder {
        let sample = sample.clamp(-1.0, 1.0);
        if sample.abs() > 0.01 {
            if first_loud.is_none() {
                first_loud = Some(sample_index);
            }
            last_loud = sample_index;
        }
        sample_index += 1;
        total_sq += (sample as f64) * (sample as f64);
        total_count += 1;
        hop_sq += (sample as f64) * (sample as f64);
        hop_count += 1;
        if hop_count >= hop_samples {
            envelope.push((hop_sq / hop_count as f64).sqrt() as f32);
            hop_sq = 0.0;
            hop_count = 0;
        }
        tail.push_back(sample);
        tail_sq += (sample as f64) * (sample as f64);
        if tail.len() > tail_limit {
            if let Some(old) = tail.pop_front() {
                tail_sq -= (old as f64) * (old as f64);
            }
        }
        buffer.push((sample * i16::MAX as f32) as i16);
        if buffer.len() >= 8192 {
            total_samples += buffer.len() as u64;
            printer.consume(&buffer);
            buffer.clear();
        }
    }
    if !buffer.is_empty() {
        total_samples += buffer.len() as u64;
        printer.consume(&buffer);
    }
    printer.finish();
    let body_rms = if total_count > 0 {
        (total_sq / total_count as f64).sqrt()
    } else {
        0.0
    };
    let tail_rms = if !tail.is_empty() {
        (tail_sq.max(0.0) / tail.len() as f64).sqrt()
    } else {
        0.0
    };
    let tail_rms_ratio = if body_rms > 0.0 {
        (tail_rms / body_rms) as f32
    } else {
        0.0
    };
    let content_samples = first_loud
        .map(|first| last_loud.saturating_sub(first) + 1)
        .unwrap_or(0);
    let content_seconds = (content_samples / (sample_rate as u64 * channels as u64))
        .min(u32::MAX as u64) as u32;
    if hop_count > 0 {
        envelope.push((hop_sq / hop_count as f64).sqrt() as f32);
    }
    let waveform = analyze_envelope(&envelope, 0.5);

    let raw = printer.fingerprint().to_vec();
    if raw.is_empty() {
        return Err("the recording is too short to fingerprint".to_string());
    }
    let compressed = base64::engine::general_purpose::STANDARD
        .encode(FingerprintCompressor::from(&config).compress(&raw));
    let frames = total_samples / channels as u64;
    Ok(Fingerprint {
        duration_seconds: (frames / sample_rate as u64).min(u32::MAX as u64) as u32,
        compressed,
        raw,
        tail_rms_ratio,
        content_seconds,
        waveform,
    })
}

/// Look up a fingerprint on AcoustID. Returns `None` when the service has no
/// match; a missing or invalid key is an error.
pub fn lookup_acoustid(
    api_key: &str,
    fingerprint: &str,
    duration_seconds: u32,
) -> Result<Option<AcoustIdMatch>, String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Ok(None);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the AcoustID client: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        let duration = duration_seconds.to_string();
        // A compressed fingerprint is several kilobytes; a GET query overflows
        // the server's request-line limit (nginx returns 400 at about 8 KiB).
        let response = client
            .post(ACOUSTID_URL)
            .form(&[
                ("client", api_key),
                ("format", "json"),
                ("meta", "recordings releasegroups"),
                ("duration", duration.as_str()),
                ("fingerprint", fingerprint),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        match parse_acoustid(&text) {
            Ok(result) => Ok(result),
            Err(e) if !status.is_success() => {
                let body = text.trim();
                if body.is_empty() {
                    Err(format!("AcoustID {status}: {e}"))
                } else {
                    let snippet: String = body.chars().take(160).collect();
                    Err(format!("AcoustID {status}: {e} (body: {snippet})"))
                }
            }
            Err(e) => Err(e),
        }
    })
}

/// Identify the opening seconds of a growing recording. The prefix is copied
/// to a temporary file because the decoder wants a path, then removed.
pub fn identify_prefix(
    path: &Path,
    seconds: u32,
    api_key: Option<&str>,
) -> Result<Option<AcoustIdMatch>, String> {
    let prefix = prefix_bytes(path, seconds)?;
    if prefix.len() < 16 * 1024 {
        return Ok(None);
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("bin");
    let mut temp = std::env::temp_dir();
    temp.push(format!(
        "viberadio-prefix-{}-{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0),
        extension
    ));
    fs::write(&temp, &prefix).map_err(|e| format!("cannot write the prefix: {e}"))?;
    let result = (|| {
        let fingerprint = fingerprint_file(&temp)?;
        // Judge by decoded audio, not bytes: a low-bitrate stream reaches the
        // same duration with far fewer bytes.
        if fingerprint.duration_seconds < 8 {
            return Ok(None);
        }
        match api_key.filter(|key| !key.trim().is_empty()) {
            Some(key) => lookup_acoustid(key, &fingerprint.compressed, fingerprint.duration_seconds),
            None => Ok(None),
        }
    })();
    let _ = fs::remove_file(&temp);
    result
}

fn prefix_bytes(path: &Path, seconds: u32) -> Result<Vec<u8>, String> {
    // 48 KiB/s covers a 384 kbps stream; the cap keeps a high-bitrate file
    // from making this expensive.
    let limit = (u64::from(seconds) * 48 * 1024).min(4 * 1024 * 1024) as usize;
    let mut file =
        fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut buffer = vec![0u8; limit];
    let read = file
        .read(&mut buffer)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    buffer.truncate(read);
    Ok(buffer)
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    status: String,
    error: Option<ApiError>,
    results: Option<Vec<ApiResult>>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiResult {
    score: Option<f32>,
    recordings: Option<Vec<ApiRecording>>,
}

#[derive(Debug, Deserialize)]
struct ApiRecording {
    id: Option<String>,
    title: Option<String>,
    artists: Option<Vec<ApiArtist>>,
    releasegroups: Option<Vec<ApiReleaseGroup>>,
    /// Milliseconds in AcoustID's MusicBrainz metadata.
    duration: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ApiArtist {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiReleaseGroup {
    id: Option<String>,
    title: Option<String>,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
}

fn parse_acoustid(json: &str) -> Result<Option<AcoustIdMatch>, String> {
    let response: ApiResponse =
        serde_json::from_str(json).map_err(|e| format!("AcoustID response: {e}"))?;
    if response.status != "ok" {
        let message = response
            .error
            .and_then(|error| error.message)
            .unwrap_or_else(|| "AcoustID returned an error".to_string());
        return Err(message);
    }
    let best = response
        .results
        .unwrap_or_default()
        .into_iter()
        .filter(|result| result.score.unwrap_or(0.0) >= MIN_ACOUSTID_SCORE)
        .max_by(|left, right| {
            left.score
                .unwrap_or(0.0)
                .partial_cmp(&right.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    let Some(result) = best else {
        return Ok(None);
    };
    let Some(recording) = result
        .recordings
        .unwrap_or_default()
        .into_iter()
        .find(|recording| recording.title.is_some())
    else {
        return Ok(None);
    };
    let artist = recording
        .artists
        .unwrap_or_default()
        .into_iter()
        .filter_map(|artist| artist.name)
        .collect::<Vec<_>>()
        .join(", ");
    let release_group = recording
        .releasegroups
        .unwrap_or_default()
        .into_iter()
        .find(|group| group.title.is_some() || group.id.is_some());
    let album = release_group.as_ref().and_then(|group| group.title.clone());
    let release_group_id = release_group.as_ref().and_then(|group| group.id.clone());
    let year = release_group
        .as_ref()
        .and_then(|group| group.first_release_date.as_deref())
        .and_then(|date| date.get(..4))
        .and_then(|year| year.parse().ok());
    // AcoustID reports the canonical length in milliseconds. A value under a
    // second is not a usable song length; downstream code ignores it.
    let expected_duration_seconds = recording
        .duration
        .and_then(|value| (value > 0).then(|| (value / 1000).min(u32::MAX as i64) as u32));
    Ok(Some(AcoustIdMatch {
        recording_id: recording.id,
        title: recording.title,
        artist: (!artist.is_empty()).then_some(artist),
        album,
        release_group_id,
        year,
        score: result.score,
        expected_duration_seconds,
    }))
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| AUDIO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// What a fingerprint pass found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FingerprintReport {
    pub fingerprinted: usize,
    /// Recordings materially shorter than their canonical length.
    pub incomplete: usize,
    /// Recordings with a gap or level jump inside them.
    pub interrupted: usize,
    /// Files whose AcoustID lookup failed; they have no sidecar and should be
    /// retried.
    pub retry: Vec<PathBuf>,
}

/// Fingerprint a specific set of finished recordings.
pub fn fingerprint_paths(
    paths: &[PathBuf],
    api_key: Option<&str>,
) -> Result<FingerprintReport, String> {
    let mut report = FingerprintReport::default();
    for path in paths {
        if !path.is_file() || !is_audio(path) || sidecar_path(path).exists() {
            continue;
        }
        let (written, incomplete, interrupted, retry) = fingerprint_one(path, api_key)?;
        if retry {
            report.retry.push(path.clone());
        }
        if written {
            report.fingerprinted += 1;
            if incomplete {
                report.incomplete += 1;
            }
            if interrupted {
                report.interrupted += 1;
            }
        }
    }
    Ok(report)
}

/// Fingerprint every audio file in `dir` that has no sidecar yet.
pub fn scan_recordings(dir: &Path, api_key: Option<&str>) -> Result<FingerprintReport, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_audio(path) && !sidecar_path(path).exists())
        .collect();
    fingerprint_paths(&paths, api_key)
}

fn fingerprint_one(path: &Path, api_key: Option<&str>) -> Result<(bool, bool, bool, bool), String> {
    let fingerprint = match fingerprint_file(path) {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            log::warn!("could not fingerprint {}: {e}", path.display());
            return Ok((false, false, false, false));
        }
    };
    let mut retry = false;
    let matched = match api_key {
        Some(key) if !key.trim().is_empty() => {
            let result =
                lookup_acoustid(key, &fingerprint.compressed, fingerprint.duration_seconds);
            // AcoustID allows about three requests per second.
            std::thread::sleep(Duration::from_millis(350));
            match result {
                Ok(matched) => matched,
                Err(e) => {
                    log::warn!("AcoustID lookup failed for {}: {e}", path.display());
                    retry = true;
                    None
                }
            }
        }
        _ => None,
    };
    // A failed lookup still writes the local fingerprint. The sidecar records
    // that identification is pending, so the audio is never left without a
    // fingerprint and the app does not have to re-analyse it to retry.
    let expected = matched
        .as_ref()
        .and_then(|matched| matched.expected_duration_seconds);
    let incomplete = is_incomplete(
        fingerprint.duration_seconds,
        fingerprint.content_seconds,
        expected,
    );
    let match_pct = expected.map(|expected| {
        match_percent(
            fingerprint.duration_seconds,
            fingerprint.content_seconds,
            expected,
        )
    });
    let interrupted = fingerprint.waveform.interrupted;
    if incomplete {
        log::info!(
            "{} looks incomplete: {}s of {}s",
            path.display(),
            fingerprint.duration_seconds,
            expected.unwrap_or(0)
        );
    }
    let sidecar = Sidecar {
        duration_seconds: fingerprint.duration_seconds,
        fingerprint: fingerprint.compressed,
        recording_id: matched.as_ref().and_then(|m| m.recording_id.clone()),
        title: matched.as_ref().and_then(|m| m.title.clone()),
        artist: matched.as_ref().and_then(|m| m.artist.clone()),
        album: matched.as_ref().and_then(|m| m.album.clone()),
        release_group_id: matched.as_ref().and_then(|m| m.release_group_id.clone()),
        year: matched.as_ref().and_then(|m| m.year),
        score: matched.as_ref().and_then(|m| m.score),
        expected_duration_seconds: expected,
        content_seconds: Some(fingerprint.content_seconds),
        match_percent: match_pct,
        incomplete,
        tail_rms_ratio: Some(fingerprint.tail_rms_ratio),
        rms_envelope: Some(fingerprint.waveform.envelope.clone()),
        hop_seconds: Some(fingerprint.waveform.hop_seconds),
        longest_gap_seconds: Some(fingerprint.waveform.longest_gap_seconds),
        largest_step_db: Some(fingerprint.waveform.largest_step_db),
        interrupted,
        fade_out: fingerprint.waveform.fade_out,
        fade_in: fingerprint.waveform.fade_in,
        lookup_failed: retry,
    };
    let json = serde_json::to_string_pretty(&sidecar).map_err(|e| e.to_string())?;
    fs::write(sidecar_path(path), json)
        .map_err(|e| format!("cannot write the sidecar for {}: {e}", path.display()))?;
    Ok((true, incomplete, interrupted, false))
}

/// Build the sliding-window profile and flag gaps or level jumps away from the
/// natural start and end. A gap is a run of windows below 10% of the median;
/// a level jump is at least 10 dB between adjacent windows.
fn analyze_envelope(envelope: &[f32], hop_seconds: f32) -> WaveformProfile {
    if envelope.is_empty() {
        return WaveformProfile {
            hop_seconds,
            ..Default::default()
        };
    }
    let mut sorted = envelope.to_vec();
    sorted.sort_by(|left, right| {
        left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
    });
    let median = sorted[sorted.len() / 2];
    let threshold = (median * 0.1).max(0.005);
    let mut longest_gap = 0.0f32;
    let mut run = 0.0f32;
    let mut largest_step = 0.0f32;
    // Only audible content counts as the recording's span. A natural start or
    // ending is often a fade or silence, so looking inside the first and last
    // audible window keeps those edges out of the interruption test.
    let first_content = envelope.iter().position(|rms| *rms >= threshold);
    let last_content = envelope.iter().rposition(|rms| *rms >= threshold);
    if let (Some(first), Some(last)) = (first_content, last_content) {
        for index in first..=last {
            let rms = envelope[index];
            if rms < threshold {
                run += hop_seconds;
                longest_gap = longest_gap.max(run);
            } else {
                run = 0.0;
            }
            if index > first {
                let previous = envelope[index - 1].max(1e-6);
                let current = rms.max(1e-6);
                let step = (20.0 * (current / previous).log10()).abs();
                largest_step = largest_step.max(step);
            }
        }
    }
    let levels: Vec<f32> = envelope
        .iter()
        .map(|rms| 20.0 * rms.max(1e-6).log10())
        .collect();
    let within = (1.5 / hop_seconds).ceil() as usize;
    WaveformProfile {
        envelope: envelope.to_vec(),
        hop_seconds,
        longest_gap_seconds: longest_gap,
        largest_step_db: largest_step,
        interrupted: longest_gap >= 0.75 || largest_step >= 10.0,
        fade_out: first_content
            .zip(last_content)
            .and_then(|(_, last)| find_fade(&levels, hop_seconds, false, last, within)),
        fade_in: first_content
            .zip(last_content)
            .and_then(|(first, _)| find_fade(&levels, hop_seconds, true, first, within)),
    }
}

const FADE_MIN_STEP_DB: f32 = 0.75;
const FADE_MAX_STEP_DB: f32 = 6.0;
const FADE_TOTAL_DB: f32 = 6.0;
const FADE_MIN_HOPS: usize = 2;

/// Find the longest run of small, same-direction level changes. A cliff (a
/// single huge step) resets the run, so silence next to a cold open is not
/// mistaken for a fade.
fn find_fade(
    levels: &[f32],
    hop_seconds: f32,
    rising: bool,
    edge: usize,
    within: usize,
) -> Option<Fade> {
    let mut best: Option<Fade> = None;
    let mut run_start: Option<usize> = None;
    for index in 1..levels.len() {
        let step = levels[index] - levels[index - 1];
        let moving = if rising {
            (FADE_MIN_STEP_DB..=FADE_MAX_STEP_DB).contains(&step)
        } else {
            (-FADE_MAX_STEP_DB..=-FADE_MIN_STEP_DB).contains(&step)
        };
        if !moving {
            run_start = None;
            continue;
        }
        let start = *run_start.get_or_insert(index - 1);
        let hops = index - start;
        let db = (levels[index] - levels[start]).abs();
        if hops < FADE_MIN_HOPS || db < FADE_TOTAL_DB {
            continue;
        }
        let at_edge = if rising {
            start <= edge.saturating_add(within)
        } else {
            index.saturating_add(within) >= edge
        };
        let candidate = Fade {
            seconds: hops as f32 * hop_seconds,
            db,
            at_edge,
        };
        if best
            .map(|best| candidate.seconds > best.seconds)
            .unwrap_or(true)
        {
            best = Some(candidate);
        }
    }
    best
}

/// Aligned content as a percentage of the canonical length.
fn match_percent(recorded_seconds: u32, content_seconds: u32, expected: u32) -> f32 {
    let content = if content_seconds > 0 {
        content_seconds
    } else {
        recorded_seconds
    };
    (content as f32 / expected as f32) * 100.0
}

/// Whether the aligned content is materially shorter than the canonical
/// length. The waveform trims leading and trailing silence first, so a file
/// padded with silence is not judged by its raw duration.
fn is_incomplete(recorded_seconds: u32, content_seconds: u32, expected_seconds: Option<u32>) -> bool {
    let Some(expected) = expected_seconds.filter(|expected| *expected > 0) else {
        return false;
    };
    match_percent(recorded_seconds, content_seconds, expected) < 90.0
}

/// Score a version for the keep-best pass: a complete copy wins, then the
/// closest duration to the canonical length, then the larger file.
pub(crate) fn version_score(
    recorded: u32,
    content: u32,
    expected: Option<u32>,
    interrupted: bool,
    bytes: u64,
) -> (bool, bool, i64, u64) {
    match expected {
        Some(expected) if expected > 0 => {
            let percent = match_percent(recorded, content, expected);
            let complete = percent >= 90.0;
            let distance = ((percent - 100.0).abs() * 1000.0) as i64;
            (complete, !interrupted, -distance, bytes)
        }
        _ => (false, !interrupted, i64::MIN, bytes),
    }
}

/// Keep the best copy of each song identified by the same AcoustID recording
/// id, deleting the other airings and their sidecars. Files without an
/// AcoustID match are left alone.
pub fn dedupe_versions(dir: &Path) -> Result<usize, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    struct Version {
        path: PathBuf,
        bytes: u64,
        recorded: u32,
        content: u32,
        expected: Option<u32>,
        interrupted: bool,
    }
    let mut groups: HashMap<String, Vec<Version>> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(audio_name) = name.strip_suffix(".fingerprint.json") else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(sidecar) = serde_json::from_str::<Sidecar>(&text) else {
            continue;
        };
        let Some(recording_id) = sidecar.recording_id else {
            continue;
        };
        let audio = path.with_file_name(audio_name);
        if let Ok(meta) = fs::metadata(&audio) {
            if meta.is_file() {
                groups.entry(recording_id).or_default().push(Version {
                    path: audio,
                    bytes: meta.len(),
                    recorded: sidecar.duration_seconds,
                    content: sidecar.content_seconds.unwrap_or(0),
                    expected: sidecar.expected_duration_seconds,
                    interrupted: sidecar.interrupted,
                });
            }
        }
    }
    let mut removed = 0usize;
    for files in groups.values() {
        if files.len() < 2 {
            continue;
        }
        let keep = files
            .iter()
            .max_by_key(|version| {
                version_score(
                    version.recorded,
                    version.content,
                    version.expected,
                    version.interrupted,
                    version.bytes,
                )
            })
            .expect("the group is not empty");
        for file in files {
            if file.path == keep.path {
                continue;
            }
            if fs::remove_file(&file.path).is_ok() {
                let _ = fs::remove_file(sidecar_path(&file.path));
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Outcome of checking metadata-less wishlist recordings against the list.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WishVerification {
    pub kept: usize,
    pub removed: usize,
    /// Files AcoustID could not identify (no key, no match or an error); they
    /// are kept rather than silently deleted.
    pub unverified: usize,
}

/// Whether the wish list asks for this identified track to be recorded.
fn wish_wants(
    wishes: &[crate::metadata::WishEntry],
    artist: Option<&str>,
    title: Option<&str>,
) -> bool {
    let (Some(artist), Some(title)) = (artist, title) else {
        return false;
    };
    let label = format!("{artist} - {title}");
    crate::metadata::wish_matches_with_format(wishes, &label, None)
        .iter()
        .any(|wish| wish.action.eq_ignore_ascii_case("record"))
}

/// Fingerprint metadata-less wishlist recordings, then keep the ones whose
/// AcoustID result matches a `record` wish and delete the rest. Files that
/// cannot be identified are kept, because deleting them would lose audio the
/// user might have wanted.
pub fn verify_wishes(
    paths: &[PathBuf],
    api_key: Option<&str>,
    wishes: &[crate::metadata::WishEntry],
) -> Result<WishVerification, String> {
    let mut result = WishVerification::default();
    for path in paths {
        let fingerprint = match fingerprint_file(path) {
            Ok(fingerprint) => fingerprint,
            Err(e) => {
                log::warn!("could not fingerprint {}: {e}", path.display());
                result.unverified += 1;
                continue;
            }
        };
        let mut lookup_failed = false;
        let matched = match api_key.filter(|key| !key.trim().is_empty()) {
            Some(key) => {
                let lookup =
                    lookup_acoustid(key, &fingerprint.compressed, fingerprint.duration_seconds);
                // AcoustID allows about three requests per second.
                std::thread::sleep(Duration::from_millis(350));
                match lookup {
                    Ok(matched) => matched,
                    Err(e) => {
                        log::warn!("AcoustID lookup failed for {}: {e}", path.display());
                        lookup_failed = true;
                        None
                    }
                }
            }
            None => None,
        };
        let expected = matched
            .as_ref()
            .and_then(|matched| matched.expected_duration_seconds);
        let incomplete = is_incomplete(
            fingerprint.duration_seconds,
            fingerprint.content_seconds,
            expected,
        );
        let match_pct = expected.map(|expected| {
            match_percent(
                fingerprint.duration_seconds,
                fingerprint.content_seconds,
                expected,
            )
        });
        let interrupted = fingerprint.waveform.interrupted;
        if incomplete {
            log::info!(
                "{} looks incomplete: {}s of {}s",
                path.display(),
                fingerprint.duration_seconds,
                expected.unwrap_or(0)
            );
        }
        let sidecar = Sidecar {
            duration_seconds: fingerprint.duration_seconds,
            fingerprint: fingerprint.compressed,
            recording_id: matched.as_ref().and_then(|m| m.recording_id.clone()),
            title: matched.as_ref().and_then(|m| m.title.clone()),
            artist: matched.as_ref().and_then(|m| m.artist.clone()),
            album: matched.as_ref().and_then(|m| m.album.clone()),
        release_group_id: matched.as_ref().and_then(|m| m.release_group_id.clone()),
        year: matched.as_ref().and_then(|m| m.year),
            score: matched.as_ref().and_then(|m| m.score),
            expected_duration_seconds: expected,
            content_seconds: Some(fingerprint.content_seconds),
            match_percent: match_pct,
            incomplete,
            tail_rms_ratio: Some(fingerprint.tail_rms_ratio),
            rms_envelope: Some(fingerprint.waveform.envelope.clone()),
            hop_seconds: Some(fingerprint.waveform.hop_seconds),
            longest_gap_seconds: Some(fingerprint.waveform.longest_gap_seconds),
            largest_step_db: Some(fingerprint.waveform.largest_step_db),
            interrupted,
            fade_out: fingerprint.waveform.fade_out,
            fade_in: fingerprint.waveform.fade_in,
            lookup_failed,
        };
        let json = serde_json::to_string_pretty(&sidecar).map_err(|e| e.to_string())?;
        fs::write(sidecar_path(path), json)
            .map_err(|e| format!("cannot write the sidecar for {}: {e}", path.display()))?;

        match matched {
            Some(matched) => {
                if wish_wants(wishes, matched.artist.as_deref(), matched.title.as_deref()) {
                    result.kept += 1;
                } else {
                    let _ = fs::remove_file(path);
                    let _ = fs::remove_file(sidecar_path(path));
                    result.removed += 1;
                }
            }
            None => result.unverified += 1,
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "viberadio-fingerprint-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sine_wav(seconds: u32) -> Vec<u8> {
        let rate = 44100u32;
        let frames = rate * seconds;
        let data_len = frames * 2;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for frame in 0..frames {
            let value = ((frame as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin()
                * 0.3
                * i16::MAX as f32) as i16;
            wav.extend_from_slice(&value.to_le_bytes());
        }
        wav
    }

    #[test]
    fn fingerprints_a_wav_file() {
        let dir = temp_dir("wav");
        let path = dir.join("tone.wav");
        fs::write(&path, sine_wav(3)).unwrap();
        let fingerprint = fingerprint_file(&path).unwrap();
        assert!(fingerprint.duration_seconds >= 2);
        assert!(!fingerprint.raw.is_empty());
        assert!(!fingerprint.compressed.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecar_name_keeps_the_audio_extension() {
        let path = Path::new("/tmp/song.mp3");
        assert_eq!(
            sidecar_path(path),
            PathBuf::from("/tmp/song.mp3.fingerprint.json")
        );
    }

    #[test]
    fn reads_only_the_requested_prefix() {
        let dir = temp_dir("prefix");
        let path = dir.join("stream.bin");
        let data: Vec<u8> = (0..300_000).map(|index| (index % 251) as u8).collect();
        fs::write(&path, &data).unwrap();
        let prefix = prefix_bytes(&path, 1).unwrap();
        assert_eq!(prefix.len(), 48 * 1024);
        assert_eq!(prefix, data[..48 * 1024]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_an_acoustid_match() {
        let json = r#"{
          "status": "ok",
          "results": [{
            "id": "abc",
            "score": 0.93,
            "recordings": [{
              "id": "43e7dfa7-6c41-4693-9e51-804998836597",
              "title": "Bohemian Rhapsody",
              "artists": [{"name": "Queen"}],
              "releasegroups": [{"id": "rg-1", "title": "A Night at the Opera", "first-release-date": "1975-11-21"}],
              "duration": 355000
            }]
          }]
        }"#;
        let matched = parse_acoustid(json).unwrap().unwrap();
        assert_eq!(matched.title.as_deref(), Some("Bohemian Rhapsody"));
        assert_eq!(matched.artist.as_deref(), Some("Queen"));
        assert_eq!(matched.album.as_deref(), Some("A Night at the Opera"));
        assert_eq!(matched.release_group_id.as_deref(), Some("rg-1"));
        assert_eq!(matched.year, Some(1975));
        assert_eq!(
            matched.recording_id.as_deref(),
            Some("43e7dfa7-6c41-4693-9e51-804998836597")
        );
        assert_eq!(matched.expected_duration_seconds, Some(355));
    }

    #[test]
    fn reports_acoustid_errors() {
        let json = r#"{"status":"error","error":{"code":4,"message":"invalid API key"}}"#;
        assert_eq!(parse_acoustid(json).unwrap_err(), "invalid API key");
    }

    #[test]
    fn rejects_a_low_confidence_match() {
        let json = r#"{"status":"ok","results":[{
            "id": "abc",
            "score": 0.2,
            "recordings": [{"id": "r1", "title": "Wrong Song", "duration": 200000}]
        }]}"#;
        assert!(parse_acoustid(json).unwrap().is_none());
    }

    #[test]
    fn reads_acoustid_duration_as_milliseconds() {
        let json = r#"{"status":"ok","results":[{
            "id": "abc",
            "score": 0.9,
            "recordings": [{"id": "r1", "title": "Short", "duration": 9500}]
        }]}"#;
        let matched = parse_acoustid(json).unwrap().unwrap();
        assert_eq!(matched.expected_duration_seconds, Some(9));
    }

    #[test]
    fn scans_only_new_audio_files() {
        let dir = temp_dir("scan");
        let path = dir.join("tone.wav");
        fs::write(&path, sine_wav(3)).unwrap();
        assert_eq!(scan_recordings(&dir, None).unwrap().fingerprinted, 1);
        assert!(sidecar_path(&path).exists());
        assert_eq!(scan_recordings(&dir, None).unwrap().fingerprinted, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wish_matching_uses_the_identified_artist_and_title() {
        let wishes = vec![crate::metadata::WishEntry {
            id: 0,
            artist: Some("Queen".into()),
            song_title: Some("Bohemian Rhapsody".into()),
            action: "record".into(),
            compare: crate::metadata::CompareMode::Contains,
        }];
        assert!(wish_wants(
            &wishes,
            Some("Queen"),
            Some("Bohemian Rhapsody")
        ));
        assert!(!wish_wants(
            &wishes,
            Some("Queen"),
            Some("Another One Bites the Dust")
        ));
        assert!(!wish_wants(&wishes, None, Some("Bohemian Rhapsody")));
    }

    #[test]
    fn unverifiable_recordings_are_kept_without_a_key() {
        let dir = temp_dir("verify-wishes");
        let path = dir.join("tone.wav");
        fs::write(&path, sine_wav(3)).unwrap();
        let wishes = vec![crate::metadata::WishEntry {
            id: 0,
            artist: Some("Queen".into()),
            song_title: Some("Bohemian Rhapsody".into()),
            action: "record".into(),
            compare: crate::metadata::CompareMode::Contains,
        }];
        let result = verify_wishes(&[path.clone()], None, &wishes).unwrap();
        assert_eq!(result.unverified, 1);
        assert_eq!(result.kept, 0);
        assert_eq!(result.removed, 0);
        assert!(path.is_file());
        assert!(sidecar_path(&path).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_the_largest_copy_of_each_identified_song() {
        let dir = temp_dir("dedupe");
        let small = dir.join("small.mp3");
        let large = dir.join("large.mp3");
        let other = dir.join("other.mp3");
        fs::write(&small, b"short").unwrap();
        fs::write(&large, b"a much longer recording").unwrap();
        fs::write(&other, b"different song").unwrap();
        for (path, id) in [
            (&small, "song-1"),
            (&large, "song-1"),
            (&other, "song-2"),
        ] {
            let sidecar = Sidecar {
                recording_id: Some(id.to_string()),
                ..Default::default()
            };
            fs::write(sidecar_path(path), serde_json::to_string(&sidecar).unwrap()).unwrap();
        }

        assert_eq!(dedupe_versions(&dir).unwrap(), 1);
        assert!(!small.exists());
        assert!(!sidecar_path(&small).exists());
        assert!(large.exists());
        assert!(other.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keeps_a_complete_copy_over_a_larger_incomplete_one() {
        let dir = temp_dir("dedupe-complete");
        let complete = dir.join("complete.mp3");
        let incomplete = dir.join("incomplete.mp3");
        fs::write(&complete, b"complete").unwrap();
        fs::write(&incomplete, vec![0u8; 200]).unwrap();
        for (path, recorded, expected) in [(&complete, 195, 200), (&incomplete, 120, 200)] {
            let sidecar = Sidecar {
                recording_id: Some("song".into()),
                duration_seconds: recorded,
                expected_duration_seconds: Some(expected),
                ..Default::default()
            };
            fs::write(sidecar_path(path), serde_json::to_string(&sidecar).unwrap()).unwrap();
        }

        assert_eq!(dedupe_versions(&dir).unwrap(), 1);
        assert!(complete.exists());
        assert!(!incomplete.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flags_gaps_and_level_jumps_in_the_envelope() {
        let flat = vec![0.2f32; 10];
        let profile = analyze_envelope(&flat, 0.5);
        assert!(!profile.interrupted);
        assert_eq!(profile.longest_gap_seconds, 0.0);

        let mut gap = vec![0.2f32; 12];
        gap[5] = 0.0;
        gap[6] = 0.0;
        let profile = analyze_envelope(&gap, 0.5);
        assert!(profile.interrupted);
        assert!(profile.longest_gap_seconds >= 0.75);

        let mut step = vec![0.1f32; 12];
        for value in &mut step[6..] {
            *value = 0.5;
        }
        let profile = analyze_envelope(&step, 0.5);
        assert!(profile.interrupted);
        assert!(profile.largest_step_db >= 10.0);

        // A gap at the very end is a natural ending, not an interruption.
        let mut tail = vec![0.2f32; 10];
        tail[9] = 0.0;
        assert!(!analyze_envelope(&tail, 0.5).interrupted);

        let empty = analyze_envelope(&[], 0.5);
        assert!(!empty.interrupted);
        assert!(empty.envelope.is_empty());
    }

    #[test]
    fn a_fade_out_at_the_end_is_a_natural_ending() {
        let mut envelope = vec![0.5f32; 12];
        for index in 5..12 {
            envelope[index] = envelope[index - 1] * 0.75;
        }
        let profile = analyze_envelope(&envelope, 0.5);
        let fade = profile.fade_out.expect("a fade-out");
        assert!(fade.at_edge);
        assert!(fade.seconds >= 3.0);
        assert!(fade.db >= 6.0);
        assert!(!profile.interrupted);
    }

    #[test]
    fn a_fade_in_at_the_start_is_a_natural_beginning() {
        let mut envelope = Vec::new();
        let mut level = 0.1f32;
        for _ in 0..7 {
            envelope.push(level);
            level *= 1.3;
        }
        envelope.extend([0.5f32; 5]);
        let profile = analyze_envelope(&envelope, 0.5);
        let fade = profile.fade_in.expect("a fade-in");
        assert!(fade.at_edge);
        assert!(fade.seconds >= 2.5);
        assert!(!profile.interrupted);
    }

    #[test]
    fn a_mid_file_dip_is_not_an_edge() {
        let mut envelope = vec![0.5f32; 20];
        for index in 5..11 {
            envelope[index] = envelope[index - 1] * 0.8;
        }
        for index in 11..17 {
            envelope[index] = (envelope[index - 1] * 1.25).min(0.5);
        }
        let profile = analyze_envelope(&envelope, 0.5);
        assert!(!profile.fade_out.expect("a fade-out").at_edge);
        assert!(!profile.fade_in.expect("a fade-in").at_edge);
        assert!(!profile.interrupted);
    }

    #[test]
    fn a_cold_open_is_not_a_fade() {
        let mut envelope = vec![0.0f32; 5];
        envelope.extend([0.5f32; 10]);
        let profile = analyze_envelope(&envelope, 0.5);
        assert!(profile.fade_in.is_none());
        assert!(!profile.interrupted);
    }

    #[test]
    fn a_fade_into_trailing_silence_still_reaches_the_edge() {
        let mut envelope = vec![0.5f32; 10];
        for _ in 0..6 {
            let next = envelope.last().copied().unwrap() * 0.7;
            envelope.push(next);
        }
        envelope.extend([0.0f32; 3]);
        let profile = analyze_envelope(&envelope, 0.5);
        let fade = profile.fade_out.expect("a fade-out");
        assert!(fade.at_edge);
        assert!(!profile.interrupted);
    }

    #[test]
    fn old_sidecars_without_fades_still_load() {
        let sidecar: Sidecar =
            serde_json::from_str(r#"{"duration_seconds":12,"fingerprint":"x"}"#).unwrap();
        assert_eq!(sidecar.duration_seconds, 12);
        assert!(sidecar.fade_out.is_none());
        assert!(sidecar.fade_in.is_none());
        assert!(!sidecar.interrupted);
    }

    #[test]
    fn flags_recordings_short_of_the_canonical_length() {
        assert!(is_incomplete(120, 120, Some(200)));
        assert!(!is_incomplete(198, 198, Some(200)));
        assert!(!is_incomplete(195, 195, Some(200)));
        assert!(is_incomplete(170, 170, Some(200)));
        assert!(!is_incomplete(0, 0, None));
        // Silence padding is trimmed by the alignment, so 210s of file with
        // 180s of content is 90% and still complete.
        assert!(!is_incomplete(210, 180, Some(200)));
        assert!(is_incomplete(210, 170, Some(200)));
    }

    #[test]
    #[ignore]
    fn live_acoustid_lookup() {
        let key = std::env::var("VIBERADIO_ACOUSTID_KEY").expect("key");
        let path = std::env::var("VIBERADIO_ACOUSTID_FILE").expect("file");
        let fingerprint = fingerprint_file(Path::new(&path)).expect("fingerprint");
        let matched = lookup_acoustid(&key, &fingerprint.compressed, fingerprint.duration_seconds)
            .expect("lookup")
            .expect("a match");
        assert!(matched.recording_id.is_some());
    }
}
