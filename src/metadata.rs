//! ICY metadata: the byte-level demuxer that keeps metadata out of the decoder,
//! plus the pure tag parser and custom-format splitter used by M4.

/// Split an ICY stream into audio bytes and `StreamTitle` updates.
///
/// The protocol inserts one length byte every `metaint` audio bytes. A zero
/// length byte means "no metadata this interval"; otherwise the length is in
/// 16-byte blocks. The demuxer is stateful because a block can be split across
/// any number of TCP chunks.
#[derive(Debug)]
pub struct IcyDemuxer {
    metaint: usize,
    until_meta: usize,
    meta_remaining: usize,
    metadata: Vec<u8>,
}

impl IcyDemuxer {
    pub fn new(metaint: usize) -> Self {
        Self {
            metaint,
            until_meta: metaint,
            meta_remaining: 0,
            metadata: Vec::new(),
        }
    }

    /// Consume a transport chunk, appending only audio to `audio`. Returns the
    /// latest title whose metadata block completed in this call, if any.
    #[allow(dead_code)]
    pub fn feed(&mut self, input: &[u8], audio: &mut Vec<u8>) -> Option<String> {
        let mut latest = None;
        self.feed_with(input, |segment, title| {
            audio.extend_from_slice(segment);
            if let Some(title) = title {
                latest = Some(title.to_string());
            }
        });
        latest
    }

    /// Consume a transport chunk and emit audio and titles **in wire order**.
    /// Recording split relies on this order: the bytes before a title belong to
    /// the previous track, the bytes after it to the new one.
    pub fn feed_with<F>(&mut self, input: &[u8], mut on_event: F)
    where
        F: FnMut(&[u8], Option<&str>),
    {
        if self.metaint == 0 {
            on_event(input, None);
            return;
        }

        let mut index = 0;
        while index < input.len() {
            if self.meta_remaining > 0 {
                let take = self.meta_remaining.min(input.len() - index);
                self.metadata.extend_from_slice(&input[index..index + take]);
                index += take;
                self.meta_remaining -= take;
                if self.meta_remaining == 0 {
                    if let Some(title) = parse_stream_title(&self.metadata) {
                        on_event(&[], Some(&title));
                    }
                    self.metadata.clear();
                    self.until_meta = self.metaint;
                }
            } else if self.until_meta == 0 {
                let length = input[index] as usize;
                index += 1;
                if length == 0 {
                    self.until_meta = self.metaint;
                } else {
                    self.meta_remaining = length * 16;
                    self.metadata.clear();
                }
            } else {
                let take = self.until_meta.min(input.len() - index);
                on_event(&input[index..index + take], None);
                index += take;
                self.until_meta -= take;
            }
        }
    }
}

/// Parse `StreamTitle='…';` from one metadata block. Tolerates NUL padding,
/// single or double quotes, and escaped quotes.
pub fn parse_stream_title(metadata: &[u8]) -> Option<String> {
    let text = decode_text(metadata);
    let text = text.trim_matches('\0').trim();
    let start = text.find("StreamTitle=")? + "StreamTitle=".len();
    let rest = text[start..].trim_start();
    let quote = rest.chars().next()?;
    let (body, end_quote) = if quote == '\'' || quote == '"' {
        (&rest[1..], quote)
    } else {
        (rest, ';')
    };
    // Real servers often leave an apostrophe unescaped (`Don't Stop`), so a
    // quote only ends the value when it is followed by `;`. This also handles
    // escaped quotes and additional `Field='…';` entries after StreamTitle.
    let mut escaped = false;
    let mut end = None;
    for (index, character) in body.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == end_quote {
            let after = &body[index + character.len_utf8()..];
            if after.trim_start().starts_with(';') {
                end = Some(index);
                break;
            }
        }
    }
    let end = end.or_else(|| body.find(end_quote))?;
    let raw = &body[..end];
    let title = raw
        .replace("\\'", "'")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\");
    let title = title.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

/// A split of an ICY title into artist and song according to a user format.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackTag {
    pub artist: Option<String>,
    pub title: Option<String>,
}

/// Default format when the station has no custom tag format.
#[allow(dead_code)]
pub fn default_tag_format(raw: &str) -> &'static str {
    if raw.contains(" - ") {
        "<Artist> - <Song->"
    } else {
        "<Song>"
    }
}

