//! Custom-format engine (*arr CF parity): declarative conditions evaluated
//! against a `ParsedRelease`, score summation across enabled formats, and
//! score-aware cutoff decisioning.
//!
//! A `CustomFormat` is a named bundle of AND-ed conditions plus an integer
//! score. Indexer/grab pipelines compute `total_score` over a profile's
//! formats and feed it through `apply_cutoff` / `decide_with_score` so that
//! releases already at or above a configured score ceiling are not grabbed
//! again (reason `"cutoff_met"`).

use crate::domain::parsing::{ParsedRelease, Quality, QualityModifier, Resolution, Source};
use crate::domain::quality::{decide_upgrade, UpgradeDecision};
use serde::{Deserialize, Serialize};

/// The set of condition kinds a custom format can express. Serialized in
/// snake_case (`"quality_modifier"`, `"title_contains"`); unknown kinds are
/// rejected at deserialization rather than silently never matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatConditionKind {
    Resolution,
    Source,
    QualityModifier,
    Edition,
    Language,
    Group,
    Year,
    TitleContains,
}

/// One predicate inside a custom format. All conditions of a format are
/// AND-ed; `negate` flips a single condition's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatCondition {
    pub kind: FormatConditionKind,
    /// Comparison target: e.g. `"1080p"`, `"web-dl"`, `"proper"`, `"2010-2019"`.
    pub value: String,
    #[serde(default)]
    pub negate: bool,
}

/// A *arr-style custom format: name, score contribution, and the conditions
/// under which the score applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomFormat {
    pub name: String,
    pub score: i64,
    #[serde(default)]
    pub conditions: Vec<FormatCondition>,
    #[serde(default = "crate::domain::formats::default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl CustomFormat {
    /// True when every condition matches the release (empty conditions match
    /// everything, mirroring *arr behavior).
    pub fn matches(&self, release: &ParsedRelease) -> bool {
        self.conditions.iter().all(|c| condition_matches(c, release))
    }
}

/// Evaluates whether `cf` matches `release`: all conditions AND-ed, each
/// condition flipped by its own `negate` flag.
pub fn evaluate(cf: &CustomFormat, release: &ParsedRelease) -> bool {
    cf.matches(release)
}

/// Sum of scores of all enabled formats matching `release`. Disabled formats
/// contribute nothing regardless of match state.
pub fn total_score(cfs: &[CustomFormat], release: &ParsedRelease) -> i64 {
    cfs.iter()
        .filter(|cf| cf.enabled && cf.matches(release))
        .map(|cf| cf.score)
        .sum()
}

fn condition_matches(c: &FormatCondition, r: &ParsedRelease) -> bool {
    use FormatConditionKind as K;
    let hit = match c.kind {
        K::Resolution => parse_resolution(&c.value)
            .is_some_and(|v| v == r.quality.resolution),
        K::Source => parse_source(&c.value).is_some_and(|v| v == r.quality.source),
        K::QualityModifier => {
            parse_quality_modifier(&c.value).is_some_and(|v| v == r.quality.modifier)
        }
        K::Edition => r.edition.as_ref().is_some_and(|e| eq_ci(e, &c.value)),
        K::Language => r.languages.iter().any(|l| eq_ci(l, &c.value)),
        K::Group => r.release_group.as_ref().is_some_and(|g| eq_ci(g, &c.value)),
        K::Year => year_matches(&c.value, r.year),
        K::TitleContains => contains_ci(r.title.as_str(), &c.value)
            || r.edition.as_ref().is_some_and(|e| contains_ci(e, &c.value))
            || r.release_group.as_ref().is_some_and(|g| contains_ci(g, &c.value)),
    };
    hit != c.negate
}

// ---------------------------------------------------------------------------
// Condition value parsing
// ---------------------------------------------------------------------------

fn parse_resolution(v: &str) -> Option<Resolution> {
    let k = normalize(v);
    Some(match k.as_str() {
        "2160p" | "2160" | "4k" | "uhd" => Resolution::R2160,
        "1080p" | "1080" | "fullhd" => Resolution::R1080,
        "720p" | "720" | "hd" => Resolution::R720,
        "576p" | "576" => Resolution::R576,
        "480p" | "480" | "sd" => Resolution::R480,
        "unknown" => Resolution::Unknown,
        _ => return None,
    })
}

