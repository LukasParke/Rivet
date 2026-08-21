//! Release decisioning: normalize release titles and score them against the
//! configured policies.
//!
//! Fixes over the Go original (`internal/decisioning/decisioning.go`):
//! - Banned tags match *tokenized* tags, not raw substrings (the substring
//!   match rejected "The Departed" for banned tag "art" and similar).
//! - ALL quality profiles are considered and the best match wins; the original
//!   only ever looked at `qualityProfiles[0]`.
//! - Unknown quality is configurable via `rejectUnknownQuality`; the default
//!   rejects when any profile defines an allowlist.
//! - Tags participate in scoring (proper/repack bonuses).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QualityProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "allowedQualities")]
    pub allowed_qualities: Vec<String>,
    #[serde(default, rename = "upgradeAllowed")]
    pub upgrade_allowed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policies {
    #[serde(default)]
    pub language: String,
    #[serde(default, rename = "bannedTags")]
    pub banned_tags: Vec<String>,
    #[serde(default, rename = "qualityProfiles")]
    pub quality_profiles: Vec<QualityProfile>,
    /// When absent, unknown quality is rejected iff any profile defines a
    /// non-empty allowlist.
    #[serde(default, rename = "rejectUnknownQuality")]
    pub reject_unknown_quality: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Normalized {
    pub title: String,
    pub quality: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionResult {
    pub accepted: bool,
    pub score: i64,
    pub reasons: Vec<String>,
}

/// Parses the `setup.policies` setting JSON.
pub fn parse_policies(json: &str) -> Result<Policies, serde_json::Error> {
    serde_json::from_str(json)
}

fn is_separator(c: char) -> bool {
    matches!(c, '.' | '-' | '_' | ' ' | '[' | ']' | '(' | ')')
}

/// Extracts the resolution token and tokenized tags from a release title.
pub fn normalize_title(title: &str) -> Normalized {
    let t = title.trim();
    let lt = t.to_ascii_lowercase();

    let mut quality = String::new();
    for q in ["2160p", "1080p", "720p", "480p"] {
        if lt.contains(q) {
            quality = q.to_string();
            break;
        }
    }

    let mut seen = HashSet::new();
    let mut tags = Vec::new();
    for part in lt.split(is_separator) {
        let p = part.trim();
        if p.is_empty() || p.len() > 32 {
            continue;
        }
        if seen.insert(p.to_string()) {
            tags.push(p.to_string());
        }
    }

    Normalized {
        title: t.to_string(),
        quality,
        tags,
    }
}

/// Base score by resolution; unknown quality scores lowest but nonzero so it
/// can still lose to nothing yet rank below every known quality.
pub fn base_quality_score(quality: &str) -> i64 {
    match quality {
        "2160p" => 40,
        "1080p" => 30,
        "720p" => 20,
        "480p" => 15,
        _ => 10,
    }
}

/// Scoring contribution from release tags. Positive signals add; everything
/// else is neutral. Hard rejections live in [`banned_tag_hit`].
pub fn tag_bonus(tags: &[String]) -> i64 {
    let mut bonus = 0;
    for t in tags {
        match t.as_str() {
            "proper" | "repack" => bonus += 5,
            _ => {}
        }
    }
    bonus
}

/// Returns the normalized banned tag that appears in the tokenized tags, if any.
fn banned_tag_hit(tags: &[String], banned: &[String]) -> Option<String> {
    tags.iter().find(|t| banned.contains(t)).cloned()
}

/// Decides whether a normalized release passes the policies, with a score for
/// ranking competing candidates.
pub fn decide(n: &Normalized, policies: &Policies) -> DecisionResult {
    // Banned tags: exact tokenized-tag membership, never substring matching.
    let banned: Vec<String> = policies
        .banned_tags
        .iter()
        .map(|b| b.trim().to_ascii_lowercase())
        .filter(|b| !b.is_empty())
        .collect();
    if let Some(hit) = banned_tag_hit(&n.tags, &banned) {
        return DecisionResult {
            accepted: false,
            score: 0,
            reasons: vec![format!("banned_tag:{hit}")],
        };
    }

    let bonus = tag_bonus(&n.tags);
    let mut reasons: Vec<String> = Vec::new();

    if policies.quality_profiles.is_empty() {
        // No profiles configured: accept on hard-reject absence alone.
        reasons.push("no_profiles".to_string());
        append_bonus_reason(&mut reasons, bonus);
        return DecisionResult {
            accepted: true,
            score: base_quality_score(&n.quality) + bonus,
            reasons,
        };
    }

    // Consider EVERY profile; the best (highest-scoring) allowlist match wins.
    let mut best_score: Option<i64> = None;
    let mut best_profile: Option<&QualityProfile> = None;
    for profile in &policies.quality_profiles {
        let allowed: HashSet<String> = profile
            .allowed_qualities
            .iter()
            .map(|q| q.trim().to_ascii_lowercase())
            .filter(|q| !q.is_empty())
            .collect();
        // An empty allowlist means "this profile does not restrict quality".
        if allowed.is_empty() || allowed.contains(&n.quality) {
            let s = base_quality_score(&n.quality);
            if best_score.is_none() || s > best_score.unwrap() {
                best_score = Some(s);
                best_profile = Some(profile);
            }
        }
    }

    if n.quality.is_empty() {
        // Unknown quality: configurable, defaulting to reject when any
        // profile restricts quality at all.
        let any_restriction = policies
            .quality_profiles
            .iter()
            .any(|p| !p.allowed_qualities.is_empty());
        let reject = policies.reject_unknown_quality.unwrap_or(any_restriction);
        if reject {
            reasons.push("quality_unknown".to_string());
            return DecisionResult {
                accepted: false,
                score: 0,
                reasons,
            };
        }
        reasons.push("quality_unknown_allowed".to_string());
        append_bonus_reason(&mut reasons, bonus);
        return DecisionResult {
            accepted: true,
            score: base_quality_score("") + bonus,
            reasons,
        };
    }

    match (best_score, best_profile) {
        (Some(s), Some(profile)) => {
            reasons.push(format!("quality_allowed:{}", n.quality));
            if !profile.name.is_empty() {
                reasons.push(format!("profile:{}", profile.name));
            }
            append_bonus_reason(&mut reasons, bonus);
            DecisionResult {
                accepted: true,
                score: s + bonus,
                reasons,
            }
        }
        _ => {
            reasons.push(format!("quality_not_allowed:{}", n.quality));
            DecisionResult {
                accepted: false,
                score: 0,
                reasons,
            }
        }
    }
}

fn append_bonus_reason(reasons: &mut Vec<String>, bonus: i64) {
    if bonus > 0 {
        reasons.push(format!("tag_bonus:{bonus}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, qualities: &[&str]) -> QualityProfile {
        QualityProfile {
            name: name.to_string(),
            allowed_qualities: qualities.iter().map(|q| q.to_string()).collect(),
            upgrade_allowed: false,
        }
    }

    #[test]
    fn normalize_extracts_quality_and_tokens() {
        let n = normalize_title("Movie.Name.2020.1080p.WEB-DL.DDP5.1.H.264");
        assert_eq!(n.quality, "1080p");
        assert!(n.tags.contains(&"web".to_string()) || n.tags.contains(&"web-dl".to_string()));
        assert!(n.tags.contains(&"ddp5".to_string()));
        // Duplicates collapse.
        let dup = normalize_title("x.x.X");
        assert_eq!(dup.tags, vec!["x"]);
    }

    #[test]
    fn banned_tag_matches_token_not_substring() {
        let policies = Policies {
            banned_tags: vec!["cam".to_string()],
            ..Default::default()
        };
        // Substring would hit ("caMera" contains "cam"); tokenization must not.
        let ok = decide(&normalize_title("Great.Camera.Movie.2020.1080p"), &policies);
        assert!(ok.accepted, "substring false positive: {:?}", ok.reasons);

        let bad = decide(&normalize_title("Some.Movie.CAM.1080p"), &policies);
        assert!(!bad.accepted);
        assert_eq!(bad.reasons, vec!["banned_tag:cam"]);
        assert_eq!(bad.score, 0);
    }

    #[test]
    fn all_profiles_considered_best_match_wins() {
        // First profile disallows 2160p; second allows it. The original only
        // read profiles[0] and would have rejected.
        let policies = Policies {
            quality_profiles: vec![
                profile("hd", &["720p", "1080p"]),
                profile("uhd", &["2160p"]),
            ],
            ..Default::default()
        };
        let d = decide(&normalize_title("Movie.2020.2160p.BluRay"), &policies);
        assert!(d.accepted, "{:?}", d.reasons);
        assert_eq!(d.score, 40);
        assert!(d.reasons.contains(&"profile:uhd".to_string()));
    }

    #[test]
    fn known_quality_missing_from_every_allowlist_is_rejected() {
        let policies = Policies {
            quality_profiles: vec![profile("uhd-only", &["2160p"])],
            ..Default::default()
        };
        let d = decide(&normalize_title("Movie.2020.720p"), &policies);
        assert!(!d.accepted);
        assert_eq!(d.reasons, vec!["quality_not_allowed:720p"]);
    }

    #[test]
    fn unknown_quality_default_rejects_when_allowlist_exists() {
        let policies = Policies {
            quality_profiles: vec![profile("any", &["1080p"])],
            ..Default::default()
        };
        let d = decide(&normalize_title("Movie.2020.DVDRip"), &policies);
        assert!(!d.accepted);
        assert_eq!(d.reasons, vec!["quality_unknown"]);

        // Explicit opt-out accepts it.
        let policies = Policies {
            reject_unknown_quality: Some(false),
            quality_profiles: vec![profile("any", &["1080p"])],
            ..Default::default()
        };
        let d = decide(&normalize_title("Movie.2020.DVDRip"), &policies);
        assert!(d.accepted);
        assert_eq!(d.score, 10);
    }

    #[test]
    fn unknown_quality_default_accepts_without_allowlists() {
        let policies = Policies {
            quality_profiles: vec![profile("unrestricted", &[])],
            ..Default::default()
        };
        let d = decide(&normalize_title("Movie.2020"), &policies);
        assert!(d.accepted);
        assert_eq!(d.score, 10);
    }

    #[test]
    fn no_profiles_accepts_with_base_score() {
        let d = decide(&normalize_title("Movie.2020.1080p"), &Policies::default());
        assert!(d.accepted);
        assert_eq!(d.score, 30);
        assert_eq!(d.reasons, vec!["no_profiles"]);
    }

    #[test]
    fn tags_contribute_to_score() {
        let policies = Policies {
            quality_profiles: vec![profile("all", &["1080p"])],
            ..Default::default()
        };
        let plain = decide(&normalize_title("Movie.2020.1080p"), &policies);
        let proper = decide(
            &normalize_title("Movie.2020.1080p.PROPER.REPACK"),
            &policies,
        );
        assert_eq!(plain.score, 30);
        assert_eq!(proper.score, 40);
        assert!(proper.reasons.contains(&"tag_bonus:10".to_string()));
    }

    #[test]
    fn scoring_matrix() {
        let policies = Policies {
            quality_profiles: vec![profile("all", &["2160p", "1080p", "720p", "480p"])],
            ..Default::default()
        };
        let cases = [
            ("Movie.2160p", 40),
            ("Movie.1080p", 30),
            ("Movie.720p", 20),
            ("Movie.480p", 15),
        ];
        for (title, want) in cases {
            let d = decide(&normalize_title(title), &policies);
            assert!(d.accepted, "{title}");
            assert_eq!(d.score, want, "{title}");
        }

        // Unknown quality with a restricting allowlist defaults to rejection
        // (see unknown_quality_default_rejects_when_allowlist_exists).
        let d = decide(&normalize_title("Movie"), &policies);
        assert!(!d.accepted);
        assert_eq!(d.reasons, vec!["quality_unknown"]);
    }

    #[test]
    fn parse_policies_camel_case() {
        let p = parse_policies(
            r#"{"language":"en","bannedTags":["cam"],"qualityProfiles":[{"name":"hd","allowedQualities":["1080p"],"upgradeAllowed":true}]}"#,
        )
        .unwrap();
        assert_eq!(p.banned_tags, vec!["cam"]);
        assert_eq!(p.quality_profiles[0].allowed_qualities, vec!["1080p"]);
        assert!(p.quality_profiles[0].upgrade_allowed);
    }
}
