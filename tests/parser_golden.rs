//! Golden tests for the release-name parsing engine.
//!
//! Two layers:
//! 1. `tests/golden/parser/cases.json` — curated cases asserting an exact
//!    SUBSET of parsed fields per title.
//! 2. Full-output snapshots for canonical titles: if output changes
//!    intentionally, regenerate with `GOLDEN_UPDATE=1 cargo test --test parser_golden`.

use rivet::domain::parsing::{parse, ParsedRelease};
use serde_json::Value;
use std::path::PathBuf;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/parser")
}

#[test]
fn curated_cases_match_expected_fields() {
    let raw = std::fs::read_to_string(golden_dir().join("cases.json")).unwrap();
    let cases: Vec<Value> = serde_json::from_str(&raw).expect("cases.json parses");
    assert!(
        cases.len() >= 300,
        "golden corpus should stay large, got {}",
        cases.len()
    );

    let mut failures = Vec::new();
    for case in &cases {
        let title = case["title"].as_str().expect("case.title").to_string();
        let parsed = parse(&title);
        let got = serde_json::to_value(&parsed).unwrap();
        // Every field present in `expect` must equal the parsed value exactly.
        for (key, want) in case["expect"].as_object().expect("case.expect") {
            let have = &got[key];
            if have != want {
                failures.push(format!(
                    "case {title:?}\n  field {key}: want {want}, got {have}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} parse mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn snapshot_path(name: &str) -> PathBuf {
    golden_dir().join(format!("snapshot_{name}.json"))
}

fn check_snapshot(name: &str, titles: &[&str]) {
    let parsed: Vec<ParsedRelease> = titles.iter().map(|t| parse(t)).collect();
    let rendered: String = serde_json::to_string_pretty(&parsed).unwrap();
    let path = snapshot_path(name);
    if std::env::var("GOLDEN_UPDATE").is_ok() || !path.exists() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
    }
    let expected = std::fs::read_to_string(&path).unwrap();
    assert!(
        rendered == expected,
        "snapshot {name} drifted — inspect the diff; if the change is intentional \
         re-run with GOLDEN_UPDATE=1",
    );
}

#[test]
fn snapshot_series_core() {
    check_snapshot(
        "series_core",
        &[
            "The.Last.of.Us.S01E04.1080p.WEB-DL.DDP5.1.H.264-GROUP",
            "Show.Name.S02E17.720p.HDTV.x264-GRP",
            "Series.1x03.Title.1080p.AMZN.WEB-DL",
            "Breaking.Bad.S05E14.Ozymandias.1080p.BluRay.x264-DEMAND",
        ],
    );
}

#[test]
fn snapshot_multi_and_packs() {
    check_snapshot(
        "multi_packs",
        &[
            "Show.S01E01-E05.1080p.WEB-DL",
            "Show.S02E04.E06.1080p",
            "Show.Season.1.Complete.1080p.BluRay",
            "Pack.S03.720p.WEB-DL",
            "Show.S01.Partial.Pack.E01-E08.1080p",
        ],
    );
}

#[test]
fn snapshot_anime_absolute() {
    check_snapshot(
        "anime",
        &[
            "[Group] Anime Title [1234] [1080p] [HEVC]",
            "[SubsHere] Show Name - 0048 [1080p][x265]",
            "One.Piece.1080p.[1100]",
        ],
    );
}

#[test]
fn snapshot_movies_editions() {
    check_snapshot(
        "movies_editions",
        &[
            "Movie.Name.2019.2160p.UHD.BluRay.Remux.HDR.HEVC.TrueHD.7.1.Atmos-GROUP",
            "Film.2020.Extended.Cut.1080p.BluRay.x264-GROUP",
            "Another.Movie.2018.1080p.AMZN.WEB-DL.DDP5.1.H.264-GRP",
            "Director.Cut.Test.1971.720p.BluRay.x264",
        ],
    );
}

#[test]
fn snapshot_dailies_and_modifiers() {
    check_snapshot(
        "dailies_modifiers",
        &[
            "Show.Name.2026.08.21.1080p.WEB.h264-GROUP",
            "Talk.Show.2026.08.19.720p.WEB-DL",
            "Show.S01E03.REPACK.1080p.WEB-DL",
            "Movie.2010.1080p.BluRay.x264.PROPER-GROUP",
            "Other.Show.S02E01.Real.PROPER.720p",
        ],
    );
}
