//! radio.net station directory.
//!
//! The radio.net web player calls an unauthenticated JSON API; this module
//! queries the same endpoints and maps the results to [`StationSeed`]s. The
//! parser is pure and covered offline; the fetch runs on a worker thread so the
//! UI never waits for the network.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::models::StationSeed;

const SEARCH_URL: &str = "https://prod.radio-api.net/stations/search";
const LOCAL_URL: &str = "https://prod.radio-api.net/stations/local";
/// The genre index is server-rendered HTML; each `/genre/<slug>` link is a
/// genre the API can search for.
const GENRE_INDEX_URL: &str = "https://www.radio.net/genre";
const USER_AGENT: &str = "VibeRadio/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Bump when the sweep below changes materially, so libraries seeded by an
/// older sweep import the new stations once.
pub const DIRECTORY_VERSION: &str = "4";

/// A slow network must not keep the first-run import running for minutes.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(90);

/// A large but practical first library. The directory holds ~40k playable
/// stations; importing all of them takes minutes because deep result pages are
/// throttled, so this stops well short of the tail.
const MAX_SEEDS: usize = 30_000;

/// The API accepts up to 100 results per request.
const PAGE_SIZE: &str = "100";
const PAGE_STEP: usize = 100;

/// How far to page a single query. Ten pages covers ~24k unique stations in
/// about fifteen seconds; deeper pages are throttled hard by the API.
const QUERY_PAGE_CAP: usize = 1000;

