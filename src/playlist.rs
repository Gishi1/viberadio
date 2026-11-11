//! Playlist resolution for station URLs.
//!
//! A station is sometimes published as a `.pls` or `.m3u` file that points at
//! the real stream. The network worker asks this module whether the start of a
//! response is a playlist, and if so which URL to fetch instead. Parsing is
//! pure, so the rules are covered by offline tests.
//!
//! HLS (`.m3u8` media playlists) is detected here but followed by the HLS path
//! in [`crate::player`]: it needs segment fetching, not a single indirection.

use std::collections::BTreeMap;
use std::fmt;

/// Largest playlist body accepted from a server. Real playlists are a few KiB.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// How many leading bytes are enough to classify a response.
pub const SNIFF_BYTES: usize = 64;

/// Playlist syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    M3u,
    Pls,
    /// HLS playlist. Followed by the HLS path in [`crate::player`].
    Hls,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::M3u => "M3U",
            Kind::Pls => "PLS",
            Kind::Hls => "HLS",
        })
    }
}

/// Why a body could not be turned into a stream URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The body is an HLS playlist; the HLS path in [`crate::player`] handles it.
    Hls,
    /// The playlist parsed but holds no HTTP(S) entry.
    Empty,
    /// The body exceeded [`MAX_BODY_BYTES`].
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hls => f.write_str("HLS playlist must be followed by the HLS path"),
            Error::Empty => f.write_str("playlist contains no HTTP stream URL"),
            Error::TooLarge => f.write_str("playlist is too large"),
        }
    }
}

impl std::error::Error for Error {}

/// One playlist entry with its URL resolved against the playlist's own URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub url: String,
    pub title: Option<String>,
}

/// What the first bytes of a response look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sniff {
    /// Not enough bytes yet to tell.
    Undecided,
    /// Not a playlist.
    Audio,
    /// A playlist of the given syntax.
    Playlist(Kind),
}

/// Markers that identify a playlist from its first bytes.
///
/// `#EXT-X-` is checked as a marker of its own so HLS is reported precisely
/// instead of being parsed as a one-entry M3U.
const MARKERS: [(&[u8], Kind); 6] = [
    (b"#EXTM3U", Kind::M3u),
    (b"#EXTINF", Kind::M3u),
    (b"#EXT-X-", Kind::Hls),
    (b"[playlist]", Kind::Pls),
    (b"File1=", Kind::Pls),
    (b"NumberOfEntries=", Kind::Pls),
];

/// Playlist syntax implied by an HTTP content type, if any.
pub fn kind_from_content_type(value: &str) -> Option<Kind> {
    let value = value.split(';').next()?.trim().to_ascii_lowercase();
    match value.as_str() {
        "audio/x-scpls" | "audio/scpls" => Some(Kind::Pls),
        // `x-mpegurl` and `vnd.apple.mpegurl` are used for plain M3U by Icecast
        // servers and for HLS by Apple tooling; the body decides which it is.
        "audio/x-mpegurl"
        | "audio/mpegurl"
        | "application/mpegurl"
        | "application/x-mpegurl"
        | "application/vnd.apple.mpegurl" => Some(Kind::M3u),
        _ => None,
    }
}

/// Playlist syntax implied by a URL path extension, if any.
pub fn kind_from_path(path: &str) -> Option<Kind> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "pls" => Some(Kind::Pls),
        "m3u" | "m3u8" => Some(Kind::M3u),
        _ => None,
    }
}

/// What kind of playlist a response is expected to be, and how sure that is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    pub kind: Kind,
    /// True when the content type said so. A hint from the URL extension alone
    /// is weak: the body still has to look like a playlist before it is read as
    /// one, because a stream can be served at a `.pls` or `.m3u` URL.
    pub trusted: bool,
}

/// Whether a response is expected to be a playlist, from its content type or
/// URL. A trusted hint lets the worker read the body directly; a weak hint only
/// makes an unmarked body count as a playlist.
pub fn hint(url: &str, content_type: Option<&str>) -> Option<Hint> {
    if let Some(kind) = content_type.and_then(kind_from_content_type) {
        return Some(Hint {
            kind,
            trusted: true,
        });
    }
    // An audio or video content type is authoritative: do not turn a stream at
    // a playlist-looking URL into a playlist body.
    if content_type.is_some_and(is_media_type) {
        return None;
    }
    let kind = url::Url::parse(url)
        .ok()
        .and_then(|parsed| kind_from_path(parsed.path()))?;
    Some(Hint {
        kind,
        trusted: false,
    })
}

fn is_media_type(value: &str) -> bool {
    let value = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    value.starts_with("audio/") || value.starts_with("video/") || value == "application/ogg"
}

