//! Copy identified recordings into a station-free Artist/Album tree.
//!
//! The raw recordings stay where they are. The best airing of each AcoustID
//! recording is copied to `verified/<artist>/<album>/`, tagged with the
//! identified metadata, and given the release group's front cover.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use id3::frame::{Picture, PictureType};
use id3::{Tag, TagLike, Version};

use crate::fingerprint::{sidecar_path, version_score, Sidecar};
use crate::recording::sanitize;

const COVER_URL: &str = "https://coverartarchive.org/release-group";
const USER_AGENT: &str = "VibeRadio/0.1 (https://github.com/santosharron/dropstone-cli)";
const MIN_MATCH_PERCENT: f32 = 90.0;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedReport {
    pub copied: usize,
    pub updated: usize,
    pub skipped: usize,
    pub errors: usize,
}

struct Candidate {
    path: PathBuf,
    sidecar: Sidecar,
    bytes: u64,
}

/// Build or refresh the verified tree for one recordings directory.
pub fn organize(dir: &Path) -> Result<VerifiedReport, String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut groups: HashMap<String, Vec<Candidate>> = HashMap::new();
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
        let Some(recording_id) = sidecar.recording_id.clone() else {
            continue;
        };
        let audio = path.with_file_name(audio_name);
        let Ok(meta) = fs::metadata(&audio) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        groups.entry(recording_id).or_default().push(Candidate {
            path: audio,
            sidecar,
            bytes: meta.len(),
        });
    }

    let mut report = VerifiedReport::default();
    let mut covers: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    for candidates in groups.into_values() {
        let Some(best) = candidates.iter().max_by_key(|candidate| score(candidate)) else {
            continue;
        };
        if !complete(best) {
            report.skipped += 1;
            continue;
        }
        let Some(artist) = best
            .sidecar
            .artist
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            report.skipped += 1;
            continue;
        };
        let Some(title) = best
            .sidecar
            .title
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            report.skipped += 1;
            continue;
        };
        let album = best
            .sidecar
            .album
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Unknown Album");
        let extension = best
            .path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin");
        let destination = verified_path(dir, artist, album, title, extension);
        let existing = existing_score(&destination);
        let candidate = score(best);
        if existing
            .map(|existing| existing >= candidate)
            .unwrap_or(false)
        {
            report.skipped += 1;
            continue;
        }
        let Some(parent) = destination.parent() else {
            report.errors += 1;
            continue;
        };
        if let Err(e) = fs::create_dir_all(parent) {
            log::warn!("could not create {}: {e}", parent.display());
            report.errors += 1;
            continue;
        }
        let existed = destination.is_file();
        if let Err(e) = fs::copy(&best.path, &destination) {
            log::warn!(
                "could not copy {} to {}: {e}",
                best.path.display(),
                destination.display()
            );
            report.errors += 1;
            continue;
        }
        let artwork = best.sidecar.release_group_id.as_deref().and_then(|id| {
            covers
                .entry(id.to_string())
                .or_insert_with(|| fetch_cover(id))
                .clone()
        });
        if let Err(e) = tag_file(&destination, &best.sidecar, artwork.as_deref()) {
            log::warn!("could not tag {}: {e}", destination.display());
        }
        if let Ok(json) = serde_json::to_string_pretty(&best.sidecar) {
            let _ = fs::write(sidecar_path(&destination), json);
        }
        if existed {
            report.updated += 1;
        } else {
            report.copied += 1;
        }
    }
    Ok(report)
}

fn score(candidate: &Candidate) -> (bool, bool, i64, u64) {
    version_score(
        candidate.sidecar.duration_seconds,
        candidate.sidecar.content_seconds.unwrap_or(0),
        candidate.sidecar.expected_duration_seconds,
        candidate.sidecar.interrupted,
        candidate.bytes,
    )
}

fn complete(candidate: &Candidate) -> bool {
    let Some(expected) = candidate
        .sidecar
        .expected_duration_seconds
        .filter(|value| *value > 0)
    else {
        return true;
    };
    let content = candidate
        .sidecar
        .content_seconds
        .unwrap_or(candidate.sidecar.duration_seconds);
    (content as f32 / expected as f32) * 100.0 >= MIN_MATCH_PERCENT
}

fn existing_score(path: &Path) -> Option<(bool, bool, i64, u64)> {
    let text = fs::read_to_string(sidecar_path(path)).ok()?;
    let sidecar: Sidecar = serde_json::from_str(&text).ok()?;
    let bytes = fs::metadata(path).ok()?.len();
    Some(version_score(
        sidecar.duration_seconds,
        sidecar.content_seconds.unwrap_or(0),
        sidecar.expected_duration_seconds,
        sidecar.interrupted,
        bytes,
    ))
}