/// Fallback queries, used when the genre index cannot be read, plus broad
/// terms that catch stations without a genre label.
const DEFAULT_QUERIES: &[&str] = &[
    "BBC",
    "jazz",
    "rock",
    "classical",
    "news",
    "pop",
    "dance",
    "country",
    "hip hop",
    "talk",
    "sports",
    "oldies",
    "indie",
    "electronic",
    "chillout",
    "metal",
    "reggae",
    "latin",
    "ambient",
    "blues",
    // Broad queries catch stations that do not carry a genre label.
    "a",
    "e",
    "i",
    "o",
    "u",
    "the",
];

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(rename = "totalCount")]
    total_count: Option<usize>,
    playables: Option<Vec<Playable>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Playable {
    id: Option<String>,
    name: Option<String>,
    country: Option<String>,
    city: Option<String>,
    genres: Option<Vec<String>>,
    topics: Option<Vec<String>>,
    #[serde(rename = "logo100x100")]
    logo: Option<String>,
    streams: Option<Vec<Stream>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stream {
    url: Option<String>,
    content_format: Option<String>,
    status: Option<String>,
}

/// Parse one search page and report the query's total result count.
fn parse_page(json: &str) -> (Vec<StationSeed>, usize) {
    let response = match serde_json::from_str::<Response>(json) {
        Ok(response) => response,
        Err(e) => {
            log::warn!("radio.net response could not be parsed: {e}");
            return (Vec::new(), 0);
        }
    };
    let mut seen = HashSet::new();
    let mut seeds = Vec::new();
    for playable in response.playables.unwrap_or_default() {
        if let Some(seed) = seed_from(&playable) {
            if seen.insert(seed.url.clone()) {
                seeds.push(seed);
            }
        }
    }
    let total = response.total_count.unwrap_or(seeds.len());
    (seeds, total)
}

/// Extract the unique genre slugs from the server-rendered genre index.
fn parse_genre_slugs(html: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut slugs = Vec::new();
    let mut rest = html;
    while let Some(position) = rest.find("/genre/") {
        rest = &rest[position + "/genre/".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(rest.len());
        let slug = &rest[..end];
        if !slug.is_empty() && slug.len() <= 64 && seen.insert(slug.to_string()) {
            slugs.push(slug.to_string());
        }
        rest = &rest[end..];
    }
    slugs
}

fn seed_from(playable: &Playable) -> Option<StationSeed> {
    let name = playable.name.as_deref()?.trim();
    if name.is_empty() {
        return None;
    }
    // Prefer a format every backend can play: rodio covers MP3/FLAC/Vorbis,
    // symphonia covers AAC-LC. OGG is ambiguous (it may be Opus), so it is the
    // last choice.
    let (_, stream) = playable
        .streams
        .as_deref()?
        .iter()
        .filter(|stream| {
            stream
                .status
                .as_deref()
                .map(|status| status.eq_ignore_ascii_case("VALID"))
                .unwrap_or(false)
                && stream
                    .url
                    .as_deref()
                    .map(is_http_audio_url)
                    .unwrap_or(false)
        })
        .filter_map(|stream| {
            let rank = stream.content_format.as_deref().and_then(format_rank)?;
            Some((rank, stream))
        })
        .min_by_key(|(rank, _)| *rank)?;
    let url = stream.url.as_deref()?.trim().to_string();

    // The sidebar groups by the exact genre string, so keep one genre there and
    // put the rest (and the topics) in tags.
    let genres = clean_list(playable.genres.as_deref());
    let topics = clean_list(playable.topics.as_deref());
    let genre = genres.first().cloned().or_else(|| topics.first().cloned());
    let mut tag_values: Vec<String> = genres.iter().skip(1).cloned().collect();
    tag_values.extend(topics.iter().cloned());
    let tags = (!tag_values.is_empty()).then(|| tag_values.join(", "));

    Some(StationSeed {
        source_id: clean(&playable.id),
        name: name.to_string(),
        url,
        genre,
        country: clean(&playable.country),
        city: clean(&playable.city),
        logo: clean(&playable.logo),
        website: None,
        tags,
        codec: clean(&stream.content_format),
        bitrate: None,
    })
}

fn clean_list(values: Option<&[String]>) -> Vec<String> {
    values
        .unwrap_or_default()
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn clean(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn format_rank(format: &str) -> Option<u8> {
    match format.to_ascii_uppercase().as_str() {
        "MP3" => Some(0),
        "AAC" | "AAC+" => Some(1),
        "FLAC" => Some(2),
        "OGG" | "VORBIS" => Some(3),
        _ => None,
    }
}

fn is_http_audio_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && !lower.contains(".m3u8")
}

/// Fetch a default station set from radio.net. Blocking; call from a worker.
pub fn fetch_stations() -> Result<Vec<StationSeed>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the radio.net client: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        let mut seeds = Vec::new();
        let mut seen = HashSet::new();
        let mut last_error: Option<String> = None;
        let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;

        // Nearby stations first, then a genre sweep. Each request, including the
        // body read, is bounded by the remaining total budget.
        let local_params = [("count", "50"), ("offset", "0")];
        match tokio::time::timeout(
            REQUEST_TIMEOUT,
            fetch_json(&client, LOCAL_URL, &local_params),
        )
        .await
        {
            Ok(Ok(text)) => extend(&mut seeds, &mut seen, parse_page(&text).0),
            Ok(Err(e)) => last_error = Some(e),
            Err(_) => last_error = Some("radio.net timed out".to_string()),
        }

        let queries = genre_queries(&client, &mut last_error).await;

        'queries: for query in &queries {
            if seeds.len() >= MAX_SEEDS {
                break;
            }
            let mut offset = 0usize;
            let mut total = usize::MAX;
            while offset < QUERY_PAGE_CAP && offset < total && seeds.len() < MAX_SEEDS {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    last_error = Some("radio.net timed out".to_string());
                    break 'queries;
                }
                let offset_text = offset.to_string();
                let params = [
                    ("query", query.as_str()),
                    ("count", PAGE_SIZE),
                    ("offset", offset_text.as_str()),
                ];
                let request = fetch_json(&client, SEARCH_URL, &params);
                match tokio::time::timeout(remaining.min(REQUEST_TIMEOUT), request).await {
                    Ok(Ok(text)) => {
                        let (page, page_total) = parse_page(&text);
                        total = page_total;
                        if page.is_empty() {
                            break;
                        }
                        extend(&mut seeds, &mut seen, page);
                        offset += PAGE_STEP;
                    }
                    Ok(Err(e)) => {
                        // A transport or API failure will not improve for the next query.
                        last_error = Some(e);
                        break 'queries;
                    }
                    Err(_) => {
                        last_error = Some("radio.net timed out".to_string());
                        break 'queries;
                    }
                }
            }
        }

        seeds.truncate(MAX_SEEDS);
        if seeds.is_empty() {
            Err(last_error.unwrap_or_else(|| "radio.net returned no playable stations".to_string()))
        } else {
            Ok(seeds)
        }
    })
}

/// Read the genre index and turn each slug into a search term. Falls back to
/// [`DEFAULT_QUERIES`] when the index cannot be read or parsed.
async fn genre_queries(client: &reqwest::Client, last_error: &mut Option<String>) -> Vec<String> {
    let mut queries: Vec<String> = Vec::new();
    match tokio::time::timeout(REQUEST_TIMEOUT, fetch_json(client, GENRE_INDEX_URL, &[])).await {
        Ok(Ok(html)) => {
            for slug in parse_genre_slugs(&html) {
                queries.push(slug.replace('-', " "));
            }
        }
        Ok(Err(e)) => *last_error = Some(e),
        Err(_) => *last_error = Some("radio.net timed out".to_string()),
    }
    if queries.is_empty() {
        queries.extend(DEFAULT_QUERIES.iter().map(|query| query.to_string()));
    } else {
        for query in DEFAULT_QUERIES {
            if !queries
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(query))
            {
                queries.push(query.to_string());
            }
        }
    }
    queries
}

async fn fetch_json(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
) -> Result<String, String> {
    let response = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("radio.net answered {}", response.status()));
    }
    response.text().await.map_err(|e| e.to_string())
}

