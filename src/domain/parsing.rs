//! Release-name parsing engine — the domain intelligence that turns raw
//! indexer titles into structured facts.
//!
//! Scope mirrors the *arr parsers: series/episode patterns (SxxExx, 1x09,
//! absolute numbering, date-based dailies, season packs), movie title+year,
//! resolution/source/codec/audio/language detection, editions, release
//! groups, proper/repack/real flags, and hardcoded-sub markers. Ordered
//! passes over a tokenized, separator-normalized string; every field is
//! evidence-based (regex match), never inferred.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseKind {
    Series,
    Movie,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Resolution {
    #[serde(rename = "2160p")]
    R2160,
    #[serde(rename = "1080p")]
    R1080,
    #[serde(rename = "720p")]
    R720,
    #[serde(rename = "576p")]
    R576,
    #[serde(rename = "480p")]
    R480,
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
}

impl Resolution {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::R2160 => "2160p",
            Self::R1080 => "1080p",
            Self::R720 => "720p",
            Self::R576 => "576p",
            Self::R480 => "480p",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    #[serde(rename = "bluray-remux")]
    BlurayRemux,
    Bluray,
    #[serde(rename = "web-dl")]
    Webdl,
    Webrip,
    Hdtv,
    Dvd,
    Sdtv,
    Cam,
    Telesync,
    #[default]
    Unknown,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BlurayRemux => "bluray-remux",
            Self::Bluray => "bluray",
            Self::Webdl => "web-dl",
            Self::Webrip => "webrip",
            Self::Hdtv => "hdtv",
            Self::Dvd => "dvd",
            Self::Sdtv => "sdtv",
            Self::Cam => "cam",
            Self::Telesync => "telesync",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QualityModifier {
    #[default]
    None,
    Remux,
    BrDisk,
    Proper,
    Repack,
    Real,
}

/// Audio tracks detected in the name (a release may advertise several).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub codec: String,          // dts-hd, truehd, atmos, dd+, ac3, aac, flac, mp3
    pub channels: Option<f32>,  // 5.1, 7.1, 2.0 ...
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedRelease {
    pub kind: ReleaseKind,
    /// Cleaned title (quality/group/episode tokens stripped).
    pub title: String,
    pub year: Option<i32>,
    // Series identity
    pub seasons: Vec<i64>,
    pub episodes: Vec<i64>,
    pub absolute_episodes: Vec<i64>,
    pub full_season: bool,
    /// Inclusive range for partial season packs (e.g. S01E01-E05).
    pub episode_range: Option<(i64, i64)>,
    pub daily: bool,
    pub air_date: Option<String>, // yyyy-MM-dd
    // Technical
    pub quality: Quality,
    pub languages: Vec<String>,
    pub edition: Option<String>,
    pub audio: Vec<AudioTrack>,
    pub release_group: Option<String>,
    // Modifiers
    pub proper: bool,
    pub repack: bool,
    pub real: bool,
    pub hardcoded_subs: bool,
    pub extended: bool,
    pub remastered: bool,
}

impl Default for ParsedRelease {
    fn default() -> Self {
        Self {
            kind: ReleaseKind::Unknown,
            title: String::new(),
            year: None,
            seasons: vec![],
            episodes: vec![],
            absolute_episodes: vec![],
            full_season: false,
            episode_range: None,
            daily: false,
            air_date: None,
            quality: Quality::default(),
            languages: vec![],
            edition: None,
            audio: vec![],
            release_group: None,
            proper: false,
            repack: false,
            real: false,
            hardcoded_subs: false,
            extended: false,
            remastered: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Quality {
    pub resolution: Resolution,
    pub source: Source,
    pub modifier: QualityModifier,
}

impl Quality {
    /// Human label used in file names and history rows.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        match self.modifier {
            QualityModifier::Remux => parts.push("Remux".to_string()),
            QualityModifier::BrDisk => parts.push("BR-DISK".to_string()),
            _ => {}
        }
        if self.source != Source::Unknown {
            parts.push(source_label(self.source));
        } else if matches!(self.modifier, QualityModifier::None) && self.resolution != Resolution::Unknown {
            // bare resolution like "1080p"
        }
        if self.resolution != Resolution::Unknown {
            parts.push(self.resolution.as_str().to_uppercase().replace('P', "p"));
        }
        if parts.is_empty() {
            return "Unknown".to_string();
        }
        parts.join(" ")
    }

    /// Rank used by upgrade decisioning: higher wins. Modifier bonuses are
    /// handled separately by scoring; this ranks technical quality only.
    pub fn rank(&self) -> i64 {
        let res = match self.resolution {
            Resolution::R2160 => 400,
            Resolution::R1080 => 300,
            Resolution::R720 => 200,
            Resolution::R576 => 100,
            Resolution::R480 => 90,
            Resolution::Unknown => 0,
        };
        let src = match self.source {
            Source::BlurayRemux => 60,
            Source::Bluray => 50,
            Source::Webdl => 40,
            Source::Webrip => 30,
            Source::Hdtv => 25,
            Source::Dvd => 20,
            Source::Sdtv => 10,
            Source::Cam | Source::Telesync => 1,
            Source::Unknown => 0,
        };
        res + src
    }
}

fn source_label(s: Source) -> String {
    match s {
        Source::BlurayRemux => "BluRay".to_string(),
        Source::Bluray => "BluRay".to_string(),
        Source::Webdl => "WEB-DL".to_string(),
        Source::Webrip => "WEBRip".to_string(),
        Source::Hdtv => "HDTV".to_string(),
        Source::Dvd => "DVD".to_string(),
        Source::Sdtv => "SDTV".to_string(),
        Source::Cam => "CAM".to_string(),
        Source::Telesync => "TELESYNC".to_string(),
        Source::Unknown => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Regexes (compiled once). Order matters in `parse`.
// ---------------------------------------------------------------------------

static RE_SXXEXX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\bS(\d{1,2})\s*E(\d{1,4})(?:[.\s_-]*E(\d{1,4}))?\b").unwrap());
static RE_SXX_EXX_SPACED: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\bS(\d{1,2})\s*-\s*E?(\d{1,4})\b").unwrap());
static RES_XYY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)(?:^|[\s.\-_])(\d{1,2})x(\d{1,3})(?:[\s.\-_]|$)").unwrap());
static RE_EPISODE_ONLY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(?:Episode[.\s_-]*|EP?)(\d{1,4})\b").unwrap());
static RE_SEASON_ONLY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(?:S|Season[.\s_-]*)(\d{1,2})\b").unwrap());
static RE_SEASON_COMPLETE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(complete|pack)\b").unwrap());
static RE_ABSOLUTE_BRACKET: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[\s*(\d{1,5})\s*\]").unwrap());
static RE_ABSOLUTE_PADDED: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:^|[.\s_\-\[])(0\d{2,4})(?:[.\s_\-\]]|$)").unwrap());
static RE_DAILY_DATE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(\d{4})[.](\d{2})[.](\d{2})\b").unwrap());
static RE_YEAR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(19\d{2}|20\d{2})\b").unwrap());
static RE_RESOLUTION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(2160|1080|720|576|480)p\b").unwrap());
static RE_SOURCE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\b(Blu-?ray|BDMux|BR-?Disk|WEB[-_. ]?DL|WEB-?Rip|HDRip|HDTV|PDTV|DSR|DVD|R5|PAL|NTSC|CAM|TELESYNC|TS|SDTV|WEB\b)",
    )
    .unwrap()
});
static RE_EDITION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)\b((?:Ultimate|Extended|Uncut|Unrated|Remastered|Directors?|Theatrical|Collectors?|Special|Definitive|Final)(?:[.\s_-]*(?:Cut|Edition|Version))?)\b"#,
    )
    .unwrap()
});
static RE_GROUP_TRAILING: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"-([A-Za-z0-9]+(?:_[A-Za-z0-9]+)?)$").unwrap());
static RE_GROUP_BRACKET: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:^|[\]}])\[([A-Za-z0-9_-]+)\]").unwrap());
static RE_LANGUAGES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\b(MULTI|MULTiSUBS|DUAL|DUBBED|(?:ENGLI?SH|CHINESE|JAPANESE|KOREAN|FRENCH|GERMAN|GERMAN DL|SPANISH|ITALIAN|RUSSIAN|HINDI|DANISH|DUTCH|SWEDISH|NORWEGIAN|FINNISH|JPN|ENG|FRE|GER|RUS)|VOSTFR|SUBFRENCH)\b",
    )
    .unwrap()
});
static RE_AUDIO: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\b(TrueHD[. ]?\d?(?:[. ]Atmos)?|DTS-?HD(?:[. ]MA)?|[. ]DTS(?:[. ]HD)?|DDP?[. ]?\d(?:[. ]\d)?|DD\+|EAC3|AC3|Dolby[. ]Digital(?:[. ]Plus)?|Atmos|FLAC|AAC(?:[. ]?\d(?:[. ]\d)?)?|MP3|Opus|LPCM|PCM)\b(?:[. ]?(\d\.\d))?",
    )
    .unwrap()
});
static RE_CODECS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(x264|x265|h\.?264|h\.?265|hevc|avc|xvid|divx|vp9|av1)\b").unwrap());