fn verified_path(root: &Path, artist: &str, album: &str, title: &str, extension: &str) -> PathBuf {
    let artist = short(&sanitize(artist), 80);
    let album = short(&sanitize(album), 80);
    let title = short(&sanitize(title), 120);
    root.join("verified")
        .join(&artist)
        .join(album)
        .join(format!("{artist} - {title}.{extension}"))
}

fn short(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        value.chars().take(limit).collect()
    }
}

fn tag_file(path: &Path, sidecar: &Sidecar, artwork: Option<&[u8]>) -> Result<(), String> {
    // Raw AAC streams do not carry ID3v2 tags; leave those copies untouched.
    let is_mp3 = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("mp3"))
        .unwrap_or(false);
    if !is_mp3 {
        return Ok(());
    }
    let mut tag = Tag::new();
    if let Some(artist) = sidecar.artist.as_deref() {
        tag.set_artist(artist);
    }
    if let Some(title) = sidecar.title.as_deref() {
        tag.set_title(title);
    }
    if let Some(album) = sidecar.album.as_deref() {
        tag.set_album(album);
    }
    if let Some(year) = sidecar.year {
        tag.set_year(year as i32);
    }
    if let Some(bytes) = artwork {
        let mime_type = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            "image/png"
        } else {
            "image/jpeg"
        };
        tag.add_frame(Picture {
            mime_type: mime_type.to_string(),
            picture_type: PictureType::CoverFront,
            description: String::new(),
            data: bytes.to_vec(),
        });
    }
    tag.write_to_path(path, Version::Id3v24)
        .map_err(|e| e.to_string())
}

fn fetch_cover(release_group: &str) -> Option<Vec<u8>> {
    let url = format!("{COVER_URL}/{release_group}/front-500");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .user_agent(USER_AGENT)
            .build()
            .ok()?;
        let response = client.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.bytes().await.ok().map(|bytes| bytes.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("viberadio-verified-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sidecar(recording_id: &str, content: u32, expected: u32) -> Sidecar {
        Sidecar {
            recording_id: Some(recording_id.to_string()),
            artist: Some("Artist".into()),
            title: Some("Song".into()),
            album: Some("Album".into()),
            expected_duration_seconds: Some(expected),
            duration_seconds: content,
            content_seconds: Some(content),
            ..Default::default()
        }
    }

    #[test]
    fn copies_a_complete_version_into_the_verified_tree() {
        let dir = temp_dir("organize");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Station - Artist - Song.bin");
        fs::write(&audio, b"audio").unwrap();
        fs::write(
            sidecar_path(&audio),
            serde_json::to_string(&sidecar("rec-1", 200, 200)).unwrap(),
        )
        .unwrap();

        let report = organize(&dir).unwrap();
        assert_eq!(report.copied, 1);
        let destination = dir.join("verified/Artist/Album/Artist - Song.bin");
        assert!(destination.is_file());
        assert!(sidecar_path(&destination).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incomplete_version_is_not_verified() {
        let dir = temp_dir("incomplete");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Station - Artist - Song.bin");
        fs::write(&audio, b"audio").unwrap();
        fs::write(
            sidecar_path(&audio),
            serde_json::to_string(&sidecar("rec-2", 100, 200)).unwrap(),
        )
        .unwrap();

        let report = organize(&dir).unwrap();
        assert_eq!(report.copied, 0);
        assert_eq!(report.skipped, 1);
        assert!(!dir.join("verified").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_tags_and_artwork_to_an_mp3() {
        use id3::TagLike;
        let dir = temp_dir("tags");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.mp3");
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.mp3"),
            &path,
        )
        .unwrap();
        let sidecar = Sidecar {
            artist: Some("Artist".into()),
            title: Some("Song".into()),
            album: Some("Album".into()),
            year: Some(2020),
            ..Default::default()
        };
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        tag_file(&path, &sidecar, Some(&png)).unwrap();

        let tag = Tag::read_from_path(&path).unwrap();
        assert_eq!(tag.artist(), Some("Artist"));
        assert_eq!(tag.title(), Some("Song"));
        assert_eq!(tag.album(), Some("Album"));
        assert_eq!(tag.year(), Some(2020));
        assert_eq!(tag.pictures().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verified_paths_use_artist_and_album_folders() {
        let path = verified_path(
            Path::new("/music"),
            "AC/DC",
            "Back in Black",
            "Hells Bells",
            "mp3",
        );
        assert_eq!(
            path,
            Path::new("/music/verified/AC_DC/Back in Black/AC_DC - Hells Bells.mp3")
        );
    }

    #[test]
    fn long_names_are_shortened() {
        let artist = "A".repeat(200);
        let title = "B".repeat(300);
        let path = verified_path(Path::new("/music"), &artist, "Album", &title, "mp3");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.len() < 255);
    }
}
