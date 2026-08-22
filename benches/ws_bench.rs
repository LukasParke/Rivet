//! WebSocket hub fanout benchmark: measures publish cost as subscriber count
//! grows, with slow-consumer drops (bounded channels).
//!
//! Run: cargo bench --bench ws_bench

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rivet::state::Role;
use rivet::ws::hub::Hub;

fn bench_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("ws_hub_publish");
    for subscribers in [10usize, 100, 500] {
        group.throughput(Throughput::Elements(subscribers as u64));
        group.bench_with_input(
            BenchmarkId::new("job_event", subscribers),
            &subscribers,
            |b, &n| {
                let hub = std::sync::Arc::new(Hub::new());
                let mut rxs = Vec::new();
                for _ in 0..n {
                    let (_id, rx) = hub.add(Role::Admin);
                    rxs.push(rx);
                }
                // Drain receivers lazily so the bounded channels never fill.
                let drainer = tokio::runtime::Runtime::new().unwrap();
                let _ = &drainer;
                b.iter(|| {
                    hub.publish("job.claimed", serde_json::json!({"kind": "search_run"}));
                    // Non-blocking drain of a few frames per iteration to keep
                    // channels from saturating and skewing TrySend behavior.
                    for rx in rxs.iter_mut() {
                        let _ = rx.try_recv();
                    }
                    black_box(n);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_fanout);
criterion_main!(benches);
