//! HLS (HTTP Live Streaming) playlist parsing.
//!
//! A `.m3u8` response is either a master playlist that lists variant streams or
//! a media playlist that lists the segments of one stream. Parsing is pure so
//! the rules are covered offline; [`crate::player`] owns the fetching and the
//! buffer that the decoder reads.

/// Largest playlist body accepted. Real playlists are a few KiB.
pub const MAX_PLAYLIST_BYTES: usize = 512 * 1024;

/// Largest single segment accepted. Radio segments are typically a few hundred
/// KiB; the cap keeps a hostile playlist from filling memory.
pub const MAX_SEGMENT_BYTES: usize = 32 * 1024 * 1024;

/// One variant of a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub bandwidth: u64,
    pub url: String,
}

/// One media segment, with its absolute sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub sequence: u64,
    pub url: String,
}

/// A parsed media playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlaylist {
    pub target_duration: u32,
    pub media_sequence: u64,
    pub segments: Vec<Segment>,
    /// `#EXT-X-ENDLIST`: the stream is complete, so stop reloading.
    pub end_list: bool,
    /// `#EXT-X-MAP`: an initialization segment that must be pushed first.
    pub init: Option<String>,
}

/// True when the body carries HLS tags rather than a plain M3U entry list.
pub fn is_hls(body: &[u8]) -> bool {
    normalize(body)
        .lines()
        .any(|line| line.trim_start().starts_with("#EXT-X-"))
}

/// Choose the highest-bandwidth variant of a master playlist.
///
/// Returns `Ok(None)` when the body is a media playlist.
pub fn master_variant(body: &[u8], base: &str) -> Result<Option<String>, String> {
    if !is_hls(body) {
        return Ok(None);
    }
    let text = normalize(body);
    let mut best: Option<(u64, String)> = None;
    let mut pending: Option<u64> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(
                attribute(rest, "BANDWIDTH")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
            );
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(bandwidth) = pending.take() {
            let url = resolve(base, line)?;
            if best.as_ref().is_none_or(|(current, _)| bandwidth > *current) {
                best = Some((bandwidth, url));
            }
        }
    }
    Ok(best.map(|(_, url)| url))
}

/// Parse a media playlist and resolve every URI against `base`.
pub fn media_playlist(body: &[u8], base: &str) -> Result<MediaPlaylist, String> {
    let text = normalize(body);
    let mut target_duration = 0u32;
    let mut media_sequence = 0u64;
    let mut end_list = false;
    let mut init = None;
    let mut segments: Vec<Segment> = Vec::new();
    let mut expect_segment = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest.trim().parse().unwrap_or(0);
        } else if line == "#EXT-X-ENDLIST" {
            end_list = true;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = attribute(rest, "URI") {
                init = Some(resolve(base, &uri)?);
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attribute(rest, "METHOD").unwrap_or_default();
            if !method.eq_ignore_ascii_case("NONE") {
                return Err("encrypted HLS playlists are not supported".to_string());
            }
        } else if line.starts_with("#EXTINF:") {
            expect_segment = true;
        } else if line.starts_with('#') {
            // Other tags (discontinuity, program date time, version) do not
            // change how the concatenated audio is fed to the decoder.
        } else if expect_segment {
            expect_segment = false;
            segments.push(Segment {
                sequence: media_sequence + segments.len() as u64,
                url: resolve(base, line)?,
            });
        }
    }

    if segments.is_empty() {
        return Err("HLS playlist contains no segments".to_string());
    }
    Ok(MediaPlaylist {
        target_duration,
        media_sequence,
        segments,
        end_list,
        init,
    })
}

/// Resolve a playlist or segment URI against the playlist URL.
fn resolve(base: &str, uri: &str) -> Result<String, String> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(uri.to_string());
    }
    let base = url::Url::parse(base).map_err(|e| format!("bad HLS playlist URL: {e}"))?;
    base.join(uri)
        .map(|url| url.to_string())
        .map_err(|e| format!("bad HLS URI {uri:?}: {e}"))
}

/// Normalise line endings and strip a UTF-8 BOM.
fn normalize(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    text.replace('\r', "\n")
}

