//! System font fallback for scripts the bundled Inter and JetBrains Mono
//! cannot render. The station library carries names in Chinese, Japanese,
//! Korean, Arabic, Hebrew, Indic and other scripts, and egui draws a tofu box
//! for every missing glyph, so system faces are appended to both font families.
//!
//! The faces are found at runtime rather than bundled: a full set of script
//! fonts is much larger than the application, and every desktop already ships
//! them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A font face loaded from a system file, with its index inside a collection.
pub struct FallbackFont {
    pub script: &'static str,
    pub data: Vec<u8>,
    pub index: u32,
    pub path: PathBuf,
}

/// One bit per script, so only the faces the library needs are loaded.
pub const ARABIC: u32 = 1 << 0;
pub const HEBREW: u32 = 1 << 1;
pub const THAI: u32 = 1 << 2;
pub const DEVANAGARI: u32 = 1 << 3;
pub const BENGALI: u32 = 1 << 4;
pub const GURMUKHI: u32 = 1 << 5;
pub const GUJARATI: u32 = 1 << 6;
pub const TAMIL: u32 = 1 << 7;
pub const TELUGU: u32 = 1 << 8;
pub const KANNADA: u32 = 1 << 9;
pub const MALAYALAM: u32 = 1 << 10;
pub const SINHALA: u32 = 1 << 11;
pub const LAO: u32 = 1 << 12;
pub const MYANMAR: u32 = 1 << 13;
pub const GEORGIAN: u32 = 1 << 14;
pub const ARMENIAN: u32 = 1 << 15;
pub const ETHIOPIC: u32 = 1 << 16;
pub const KHMER: u32 = 1 << 17;
/// Han, Kana and Hangul are all covered by one CJK collection.
pub const CJK: u32 = 1 << 18;

/// Scripts worth loading, with one sample character each. The CJK face is
/// handled separately because it is a collection and needs a face preference.
const SCRIPT_FACES: &[(&str, &str, char, u32)] = &[
    ("Arabic", "Noto Sans Arabic", '\u{0627}', ARABIC),
    ("Hebrew", "Noto Sans Hebrew", '\u{05d0}', HEBREW),
    ("Thai", "Noto Sans Thai", '\u{0e01}', THAI),
    ("Devanagari", "Noto Sans Devanagari", '\u{0915}', DEVANAGARI),
    ("Bengali", "Noto Sans Bengali", '\u{0995}', BENGALI),
    ("Gurmukhi", "Noto Sans Gurmukhi", '\u{0a15}', GURMUKHI),
    ("Gujarati", "Noto Sans Gujarati", '\u{0a95}', GUJARATI),
    ("Tamil", "Noto Sans Tamil", '\u{0b95}', TAMIL),
    ("Telugu", "Noto Sans Telugu", '\u{0c15}', TELUGU),
    ("Kannada", "Noto Sans Kannada", '\u{0c95}', KANNADA),
    ("Malayalam", "Noto Sans Malayalam", '\u{0d15}', MALAYALAM),
    ("Sinhala", "Noto Sans Sinhala", '\u{0d9a}', SINHALA),
    ("Lao", "Noto Sans Lao", '\u{0e81}', LAO),
    ("Myanmar", "Noto Sans Myanmar", '\u{1000}', MYANMAR),
    ("Georgian", "Noto Sans Georgian", '\u{10d0}', GEORGIAN),
    ("Armenian", "Noto Sans Armenian", '\u{0531}', ARMENIAN),
    ("Ethiopic", "Noto Sans Ethiopic", '\u{1200}', ETHIOPIC),
    ("Khmer", "Noto Sans Khmer", '\u{1780}', KHMER),
];

/// Which scripts occur in these names. Inter already covers Latin, Greek and
/// Cyrillic, so only the other ranges are reported.
pub fn scripts_in<'a>(names: impl Iterator<Item = &'a str>) -> u32 {
    let mut mask = 0;
    for name in names {
        for character in name.chars() {
            mask |= match character as u32 {
                0x0590..=0x05ff | 0xfb1d..=0xfb4f => HEBREW,
                0x0600..=0x06ff
                | 0x0750..=0x077f
                | 0x08a0..=0x08ff
                | 0xfb50..=0xfdff
                | 0xfe70..=0xfeff => ARABIC,
                0x0900..=0x097f => DEVANAGARI,
                0x0980..=0x09ff => BENGALI,
                0x0a00..=0x0a7f => GURMUKHI,
                0x0a80..=0x0aff => GUJARATI,
                0x0b80..=0x0bff => TAMIL,
                0x0c00..=0x0c7f => TELUGU,
                0x0c80..=0x0cff => KANNADA,
                0x0d00..=0x0d7f => MALAYALAM,
                0x0d80..=0x0dff => SINHALA,
                0x0e00..=0x0e7f => THAI,
                0x0e80..=0x0eff => LAO,
                0x1000..=0x109f => MYANMAR,
                0x10a0..=0x10ff | 0x2d00..=0x2d2f => GEORGIAN,
                0x1200..=0x137f => ETHIOPIC,
                0x1780..=0x17ff => KHMER,
                0x0530..=0x058f => ARMENIAN,
                0x1100..=0x11ff
                | 0x3040..=0x30ff
                | 0x3400..=0x4dbf
                | 0x4e00..=0x9fff
                | 0xac00..=0xd7af
                | 0xf900..=0xfaff => CJK,
                _ => 0,
            };
        }
    }
    mask
}

