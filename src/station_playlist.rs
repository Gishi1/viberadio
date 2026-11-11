//! Station playlist scraping.
//!
//! radio.net's details API reports each station's homepage. Many stations
//! publish a server-rendered "recently played" page; this module follows the
//! homepage link and parses the common `<li><time>…<strong>Artist</strong> –
//! Title` shape. It is deliberately conservative: a page that does not look
//! like a track list yields no entries rather than guesses.

use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Utc};
use serde::Deserialize;

const DETAILS_URL: &str = "https://prod.radio-api.net/stations/details";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) VibeRadio/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    /// ISO timestamp or the page's display time, when it carries one.
    pub at: Option<String>,
    pub artist: String,
    pub title: String,
    pub image: Option<String>,
}

/// Parse the timestamp forms providers publish: RFC 3339, a naive
/// `YYYY-MM-DD HH:MM:SS`, unix seconds or milliseconds, or a clock time such
/// as `12:21 am`. Clock-only values are local; a value more than a few
/// minutes in the future is treated as yesterday's midnight wrap.
pub fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return Some(time.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M:%S%.f%z"] {
        if let Ok(time) = DateTime::parse_from_str(text, format) {
            return Some(time.with_timezone(&Utc));
        }
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(Utc.from_utc_datetime(&time));
        }
    }
    if let Ok(value) = text.parse::<i64>() {
        let seconds = if value > 1_000_000_000_000 {
            value / 1000
        } else {
            value
        };
        return Utc.timestamp_opt(seconds, 0).single();
    }
    for format in ["%H:%M", "%I:%M %p", "%I:%M%p"] {
        if let Ok(time) = chrono::NaiveTime::parse_from_str(text, format) {
            let local = Local::now().date_naive().and_time(time);
            let utc = Local
                .from_local_datetime(&local)
                .single()?
                .with_timezone(&Utc);
            return Some(if utc > Utc::now() + chrono::Duration::minutes(5) {
                utc - chrono::Duration::days(1)
            } else {
                utc
            });
        }
    }
    None
}

/// Whether a playlist entry is newer than the last ICY title change. Scraped
/// pages can lag by several songs; without this check a stale entry would cut
/// a recording that the live ICY title already split correctly.
pub fn entry_is_newer_than(entry: &PlaylistEntry, icy_changed: Option<DateTime<Utc>>) -> bool {
    let Some(changed) = icy_changed else {
        return true;
    };
    match entry.at.as_deref().and_then(parse_time) {
        // Provider timestamps are often rounded or imported in batches, so a
        // small margin is not enough to prove the entry is a new song.
        Some(at) => at > changed + chrono::Duration::seconds(60),
        // No usable timestamp: trust the page only when the ICY title has
        // been static long enough that a playlist split is the only source.
        None => Utc::now() - changed >= chrono::Duration::seconds(120),
    }
}

#[derive(Debug, Clone, Default)]
pub struct StationPlaylist {
    pub homepage: Option<String>,
    pub page_url: Option<String>,
    /// Platform that supplied the list, when it was a provider adapter.
    pub provider: Option<&'static str>,
    /// The station's published next track, where the platform exposes one.
    pub next: Option<PlaylistEntry>,
    pub entries: Vec<PlaylistEntry>,
}

/// Parse a server-rendered track list. Accepts list items that contain a
/// `<strong>` artist followed by a title, and at least a time or an image so
/// navigation lists are ignored.
pub fn parse_recent_spins(html: &str) -> Vec<PlaylistEntry> {
    let lower = html.to_ascii_lowercase();
    let mut entries = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("<li") {
        let start = cursor + relative;
        // `<link` is not a list item.
        let is_item = match lower[start + 3..].chars().next() {
            Some('>') => true,
            Some(character) if character.is_whitespace() => true,
            _ => false,
        };
        if !is_item {
            cursor = start + 3;
            continue;
        }
        let Some(relative_end) = lower[start..].find("</li>") else {
            break;
        };
        let end = start + relative_end;
        let block = &html[start..end];
        if let Some(entry) = parse_item(block) {
            let duplicate = entries.iter().any(|existing: &PlaylistEntry| {
                existing.artist == entry.artist && existing.title == entry.title
            });
            if !duplicate {
                entries.push(entry);
                if entries.len() >= MAX_ENTRIES {
                    break;
                }
            }
        }
        cursor = end + "</li>".len();
    }
    entries
}