fn extend(seeds: &mut Vec<StationSeed>, seen: &mut HashSet<String>, page: Vec<StationSeed>) {
    for seed in page {
        if seen.insert(seed.url.clone()) {
            seeds.push(seed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_maps_metadata_and_skips_unplayable_entries() {
        let json = r#"{
          "playables": [
            {
              "name": "Jazz FM",
              "country": "UK",
              "city": "London",
              "genres": ["Jazz"],
              "topics": ["Music"],
              "logo100x100": "https://img.example/jazz.png",
              "streams": [
                {"url": "https://a.example/jazz", "contentFormat": "MP3", "status": "VALID"}
              ]
            },
            {
              "name": "HLS only",
              "streams": [
                {"url": "https://a.example/live.m3u8", "contentFormat": "HLS", "status": "VALID"}
              ]
            },
            {
              "name": "Bad status",
              "streams": [
                {"url": "https://a.example/bad", "contentFormat": "MP3", "status": "INVALID"}
              ]
            },
            {
              "name": "Duplicate URL",
              "streams": [
                {"url": "https://a.example/jazz", "contentFormat": "AAC", "status": "VALID"}
              ]
            },
            {
              "name": "MP3 wins",
              "genres": ["Rock", "Indie"],
              "topics": ["Music"],
              "streams": [
                {"url": "https://a.example/prefer-aac", "contentFormat": "AAC", "status": "VALID"},
                {"url": "https://a.example/prefer-mp3", "contentFormat": "MP3", "status": "VALID"}
              ]
            },
            {
              "name": "M3U8 despite MP3",
              "streams": [
                {"url": "https://a.example/live.m3u8", "contentFormat": "MP3", "status": "VALID"}
              ]
            },
            {
              "name": "",
              "streams": [
                {"url": "https://a.example/empty", "contentFormat": "MP3", "status": "VALID"}
              ]
            }
          ]
        }"#;
        let seeds = parse_page(json).0;
        assert_eq!(seeds.len(), 2);
        let seed = &seeds[0];
        assert_eq!(seed.name, "Jazz FM");
        assert_eq!(seed.genre.as_deref(), Some("Jazz"));
        assert_eq!(seed.country.as_deref(), Some("UK"));
        assert_eq!(seed.city.as_deref(), Some("London"));
        assert_eq!(seed.logo.as_deref(), Some("https://img.example/jazz.png"));
        assert_eq!(seed.tags.as_deref(), Some("Music"));
        assert_eq!(seed.codec.as_deref(), Some("MP3"));

        // MP3 is preferred over AAC, and the extra genre goes to tags.
        let preferred = &seeds[1];
        assert_eq!(preferred.name, "MP3 wins");
        assert_eq!(preferred.url, "https://a.example/prefer-mp3");
        assert_eq!(preferred.genre.as_deref(), Some("Rock"));
        assert_eq!(preferred.tags.as_deref(), Some("Indie, Music"));
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetch_returns_playable_stations() {
        let seeds = fetch_stations().expect("radio.net must answer");
        println!(
            "radio.net returned {} playable stations, first id {:?}",
            seeds.len(),
            seeds.first().and_then(|seed| seed.source_id.as_deref())
        );
        assert!(seeds.len() > 5000, "expected a genre-wide library");
        assert!(seeds.iter().any(|seed| seed.source_id.is_some()));
        assert!(seeds
            .iter()
            .all(|seed| seed.url.to_ascii_lowercase().starts_with("http")));
        assert!(seeds.iter().all(|seed| !seed.name.trim().is_empty()));
    }

    #[test]
    fn genre_index_extracts_unique_slugs() {
        let html = r#"
          <a href="/genre/jazz">Jazz</a>
          <a href="/genre/rock">Rock</a>
          <a href="/genre/jazz?page=2">Jazz again</a>
          <a href="https://au.radio.net/genre/big-band">Big Band</a>
          <a href="/genre/">index</a>
          <script>const url = `/genre/${slug}`;</script>
        "#;
        assert_eq!(parse_genre_slugs(html), vec!["jazz", "rock", "big-band"]);
    }

    #[test]
    fn page_reports_total_count() {
        let json = r#"{"totalCount": 250, "playables": [
            {"name": "One", "streams": [
                {"url": "https://a.example/one", "contentFormat": "MP3", "status": "VALID"}
            ]}
        ]}"#;
        let (seeds, total) = parse_page(json);
        assert_eq!(seeds.len(), 1);
        assert_eq!(total, 250);
    }

    #[test]
    fn parser_returns_empty_for_garbage() {
        assert!(parse_page("not json").0.is_empty());
        assert!(parse_page("{}").0.is_empty());
        assert!(parse_page(r#"{"playables": null}"#).0.is_empty());
    }
}