fn parse_source(v: &str) -> Option<Source> {
    let k = normalize(v);
    Some(match k.as_str() {
        "blurayremux" | "remux" => Source::BlurayRemux,
        "bluray" | "br" | "bd" => Source::Bluray,
        "webdl" | "web" => Source::Webdl,
        "webrip" => Source::Webrip,
        "hdtv" | "pdtv" | "dsr" => Source::Hdtv,
        "dvd" | "r5" | "pal" | "ntsc" => Source::Dvd,
        "sdtv" => Source::Sdtv,
        "cam" => Source::Cam,
        "telesync" | "ts" => Source::Telesync,
        "unknown" => Source::Unknown,
        _ => return None,
    })
}

fn parse_quality_modifier(v: &str) -> Option<QualityModifier> {
    let k = normalize(v);
    Some(match k.as_str() {
        "remux" => QualityModifier::Remux,
        "brdisk" | "bdisk" => QualityModifier::BrDisk,
        "proper" => QualityModifier::Proper,
        "repack" => QualityModifier::Repack,
        "real" => QualityModifier::Real,
        "none" | "unknown" => QualityModifier::None,
        _ => return None,
    })
}

/// Lowercased alphanumerics only, so `"WEB-DL"`, `"web_dl"`, `"WebDL"` unify.
fn normalize(v: &str) -> String {
    v.trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Year condition: either a single year (`"2015"`) or an inclusive range
/// (`"2010-2019"`). A release without a parsed year never matches.
fn year_matches(value: &str, year: Option<i32>) -> bool {
    let Some(year) = year else {
        return false;
    };
    let v = value.trim();
    if let Ok(y) = v.parse::<i32>() {
        return y == year;
    }
    let (lo, hi) = match v.split_once('-') {
        Some((a, b)) => (
            a.trim().parse::<i32>().ok(),
            b.trim().parse::<i32>().ok(),
        ),
        None => (None, None),
    };
    match (lo, hi) {
        (Some(lo), Some(hi)) => (lo..=hi).contains(&year),
        _ => false,
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

// ---------------------------------------------------------------------------
// Score-aware cutoff decisioning
// ---------------------------------------------------------------------------

/// Result of combining quality upgrade logic with a custom-format score
/// cutoff.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredDecision {
    /// Final decision after `apply_cutoff`.
    pub decision: UpgradeDecision,
    /// Total custom-format score that produced it.
    pub score: i64,
    /// True when a live cutoff was met or exceeded.
    pub cutoff_met: bool,
}

/// Applies a custom-format score cutoff to a grab/upgrade decision: a would-be
/// grab whose total score is already at or above `cutoff_score` is blocked
/// with reason `"cutoff_met"`. Non-grab decisions pass through unchanged.
///
/// A cutoff of `i64::MIN` disables the check entirely (sentinel for "no
/// cutoff configured").
pub fn apply_cutoff(score_total: i64, cutoff_score: i64, d: UpgradeDecision) -> UpgradeDecision {
    if d.grab && cutoff_score != i64::MIN && score_total >= cutoff_score {
        UpgradeDecision { grab: false, reason: "cutoff_met".into(), upgrade: false }
    } else {
        d
    }
}

/// Convenience wrapper used by handlers: runs the quality upgrade decision,
/// then applies the custom-format score cutoff.
pub fn decide_with_score(
    candidate: &Quality,
    candidate_modifier: QualityModifier,
    current_quality: Option<&Quality>,
    current_modifier: Option<QualityModifier>,
    score_total: i64,
    cutoff_score: i64,
) -> ScoredDecision {
    let raw = decide_upgrade(candidate, candidate_modifier, current_quality, current_modifier);
    let decision = apply_cutoff(score_total, cutoff_score, raw);
    let cutoff_met =
        cutoff_score != i64::MIN && score_total >= cutoff_score;
    ScoredDecision { decision, score: score_total, cutoff_met }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::parsing::parse;

    #[derive(serde::Deserialize)]
    struct GoldenCase {
        name: String,
        format: CustomFormat,
        releaseTitle: String,
        expectMatch: bool,
    }

    #[derive(serde::Deserialize)]
    struct GoldenScore {
        name: String,
        formats: Vec<CustomFormat>,
        releaseTitle: String,
        expectTotal: i64,
    }

    fn release(title: &str) -> ParsedRelease {
        parse(title)
    }

    #[test]
    fn golden_cases_match_exactly() {
        let cases: Vec<GoldenCase> = serde_json::from_str(include_str!(
            "../../tests/golden/formats/cases.json"
        ))
        .expect("cases.json is valid");
        assert!(cases.len() >= 24, "expected at least 24 golden cases");
        for case in &cases {
            let got = evaluate(&case.format, &release(&case.releaseTitle));
            assert_eq!(
                got, case.expectMatch,
                "case `{}` (format `{}` on `{}`)",
                case.name, case.format.name, case.releaseTitle
            );
        }
    }

    #[test]
    fn golden_scores_sum_exactly() {
        let cases: Vec<GoldenScore> = serde_json::from_str(include_str!(
            "../../tests/golden/formats/scores.json"
        ))
        .expect("scores.json is valid");
        for case in &cases {
            let got = total_score(&case.formats, &release(&case.releaseTitle));
            assert_eq!(
                got, case.expectTotal,
                "score case `{}` on `{}`",
                case.name, case.releaseTitle
            );
        }
    }

    #[test]
    fn apply_cutoff_blocks_grab_at_or_above_cutoff() {
        let d = UpgradeDecision { grab: true, reason: "no_existing_file".into(), upgrade: false };
        let out = apply_cutoff(100, 100, d.clone());
        assert!(!out.grab);
        assert_eq!(out.reason, "cutoff_met");
        assert!(!out.upgrade);

        let below = apply_cutoff(99, 100, d.clone());
        assert!(below.grab);
        assert_eq!(below.reason, "no_existing_file");
    }

    #[test]
    fn disabled_format_never_counts_in_total_score() {
        let cf = CustomFormat {
            name: "off".into(),
            score: 1000,
            conditions: vec![],
            enabled: false,
        };
        let r = release("The.Matrix.1999.1080p.BluRay.x264-GRP");
        assert_eq!(total_score(&[cf], &r), 0);
    }

    #[test]
    fn apply_cutoff_passes_through_non_grabs_and_disabled_cutoff() {
        let rejected = UpgradeDecision { grab: false, reason: "below_current".into(), upgrade: false };
        // Even above cutoff, a non-grab decision keeps its own reason.
        assert_eq!(apply_cutoff(5000, 100, rejected.clone()), rejected);
        // i64::MIN disables the cutoff entirely.
        let grab = UpgradeDecision { grab: true, reason: "upgrade".into(), upgrade: true };
        assert_eq!(apply_cutoff(i64::MAX, i64::MIN, grab.clone()), grab);
    }

    #[test]
    fn decide_with_score_combines_quality_and_cutoff() {
        let cand = Quality { resolution: Resolution::R1080, source: Source::Bluray, modifier: QualityModifier::None };
        let cur = Quality { resolution: Resolution::R720, source: Source::Hdtv, modifier: QualityModifier::None };

        let up = decide_with_score(&cand, QualityModifier::None, Some(&cur), Some(QualityModifier::None), 150, 150);
        assert!(!up.decision.grab);
        assert_eq!(up.decision.reason, "cutoff_met");
        assert!(up.cutoff_met);
        assert_eq!(up.score, 150);

        let kept = decide_with_score(&cand, QualityModifier::None, Some(&cur), Some(QualityModifier::None), 149, 150);
        assert!(kept.decision.reason.starts_with("upgrade_"));
        assert!(!kept.cutoff_met);

        let fresh = decide_with_score(&cand, QualityModifier::None, None, None, 10, 20);
        assert!(fresh.decision.grab);
        assert_eq!(fresh.decision.reason, "no_existing_file");
    }

    #[test]
    fn condition_values_normalize_separators_and_case() {
        let r = release("Show.S01E01.720p.WEB-DL.H.264-GRP");
        let mk = |value: &str| FormatCondition {
            kind: FormatConditionKind::Source,
            value: value.into(),
            negate: false,
        };
        for v in ["web-dl", "WEB_DL", "WebDL", "webdl"] {
            let cf = CustomFormat { name: "s".into(), score: 1, conditions: vec![mk(v)], enabled: true };
            assert!(evaluate(&cf, &r), "source value `{v}` should match web-dl");
        }
    }
}
