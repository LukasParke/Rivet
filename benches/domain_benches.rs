//! Domain-layer benchmarks: release parsing, quality decisioning, custom
//! format scoring, naming templates.
//!
//! Run: cargo bench --bench domain_benches

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use rivet::domain::formats::{self, CustomFormat, FormatCondition, FormatConditionKind};
use rivet::domain::naming::NamingContext;
use rivet::domain::parsing::parse;
use rivet::domain::quality::decide_upgrade;

fn sample_titles() -> Vec<&'static str> {
    vec![
        "The.Last.of.Us.S01E04.1080p.WEB-DL.DDP5.1.H.264-GROUP",
        "Breaking.Bad.S05E14.Ozymandias.1080p.BluRay.x264-DEMAND",
        "Movie.2019.2160p.UHD.BluRay.Remux.HDR.HEVC.TrueHD.7.1.Atmos-GRP",
        "[SubsPlease] Anime.Title.-.0048.[1080p][x265]",
        "Talk.Show.2026.08.19.720p.WEB-DL.GROUP",
        "Film.2020.Extended.Cut.1080p.BluRay.x264-GROUP",
        "Show.S01E01-E05.1080p.WEB-DL.DDP5.1.H.264",
        "Series.Title.S10E09.German.DL.1080p.AMZN.WEB-DL.DDP5.1.H.264-GRP",
    ]
}

fn bench_parse(c: &mut Criterion) {
    let titles = sample_titles();
    let mut group = c.benchmark_group("parse");
    group.throughput(Throughput::Elements(titles.len() as u64));
    group.bench_function("8_mixed_titles", |b| {
        b.iter(|| {
            for t in &titles {
                black_box(parse(black_box(t)));
            }
        })
    });
    group.bench_function("single_hard_title", |b| {
        b.iter(|| {
            black_box(parse(black_box(
                "Movie.2019.2160p.UHD.BluRay.Remux.HDR.HEVC.TrueHD.7.1.Atmos-GROUP",
            )))
        })
    });
    group.finish();
}

fn bench_decide_upgrade(c: &mut Criterion) {
    let have = parse("Show.S01E01.1080p.WEB-DL.x264");
    let cand = parse("Show.S01E01.2160p.WEB-DL.x265-GROUP");
    c.bench_function("decide_upgrade", |b| {
        b.iter(|| {
            black_box(decide_upgrade(
                black_box(&cand.quality),
                cand.quality.modifier,
                Some(black_box(&have.quality)),
                None,
            ))
        })
    });
}

fn sample_formats() -> Vec<CustomFormat> {
    let mk = |name: &str, kind: FormatConditionKind, value: &str, score: i64| CustomFormat {
        name: name.into(),
        score,
        conditions: vec![FormatCondition {
            kind,
            value: value.into(),
            negate: false,
        }],
        enabled: true,
    };
    vec![
        mk("2160p", FormatConditionKind::Resolution, "2160p", 120),
        mk("remux", FormatConditionKind::QualityModifier, "remux", 200),
        mk("webdl", FormatConditionKind::Source, "web-dl", 30),
        mk("german", FormatConditionKind::Language, "german", -50),
        mk("grp", FormatConditionKind::Group, "GROUP", 10),
        mk("y2019", FormatConditionKind::Year, "2019", 5),
    ]
}

fn bench_format_scoring(c: &mut Criterion) {
    let formats = sample_formats();
    let title = "Movie.2019.2160p.UHD.BluRay.Remux.HDR.HEVC.TrueHD.7.1.Atmos-GROUP";
    c.bench_function("score_6_formats", |b| {
        b.iter(|| {
            let parsed = parse(black_box(title));
            black_box(formats::total_score(black_box(&formats), &parsed))
        })
    });
}

fn bench_naming(c: &mut Criterion) {
    let ctx = NamingContext {
        title: "The Last of Us".into(),
        year: None,
        season: Some(1),
        episodes: vec![4],
        episode_titles: vec!["Please Hold to My Hand".into()],
        quality_label: "WEB-DL 1080p".into(),
        languages: vec![],
        group: Some("GROUP".into()),
        edition: None,
        original_title: None,
        air_date: None,
    };
    c.bench_function("render_series_template", |b| {
        b.iter(|| {
            black_box(rivet::domain::naming::render_series(
                black_box("{Title} - S{Season:00}E{Episode:00} - {Quality Full}"),
                "{Title} - S{Season:00}E{Episode}",
                black_box(&ctx),
                true,
            ))
        })
    });
}

criterion_group!(
    benches,
    bench_parse,
    bench_decide_upgrade,
    bench_format_scoring,
    bench_naming
);
criterion_main!(benches);
