//! radio-browser.info station directory.
//!
//! The community directory is free and unauthenticated. It returns one stream
//! URL per station, so HLS-only and broken entries are filtered out here; the
//! player cannot decode HLS yet.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::models::StationSeed;

const TOP_URL: &str = "https://de1.api.radio-browser.info/json/stations/topclick";
const USER_AGENT: &str = "VibeRadio/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STATIONS: usize = 5000;

#[derive(Debug, Deserialize)]
struct ApiStation {
    stationuuid: Option<String>,
    name: Option<String>,
    url: Option<String>,
    url_resolved: Option<String>,
    homepage: Option<String>,
    favicon: Option<String>,
    tags: Option<String>,
    country: Option<String>,
    countrycode: Option<String>,
    state: Option<String>,
    language: Option<String>,
    codec: Option<String>,
    bitrate: Option<i64>,
    hls: Option<i32>,
    lastcheckok: Option<i32>,
}

/// Parse a radio-browser response into playable station seeds.
pub fn parse_stations(json: &str) -> Vec<StationSeed> {
    let stations: Vec<ApiStation> = match serde_json::from_str(json) {
        Ok(stations) => stations,
        Err(e) => {
            log::warn!("radio-browser response could not be parsed: {e}");
            return Vec::new();
        }
    };
    let mut seen = HashSet::new();
    let mut seeds = Vec::new();
    for station in stations {
        if station.hls.unwrap_or(0) != 0 || station.lastcheckok.unwrap_or(1) != 1 {
            continue;
        }
        let url = station
            .url_resolved
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .or(station.url.as_deref())
            .map(str::trim)
            .filter(|url| is_playable_url(url));
        let Some(url) = url else {
            continue;
        };
        let name = station.name.as_deref().map(str::trim).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let tags = split_tags(station.tags.as_deref());
        let genre = tags.first().cloned();
        let mut tag_values = tags;
        if let Some(state) = clean(station.state.as_deref()) {
            tag_values.push(state);
        }
        if let Some(language) = clean(station.language.as_deref()) {
            tag_values.push(language);
        }
        let tags = (!tag_values.is_empty()).then(|| tag_values.join(", "));
        let country =
            clean(station.country.as_deref()).or_else(|| clean(station.countrycode.as_deref()));
        if !seen.insert(url.to_string()) {
            continue;
        }
        seeds.push(StationSeed {
            source_id: clean(station.stationuuid.as_deref()),
            name: name.to_string(),
            url: url.to_string(),
            genre,
            country,
            city: clean(station.state.as_deref()),
            logo: clean(station.favicon.as_deref()),
            website: clean(station.homepage.as_deref()),
            tags,
            codec: clean(station.codec.as_deref()),
            bitrate: station
                .bitrate
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0),
        });
        if seeds.len() >= MAX_STATIONS {
            break;
        }
    }
    seeds
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn split_tags(tags: Option<&str>) -> Vec<String> {
    tags.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_playable_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !lower.contains(".m3u8")
        && url.len() <= 2048
}

/// Fetch the most-clicked stations. Blocking; call from a worker.
pub fn fetch_stations() -> Result<Vec<StationSeed>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the radio-browser client: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        let url = format!("{TOP_URL}/{MAX_STATIONS}");
        let response = client
            .get(&url)
            .query(&[("hidebroken", "true")])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("radio-browser answered {}", response.status()));
        }
        let text = response.text().await.map_err(|e| e.to_string())?;
        let seeds = parse_stations(&text);
        if seeds.is_empty() {
            Err("radio-browser returned no playable stations".to_string())
        } else {
            Ok(seeds)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_maps_fields_and_skips_hls_broken_and_duplicate_entries() {
        let json = r#"[
          {
            "stationuuid": "uuid-1",
            "name": "Jazz FM",
            "url": "http://stream.example/jazz.mp3",
            "url_resolved": "https://stream.example/jazz.mp3",
            "homepage": "https://jazz.example",
            "favicon": "https://jazz.example/icon.png",
            "tags": "jazz,smooth",
            "country": "United Kingdom",
            "countrycode": "GB",
            "state": "London",
            "language": "english",
            "codec": "MP3",
            "bitrate": 128,
            "hls": 0,
            "lastcheckok": 1
          },
          {
            "stationuuid": "uuid-hls",
            "name": "HLS only",
            "url": "https://stream.example/live.m3u8",
            "url_resolved": "https://stream.example/live.m3u8",
            "hls": 1,
            "lastcheckok": 1
          },
          {
            "stationuuid": "uuid-broken",
            "name": "Broken",
            "url": "https://stream.example/broken.mp3",
            "hls": 0,
            "lastcheckok": 0
          },
          {
            "stationuuid": "uuid-1",
            "name": "Duplicate",
            "url": "https://stream.example/jazz.mp3",
            "hls": 0,
            "lastcheckok": 1
          }
        ]"#;
        let seeds = parse_stations(json);
        assert_eq!(seeds.len(), 1);
        let seed = &seeds[0];
        assert_eq!(seed.source_id.as_deref(), Some("uuid-1"));
        assert_eq!(seed.name, "Jazz FM");
        assert_eq!(seed.url, "https://stream.example/jazz.mp3");
        assert_eq!(seed.genre.as_deref(), Some("jazz"));
        assert_eq!(seed.country.as_deref(), Some("United Kingdom"));
        assert_eq!(seed.city.as_deref(), Some("London"));
        assert_eq!(seed.website.as_deref(), Some("https://jazz.example"));
        assert_eq!(seed.codec.as_deref(), Some("MP3"));
        assert_eq!(seed.bitrate, Some(128));
        assert!(seed
            .tags
            .as_deref()
            .is_some_and(|tags| tags.contains("smooth") && tags.contains("english")));
    }

    #[test]
    fn parser_returns_empty_for_garbage() {
        assert!(parse_stations("not json").is_empty());
        assert!(parse_stations("[]").is_empty());
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetch_returns_playable_stations() {
        let seeds = fetch_stations().expect("radio-browser must answer");
        println!("radio-browser returned {} stations", seeds.len());
        assert!(seeds.len() > 1000);
        assert!(seeds.iter().all(|seed| !seed.url.contains(".m3u8")));
        assert!(seeds
            .iter()
            .all(|seed| seed.website.is_some() || seed.source_id.is_some()));
    }
}
