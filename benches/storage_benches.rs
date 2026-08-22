//! Storage + queue benchmarks: SQLite CRUD at scale, durable queue
//! enqueue/claim/complete throughput, history inserts.
//!
//! Run: cargo bench --bench storage_benches

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use rivet::storage::jobs as queue;
use sqlx::SqlitePool;

async fn pool() -> SqlitePool {
    let dir = tempfile::tempdir().unwrap();
    let dsn = format!("file://{}", dir.path().join("b.db").display());
    let p = rivet::storage::open(&dsn).await.unwrap();
    rivet::storage::migrations::apply(&p).await.unwrap();
    // Keep the tempdir alive for the process duration.
    std::mem::forget(dir);
    p
}

fn bench_queue(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("queue");
    group.throughput(Throughput::Elements(1));

    group.bench_function("enqueue", |b| {
        b.iter_batched(
            || rt.block_on(pool()),
            |pool| {
                rt.block_on(async {
                    let j = queue::enqueue::<serde_json::Value>(&pool, "search_run", None, None)
                        .await
                        .unwrap();
                    black_box(j.id);
                })
            },
            criterion::BatchSize::PerIteration,
        )
    });

    group.bench_function("claim_complete_cycle", |b| {
        let pool = rt.block_on(pool());
        b.iter(|| {
            rt.block_on(async {
                queue::enqueue::<serde_json::Value>(&pool, "search_run", None, None)
                    .await
                    .unwrap();
                let opts = queue::ClaimOpts {
                    lease_owner: "bench".into(),
                    lease_duration: chrono::Duration::seconds(30),
                };
                let job = queue::claim_one(&pool, &opts).await.unwrap();
                queue::complete(&pool, &job.id, &opts.lease_owner)
                    .await
                    .unwrap();
            })
        })
    });
    group.finish();
}

fn bench_series_crud(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("series_storage");

    group.bench_function("upsert_100_episodes", |b| {
        b.iter_batched(
            || {
                let pool = rt.block_on(pool());
                let series = rt.block_on(async {
                    let (series, _) = rivet::storage::series::upsert_series(
                        &pool,
                        &rivet::storage::series::SeriesUpsert {
                            id: None,
                            tvdb_id: Some(1),
                            tmdb_id: None,
                            imdb_id: None,
                            title: "Bench Show".into(),
                            sort_title: None,
                            status: "continuing".into(),
                            overview: Some("".into()),
                            network: None,
                            first_aired: None,
                            genres: vec![],
                            path: "/tv/bench".into(),
                            quality_profile: "default".into(),
                            monitored: true,
                            season_folder: true,
                            tags: vec![],
                        },
                    )
                    .await
                    .unwrap();
                    series
                });
                (series, pool)
            },
            |(series, pool)| {
                rt.block_on(async move {
                    let batch: Vec<_> = (0..100)
                        .map(|i| rivet::storage::series::EpisodeUpsert {
                            season_number: 1,
                            episode_number: i as i64,
                            absolute_episode_number: None,
                            title: Some(format!("Ep {i}")),
                            air_date_utc: Some("2026-01-01T00:00:00Z".into()),
                            runtime: Some(42),
                            overview: None,
                            monitored: true,
                        })
                        .collect();
                    rivet::storage::series::bulk_upsert_episodes(&pool, &series.id, &batch)
                        .await
                        .unwrap();
                })
            },
            criterion::BatchSize::PerIteration,
        )
    });

    group.bench_function("list_missing_1000_episodes", |b| {
        let pool = rt.block_on(pool());
        rt.block_on(async {
            let (series, _) = rivet::storage::series::upsert_series(
                &pool,
                &rivet::storage::series::SeriesUpsert {
                    id: None,
                    tvdb_id: Some(2),
                    tmdb_id: None,
                    imdb_id: None,
                    title: "Big Show".into(),
                    sort_title: None,
                    status: "continuing".into(),
                    overview: None,
                    network: None,
                    first_aired: None,
                    genres: vec![],
                    path: "/tv/big".into(),
                    quality_profile: "default".into(),
                    monitored: true,
                    season_folder: true,
                    tags: vec![],
                },
            )
            .await
            .unwrap();
            for season in 1..=10u32 {
                let batch: Vec<_> = ((season - 1) * 100..season * 100)
                    .map(|i| rivet::storage::series::EpisodeUpsert {
                        season_number: season as i64,
                        episode_number: i as i64 % 100,
                        absolute_episode_number: Some(i as i64),
                        title: Some(format!("Ep {i}")),
                        air_date_utc: Some("2020-01-01T00:00:00Z".into()),
                        runtime: Some(42),
                        overview: None,
                        monitored: true,
                    })
                    .collect();
                rivet::storage::series::bulk_upsert_episodes(&pool, &series.id, &batch)
                    .await
                    .unwrap();
            }
        });
        b.iter(|| {
            rt.block_on(async {
                black_box(
                    rivet::storage::series::list_missing_episodes(&pool, "2099-01-01T00:00:00Z")
                        .await
                        .unwrap()
                        .len(),
                );
            })
        });
    });
    group.finish();
}

criterion_group!(benches, bench_queue, bench_series_crud);
criterion_main!(benches);