/// Whether an ignore-list entry matches a raw title. Entries are
/// `(artist, song)`; either side may be empty. Matching is case-insensitive and
/// substring-based.
#[allow(dead_code)]
pub fn title_is_ignored(entries: &[(Option<String>, Option<String>)], raw: &str) -> bool {
    title_is_ignored_with_format(entries, raw, None)
}

pub fn title_is_ignored_with_format(
    entries: &[(Option<String>, Option<String>)],
    raw: &str,
    format: Option<&str>,
) -> bool {
    let raw = raw.trim();
    let parsed = split_title(raw, format.unwrap_or_else(|| default_tag_format(raw)));
    let raw_lower = raw.to_lowercase();
    let artist_lower = parsed.artist.as_deref().map(str::to_lowercase);
    let song_lower = parsed.title.as_deref().map(str::to_lowercase);

    let normalise = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase)
    };

    entries.iter().any(|(artist, song)| {
        let artist = normalise(artist);
        let song = normalise(song);
        match (artist, song) {
            (Some(artist), Some(song)) => {
                artist_lower.as_deref() == Some(artist.as_str())
                    && song_lower.as_deref() == Some(song.as_str())
            }
            (Some(artist), None) => {
                artist == raw_lower
                    || artist_lower.as_deref() == Some(artist.as_str())
                    || song_lower.as_deref() == Some(artist.as_str())
            }
            (None, Some(song)) => {
                song == raw_lower
                    || artist_lower.as_deref() == Some(song.as_str())
                    || song_lower.as_deref() == Some(song.as_str())
            }
            (None, None) => false,
        }
    })
}

/// How a wish-list entry compares with a raw ICY title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareMode {
    Exact,
    Contains,
    StartsWith,
}

impl CompareMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact_match",
            Self::Contains => "contains_text",
            Self::StartsWith => "starts_with",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "exact" | "exact_match" => Self::Exact,
            "starts_with" | "startswith" | "prefix" => Self::StartsWith,
            "contains" | "contains_text" => Self::Contains,
            _ => Self::Contains,
        }
    }
}

/// One wish-list rule. Actions are stored as text so recording/playback can be
/// wired as those features land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WishEntry {
    pub id: i64,
    pub artist: Option<String>,
    pub song_title: Option<String>,
    pub action: String,
    pub compare: CompareMode,
}

/// Return every wish-list entry matching a raw title. All non-empty fields in an
/// entry must match; an entry with neither field never matches.
#[allow(dead_code)]
pub fn wish_matches<'a>(entries: &'a [WishEntry], raw: &str) -> Vec<&'a WishEntry> {
    wish_matches_with_format(entries, raw, None)
}

/// Format-aware variant used by the app when a station has a custom tag format.
pub fn wish_matches_with_format<'a>(
    entries: &'a [WishEntry],
    raw: &str,
    format: Option<&str>,
) -> Vec<&'a WishEntry> {
    let raw = raw.trim();
    let parsed = split_title(raw, format.unwrap_or_else(|| default_tag_format(raw)));
    entries
        .iter()
        .filter(|entry| {
            let artist = entry
                .artist
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let song = entry
                .song_title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            if artist.is_none() && song.is_none() {
                return false;
            }
            let field_matches = |candidate: Option<&str>, needle: &str| -> bool {
                let Some(candidate) = candidate.or(Some(raw)) else {
                    return false;
                };
                let candidate = collapse_spaces(candidate);
                let needle = collapse_spaces(needle);
                match entry.compare {
                    CompareMode::Exact => candidate.eq_ignore_ascii_case(&needle),
                    CompareMode::Contains => candidate
                        .to_ascii_lowercase()
                        .contains(&needle.to_ascii_lowercase()),
                    CompareMode::StartsWith => candidate
                        .to_ascii_lowercase()
                        .starts_with(&needle.to_ascii_lowercase()),
                }
            };
            artist.map_or(true, |needle| {
                field_matches(parsed.artist.as_deref(), needle)
            }) && song.map_or(true, |needle| {
                field_matches(parsed.title.as_deref(), needle)
            })
        })
        .collect()
}

/// The label the recorder and history use for a parsed tag.
pub fn label(artist: Option<&str>, title: Option<&str>) -> String {
    match (artist, title) {
        (Some(artist), Some(title)) => format!("{artist} - {title}"),
        (Some(artist), None) => artist.to_string(),
        (None, Some(title)) => title.to_string(),
        (None, None) => String::new(),
    }
}