/// Classify the first bytes of a response body.
///
/// Returns [`Sniff::Undecided`] while the prefix is still a prefix of a marker,
/// so a playlist split across two reads is still recognised.
pub fn classify(prefix: &[u8]) -> Sniff {
    let text = String::from_utf8_lossy(prefix);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let bytes = text.trim_start_matches(char::is_whitespace).as_bytes();
    if bytes.is_empty() {
        return Sniff::Undecided;
    }

    for (marker, kind) in MARKERS {
        if bytes.len() >= marker.len() && bytes[..marker.len()].eq_ignore_ascii_case(marker) {
            return Sniff::Playlist(kind);
        }
    }
    for (marker, _) in MARKERS {
        if marker.len() > bytes.len() && marker[..bytes.len()].eq_ignore_ascii_case(bytes) {
            return Sniff::Undecided;
        }
    }
    Sniff::Audio
}

/// Parse a playlist body. Relative entries resolve against `base`.
pub fn parse(body: &[u8], base: &str) -> Result<Vec<Entry>, Error> {
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::TooLarge);
    }
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    // `str::lines` splits on LF only; normalise lone CR too.
    let normalized = text.replace('\r', "\n");
    let text = normalized.as_str();
    if text
        .lines()
        .any(|line| starts_with_ignore_case(line.trim_start(), "#EXT-X-"))
    {
        return Err(Error::Hls);
    }
    if has_pls_key(text) {
        Ok(parse_pls(text, base))
    } else {
        Ok(parse_m3u(text, base))
    }
}

/// The first HTTP(S) entry of a playlist body.
pub fn next_stream(body: &[u8], base: &str) -> Result<String, Error> {
    let entries = parse(body, base)?;
    entries
        .iter()
        .find(|entry| is_http(&entry.url))
        .map(|entry| entry.url.clone())
        .ok_or(Error::Empty)
}

