//! Quality hierarchy and upgrade/cutoff decisioning.

use crate::domain::parsing::Quality;

/// Ordered quality definitions (best last is NOT the rule here; rank() on
/// Quality carries the ordering). Names match labels used by profiles.
pub const QUALITY_HIERARCHY: &[&str] = &[
    "Unknown",
    "SDTV",
    "WEBRip 480p",
    "DVD",
    "HDTV 720p",
    "HDTV 1080p",
    "WEBRip 720p",
    "WEB-DL 720p",
    "BluRay 720p",
    "WEBRip 1080p",
    "WEB-DL 1080p",
    "BluRay 1080p",
    "Remux 2160p",
    "WEB-DL 2160p",
    "BluRay 2160p",
];

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct UpgradeDecision {
    pub grab: bool,
    pub reason: String,
    /// true when the new item beats an existing file's quality (upgrade).
    pub upgrade: bool,
}

/// Decides whether a candidate should be grabbed given what is already on
/// disk. Rules (mirroring *arr semantics):
/// - No existing file + monitored → grab.
/// - Candidate quality rank strictly greater than current → upgrade.
/// - Equal quality: Proper/Repack beats plain; a newer Repack beats Proper.
/// - Anything below current quality is rejected.
pub fn decide_upgrade(
    candidate: &Quality,
    candidate_modifier: crate::domain::parsing::QualityModifier,
    current_quality: Option<&Quality>,
    current_modifier: Option<crate::domain::parsing::QualityModifier>,
) -> UpgradeDecision {
    use crate::domain::parsing::QualityModifier as M;
    let cand = candidate.rank()
        + match candidate_modifier {
            M::Proper | M::Repack => 5,
            M::Real => 6,
            _ => 0,
        };
    let Some(cur) = current_quality else {
        return UpgradeDecision { grab: true, reason: "no_existing_file".into(), upgrade: false };
    };
    let cur_rank = cur.rank()
        + match current_modifier.unwrap_or(M::None) {
            M::Proper | M::Repack => 5,
            M::Real => 6,
            _ => 0,
        };
    if cand > cur_rank {
        UpgradeDecision {
            grab: true,
            reason: format!("upgrade_{}→{}", cur.label(), candidate.label()),
            upgrade: true,
        }
    } else if cand == cur_rank {
        // Equal technical quality: prefer proper/repack freshness.
        let cand_fresh = matches!(candidate_modifier, M::Proper | M::Repack | M::Real);
        let cur_fresh = matches!(current_modifier.unwrap_or(M::None), M::Proper | M::Repack | M::Real);
        if cand_fresh && !cur_fresh {
            UpgradeDecision { grab: true, reason: "proper_or_repack".into(), upgrade: true }
        } else {
            UpgradeDecision { grab: false, reason: "already_have_equal".into(), upgrade: false }
        }
    } else {
        UpgradeDecision {
            grab: false,
            reason: format!("worse_than_existing_{}", cur.label()),
            upgrade: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::parsing::{parse, Source, Resolution};

    fn q(res: Resolution, src: Source) -> Quality {
        Quality { resolution: res, source: src, modifier: crate::domain::parsing::QualityModifier::None }
    }

    #[test]
    fn ranks_order_sensibly() {
        assert!(q(Resolution::R2160, Source::Bluray).rank() > q(Resolution::R1080, Source::Bluray).rank());
        assert!(q(Resolution::R1080, Source::Bluray).rank() > q(Resolution::R1080, Source::Webdl).rank());
        assert!(q(Resolution::R480, Source::Cam).rank() < q(Resolution::R720, Source::Hdtv).rank());
    }

    #[test]
    fn upgrades_and_rejects() {
        let have = parse("Show.S01E01.1080p.WEB-DL.x264");
        let better = parse("Show.S01E01.2160p.WEB-DL.x265");
        let worse = parse("Show.S01E01.720p.HDTV.x264");
        let d_up = decide_upgrade(&better.quality, better.quality.modifier, Some(&have.quality), None);
        assert!(d_up.grab && d_up.upgrade);
        let d_down = decide_upgrade(&worse.quality, worse.quality.modifier, Some(&have.quality), None);
        assert!(!d_down.grab);
        // Equal quality without modifier: no re-grab.
        let same = parse("Show.S01E01.1080p.WEB-DL.x265-GROUP");
        let d_eq = decide_upgrade(&same.quality, same.quality.modifier, Some(&have.quality), None);
        assert!(!d_eq.grab);
    }

    #[test]
    fn proper_beats_plain_at_same_quality() {
        let have = parse("Show.S01E01.1080p.WEB-DL.x264");
        let proper = parse("Show.S01E01.1080p.WEB-DL.x264.PROPER");
        let d = decide_upgrade(&proper.quality, proper.quality.modifier, Some(&have.quality), None);
        assert!(d.grab && d.upgrade);
        // But a proper does not replace an existing repack.
        let repack_have = parse("Show.S01E01.1080p.WEB-DL.x264.REPACK3");
        let d2 = decide_upgrade(&proper.quality, proper.quality.modifier, Some(&repack_have.quality), Some(repack_have.quality.modifier));
        assert!(!d2.grab);
    }
}