/// Split a raw title with a format such as `<Artist> - <Song->`. Literal text
/// around the placeholders is used as the separator. Unknown formats fall back
/// to the whole raw string in `title`.
#[allow(dead_code)] // used by the M4 tag-format UI
pub fn split_title(raw: &str, format: &str) -> TrackTag {
    let artist_at = format.find("<Artist>");
    let song_at = format
        .find("<Song>")
        .or_else(|| format.find("<Song->"))
        .or_else(|| format.find("<Title>"));
    match (artist_at, song_at) {
        (Some(a), Some(s)) if s > a => {
            let separator = format[a + "<Artist>".len()..s].trim();
            let (artist, title) = split_once(raw, separator);
            TrackTag {
                artist: non_empty(artist),
                title: non_empty(title),
            }
        }
        (Some(a), Some(s)) if a > s => {
            let placeholder_len = if format[s..].starts_with("<Song->") {
                "<Song->".len()
            } else if format[s..].starts_with("<Title>") {
                "<Title>".len()
            } else {
                "<Song>".len()
            };
            let separator = format[s + placeholder_len..a].trim();
            let (title, artist) = split_once(raw, separator);
            TrackTag {
                artist: non_empty(artist),
                title: non_empty(title),
            }
        }
        (Some(a), None) => {
            let prefix = format[..a].trim();
            let value = raw.strip_prefix(prefix).unwrap_or(raw).trim();
            TrackTag {
                artist: non_empty(value),
                title: None,
            }
        }
        (None, Some(s)) => {
            let prefix = format[..s].trim();
            let value = raw.strip_prefix(prefix).unwrap_or(raw).trim();
            TrackTag {
                artist: None,
                title: non_empty(value),
            }
        }
        _ => TrackTag {
            artist: None,
            title: non_empty(raw),
        },
    }
}

/// Some stations put "Artist - Title" inside the song field, usually because
/// the uploader is the outer artist. Return that inner pair so a catalog
/// lookup can retry with it.
pub fn inner_pair(title: &str) -> Option<(String, String)> {
    let mut best: Option<(usize, &str)> = None;
    for separator in [" - ", " – ", " — "] {
        if let Some(at) = title.find(separator) {
            if best.map_or(true, |(best_at, _)| at < best_at) {
                best = Some((at, separator));
            }
        }
    }
    let (at, separator) = best?;
    let artist = title[..at].trim();
    let song = title[at + separator.len()..].trim();
    if artist.is_empty() || song.is_empty() {
        return None;
    }
    Some((artist.to_string(), song.to_string()))
}

#[allow(dead_code)]
/// ICY metadata is nominally Latin-1 but many stations send UTF-8. Prefer UTF-8
/// and decode Windows-1252 only when that fails.
fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|byte| cp1252(*byte)).collect(),
    }
}

fn cp1252(byte: u8) -> char {
    match byte {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x86 => '†',
        0x87 => '‡',
        0x88 => 'ˆ',
        0x89 => '‰',
        0x8A => 'Š',
        0x8B => '‹',
        0x8C => 'Œ',
        0x8E => 'Ž',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x98 => '˜',
        0x99 => '™',
        0x9A => 'š',
        0x9B => '›',
        0x9C => 'œ',
        0x9E => 'ž',
        0x9F => 'Ÿ',
        other => char::from(other),
    }
}

#[allow(dead_code)]
fn collapse_spaces(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[allow(dead_code)]
fn split_once<'a>(raw: &'a str, separator: &str) -> (&'a str, &'a str) {
    if separator.is_empty() {
        return (raw, "");
    }
    match raw.split_once(separator) {
        Some(parts) => parts,
        None => (raw, ""),
    }
}