fn parse_item(block: &str) -> Option<PlaylistEntry> {
    let lower = block.to_ascii_lowercase();
    let artist = element_text(block, "strong")?;
    let close = lower.find("</strong>")? + "</strong>".len();
    let title = strip_tags(&block[close..]);
    let title = title.trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, '–' | '—' | '-' | ':' | '|')
    });
    if title.is_empty() {
        return None;
    }
    // Navigation lists rarely carry a time or an image; a track list usually
    // carries at least one.
    let at = attribute(block, "time", "datetime").or_else(|| element_text(block, "time"));
    let image = attribute(block, "img", "src");
    if at.is_none() && image.is_none() {
        return None;
    }
    Some(PlaylistEntry {
        at,
        artist,
        title: title.to_string(),
        image,
    })
}

/// Find the most likely playlist link on a homepage.
pub fn find_playlist_url(html: &str, base: &str) -> Option<String> {
    let base = url::Url::parse(base).ok()?;
    let lower = html.to_ascii_lowercase();
    let mut best: Option<(u8, String)> = None;
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("href=") {
        let position = cursor + relative;
        let Some((href, consumed)) = attribute_value(&html[position + 5..]) else {
            cursor = position + 5;
            continue;
        };
        cursor = position + 5 + consumed;
        let Ok(joined) = base.join(&href) else {
            continue;
        };
        if !matches!(joined.scheme(), "http" | "https") {
            continue;
        }
        let text = anchor_text(html, position)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let haystack = format!("{} {text}", href.to_ascii_lowercase());
        let score = link_score(&haystack);
        // Only trust an explicit playlist/recent-spins link; generic "music"
        // pages are usually not track lists and parsing them yields nothing.
        if score >= 3 && best.as_ref().map(|(best, _)| score > *best).unwrap_or(true) {
            best = Some((score, joined.to_string()));
        }
    }
    best.map(|(_, url)| url)
}

fn link_score(text: &str) -> u8 {
    if text.contains("playlist") {
        5
    } else if text.contains("recent") || text.contains("spins") {
        4
    } else if text.contains("last-played")
        || text.contains("last_played")
        || text.contains("last played")
    {
        3
    } else if text.contains("songs") {
        2
    } else if text.contains("music") {
        1
    } else {
        0
    }
}

/// Read the quoted value of an `href=` attribute.
fn attribute_value(after: &str) -> Option<(String, usize)> {
    let after = after.trim_start();
    let mut chars = after.char_indices();
    let (_, first) = chars.next()?;
    if first == '"' || first == '\'' {
        let rest = &after[1..];
        let end = rest.find(first)?;
        Some((rest[..end].to_string(), end + 2))
    } else {
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(after.len());
        Some((after[..end].to_string(), end))
    }
}

fn anchor_text(html: &str, href_position: usize) -> Option<String> {
    let open_end = html[href_position..].find('>')? + href_position + 1;
    let close = html[open_end..].to_ascii_lowercase().find("</a>")? + open_end;
    Some(strip_tags(&html[open_end..close]))
}

fn element_text(block: &str, tag: &str) -> Option<String> {
    let lower = block.to_ascii_lowercase();
    let open = format!("<{tag}");
    let start = lower.find(&open)?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = lower[open_end..].find(&close)? + open_end;
    let text = strip_tags(&block[open_end..end]);
    (!text.is_empty()).then_some(text)
}

fn attribute(block: &str, tag: &str, name: &str) -> Option<String> {
    let lower = block.to_ascii_lowercase();
    let start = lower.find(&format!("<{tag}"))?;
    let open_end = lower[start..].find('>')? + start;
    let tag_text = &block[start..open_end];
    let lower_tag = &lower[start..open_end];
    let key = format!("{name}=");
    let position = lower_tag.find(&key)? + key.len();
    attribute_value(&tag_text[position..]).map(|(value, _)| value)
}

