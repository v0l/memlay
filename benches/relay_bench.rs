use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use memlay::event::Event;
use memlay::store::{EventStore, StoreConfig};
use rand::prelude::*;
use std::hint::black_box;
use std::sync::Arc;

fn new_rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

fn generate_random_event(rng: &mut StdRng, id: u64, pubkey_pool: &[[u8; 32]]) -> Event {
    let mut id_bytes = [0u8; 32];
    id_bytes[..8].copy_from_slice(&id.to_be_bytes());
    rng.fill_bytes(&mut id_bytes[8..]);

    let pubkey = pubkey_pool[rng.random_range(0..pubkey_pool.len())];
    let kind = *[0, 1, 3, 4, 7, 1984, 30023].choose(rng).unwrap();
    let created_at = rng.random_range(1700000000u64..1710000000u64);

    let mut sig = [0u8; 64];
    rng.fill_bytes(&mut sig);

    Event::from_json_unchecked(
        format!(
            r#"{{"id":"{}","pubkey":"{}","created_at":{},"kind":{},"tags":[],"content":"benchmark test content","sig":"{}"}}"#,
            hex_encode(&id_bytes),
            hex_encode(&pubkey),
            created_at,
            kind,
            hex_encode(&sig)
        )
        .as_bytes(),
    ).unwrap()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn generate_pubkey_pool(rng: &mut StdRng, size: usize) -> Vec<[u8; 32]> {
    (0..size)
        .map(|_| {
            let mut pk = [0u8; 32];
            rng.fill_bytes(&mut pk);
            pk
        })
        .collect()
}

fn make_config() -> StoreConfig {
    StoreConfig {
        max_bytes: 0,
        persistence_path: None,
        use_wal: true,
    }
}

fn bench_filter_queries(c: &mut Criterion) {
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(make_config());

    for i in 0..100_000u64 {
        let event = generate_random_event(&mut rng, i, &pubkey_pool);
        store.insert(Arc::new(event));
    }

    c.bench_function("query_by_kind", |b| {
        b.iter(|| black_box(store.query_by_kind(1, 100)));
    });

    let sample_pubkey = pubkey_pool[0];
    c.bench_function("query_by_pubkey", |b| {
        b.iter(|| black_box(store.query_by_pubkey(&sample_pubkey, 100)));
    });

    c.bench_function("query_by_kind_since_until", |b| {
        let since = Some(1705000000);
        let until = Some(1708000000);
        b.iter(|| {
            let events: Vec<_> = store
                .query_by_kind(1, 100)
                .into_iter()
                .filter(|e| {
                    since.map_or(true, |s| e.created_at >= s)
                        && until.map_or(true, |u| e.created_at <= u)
                })
                .collect();
            black_box(events.len());
        });
    });

    c.bench_function("query_by_pubkey_since", |b| {
        let since = 1705000000;
        b.iter(|| {
            black_box(store.query_by_pubkey_since(&sample_pubkey, since, 100));
        });
    });
}

fn bench_multi_filters(c: &mut Criterion) {
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(make_config());

    for i in 0..100_000u64 {
        let event = generate_random_event(&mut rng, i, &pubkey_pool);
        store.insert(Arc::new(event));
    }

    let filter_counts = [1, 5, 10, 20];

    for num_filters in filter_counts {
        c.bench_with_input(
            BenchmarkId::new("multi_filter_query", num_filters),
            &num_filters,
            |b, &n| {
                b.iter(|| {
                    let mut results = Vec::new();
                    for _ in 0..n {
                        let result = store.query_by_kind(1, 50);
                        results.extend(result);
                    }
                    black_box(results.len());
                });
            },
        );
    }
}

fn bench_concurrent_queries(c: &mut Criterion) {
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = Arc::new(EventStore::new(make_config()));

    for i in 0..100_000u64 {
        let event = generate_random_event(&mut rng, i, &pubkey_pool);
        store.insert(Arc::new(event));
    }

    let sample_pubkey = pubkey_pool[0];

    for num_threads in [1, 4, 8, 16] {
        c.bench_with_input(
            BenchmarkId::new("concurrent_filter_queries", num_threads),
            &num_threads,
            |b, &n| {
                b.iter(|| {
                    std::thread::scope(|s| {
                        for _ in 0..n {
                            let store = Arc::clone(&store);
                            let pubkey = sample_pubkey;
                            s.spawn(move || {
                                black_box(store.query_by_kind(1, 50));
                                black_box(store.query_by_pubkey(&pubkey, 50));
                            });
                        }
                    });
                });
            },
        );
    }
}

fn bench_insertion(c: &mut Criterion) {
    for load in [1_000, 10_000, 50_000] {
        let mut group = c.benchmark_group(format!("insert_load_{}", load));
        group.throughput(Throughput::Elements(load as u64));

        group.bench_function("sequential_insert", |b| {
            b.iter_with_setup(
                || EventStore::new(make_config()),
                |store| {
                    let mut rng = new_rng(42);
                    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

                    for i in 0..load as u64 {
                        let event = generate_random_event(&mut rng, i, &pubkey_pool);
                        store.insert(Arc::new(event));
                    }
                },
            );
        });

        group.finish();
    }
}

criterion_group!(
    benches,
    bench_filter_queries,
    bench_multi_filters,
    bench_concurrent_queries,
    bench_insertion,
);

criterion_main!(benches);