#[allow(dead_code)]
fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad_metadata(text: &str) -> Vec<u8> {
        let blocks = text.len().div_ceil(16);
        let mut block = vec![0u8; blocks * 16];
        block[..text.len()].copy_from_slice(text.as_bytes());
        block
    }

    #[test]
    fn parses_stream_title_with_quotes_and_padding() {
        let mut block = pad_metadata("StreamTitle='Artist - Song';");
        block.extend_from_slice(&[0, 0, 0]);
        assert_eq!(parse_stream_title(&block).as_deref(), Some("Artist - Song"));
        assert_eq!(
            parse_stream_title(b"StreamTitle=\"A \\\"B\\\"\";").as_deref(),
            Some("A \"B\"")
        );
        assert_eq!(parse_stream_title(&[0u8; 16]), None);
        assert_eq!(
            parse_stream_title(b"StreamTitle='Don't Stop';").as_deref(),
            Some("Don't Stop")
        );
        assert_eq!(
            parse_stream_title(b"StreamTitle='Caf\xe9';").as_deref(),
            Some("Café")
        );
    }

    #[test]
    fn demuxes_odd_chunks_and_keeps_audio() {
        // 4 audio bytes, one 16-byte metadata block, then 4 more audio bytes.
        let title = pad_metadata("StreamTitle='One';");
        let mut wire = Vec::new();
        wire.extend_from_slice(b"aaaa");
        wire.push((title.len() / 16) as u8);
        wire.extend_from_slice(&title);
        wire.extend_from_slice(b"bbbb");
        wire.push(0);
        wire.extend_from_slice(b"cccc");

        let mut demux = IcyDemuxer::new(4);
        let mut audio = Vec::new();
        let mut titles = Vec::new();
        for byte in &wire {
            if let Some(title) = demux.feed(&[*byte], &mut audio) {
                titles.push(title);
            }
        }
        assert_eq!(audio, b"aaaabbbbcccc");
        assert_eq!(titles, vec!["One".to_string()]);
    }

    #[test]
    fn multiple_metadata_blocks_only_emit_the_latest() {
        let mut wire = Vec::new();
        for (audio, title) in [(b"aa", "First"), (b"bb", "Second")] {
            wire.extend_from_slice(audio);
            let block = pad_metadata(&format!("StreamTitle='{title}';"));
            wire.push((block.len() / 16) as u8);
            wire.extend_from_slice(&block);
        }
        let mut demux = IcyDemuxer::new(2);
        let mut audio = Vec::new();
        assert_eq!(demux.feed(&wire, &mut audio).as_deref(), Some("Second"));
        assert_eq!(audio, b"aabb");
    }

    #[test]
    fn arbitrary_chunk_boundaries_never_leak_metadata() {
        let first = pad_metadata("StreamTitle='First';");
        let second = pad_metadata("StreamTitle='Second';");
        let mut wire = Vec::new();
        wire.extend_from_slice(b"aaaa");
        wire.push((first.len() / 16) as u8);
        wire.extend_from_slice(&first);
        wire.extend_from_slice(b"bbbb");
        wire.push((second.len() / 16) as u8);
        wire.extend_from_slice(&second);
        wire.extend_from_slice(b"cccc");

        for chunk_size in 1..=wire.len() {
            let mut demux = IcyDemuxer::new(4);
            let mut audio = Vec::new();
            let mut latest = None;
            for chunk in wire.chunks(chunk_size) {
                // An empty transport chunk must not disturb the state machine.
                assert!(demux.feed(&[], &mut audio).is_none());
                if let Some(title) = demux.feed(chunk, &mut audio) {
                    latest = Some(title);
                }
            }
            assert_eq!(audio, b"aaaabbbbcccc", "chunk size {chunk_size}");
            // One large transport chunk may contain several metadata blocks; the
            // demuxer deliberately returns only the newest to avoid repaint churn.
            assert_eq!(latest.as_deref(), Some("Second"), "chunk size {chunk_size}");
        }
    }

    #[test]
    fn new_demuxer_starts_clean_after_a_dropped_attempt() {
        let block = pad_metadata("StreamTitle='After';");
        let mut dropped = Vec::new();
        dropped.extend_from_slice(b"aa");
        dropped.push((block.len() / 16) as u8);
        dropped.extend_from_slice(&block[..7]); // cut inside the metadata block

        let mut demux = IcyDemuxer::new(2);
        let mut audio = Vec::new();
        assert!(demux.feed(&dropped, &mut audio).is_none());
        assert_eq!(audio, b"aa");

        // A reconnect creates a new demuxer; the fresh stream must decode cleanly
        // from its first byte and expose only its own title.
        let mut wire = Vec::new();
        wire.extend_from_slice(b"aaaa");
        wire.push((block.len() / 16) as u8);
        wire.extend_from_slice(&block);
        wire.extend_from_slice(b"bbbb");
        let mut fresh = IcyDemuxer::new(4);
        let mut audio = Vec::new();
        assert_eq!(fresh.feed(&wire, &mut audio).as_deref(), Some("After"));
        assert_eq!(audio, b"aaaabbbb");
    }

    #[test]
    fn zero_metaint_is_a_passthrough() {
        let mut demux = IcyDemuxer::new(0);
        let mut audio = Vec::new();
        assert_eq!(demux.feed(b"raw audio", &mut audio), None);
        assert_eq!(audio, b"raw audio");
    }

    #[test]
    fn wish_list_compare_modes() {
        let entry = |artist: Option<&str>, song: Option<&str>, mode| WishEntry {
            id: 0,
            artist: artist.map(str::to_string),
            song_title: song.map(str::to_string),
            action: "record".to_string(),
            compare: mode,
        };
        let exact = vec![entry(
            Some("Miles Davis"),
            Some("Blue in Green"),
            CompareMode::Exact,
        )];
        assert_eq!(wish_matches(&exact, "Miles Davis - Blue in Green").len(), 1);
        assert!(wish_matches(&exact, "Miles Davis - Blue in Green (live)").is_empty());

        let contains = vec![entry(None, Some("blue"), CompareMode::Contains)];
        assert_eq!(
            wish_matches(&contains, "Miles Davis - Blue in Green").len(),
            1
        );

        let prefix = vec![entry(Some("miles"), None, CompareMode::StartsWith)];
        assert_eq!(wish_matches(&prefix, "Miles Davis - So What").len(), 1);
        assert!(wish_matches(&prefix, "The Miles Davis Quartet - So What").is_empty());

        assert_eq!(CompareMode::parse("exact_match"), CompareMode::Exact);
        assert_eq!(CompareMode::parse("contains_text"), CompareMode::Contains);
        assert_eq!(CompareMode::parse("wat"), CompareMode::Contains);

        let empty_entry = vec![entry(None, None, CompareMode::Exact)];
        assert!(wish_matches(&empty_entry, "Anything - At All").is_empty());

        let reversed = vec![entry(Some("Artist"), Some("Song"), CompareMode::Exact)];
        assert_eq!(
            wish_matches_with_format(&reversed, "Song - Artist", Some("<Song> - <Artist>")).len(),
            1
        );
        assert_eq!(
            wish_matches_with_format(&reversed, "Song - Artist", Some("<Song-> - <Artist>")).len(),
            1
        );
        let spaced = vec![entry(Some("Miles  Davis"), None, CompareMode::Exact)];
        assert_eq!(wish_matches(&spaced, "Miles Davis - So What").len(), 1);
    }

    #[test]
    fn ignore_entries_match_case_insensitively() {
        let entries = vec![
            (Some("Ads".to_string()), Some("Station ID".to_string())),
            (None, Some("Jingle".to_string())),
            (None, None),
        ];
        assert!(title_is_ignored(&entries, "ADS - station id"));
        assert!(title_is_ignored(&entries, "Artist - Jingle"));
        assert!(!title_is_ignored(&entries, "Meads - Song"));
        assert!(!title_is_ignored(&entries, "Real Artist - Real Song"));
        assert_eq!(default_tag_format("A - B"), "<Artist> - <Song->");
        assert_eq!(default_tag_format("Single"), "<Song>");
    }

    #[test]
    fn split_custom_formats() {
        assert_eq!(
            split_title("Artist - Song", "<Artist> - <Song->"),
            TrackTag {
                artist: Some("Artist".into()),
                title: Some("Song".into())
            }
        );
        assert_eq!(
            split_title("Artist | Song", "<Artist> | <Song>"),
            TrackTag {
                artist: Some("Artist".into()),
                title: Some("Song".into())
            }
        );
        assert_eq!(
            split_title("Song - Artist", "<Song> - <Artist>"),
            TrackTag {
                artist: Some("Artist".into()),
                title: Some("Song".into())
            }
        );
        assert_eq!(
            split_title("Only a song", "<Song>"),
            TrackTag {
                artist: None,
                title: Some("Only a song".into())
            }
        );
        assert_eq!(
            split_title("Plain", "unknown"),
            TrackTag {
                artist: None,
                title: Some("Plain".into())
            }
        );
    }

    #[test]
    fn finds_the_artist_and_song_inside_a_dj_title() {
        assert_eq!(
            inner_pair("CeCe Peniston - Finally（Free Download）"),
            Some(("CeCe Peniston".into(), "Finally（Free Download）".into()))
        );
        assert_eq!(
            inner_pair("Artist – Song"),
            Some(("Artist".into(), "Song".into()))
        );
        assert_eq!(inner_pair("Finally"), None);
        assert_eq!(inner_pair(" - "), None);
    }
}