fn strip_tags(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    decode_entities(&collapsed)
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        rest = &rest[position..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(character) = decoded {
            out.push(character);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Provider adapters
//
// A stream URL usually identifies its platform. These adapters cover the
// platforms that publish a track list; the homepage scraper below remains the
// fallback for everything else.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LautSong {
    #[serde(rename = "type")]
    kind: Option<String>,
    title: Option<String>,
    started_at: Option<String>,
    artist: Option<LautArtist>,
}

#[derive(Debug, Deserialize)]
struct LautArtist {
    name: Option<String>,
}

fn parse_lautfm(json: &str) -> Vec<PlaylistEntry> {
    let songs: Vec<LautSong> = serde_json::from_str(json).unwrap_or_default();
    songs
        .into_iter()
        .filter(|song| song.kind.as_deref() == Some("song"))
        .filter_map(|song| {
            let title = song.title?.trim().to_string();
            if title.is_empty() {
                return None;
            }
            Some(PlaylistEntry {
                at: song.started_at,
                artist: song
                    .artist
                    .and_then(|artist| artist.name)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                title,
                image: None,
            })
        })
        .collect()
}

#[derive(Debug, Deserialize, Default)]
struct AzuraResponse {
    now_playing: Option<AzuraWrapper>,
    playing_next: Option<AzuraWrapper>,
    song_history: Option<Vec<AzuraHistory>>,
}

#[derive(Debug, Deserialize)]
struct AzuraWrapper {
    song: Option<AzuraSong>,
}

#[derive(Debug, Deserialize)]
struct AzuraSong {
    text: Option<String>,
    artist: Option<String>,
    title: Option<String>,
    art: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AzuraHistory {
    song: Option<AzuraSong>,
    played_at: Option<i64>,
}

fn parse_azuracast(json: &str) -> Option<(Option<PlaylistEntry>, Vec<PlaylistEntry>)> {
    let response: AzuraResponse = serde_json::from_str(json).ok()?;
    if response.now_playing.is_none() && response.song_history.is_none() {
        return None;
    }
    let next = response
        .playing_next
        .as_ref()
        .and_then(|wrapper| wrapper.song.as_ref())
        .and_then(azura_entry);
    let entries = response
        .song_history
        .unwrap_or_default()
        .into_iter()
        .filter_map(|history| {
            let mut entry = azura_entry(history.song.as_ref()?)?;
            if let Some(timestamp) = history.played_at {
                entry.at = chrono::Local
                    .timestamp_opt(timestamp, 0)
                    .single()
                    .map(|time| time.format("%H:%M").to_string());
            }
            Some(entry)
        })
        .collect();
    Some((next, entries))
}

fn azura_entry(song: &AzuraSong) -> Option<PlaylistEntry> {
    let mut artist = song.artist.clone().unwrap_or_default().trim().to_string();
    let mut title = song.title.clone().unwrap_or_default().trim().to_string();
    if title.is_empty() {
        let (parsed_artist, parsed_title) = split_artist_title(song.text.as_deref().unwrap_or(""));
        if artist.is_empty() {
            artist = parsed_artist;
        }
        title = parsed_title;
    }
    if title.is_empty() {
        return None;
    }
    Some(PlaylistEntry {
        at: None,
        artist,
        title,
        image: song.art.clone(),
    })
}

#[derive(Debug, Deserialize, Default)]
struct RadioCoStatus {
    history: Option<Vec<RadioCoTrack>>,
}

#[derive(Debug, Deserialize)]
struct RadioCoTrack {
    title: Option<String>,
}

fn parse_radioco(json: &str) -> Vec<PlaylistEntry> {
    let status: RadioCoStatus = serde_json::from_str(json).unwrap_or_default();
    status
        .history
        .unwrap_or_default()
        .into_iter()
        .filter_map(|track| {
            let (artist, title) = split_artist_title(track.title.as_deref()?);
            if title.is_empty() {
                return None;
            }
            Some(PlaylistEntry {
                at: None,
                artist,
                title,
                image: None,
            })
        })
        .collect()
}

#[derive(Debug, Deserialize, Default)]
struct Live365Station {
    #[serde(rename = "last-played")]
    last_played: Option<Vec<Live365Track>>,
}

#[derive(Debug, Deserialize)]
struct Live365Track {
    title: Option<String>,
    artist: Option<String>,
    art: Option<String>,
    start: Option<String>,
}

fn parse_live365(json: &str) -> Vec<PlaylistEntry> {
    let station: Live365Station = serde_json::from_str(json).unwrap_or_default();
    station
        .last_played
        .unwrap_or_default()
        .into_iter()
        .filter_map(|track| {
            let title = track.title?.trim().to_string();
            if title.is_empty() {
                return None;
            }
            Some(PlaylistEntry {
                at: track.start,
                artist: track.artist.unwrap_or_default().trim().to_string(),
                title,
                image: track.art,
            })
        })
        .collect()
}

fn split_artist_title(text: &str) -> (String, String) {
    match text.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_string(), title.trim().to_string()),
        None => (String::new(), text.trim().to_string()),
    }
}

fn azuracast_target(url: &url::Url) -> Option<(String, String)> {
    let base = match url.port() {
        Some(port) => format!("{}://{}:{port}", url.scheme(), url.host_str()?),
        None => format!("{}://{}", url.scheme(), url.host_str()?),
    };
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    let position = segments
        .iter()
        .position(|segment| *segment == "listen" || *segment == "radio")?;
    let station = segments.get(position + 1)?;
    (!station.is_empty()).then(|| (base, station.to_string()))
}

async fn lautfm_playlist(client: &reqwest::Client, url: &url::Url) -> Option<StationPlaylist> {
    let slug = url
        .path_segments()?
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
        .or_else(|| url.host_str()?.split('.').next().map(str::to_string))?;
    let api = format!("https://api.laut.fm/station/{slug}/last_songs");
    let text = get(client, &api, &[]).await.ok()?;
    let entries = parse_lautfm(&text);
    if entries.is_empty() {
        return None;
    }
    Some(StationPlaylist {
        homepage: Some(format!("https://laut.fm/{slug}")),
        page_url: Some(api),
        provider: Some("laut.fm"),
        next: None,
        entries,
    })
}

async fn azuracast_playlist(
    client: &reqwest::Client,
    base: &str,
    station: &str,
) -> Option<StationPlaylist> {
    let api = format!("{base}/api/nowplaying/{station}");
    let text = get(client, &api, &[]).await.ok()?;
    let (next, entries) = parse_azuracast(&text)?;
    if entries.is_empty() && next.is_none() {
        return None;
    }
    Some(StationPlaylist {
        homepage: None,
        page_url: Some(api),
        provider: Some("AzuraCast"),
        next,
        entries,
    })
}

/// Zeno.fm stations publish the current track over a public SSE endpoint.
fn zenofm_mount(url: &url::Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "zeno.fm" && !host.ends_with(".zeno.fm") {
        return None;
    }
    url.path_segments()?
        .find(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// Parse one `data:` line from Zeno's event stream.
fn parse_zeno_event(line: &str) -> Option<PlaylistEntry> {
    let json = line.strip_prefix("data:")?.trim();
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let title = value.get("streamTitle")?.as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    let (artist, song) = split_artist_title(title);
    if song.is_empty() {
        return None;
    }
    Some(PlaylistEntry {
        at: None,
        artist,
        title: song,
        image: None,
    })
}

async fn zenofm_playlist(client: &reqwest::Client, mount: &str) -> Option<StationPlaylist> {
    let api = format!("https://api.zeno.fm/mounts/metadata/subscribe/{mount}");
    let mut response = client
        .get(&api)
        .header("Accept", "text/event-stream")
        .timeout(Duration::from_secs(6))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut buffer = String::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        for line in buffer.lines() {
            if let Some(entry) = parse_zeno_event(line) {
                return Some(StationPlaylist {
                    homepage: None,
                    page_url: Some(api),
                    provider: Some("Zeno.fm"),
                    next: None,
                    entries: vec![entry],
                });
            }
        }
        // Keep only the tail in case a line was split across chunks.
        if buffer.len() > 16 * 1024 {
            buffer = buffer[buffer.len() - 4096..].to_string();
        }
    }
    None
}

#[derive(Debug, PartialEq, Eq)]
enum RadioKingTarget {
    Slug(String),
    Id(String),
}

/// RadioKing's widget API is keyed by slug; numeric stream URLs carry only an
/// id, which the station API can resolve to a slug.
fn radioking_target(url: &url::Url) -> Option<RadioKingTarget> {
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    if host == "listen.radioking.com" && segments.first() == Some(&"radio") {
        return segments
            .get(1)
            .map(|id| RadioKingTarget::Id(id.to_string()));
    }
    if host == "play.radioking.io" {
        return segments
            .first()
            .map(|slug| RadioKingTarget::Slug(slug.to_string()));
    }
    if host == "radioking.com" || host == "www.radioking.com" {
        if segments.first() == Some(&"play") {
            return segments
                .get(1)
                .map(|slug| RadioKingTarget::Slug(slug.to_string()));
        }
    }
    None
}

fn parse_radioking_track(json: &str) -> Option<PlaylistEntry> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let title = value.get("title")?.as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    let artist = value
        .get("artist")
        .and_then(|artist| artist.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let cover = value
        .get("cover")
        .or_else(|| value.get("cover_url"))
        .and_then(|cover| cover.as_str())
        .map(str::to_string);
    Some(PlaylistEntry {
        at: value
            .get("started_at")
            .and_then(|at| at.as_str())
            .map(str::to_string),
        artist,
        title: title.to_string(),
        image: cover,
    })
}

fn parse_radioking_next(json: &str) -> Option<PlaylistEntry> {
    let values: Vec<serde_json::Value> = serde_json::from_str(json).ok()?;
    let value = values.first()?;
    parse_radioking_track(&value.to_string())
}

async fn radioking_playlist(
    client: &reqwest::Client,
    target: RadioKingTarget,
) -> Option<StationPlaylist> {
    let slug = match target {
        RadioKingTarget::Slug(slug) => slug,
        RadioKingTarget::Id(id) => {
            let api = format!("https://www.radioking.com/api/radio/{id}");
            let text = get(client, &api, &[]).await.ok()?;
            let value: serde_json::Value = serde_json::from_str(&text).ok()?;
            value.get("data")?.get("slug")?.as_str()?.to_string()
        }
    };
    let current_api = format!("https://api.radioking.io/widget/radio/{slug}/track/current");
    let current = get(client, &current_api, &[]).await.ok()?;
    let entry = parse_radioking_track(&current)?;
    let next_api = format!("https://api.radioking.io/widget/radio/{slug}/track/next");
    let next = get(client, &next_api, &[])
        .await
        .ok()
        .and_then(|text| parse_radioking_next(&text));
    Some(StationPlaylist {
        homepage: None,
        page_url: Some(current_api),
        provider: Some("RadioKing"),
        next,
        entries: vec![entry],
    })
}

async fn radioco_playlist(client: &reqwest::Client, url: &url::Url) -> Option<StationPlaylist> {
    let id = url
        .path_segments()?
        .find(|segment| segment.starts_with('s'))?;
    let api = format!("https://public.radio.co/stations/{id}/status");
    let text = get(client, &api, &[]).await.ok()?;
    let entries = parse_radioco(&text);
    if entries.is_empty() {
        return None;
    }
    Some(StationPlaylist {
        homepage: None,
        page_url: Some(api),
        provider: Some("Radio.co"),
        next: None,
        entries,
    })
}

async fn live365_playlist(client: &reqwest::Client, url: &url::Url) -> Option<StationPlaylist> {
    let id = url.path_segments()?.find(|segment| !segment.is_empty())?;
    let api = format!("https://api.live365.com/station/{id}");
    let text = get(client, &api, &[]).await.ok()?;
    let entries = parse_live365(&text);
    if entries.is_empty() {
        return None;
    }
    Some(StationPlaylist {
        homepage: None,
        page_url: Some(api),
        provider: Some("Live365"),
        next: None,
        entries,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Details {
    homepage_url: Option<String>,
}

/// Fetch a station's playlist, preferring its platform API and falling back to
/// the station homepage. A radio.net id resolves its homepage through the
/// details API; a radio-browser id is a UUID, so its stored homepage is used
/// instead.
pub fn fetch_playlist(
    source_id: &str,
    stream_url: &str,
    homepage: Option<&str>,
) -> Result<StationPlaylist, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the playlist client: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;

        if let Ok(url) = url::Url::parse(stream_url) {
            let host = url.host_str().unwrap_or("").to_ascii_lowercase();
            let provider = if host.ends_with("laut.fm") {
                lautfm_playlist(&client, &url).await
            } else if host.ends_with("radio.co") {
                radioco_playlist(&client, &url).await
            } else if host.ends_with("live365.com") {
                live365_playlist(&client, &url).await
            } else if let Some(mount) = zenofm_mount(&url) {
                zenofm_playlist(&client, &mount).await
            } else if let Some(target) = radioking_target(&url) {
                radioking_playlist(&client, target).await
            } else if let Some((base, station)) = azuracast_target(&url) {
                azuracast_playlist(&client, &base, &station).await
            } else {
                None
            };
            if let Some(playlist) = provider {
                return Ok(playlist);
            }
        }

        let homepage = match homepage
            .map(str::trim)
            .filter(|homepage| !homepage.is_empty())
        {
            Some(homepage) => homepage.to_string(),
            None if uuid::Uuid::parse_str(source_id).is_err() => {
                let details = get(&client, DETAILS_URL, &[("stationIds", source_id)]).await?;
                let stations: Vec<Details> = serde_json::from_str(&details)
                    .map_err(|e| format!("radio.net details: {e}"))?;
                stations
                    .into_iter()
                    .find_map(|station| station.homepage_url)
                    .ok_or_else(|| "the station has no homepage".to_string())?
            }
            None => return Ok(StationPlaylist::default()),
        };
        let homepage_html = get(&client, &homepage, &[]).await?;

        let mut page_url = find_playlist_url(&homepage_html, &homepage);
        let mut entries = match &page_url {
            Some(url) => {
                let html = get(&client, url, &[]).await?;
                parse_recent_spins(&html)
            }
            None => parse_recent_spins(&homepage_html),
        };

        if entries.is_empty() {
            for candidate in [
                "/listen/playlist/",
                "/playlist/",
                "/playlist",
                "/recently-played",
            ] {
                let Ok(base) = url::Url::parse(&homepage) else {
                    break;
                };
                let Ok(url) = base.join(candidate) else {
                    continue;
                };
                let Ok(html) = get(&client, url.as_str(), &[]).await else {
                    continue;
                };
                let found = parse_recent_spins(&html);
                if !found.is_empty() {
                    page_url = Some(url.to_string());
                    entries = found;
                    break;
                }
            }
        }

        Ok(StationPlaylist {
            homepage: Some(homepage),
            page_url,
            provider: None,
            next: None,
            entries,
        })
    })
}

async fn get(
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
        return Err(format!("{url} answered {}", response.status()));
    }
    response.text().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lautfm_songs_and_skips_jingles() {
        let json = r#"[
          {"type":"song","title":"Seit a poa Tog","started_at":"2026-11-10 06:49:07 +0100",
           "artist":{"name":"folkshilfe"}},
          {"type":"jingle","title":"Werbepause","started_at":"2026-11-10 06:48:32 +0100"},
          {"type":"song","title":"Scheena Dog","started_at":"2026-11-10 06:52:13 +0100",
           "artist":{"name":"LaBrassBanda"}}
        ]"#;
        let entries = parse_lautfm(json);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].artist, "folkshilfe");
        assert_eq!(entries[0].title, "Seit a poa Tog");
    }

    #[test]
    fn parses_azuracast_next_and_history() {
        let json = r#"{
          "now_playing": {"song": {"text": "Bryan Martin - We Ride"}},
          "playing_next": {"song": {"text": "Pointer Sisters - Automatic"}},
          "song_history": [
            {"song": {"text": "Bryan Martin - We Ride"}, "played_at": 1762750000},
            {"song": {"artist": "Clifford Jordan", "title": "Spellbound"}, "played_at": 1762749900}
          ]
        }"#;
        let (next, entries) = parse_azuracast(json).expect("azura response");
        assert_eq!(
            next.as_ref().map(|entry| entry.title.as_str()),
            Some("Automatic")
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].artist, "Clifford Jordan");
        assert_eq!(entries[1].title, "Spellbound");
    }

    #[test]
    fn parses_radioco_history() {
        let json = r#"{"history": [
          {"title": "Gil Mantera's Party Dream - Elmo's Wish"},
          {"title": "Peaking Lights - Amazing & Wonderful"}
        ]}"#;
        let entries = parse_radioco(json);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].artist, "Gil Mantera's Party Dream");
        assert_eq!(entries[0].title, "Elmo's Wish");
    }

    #[test]
    fn parses_live365_last_played() {
        let json = r#"{"last-played": [
          {"title": "Another Reason", "artist": "Gayla James", "start": "2026-11-10 05:49:01 +00:00"},
          {"title": "Brighter Day", "artist": "SHAUN NIX", "start": "2026-11-10 05:52:59 +00:00"}
        ]}"#;
        let entries = parse_live365(json);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].artist, "Gayla James");
        assert_eq!(entries[0].title, "Another Reason");
    }

    #[test]
    fn derives_zenofm_mount() {
        let cases = [
            ("https://stream.zeno.fm/650r0dwas9zvv", Some("650r0dwas9zvv")),
            ("https://stream-176.zeno.fm/abc?zt=1", Some("abc")),
            ("https://example.com/abc", None),
        ];
        for (url, expected) in cases {
            let parsed = url::Url::parse(url).unwrap();
            assert_eq!(zenofm_mount(&parsed).as_deref(), expected);
        }
    }

    #[test]
    fn parses_zeno_sse_title() {
        let line = r#"data:{"mount":"650r0dwas9zvv","streamTitle":"Jamaica Suk - Steel Rose"}"#;
        let entry = parse_zeno_event(line).expect("zeno entry");
        assert_eq!(entry.artist, "Jamaica Suk");
        assert_eq!(entry.title, "Steel Rose");
        assert!(parse_zeno_event("event:ping").is_none());
        assert!(parse_zeno_event(r#"data:{"streamTitle":""}"#).is_none());
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_zenofm_playlist() {
        let playlist = fetch_playlist("x", "https://stream.zeno.fm/650r0dwas9zvv", None)
            .expect("zeno playlist");
        assert_eq!(playlist.provider, Some("Zeno.fm"));
        assert!(!playlist.entries.is_empty());
    }

    #[test]
    fn derives_radioking_targets() {
        let cases = [
            (
                "https://www.radioking.com/play/popradio1",
                Some(RadioKingTarget::Slug("popradio1".into())),
            ),
            (
                "https://www.radioking.com/play/levillagepop/244664",
                Some(RadioKingTarget::Slug("levillagepop".into())),
            ),
            (
                "https://play.radioking.io/rock957",
                Some(RadioKingTarget::Slug("rock957".into())),
            ),
            (
                "https://listen.radioking.com/radio/427889/stream/481499",
                Some(RadioKingTarget::Id("427889".into())),
            ),
            ("https://example.com/play/x", None),
        ];
        for (url, expected) in cases {
            let parsed = url::Url::parse(url).unwrap();
            assert_eq!(radioking_target(&parsed), expected);
        }
    }

    #[test]
    fn parses_radioking_current_and_next() {
        let current = r#"{"artist":"Kings Of Leon","title":"Sex On Fire","started_at":"2026-11-10T10:26:28+0000","cover":"https://image/cover.jpg"}"#;
        let entry = parse_radioking_track(current).expect("current");
        assert_eq!(entry.artist, "Kings Of Leon");
        assert_eq!(entry.title, "Sex On Fire");
        assert!(parse_time(entry.at.as_deref().unwrap()).is_some());

        let next = r#"[{"artist":"Chappel Roan","title":"Good Luck, Babe!","started_at":"2026-11-10T10:29:50.729Z","cover_url":"https://image/next.jpg"}]"#;
        let entry = parse_radioking_next(next).expect("next");
        assert_eq!(entry.artist, "Chappel Roan");
        assert_eq!(entry.title, "Good Luck, Babe!");
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_radioking_playlist() {
        let playlist = fetch_playlist(
            "x",
            "https://listen.radioking.com/radio/427889/stream/481499",
            None,
        )
        .expect("radioking playlist");
        assert_eq!(playlist.provider, Some("RadioKing"));
        assert!(!playlist.entries.is_empty());
    }

    #[test]
    fn parses_provider_timestamps() {
        let rfc = parse_time("2026-11-10T00:21:18-08:00").expect("rfc3339");
        assert_eq!(rfc.to_rfc3339(), "2026-11-10T08:21:18+00:00");
        assert_eq!(parse_time("1762762878").expect("unix").timestamp(), 1762762878);
        assert!(parse_time("12:21 am").is_some());
        assert!(parse_time("2026-11-10T10:26:28+0000").is_some());
        assert!(parse_time("not a time").is_none());
        assert!(parse_time("").is_none());
    }

    #[test]
    fn stale_playlist_entries_do_not_beat_a_newer_icy_title() {
        let entry = PlaylistEntry {
            at: Some("2026-11-10T00:21:18-08:00".into()),
            artist: "Steve Aoki".into(),
            title: "Gravity".into(),
            image: None,
        };
        let newer_icy = DateTime::parse_from_rfc3339("2026-11-10T00:23:44-08:00")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!entry_is_newer_than(&entry, Some(newer_icy)));

        let older_icy = DateTime::parse_from_rfc3339("2026-11-10T00:20:00-08:00")
            .unwrap()
            .with_timezone(&Utc);
        assert!(entry_is_newer_than(&entry, Some(older_icy)));

        // A rounded or batched entry can be tens of seconds newer without
        // being a different song.
        let close_icy = DateTime::parse_from_rfc3339("2026-11-10T00:20:50-08:00")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!entry_is_newer_than(&entry, Some(close_icy)));
        assert!(entry_is_newer_than(&entry, None));

        let undated = PlaylistEntry {
            at: None,
            ..entry.clone()
        };
        let long_ago = Utc::now() - chrono::Duration::minutes(5);
        assert!(entry_is_newer_than(&undated, Some(long_ago)));
        let just_now = Utc::now() - chrono::Duration::seconds(5);
        assert!(!entry_is_newer_than(&undated, Some(just_now)));
    }

    #[test]
    fn derives_azuracast_target_from_stream_urls() {
        let cases = [
            (
                "https://azura.example.org/listen/shortcode/radio.mp3",
                Some(("https://azura.example.org", "shortcode")),
            ),
            (
                "http://host.example:8000/radio/8120/stream.mp3",
                Some(("http://host.example:8000", "8120")),
            ),
            ("https://stream.example.org/live.mp3", None),
        ];
        for (url, expected) in cases {
            let parsed = url::Url::parse(url).unwrap();
            assert_eq!(
                azuracast_target(&parsed),
                expected.map(|(base, station)| (base.to_string(), station.to_string()))
            );
        }
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_lautfm_playlist() {
        let playlist = fetch_playlist("boarisch", "https://boarisch.stream.laut.fm/boarisch", None)
            .expect("laut.fm playlist");
        assert_eq!(playlist.provider, Some("laut.fm"));
        assert!(!playlist.entries.is_empty());
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_azuracast_playlist() {
        let playlist = fetch_playlist(
            "blitzradio",
            "https://azura.blitzradio.club/listen/blitzradio/radio.mp3",
            None,
        )
        .expect("AzuraCast playlist");
        assert_eq!(playlist.provider, Some("AzuraCast"));
        assert!(playlist.next.is_some() || !playlist.entries.is_empty());
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_radioco_playlist() {
        let playlist = fetch_playlist(
            "s0d090ee43",
            "https://streaming.radio.co/s0d090ee43/listen",
            None,
        )
        .expect("Radio.co playlist");
        assert_eq!(playlist.provider, Some("Radio.co"));
        assert!(!playlist.entries.is_empty());
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_live365_playlist() {
        let playlist = fetch_playlist("a01677", "https://streaming.live365.com/a01677", None)
            .expect("Live365 playlist");
        assert_eq!(playlist.provider, Some("Live365"));
        assert!(!playlist.entries.is_empty());
    }

    #[test]
    fn parses_a_recent_spins_list() {
        let html = r#"
          <ul>
            <li><time datetime="2026-11-09T16:15:58-08:00">4:15 pm</time>
              <img src="https://img.example/a.jpg" alt="">
              <strong>Two Friends</strong> – fire escape (kastra remix)</li>
            <li><time datetime="2026-11-09T16:11:13-08:00">4:11 pm</time>
              <img src="https://img.example/b.jpg" alt="">
              <strong>Axwell /\ Ingrosso</strong> – Dreamer &amp; More</li>
            <li><a href="/shows/">All Shows</a></li>
          </ul>
        "#;
        let entries = parse_recent_spins(html);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].artist, "Two Friends");
        assert_eq!(entries[0].title, "fire escape (kastra remix)");
        assert_eq!(
            entries[0].image.as_deref(),
            Some("https://img.example/a.jpg")
        );
        assert_eq!(entries[1].artist, "Axwell /\\ Ingrosso");
        assert_eq!(entries[1].title, "Dreamer & More");
    }

    #[test]
    fn ignores_navigation_without_a_time_or_image() {
        let html = r#"<ul><li><strong>Playlist</strong> – All shows</li></ul>"#;
        assert!(parse_recent_spins(html).is_empty());
    }

    #[test]
    fn finds_the_playlist_link() {
        let html = r#"
          <nav><a href="/shows/">All Shows</a></nav>
          <a href="/listen/playlist/">Playlist</a>
        "#;
        assert_eq!(
            find_playlist_url(html, "https://www.example.org/").as_deref(),
            Some("https://www.example.org/listen/playlist/")
        );
    }

    #[test]
    fn resolves_a_cross_site_playlist_link() {
        let html = r#"<a href="https://player.example.net/listen/playlist/">Recent spins</a>"#;
        assert_eq!(
            find_playlist_url(html, "https://www.example.org/").as_deref(),
            Some("https://player.example.net/listen/playlist/")
        );
    }

    #[test]
    #[ignore = "needs outbound network"]
    fn fetches_knhc_playlist() {
        let playlist =
            fetch_playlist("knhc", "http://streams.c895.org/live", None).expect("knhc playlist");
        println!(
            "homepage {:?}, page {:?}, {} entries, first {:?}",
            playlist.homepage,
            playlist.page_url,
            playlist.entries.len(),
            playlist.entries.first()
        );
        assert!(!playlist.entries.is_empty());
        assert!(playlist.entries.iter().all(|entry| !entry.title.is_empty()));
    }
}
