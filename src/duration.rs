//! Track length and catalog metadata from the iTunes Search API.
//!
//! The search endpoint needs no key and returns the full `trackTimeMillis`,
//! catalog title, artist, album and a cover URL for each song. Two matches are
//! kept: an exact title and artist match for scheduling a recording close, and
//! a looser artist/title match for the stream bar so featured-artist titles
//! still show their cover and metadata.

use std::time::Duration;

const ITUNES_URL: &str = "https://itunes.apple.com/search";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_SECONDS: u32 = 30;
const MAX_SECONDS: u32 = 1800;

/// One confidently matched catalog recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub duration_seconds: u32,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    /// 300×300 cover, when the catalog has one.
    pub artwork_url: Option<String>,
}

/// The two views of one search response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LookupResult {
    /// Exact title and artist match; safe for scheduling a close.
    pub strict: Option<TrackInfo>,
    /// Looser artist and title match; safe for the stream bar.
    pub display: Option<TrackInfo>,
}

/// Stable key for the in-memory duration cache.
pub fn cache_key(artist: &str, title: &str) -> String {
    format!("{}|{}", normalize(artist), normalize(title))
}

/// Lowercase words without punctuation, so "Gil Glaze" and "gil  glaze" share
/// a cache entry.
pub fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            out.push(ch);
            gap = false;
        } else {
            gap = true;
        }
    }
    out
}

/// Look up the catalog entry for one song. An empty result means iTunes has no
/// confident match. An empty artist searches by title alone, which covers
/// titles that contain a dash but no artist.
pub fn lookup(artist: &str, title: &str) -> Result<LookupResult, String> {
    if title.trim().is_empty() {
        return Ok(LookupResult::default());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the duration client: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        let term = if artist.trim().is_empty() {
            title.trim().to_string()
        } else {
            format!("{} {}", artist.trim(), title.trim())
        };
        let response = client
            .get(ITUNES_URL)
            .query(&[("term", term.as_str()), ("entity", "song"), ("limit", "8")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("iTunes answered {}", response.status()));
        }
        let text = response.text().await.map_err(|e| e.to_string())?;
        Ok(parse_search(&text, artist, title))
    })
}