/// Find system faces for the scripts present in `mask`. Missing scripts are
/// skipped; the UI then keeps its bundled fonts for them.
pub fn fallbacks(mask: u32) -> Vec<FallbackFont> {
    let mut faces = Vec::new();
    let mut seen = HashSet::new();
    if mask & CJK != 0 {
        if let Some(cjk) = find_cjk() {
            seen.insert((cjk.path.clone(), cjk.index));
            faces.push(cjk);
        }
    }
    for (script, family, sample, bit) in SCRIPT_FACES {
        if mask & bit == 0 {
            continue;
        }
        let Some(face) = find_script(script, family, *sample) else {
            continue;
        };
        if seen.insert((face.path.clone(), face.index)) {
            faces.push(face);
        }
    }
    faces
}

/// Find a CJK face, preferring Simplified Chinese. CJK fonts are usually
/// collections, so the face is scored by family name.
fn find_cjk() -> Option<FallbackFont> {
    let mut best: Option<(u8, PathBuf, u32)> = None;
    for (path, indices) in candidate_faces() {
        if best.as_ref().map(|(score, _, _)| *score >= 9).unwrap_or(false) {
            break;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let faces = ttf_parser::fonts_in_collection(&data).unwrap_or(1);
        let indices = if indices.is_empty() {
            (0..faces).collect::<Vec<_>>()
        } else {
            indices
        };
        for index in indices {
            if index >= faces {
                continue;
            }
            let Ok(face) = ttf_parser::Face::parse(&data, index) else {
                continue;
            };
            if !renders(&face, '\u{4e2d}') {
                continue;
            }
            let score = family_score(&face);
            if best
                .as_ref()
                .map(|(best, _, _)| score > *best)
                .unwrap_or(true)
            {
                best = Some((score, path.clone(), index));
            }
        }
    }
    let (_, path, index) = best?;
    let data = std::fs::read(&path).ok()?;
    Some(FallbackFont {
        script: "CJK",
        data,
        index,
        path,
    })
}

/// Find a face for one script: try its Noto family first, then let fontconfig
/// match the sample character, and finally scan likely files elsewhere.
fn find_script(script: &'static str, family: &str, sample: char) -> Option<FallbackFont> {
    #[cfg(target_os = "linux")]
    {
        for pattern in [family.to_string(), format!(":charset={:04x}", sample as u32)] {
            if let Some((path, index)) = fontconfig_match(&pattern) {
                if let Some(face) = load_face(script, &path, index, sample) {
                    return Some(face);
                }
            }
        }
    }
    let needle = family.to_ascii_lowercase().replace(' ', "");
    for path in scanned_files() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if name.contains(&needle) {
            if let Some(face) = load_face(script, &path, 0, sample) {
                return Some(face);
            }
        }
    }
    None
}

fn load_face(
    script: &'static str,
    path: &Path,
    index: u32,
    sample: char,
) -> Option<FallbackFont> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, index).ok()?;
    if !renders(&face, sample) {
        return None;
    }
    Some(FallbackFont {
        script,
        data,
        index,
        path: path.to_path_buf(),
    })
}

/// Whether the face has a glyph for `sample`.
fn renders(face: &ttf_parser::Face<'_>, sample: char) -> bool {
    face.glyph_index(sample).is_some()
}

/// Prefer a Simplified-Chinese face, then any CJK face. The score only needs
/// to be ordinal.
fn family_score(face: &ttf_parser::Face<'_>) -> u8 {
    let mut score = 1;
    for name in face.names() {
        if name.name_id != ttf_parser::name_id::FAMILY
            && name.name_id != ttf_parser::name_id::TYPOGRAPHIC_FAMILY
        {
            continue;
        }
        let Some(text) = name.to_string() else {
            continue;
        };
        let family = text.to_ascii_lowercase();
        let mut candidate = 1;
        if family.contains("cjk") || family.contains("source han") {
            candidate += 4;
        }
        if family.contains(" sc")
            || family.ends_with("sc")
            || family.contains("simplified")
            || family.contains("gb")
        {
            candidate += 4;
        }
        if family.contains("noto")
            || family.contains("pingfang")
            || family.contains("yahei")
            || family.contains("wenquanyi")
            || family.contains("droid sans fallback")
        {
            candidate += 2;
        }
        score = score.max(candidate);
    }
    score
}

/// Faces to try for CJK, grouped by file. An empty index list means "every
/// face in the file". Fontconfig supplies exact faces on Linux; elsewhere the
/// font directories are scanned.
fn candidate_faces() -> Vec<(PathBuf, Vec<u32>)> {
    #[cfg(target_os = "linux")]
    {
        let faces = fontconfig_faces();
        if !faces.is_empty() {
            return faces;
        }
    }
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    for dir in font_dirs() {
        collect(&dir, 0, &mut visited, &mut files);
    }
    files.sort();
    files.dedup();
    files.into_iter().map(|path| (path, Vec::new())).collect()
}