fn is_http(url: &str) -> bool {
    url::Url::parse(url)
        .map(|parsed| matches!(parsed.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len()
        && value.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// True when the body carries a PLS marker rather than an M3U one.
fn has_pls_key(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.eq_ignore_ascii_case("[playlist]") {
            return true;
        }
        // A PLS without its `[playlist]` header still starts with `File1=`.
        match line.split_once('=') {
            Some((key, _)) => {
                pls_index(key, "File").is_some() || pls_index(key, "NumberOfEntries").is_some()
            }
            None => false,
        }
    })
}

fn parse_m3u(text: &str, base: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut title: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = strip_prefix_ignore_case(line, "#EXTINF:") {
            title = rest
                .split_once(',')
                .map(|(_, name)| name.trim().to_string())
                .filter(|name| !name.is_empty());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(url) = resolve(base, line) {
            entries.push(Entry {
                url,
                title: title.take(),
            });
        }
    }
    entries
}

fn parse_pls(text: &str, base: &str) -> Vec<Entry> {
    let mut files: BTreeMap<u32, String> = BTreeMap::new();
    let mut titles: BTreeMap<u32, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('[')
            || line.starts_with(';')
            || line.starts_with('#')
        {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        if let Some(index) = pls_index(key, "File") {
            files.insert(index, value.to_string());
        } else if let Some(index) = pls_index(key, "Title") {
            titles.insert(index, value.to_string());
        }
    }
    files
        .into_iter()
        .filter_map(|(index, raw)| {
            let url = resolve(base, &raw)?;
            Some(Entry {
                url,
                title: titles.remove(&index),
            })
        })
        .collect()
}

/// `File12=` -> `Some(12)`, case-insensitively.
fn pls_index(key: &str, prefix: &str) -> Option<u32> {
    let key = key.trim();
    if key.len() <= prefix.len()
        || !key.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        return None;
    }
    key[prefix.len()..].trim().parse().ok()
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if starts_with_ignore_case(value, prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

/// Resolve one playlist line against the playlist URL.
fn resolve(base: &str, raw: &str) -> Option<String> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return None;
    }
    let joined = match url::Url::parse(base) {
        Ok(base) => base.join(raw).ok()?,
        Err(_) => url::Url::parse(raw).ok()?,
    };
    Some(joined.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "http://radio.example/listen/station.m3u";

    #[test]
    fn m3u_resolves_relative_urls_and_keeps_titles() {
        let body = "\u{feff}#EXTM3U\r\n#EXTINF:-1,First Stream\r\nhttp://a.example/one\r\n\r\n# a comment\r\n#EXTINF:-1,Second, with comma\r\n/two\r\n";
        let entries = parse(body.as_bytes(), BASE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].url, "http://a.example/one");
        assert_eq!(entries[0].title.as_deref(), Some("First Stream"));
        assert_eq!(entries[1].url, "http://radio.example/two");
        assert_eq!(entries[1].title.as_deref(), Some("Second, with comma"));
    }

    #[test]
    fn m3u_with_lone_cr_line_endings_parses() {
        let entries = parse(b"#EXTM3U\r#EXTINF:-1,One\rhttp://a.example/one\r", BASE).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "http://a.example/one");
        assert_eq!(entries[0].title.as_deref(), Some("One"));
    }

    #[test]
    fn m3u_without_a_header_lists_bare_urls() {
        let entries = parse(b"http://a.example/one\nhttp://b.example/two\n", BASE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].url, "http://b.example/two");
        assert_eq!(entries[1].title, None);
    }

    #[test]
    fn pls_orders_by_index_and_pairs_titles() {
        let body = "[playlist]\nNumberOfEntries=2\nFile2=http://b.example/two\nTitle2=Second\nFile1=http://a.example/one\nTitle1=First\nVersion=2\n";
        let entries = parse(body.as_bytes(), BASE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].url, "http://a.example/one");
        assert_eq!(entries[0].title.as_deref(), Some("First"));
        assert_eq!(entries[1].url, "http://b.example/two");
        assert_eq!(entries[1].title.as_deref(), Some("Second"));
    }

    #[test]
    fn pls_handles_quotes_relative_paths_and_missing_titles() {
        let body = "[playlist]\nfile1=\"stream.mp3\"\nTitle1=\"Quoted\"\nFile2=\"http://b.example/two\"\n";
        let entries = parse(body.as_bytes(), "http://radio.example/dir/list.pls").unwrap();
        assert_eq!(entries[0].url, "http://radio.example/dir/stream.mp3");
        assert_eq!(entries[0].title.as_deref(), Some("Quoted"));
        assert_eq!(entries[1].url, "http://b.example/two");
        assert_eq!(entries[1].title, None);
    }

    #[test]
    fn hls_is_recognised_and_refused() {
        let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nseg1.aac\n";
        assert_eq!(parse(body.as_bytes(), BASE), Err(Error::Hls));
    }

    #[test]
    fn classify_needs_enough_bytes_but_tolerates_bom_and_space() {
        assert_eq!(classify(b""), Sniff::Undecided);
        assert_eq!(classify(b"#EX"), Sniff::Undecided);
        assert_eq!(classify(b"#EXTM3U\n"), Sniff::Playlist(Kind::M3u));
        assert_eq!(classify(b"#EXTINF:-1,x"), Sniff::Playlist(Kind::M3u));
        assert_eq!(classify(b"#EXT-X-STREAM-INF"), Sniff::Playlist(Kind::Hls));
        assert_eq!(classify(b"[playlist]"), Sniff::Playlist(Kind::Pls));
        assert_eq!(classify(b"File1=http://a"), Sniff::Playlist(Kind::Pls));
        assert_eq!(
            classify(b"\xEF\xBB\xBF#EXTM3U\n"),
            Sniff::Playlist(Kind::M3u)
        );
        assert_eq!(classify(b" \r\n#EXTM3U"), Sniff::Playlist(Kind::M3u));
        assert_eq!(classify(b"ID3\x04"), Sniff::Audio);
        assert_eq!(classify(&[0xff, 0xfb, 0x90, 0x00]), Sniff::Audio);
    }

    #[test]
    fn content_type_and_extension_hints() {
        assert_eq!(
            kind_from_content_type("audio/x-scpls; charset=utf-8"),
            Some(Kind::Pls)
        );
        assert_eq!(
            kind_from_content_type("Audio/X-MPEGURL"),
            Some(Kind::M3u)
        );
        assert_eq!(kind_from_content_type("audio/mpeg"), None);
        assert_eq!(kind_from_path("/listen/radio.pls"), Some(Kind::Pls));
        assert_eq!(kind_from_path("/listen/radio.M3U8"), Some(Kind::M3u));
        assert_eq!(kind_from_path("/listen/radio"), None);

        assert_eq!(
            hint("http://a/radio.pls", None),
            Some(Hint {
                kind: Kind::Pls,
                trusted: false
            })
        );
        assert_eq!(
            hint("http://a/radio.pls", Some("application/octet-stream")),
            Some(Hint {
                kind: Kind::Pls,
                trusted: false
            })
        );
        assert_eq!(
            hint("http://a/radio.pls", Some("audio/x-scpls")),
            Some(Hint {
                kind: Kind::Pls,
                trusted: true
            })
        );
        // An audio content type wins over a playlist-looking extension.
        assert_eq!(hint("http://a/radio.pls", Some("audio/mpeg")), None);
        assert_eq!(hint("http://a/radio.m3u", Some("application/ogg")), None);
        assert_eq!(hint("http://a/stream", Some("audio/mpeg")), None);
    }

    #[test]
    fn pls_without_a_header_is_still_pls() {
        let body = "File1=http://a.example/one\nTitle1=One\nNumberOfEntries=1\n";
        let entries = parse(body.as_bytes(), BASE).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "http://a.example/one");
        assert_eq!(entries[0].title.as_deref(), Some("One"));
    }

    #[test]
    fn next_stream_skips_entries_this_player_cannot_open() {
        let body = "#EXTM3U\nfile:///local/a.mp3\nftp://a.example/b.mp3\nhttps://a.example/c.mp3\n";
        assert_eq!(
            next_stream(body.as_bytes(), BASE).unwrap(),
            "https://a.example/c.mp3"
        );
        assert_eq!(
            next_stream(b"#EXTM3U\nfile:///local/a.mp3\n", BASE),
            Err(Error::Empty)
        );
    }

    #[test]
    fn oversized_bodies_are_refused() {
        let body = vec![b'#'; MAX_BODY_BYTES + 1];
        assert_eq!(parse(&body, BASE), Err(Error::TooLarge));
    }
}