/// Value of one `KEY=VALUE` attribute in an HLS tag's attribute list.
fn attribute(list: &str, key: &str) -> Option<String> {
    attributes(list)
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.to_string())
}

/// Split an HLS attribute list on commas that are not inside quotes.
fn attributes(list: &str) -> Vec<(&str, &str)> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (index, character) in list.char_indices() {
        match character {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&list[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&list[start..]);
    parts
        .into_iter()
        .filter_map(|part| part.split_once('='))
        .map(|(name, value)| (name.trim(), value.trim().trim_matches('"')))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://radio.example/live/index.m3u8";

    #[test]
    fn plain_m3u_is_not_hls() {
        assert!(!is_hls(b"#EXTM3U\n#EXTINF:-1,Station\nhttps://radio.example/stream\n"));
    }

    #[test]
    fn hls_tags_are_detected() {
        assert!(is_hls(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4,\nseg.aac\n"
        ));
    }

    #[test]
    fn master_playlist_picks_the_highest_bandwidth() {
        let body = br#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS="mp4a.40.2"
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS="mp4a.40.2,avc1.4d401f"
high/index.m3u8
"#;
        let variant = master_variant(body, BASE).unwrap().unwrap();
        assert_eq!(variant, "https://radio.example/live/high/index.m3u8");
    }

    #[test]
    fn media_playlist_is_not_a_master() {
        let body = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nseg.aac\n";
        assert_eq!(master_variant(body, BASE).unwrap(), None);
    }

    #[test]
    fn media_playlist_reads_sequence_and_segments() {
        let body = br#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:42
#EXTINF:6.0,
seg-42.aac
#EXT-X-DISCONTINUITY
#EXTINF:6.0,
seg-43.aac
#EXT-X-ENDLIST
"#;
        let playlist = media_playlist(body, BASE).unwrap();
        assert_eq!(playlist.target_duration, 6);
        assert_eq!(playlist.media_sequence, 42);
        assert!(playlist.end_list);
        assert_eq!(
            playlist.segments,
            vec![
                Segment {
                    sequence: 42,
                    url: "https://radio.example/live/seg-42.aac".to_string()
                },
                Segment {
                    sequence: 43,
                    url: "https://radio.example/live/seg-43.aac".to_string()
                },
            ]
        );
    }

    #[test]
    fn absolute_segment_urls_are_kept() {
        let body = b"#EXTM3U\n#EXTINF:4,\nhttps://cdn.example/a.aac\n";
        let playlist = media_playlist(body, BASE).unwrap();
        assert_eq!(playlist.segments[0].url, "https://cdn.example/a.aac");
    }

    #[test]
    fn init_segment_is_resolved() {
        let body = br#"#EXTM3U
#EXT-X-MAP:URI="init.mp4"
#EXTINF:4,
seg-1.m4s
#EXT-X-ENDLIST
"#;
        let playlist = media_playlist(body, BASE).unwrap();
        assert_eq!(
            playlist.init.as_deref(),
            Some("https://radio.example/live/init.mp4")
        );
    }

    #[test]
    fn encrypted_playlists_are_rejected() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI="key.bin"
#EXTINF:4,
seg.aac
"#;
        let error = media_playlist(body, BASE).unwrap_err();
        assert!(error.contains("encrypted"), "{error}");
    }

    #[test]
    fn unencrypted_key_tag_is_allowed() {
        let body = br#"#EXTM3U
#EXT-X-KEY:METHOD=NONE
#EXTINF:4,
seg.aac
#EXT-X-ENDLIST
"#;
        assert!(media_playlist(body, BASE).is_ok());
    }

    #[test]
    fn playlist_without_segments_is_rejected() {
        let body = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-ENDLIST\n";
        assert!(media_playlist(body, BASE).is_err());
    }

    #[test]
    fn attributes_ignore_commas_inside_quotes() {
        let values = attributes(r#"BANDWIDTH=128000,CODECS="mp4a.40.2,avc1.4d401f""#);
        assert_eq!(
            values,
            vec![("BANDWIDTH", "128000"), ("CODECS", "mp4a.40.2,avc1.4d401f")]
        );
    }
}