/// Parses an indexer/release title into structured facts.
///
/// Never panics; unknown fields stay default. The caller decides what to do
/// with `kind == Unknown`.
pub fn parse(input: &str) -> ParsedRelease {
    let mut out = ParsedRelease::default();
    let normalized = input.replace('_', ".");
    let lower = normalized.to_ascii_lowercase();

    // ---- modifiers -------------------------------------------------------
    if regex::Regex::new(r"(?i)\bproper\b").unwrap().is_match(&normalized) {
        out.proper = true;
        out.quality.modifier = QualityModifier::Proper;
    }
    if regex::Regex::new(r"(?i)\brepack\d*\b").unwrap().is_match(&normalized) {
        out.repack = true;
        out.quality.modifier = QualityModifier::Repack;
    }
    if (regex::Regex::new(r"(?i)\breal\b").unwrap().is_match(&normalized)
        && regex::Regex::new(r"(?i)\bproper\b").unwrap().is_match(&normalized))
        || normalized.contains("REAL")
    {
        out.real = true;
    }
    if regex::Regex::new(r"(?i)\bhardcoded\s?subs\b|\bhc\b").unwrap().is_match(&lower) {
        out.hardcoded_subs = true;
    }

    // ---- resolution / source / codec / audio ------------------------------
    if let Some(caps) = RE_RESOLUTION.captures(&normalized) {
        out.quality.resolution = match caps[1].parse::<u16>().unwrap_or(0) {
            2160 => Resolution::R2160,
            1080 => Resolution::R1080,
            720 => Resolution::R720,
            576 => Resolution::R576,
            480 => Resolution::R480,
            _ => Resolution::Unknown,
        };
    }
    if let Some(m) = RE_SOURCE.find(&normalized) {
        let t = m.as_str().to_ascii_lowercase();
        // REMUX must be checked before plain BluRay.
        if regex::Regex::new(r"(?i)remux").unwrap().is_match(&normalized)
            && t.contains("bdmux") || (t.contains("blu") && regex::Regex::new(r"(?i)remux").unwrap().is_match(&normalized))
        {
            out.quality.source = Source::BlurayRemux;
            out.quality.modifier = QualityModifier::Remux;
        } else if t.contains("br-disk") || t.contains("brdisk") {
            out.quality.source = Source::Bluray;
            out.quality.modifier = QualityModifier::BrDisk;
        } else if t.starts_with("web") && (t.contains("dl") || normalized.to_ascii_lowercase().contains("web-dl")) {
            out.quality.source = Source::Webdl;
        } else if t.contains("web") {
            // WEB alone or WEBRip/HDRip
            if t.contains("rip") || t == "hdrip" {
                out.quality.source = Source::Webrip;
            } else {
                out.quality.source = Source::Webdl;
            }
        } else if t.contains("hdtv") || t.contains("pdtv") || t.contains("dsr") {
            out.quality.source = Source::Hdtv;
        } else if t == "ts" || t.contains("telesync") {
            out.quality.source = Source::Telesync;
        } else if t.contains("cam") {
            out.quality.source = Source::Cam;
        } else if t.contains("dvd") || t == "r5" {
            out.quality.source = Source::Dvd;
        } else if t.contains("sdtv") {
            out.quality.source = Source::Sdtv;
        } else if t.contains("blu") {
            out.quality.source = Source::Bluray;
        }
    }
    for cap in RE_AUDIO.captures_iter(&normalized) {
        let codec = cap[1].replace(['.', '_'], " ").trim().to_string();
        let channels = cap.get(2).and_then(|c| c.as_str().parse().ok());
        let track = AudioTrack { codec, channels };
        if !out.audio.contains(&track) {
            out.audio.push(track);
        }
    }
    if !RE_CODECS.is_match(&normalized) {
        // no-op; codecs are informational today
    }

    // ---- languages --------------------------------------------------------
    for m in RE_LANGUAGES.find_iter(&normalized) {
        let l = canonical_language(m.as_str());
        if !out.languages.contains(&l) {
            out.languages.push(l);
        }
    }

    // ---- edition ----------------------------------------------------------
    if let Some(caps) = RE_EDITION.captures(&normalized) {
        let e = caps[1]
            .replace(['.', '_', '\''], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let low = e.to_ascii_lowercase();
        if low.contains("extended") {
            out.extended = true;
        }
        if low.contains("remastered") {
            out.remastered = true;
        }
        out.edition = Some(e);
    }

    // ---- dates (daily) ----------------------------------------------------
    if let Some(caps) = RE_DAILY_DATE.captures(&normalized) {
        if let Some(d) = NaiveDate::from_ymd_opt(
            caps[1].parse().unwrap_or(0),
            caps[2].parse().unwrap_or(0),
            caps[3].parse().unwrap_or(0),
        ) {
            out.daily = true;
            out.air_date = Some(d.format("%Y-%m-%d").to_string());
        }
    }

    // ---- series vs movie structure ----------------------------------------
    let mut series_matched = false;

    if let Some(caps) = RE_SXXEXX.captures(&normalized) {
        series_matched = true;
        out.kind = ReleaseKind::Series;
        let season: i64 = caps[1].parse().unwrap_or(0);
        let ep: i64 = caps[2].parse().unwrap_or(0);
        out.seasons.push(season);
        out.episodes.push(ep);
        if let Some(e2) = caps.get(3).and_then(|c| c.as_str().parse().ok()) {
            // E01-E05 style multi-episode
            if e2 > ep {
                out.episode_range = Some((ep, e2));
                for n in (ep + 1)..=e2 {
                    out.episodes.push(n);
                }
            }
        }
    } else if let Some(caps) = RE_SXX_EXX_SPACED.captures(&normalized) {
        series_matched = true;
        out.kind = ReleaseKind::Series;
        out.seasons.push(caps[1].parse().unwrap_or(0));
        out.episodes.push(caps[2].parse().unwrap_or(0));
    } else if let Some(caps) = RES_XYY.captures(&normalized) {
        series_matched = true;
        out.kind = ReleaseKind::Series;
        out.seasons.push(caps[1].parse().unwrap_or(0));
        out.episodes.push(caps[2].parse().unwrap_or(0));
    }

    if !series_matched {
        if let Some(caps) = RE_EPISODE_ONLY.captures(&normalized) {
            if let Ok(ep) = caps[1].parse::<i64>() {
                out.kind = ReleaseKind::Series;
                out.episodes.push(ep);
                series_matched = true;
                // "Show.S01.E10" — season marker before the episode token.
                if let Some(sm) = RE_SEASON_ONLY.captures(&normalized[..caps.get(0).unwrap().start()]) {
                    if let Ok(s) = sm[1].parse::<i64>() {
                        out.seasons.push(s);
                    }
                }
            }
        }
    }

    // Absolute numbering (anime): [1234] or (1234) bracket numbers.
    let mut abs_push = |n: i64, out: &mut ParsedRelease, series_matched: &mut bool| {
        if (1..=999999).contains(&n) && !out.absolute_episodes.contains(&n) {
            out.absolute_episodes.push(n);
            if out.kind == ReleaseKind::Unknown {
                out.kind = ReleaseKind::Series;
                *series_matched = true;
            }
        }
    };
    for caps in RE_ABSOLUTE_BRACKET.captures_iter(&normalized) {
        if let Ok(n) = caps[1].parse::<i64>() {
            abs_push(n, &mut out, &mut series_matched);
        }
    }
    // Padded unbracketed numbers (0048) — anime convention.
    for caps in RE_ABSOLUTE_PADDED.captures_iter(&normalized) {
        if let Ok(n) = caps[1].parse::<i64>() {
            abs_push(n, &mut out, &mut series_matched);
        }
    }

    // Season pack: "S01" or "Season 01 Complete".
    if out.episodes.is_empty() {
        if let Some(caps) = RE_SEASON_ONLY.captures(&normalized) {
            if let Ok(s) = caps[1].parse::<i64>() {
                out.kind = ReleaseKind::Series;
                out.seasons.push(s);
                out.full_season = true;
                series_matched = true;
            }
        }
    }

    // Movie fallback: a year present and not daily/series.
    if !series_matched {
        out.kind = if out.daily { ReleaseKind::Series } else { ReleaseKind::Movie };
    }

    // Year (movies mainly; series titles may include one).
    if let Some(caps) = RE_YEAR.captures(&normalized) {
        let y: i32 = caps[1].parse().unwrap_or(0);
        if (1900..2100).contains(&y) {
            out.year = Some(y);
        }
    }

    // ---- group ------------------------------------------------------------
    // Prefer trailing -GROUP after stripping extension-like suffixes.
    let without_ext = strip_extension(&normalized);
    if let Some(caps) = RE_GROUP_TRAILING.captures(without_ext.trim_end_matches('.')) {
        out.release_group = Some(caps[1].to_string());
    } else if let Some(caps) = RE_GROUP_BRACKET.captures(without_ext) {
        out.release_group = Some(caps[1].to_string());
    }

    // ---- title cleanup ------------------------------------------------------
    out.title = clean_title(&normalized);

    out
}

fn strip_extension(name: &str) -> &str {
    const EXTS: [&str; 8] = [".mkv", ".mp4", ".avi", ".ts", ".m2ts", ".iso", ".nzb", ".torrent"];
    let lower = name.to_ascii_lowercase();
    let mut cut = name.len();
    for e in EXTS {
        if lower.ends_with(e) {
            cut = cut.min(name.len() - e.len());
        }
    }
    &name[..cut]
}

/// Strips technical tokens from the front-half of the name to recover the
/// human title: everything before the first episode/date marker, minus
/// resolution/source/audio tokens and separators.
fn clean_title(normalized: &str) -> String {
    // Cut at the first structural marker.
    let mut cut = normalized.len();
    for re in [&*RE_SXXEXX, &*RES_XYY, &*RE_DAILY_DATE] {
        if let Some(m) = re.find(normalized) {
            cut = cut.min(m.start());
        }
    }
    if let Some(m) = RE_SXX_EXX_SPACED.find(normalized) {
        cut = cut.min(m.start());
    }
    if let Some(m) = RE_SEASON_ONLY.find(normalized) {
        cut = cut.min(m.start());
    }
    // Movie: cut after a standalone year when it is followed by more tokens.
    if let Some(m) = RE_YEAR.find(normalized) {
        let after = &normalized[m.end()..];
        let starts_new_token = after.starts_with('.') || after.starts_with(' ') || after.is_empty();
        if starts_new_token && m.start() > 0 {
            cut = cut.min(m.end());
        }
    }
    let head = &normalized[..cut];
    let mut tokens: Vec<&str> = Vec::new();
    'outer: for tok in head.split(['.', ' ', '_', '-']) {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        let lt = t.to_ascii_lowercase();
        // Drop pure years inside the title head ("Show.2011.S01..." keeps year? drop).
        if lt.parse::<i32>().map(|y| (1900..2100).contains(&y)).unwrap_or(false) {
            continue;
        }
        for banned in [
            "2160p", "1080p", "720p", "576p", "480p", "bluray", "blu-ray", "bdmux", "br-disk",
            "web-dl", "webdl", "webrip", "web", "hdtv", "pdtv", "dsr", "dvdrip", "dvd", "sdtv",
            "cam", "telesync", "proper", "repack", "real", "remux", "hdr", "dv", "dolby",
            "truehd", "atmos", "dts-hd", "dts", "ddp", "dd", "eac3", "ac3", "aac", "flac",
            "mp3", "opus", "lpcm", "pcm", "x264", "x265", "h264", "h.264", "h265", "h.265",
            "hevc", "avc", "xvid", "divx", "vp9", "av1", "multi", "dual", "dubbed", "hc",
        ] {
            if lt == banned {
                continue 'outer;
            }
        }
        if RE_LANGUAGES.is_match(t) {
            continue;
        }
        tokens.push(t);
    }
    let title = tokens.join(" ");
    title.trim().to_string()
}

fn canonical_language(raw: &str) -> String {
    let l = raw.to_ascii_lowercase();
    match l.as_str() {
        "multi" => "multi".into(),
        "multisubs" => "multisubs".into(),
        "dual" => "dual".into(),
        "dubbed" => "dubbed".into(),
        "vostfr" => "vostfr".into(),
        "subfrench" => "subfrench".into(),
        "german dl" | "ger" => "german".into(),
        "eng" | "english" => "english".into(),
        "jpn" | "japanese" => "japanese".into(),
        "fre" | "french" => "french".into(),
        other => other.into(),
    }
}