/// Font files found by walking the usual directories. Used when fontconfig is
/// unavailable and as the per-script fallback.
fn scanned_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut visited = HashSet::new();
    for dir in font_dirs() {
        collect(&dir, 0, &mut visited, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

/// Ask fontconfig which installed faces cover Chinese. This is the only
/// reliable way to find fonts on distributions that keep them outside the
/// usual directories (NixOS stores every package separately).
#[cfg(target_os = "linux")]
fn fontconfig_faces() -> Vec<(PathBuf, Vec<u32>)> {
    let Ok(output) = std::process::Command::new("fc-list")
        .args([":lang=zh", "file", "index", "family"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut faces: Vec<(PathBuf, Vec<u32>)> = Vec::new();
    for line in text.lines() {
        // `/path/to/font.ttc: Family:index=2`
        let Some((head, index_text)) = line.rsplit_once(":index=") else {
            continue;
        };
        let Ok(index) = index_text.trim().parse::<u32>() else {
            continue;
        };
        let Some((path, _family)) = head.rsplit_once(": ") else {
            continue;
        };
        let path = Path::new(path);
        if !is_font_file(path) {
            continue;
        }
        // Fontconfig stores named variable-font instances in the upper bits;
        // ttf-parser wants the collection face index in the lower 16.
        let index = index & 0xFFFF;
        let path = path.to_path_buf();
        if let Some((_, indices)) = faces.iter_mut().find(|(existing, _)| *existing == path) {
            if !indices.contains(&index) {
                indices.push(index);
            }
        } else {
            faces.push((path, vec![index]));
        }
    }
    // Try the obvious CJK packages first so a hit does not read every font.
    faces.sort_by_key(|(path, _)| {
        let name = path.to_string_lossy().to_ascii_lowercase();
        if name.contains("cjk") {
            0
        } else if name.contains("wqy") || name.contains("droid") {
            1
        } else {
            2
        }
    });
    faces
}

/// One fontconfig lookup, returning a file and collection face index.
#[cfg(target_os = "linux")]
fn fontconfig_match(pattern: &str) -> Option<(PathBuf, u32)> {
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\t%{index}\n", pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?;
    let (path, index) = line.split_once('\t')?;
    let path = Path::new(path.trim());
    if !is_font_file(path) {
        return None;
    }
    let index = index.trim().parse::<u32>().ok()? & 0xFFFF;
    Some((path.to_path_buf(), index))
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    #[cfg(target_os = "linux")]
    {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        dirs.push(PathBuf::from("/run/current-system/sw/share/fonts"));
        dirs.push(PathBuf::from("/nix/var/nix/profiles/default/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join(".local/share/fonts"));
            dirs.push(Path::new(&home).join(".fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join("Library/Fonts"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(windir) = std::env::var_os("WINDIR") {
            dirs.push(Path::new(&windir).join("Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(Path::new(&local).join("Microsoft/Windows/Fonts"));
        }
    }
    if let Some(dir) = dirs::font_dir() {
        dirs.push(dir);
    }
    dirs.retain(|dir| dir.is_dir());
    dirs
}

fn collect(dir: &Path, depth: usize, visited: &mut HashSet<PathBuf>, files: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    // NixOS exposes packages through symlinked directories; follow them, but
    // canonicalise so a link cycle cannot recurse forever.
    let Ok(canonical) = dir.canonicalize() else {
        return;
    };
    if !visited.insert(canonical) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let path = entry.path();
        if metadata.is_dir() {
            collect(&path, depth + 1, visited, files);
        } else if metadata.is_file() && is_candidate_name(&path) {
            files.push(path);
        }
    }
}

fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            let extension = extension.to_ascii_lowercase();
            matches!(extension.as_str(), "ttf" | "otf" | "ttc" | "otc")
        })
        .unwrap_or(false)
}

fn is_candidate_name(path: &Path) -> bool {
    if !is_font_file(path) {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    [
        "cjk",
        "notosanssc",
        "notosanstc",
        "notosansjp",
        "notosanskr",
        "notosansarabic",
        "notosanshebrew",
        "notosansthai",
        "notosansdevanagari",
        "notosansbengali",
        "notosansgurmukhi",
        "notosansgujarati",
        "notosanstamil",
        "notosanstelugu",
        "notosanskannada",
        "notosansmalayalam",
        "notosanssinhala",
        "notosanslao",
        "notosansmyanmar",
        "notosansgeorgian",
        "notosansarmenian",
        "notosansethiopic",
        "notosanskhmer",
        "sourcehansans",
        "sourcehanserif",
        "wqy",
        "droidsansfallback",
        "msyh",
        "simsun",
        "simhei",
        "malgun",
        "pingfang",
        "hiragino",
        "stheiti",
        "applegothic",
        "segoeui",
        "arial",
        "tahoma",
        "mangal",
        "nirmala",
        "leelawui",
        "geeza",
        "david",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}