/// Whether `needle` appears in `haystack` as whole words. Both sides are already
/// normalized to lowercase words separated by single spaces, so this rejects
/// "one" inside "someone" while still accepting "finally" in
/// "finally extended mix".
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || haystack.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before_ok = start == 0 || haystack[..start].ends_with(' ');
        let after_ok = end == haystack.len() || haystack[end..].starts_with(' ');
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len();
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// Pick the best strict and display matches. Pure, so the selection rules can
/// be tested without the network.
pub fn parse_search(json: &str, artist: &str, title: &str) -> LookupResult {
    let Some(value) = serde_json::from_str::<serde_json::Value>(json).ok() else {
        return LookupResult::default();
    };
    let Some(results) = value.get("results").and_then(|value| value.as_array()) else {
        return LookupResult::default();
    };
    let wanted_artist = normalize(artist);
    let wanted_title = normalize(title);
    let mut strict: Option<(u8, TrackInfo)> = None;
    let mut display: Option<(u8, TrackInfo)> = None;
    for result in results {
        let Some(name) = result.get("trackName").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(by) = result.get("artistName").and_then(|value| value.as_str()) else {
            continue;
        };
        let normalized_name = normalize(name);
        let normalized_artist = normalize(by);
        let title_exact = normalized_name == wanted_title;
        let title_contains = contains_word(&normalized_name, &wanted_title)
            || contains_word(&wanted_title, &normalized_name);
        if !title_exact && !title_contains {
            continue;
        }
        let artist_exact = !wanted_artist.is_empty() && normalized_artist == wanted_artist;
        let artist_contains = !wanted_artist.is_empty()
            && (contains_word(&normalized_artist, &wanted_artist)
                || contains_word(&wanted_artist, &normalized_artist));
        if !wanted_artist.is_empty() && !artist_exact && !artist_contains {
            continue;
        }
        // A strict match may drop words the station omitted (a featured artist),
        // but must not add words that suggest a different act (a tribute band).
        let artist_strict = wanted_artist.is_empty()
            || artist_exact
            || contains_word(&wanted_artist, &normalized_artist);
        let millis = result
            .get("trackTimeMillis")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let seconds = (millis / 1000) as u32;
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
            continue;
        }
        let info = TrackInfo {
            duration_seconds: seconds,
            title: name.to_string(),
            artist: by.to_string(),
            album: result
                .get("collectionName")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            genre: result
                .get("primaryGenreName")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            year: result
                .get("releaseDate")
                .and_then(|value| value.as_str())
                .and_then(|date| date.get(..4))
                .and_then(|year| year.parse().ok()),
            artwork_url: result
                .get("artworkUrl100")
                .and_then(|value| value.as_str())
                .map(|url| url.replace("100x100bb", "300x300bb")),
        };
        let title_score = if title_exact { 2 } else { 1 };
        let artist_score = if artist_exact { 2 } else { 1 };
        let score = title_score + artist_score;
        if display
            .as_ref()
            .map(|(best, _)| score > *best)
            .unwrap_or(true)
        {
            display = Some((score, info.clone()));
        }
        if title_exact
            && artist_strict
            && strict
                .as_ref()
                .map(|(best, _)| score > *best)
                .unwrap_or(true)
        {
            strict = Some((score, info));
        }
    }
    LookupResult {
        strict: strict.map(|(_, info)| info),
        display: display.map(|(_, info)| info),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_matching_recording_and_its_metadata() {
        let json = r#"{"resultCount":3,"results":[
            {"artistName":"Queen","trackName":"Bohemian Rhapsody","collectionName":"A Night at the Opera","primaryGenreName":"Rock","releaseDate":"1975-11-21T08:00:00Z","artworkUrl100":"https://example.test/100x100bb.jpg","trackTimeMillis":354320},
            {"artistName":"Queen","trackName":"Bohemian Rhapsody (Live Aid)","trackTimeMillis":1200000},
            {"artistName":"Some Cover Band","trackName":"Bohemian Rhapsody","trackTimeMillis":200000}
        ]}"#;
        let result = parse_search(json, "Queen", "Bohemian Rhapsody");
        let info = result.strict.unwrap();
        assert_eq!(info.duration_seconds, 354);
        assert_eq!(info.title, "Bohemian Rhapsody");
        assert_eq!(info.artist, "Queen");
        assert_eq!(info.album.as_deref(), Some("A Night at the Opera"));
        assert_eq!(info.genre.as_deref(), Some("Rock"));
        assert_eq!(info.year, Some(1975));
        assert_eq!(
            info.artwork_url.as_deref(),
            Some("https://example.test/300x300bb.jpg")
        );
        assert_eq!(result.display, Some(info));
    }

    #[test]
    fn keeps_a_featured_title_for_display_but_not_for_duration() {
        let json = r#"{"resultCount":2,"results":[
            {"artistName":"ANOTR","trackName":"Talk To You (feat. 54 Ultra)","collectionName":"Talk To You (feat. 54 Ultra) - Single","artworkUrl100":"https://example.test/100x100bb.jpg","trackTimeMillis":180000},
            {"artistName":"Some Cover Band","trackName":"Talk To You","trackTimeMillis":170000}
        ]}"#;
        let result = parse_search(json, "ANOTR", "Talk To You");
        assert!(result.strict.is_none());
        assert_eq!(
            result.display.unwrap().title,
            "Talk To You (feat. 54 Ultra)"
        );
    }

    #[test]
    fn matches_a_remix_suffix_for_display_only() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"CeCe Peniston","trackName":"Finally","collectionName":"Finally","artworkUrl100":"https://example.test/100x100bb.jpg","trackTimeMillis":240000}
        ]}"#;
        let result = parse_search(json, "CeCe Peniston", "Finally (Free Download)");
        assert!(result.strict.is_none());
        assert_eq!(result.display.unwrap().title, "Finally");
    }

    #[test]
    fn a_dj_uploaded_title_reaches_the_catalog_through_the_inner_pair() {
        let raw = "DJ PinkGesia - CeCe Peniston - Finally（Free Download）";
        let format = crate::metadata::default_tag_format(raw);
        let tag = crate::metadata::split_title(raw, format);
        assert_eq!(tag.artist.as_deref(), Some("DJ PinkGesia"));
        assert_eq!(
            tag.title.as_deref(),
            Some("CeCe Peniston - Finally（Free Download）")
        );
        let (artist, title) = crate::metadata::inner_pair(tag.title.as_deref().unwrap()).unwrap();
        assert_eq!(artist, "CeCe Peniston");
        assert_eq!(title, "Finally（Free Download）");
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"CeCe Peniston","trackName":"Finally","collectionName":"Finally","artworkUrl100":"https://example.test/100x100bb.jpg","trackTimeMillis":240000}
        ]}"#;
        let result = parse_search(json, &artist, &title);
        assert!(result.strict.is_none());
        assert_eq!(result.display.unwrap().title, "Finally");
    }

    #[test]
    fn ignores_results_without_a_title_match() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"Queen","trackName":"Another One Bites the Dust","trackTimeMillis":210000}
        ]}"#;
        let result = parse_search(json, "Queen", "Bohemian Rhapsody");
        assert!(result.strict.is_none());
        assert!(result.display.is_none());
    }

    #[test]
    fn ignores_implausible_lengths() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"Queen","trackName":"Bohemian Rhapsody","trackTimeMillis":5000}
        ]}"#;
        assert!(parse_search(json, "Queen", "Bohemian Rhapsody").strict.is_none());
    }

    #[test]
    fn rejects_a_karaoke_version() {
        let json = r#"{"resultCount":2,"results":[
            {"artistName":"Pop Music Workshop","trackName":"Bangarang (Karaoke Version Originally Performed by Skrillex & Sirah)","trackTimeMillis":197741},
            {"artistName":"Skrillex","trackName":"Right In","trackTimeMillis":180400}
        ]}"#;
        let result = parse_search(json, "Skrillex ft. Sirah", "Bangarang");
        assert!(result.strict.is_none());
        assert!(result.display.is_none());
    }

    #[test]
    fn a_short_title_does_not_match_inside_a_longer_word() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"Adele","trackName":"Someone Like You","trackTimeMillis":285000}
        ]}"#;
        let result = parse_search(json, "Adele", "One");
        assert!(result.strict.is_none());
        assert!(result.display.is_none());
    }

    #[test]
    fn a_tribute_band_is_not_a_strict_match() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"Queen Tribute Band","trackName":"Bohemian Rhapsody","trackTimeMillis":355000}
        ]}"#;
        let result = parse_search(json, "Queen", "Bohemian Rhapsody");
        assert!(result.strict.is_none());
        assert_eq!(result.display.unwrap().artist, "Queen Tribute Band");
    }

    #[test]
    fn a_title_only_lookup_matches_a_dash_title() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"CeCe Peniston","trackName":"Finally - Radio Edit","trackTimeMillis":240000}
        ]}"#;
        let result = parse_search(json, "", "Finally - Radio Edit");
        assert_eq!(result.strict.unwrap().title, "Finally - Radio Edit");
        assert!(result.display.is_some());
    }

    #[test]
    fn a_featured_artist_can_still_be_strict() {
        let json = r#"{"resultCount":1,"results":[
            {"artistName":"Skrillex","trackName":"Bangarang","trackTimeMillis":210000}
        ]}"#;
        let result = parse_search(json, "Skrillex ft. Sirah", "Bangarang");
        assert_eq!(result.strict.unwrap().artist, "Skrillex");
    }

    #[test]
    fn normalizes_punctuation_for_cache_keys() {
        assert_eq!(
            cache_key("Gil Glaze", "Gravity (Extended Mix)"),
            cache_key("gil  glaze", "gravity extended mix")
        );
    }

    #[test]
    #[ignore]
    fn live_lookup_finds_a_known_song() {
        let result = lookup("Queen", "Bohemian Rhapsody").unwrap();
        let info = result.strict.unwrap();
        assert!((300..=420).contains(&info.duration_seconds));
        assert!(info.artwork_url.is_some());
    }
}
