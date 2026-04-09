use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use memlay::event::Event;
use memlay::store::{EventStore, StoreConfig};
use memlay::subscription::{Filter, SubscriptionManager};
use rand::prelude::*;
use std::hint::black_box;
use std::sync::Arc;

fn new_rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

fn random_event(rng: &mut StdRng, id: u64, pubkey_pool: &[[u8; 32]], kind: u32) -> Arc<Event> {
    let mut id_bytes = [0u8; 32];
    id_bytes[..8].copy_from_slice(&id.to_be_bytes());
    rng.fill_bytes(&mut id_bytes[8..]);

    let pubkey = pubkey_pool[rng.random_range(0..pubkey_pool.len())];
    let created_at = rng.random_range(1700000000u64..1710000000u64);

    let mut sig = [0u8; 64];
    rng.fill_bytes(&mut sig);

    let json = format!(
        r#"{{"id":"{}","pubkey":"{}","created_at":{},"kind":{},"tags":[],"content":"benchmark test content","sig":"{}"}}"#,
        hex_encode(&id_bytes),
        hex_encode(&pubkey),
        created_at,
        kind,
        hex_encode(&sig)
    );

    Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
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

fn bench_query_without_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("subscription_query");
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut new_rng(42 + i), i, &pubkey_pool, 1))
        .collect();

    for size in [10, 100, 1000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(
            BenchmarkId::new("query_by_kind_no_cache", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let store = EventStore::new(make_config());
                    let sub_mgr = SubscriptionManager::new(Arc::new(store));

                    for event in &events {
                        sub_mgr.store.insert(Arc::clone(event));
                    }

                    let filter = Filter {
                        kinds: Some(vec![1]),
                        limit: Some(size),
                        ..Default::default()
                    };

                    for _ in 0..size {
                        black_box(sub_mgr.query_filter(&filter));
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_query_with_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("subscription_query");
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut new_rng(42 + i), i, &pubkey_pool, 1))
        .collect();

    for size in [10, 100, 1000] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(
            BenchmarkId::new("query_by_kind_with_cache", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let store = EventStore::new(make_config());
                    let sub_mgr = SubscriptionManager::new(Arc::new(store));

                    for event in &events {
                        sub_mgr.store.insert(Arc::clone(event));
                    }

                    let filter = Filter {
                        kinds: Some(vec![1]),
                        limit: Some(size),
                        ..Default::default()
                    };

                    let results = sub_mgr.query_filter(&filter);

                    for _ in 0..size {
                        black_box(sub_mgr.query_filter(&filter));
                    }

                    black_box(results.len());
                });
            },
        );
    }

    group.finish();
}

fn bench_query_cache_hit_vs_miss(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_hit_vs_miss");
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    let kind1_events: Vec<Arc<Event>> = (0..50_000u64)
        .map(|i| random_event(&mut new_rng(42 + i), i, &pubkey_pool, 1))
        .collect();
    let _kind7_events: Vec<Arc<Event>> = (0..50_000u64)
        .map(|i| random_event(&mut new_rng(42 + i + 50_000), i + 50_000, &pubkey_pool, 7))
        .collect();

    group.throughput(Throughput::Elements(1000));

    group.bench_with_input(BenchmarkId::new("cache_hit", 1000), &1000, |b, _| {
        b.iter_with_setup(
            || {
                let store = EventStore::new(make_config());
                let sub_mgr = SubscriptionManager::new(Arc::new(store));

                for event in &kind1_events {
                    sub_mgr.store.insert(Arc::clone(event));
                }

                let filter = Filter {
                    kinds: Some(vec![1]),
                    limit: Some(100),
                    ..Default::default()
                };

                sub_mgr.query_filter(&filter);

                sub_mgr
            },
            |sub_mgr| {
                let filter = Filter {
                    kinds: Some(vec![1]),
                    limit: Some(100),
                    ..Default::default()
                };

                for _ in 0..1000 {
                    black_box(sub_mgr.query_filter(&filter));
                }
            },
        );
    });

    group.bench_with_input(BenchmarkId::new("cache_miss", 1000), &1000, |b, _| {
        b.iter_with_setup(
            || {
                let store = EventStore::new(make_config());
                let sub_mgr = SubscriptionManager::new(Arc::new(store));

                for event in &kind1_events {
                    sub_mgr.store.insert(Arc::clone(event));
                }

                sub_mgr
            },
            |sub_mgr| {
                for i in 0..1000 {
                    let filter = Filter {
                        kinds: Some(vec![if i % 2 == 0 { 1 } else { 7 }]),
                        limit: Some(100),
                        ..Default::default()
                    };
                    black_box(sub_mgr.query_filter(&filter));
                }
            },
        );
    });

    group.finish();
}

fn bench_query_with_author_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("subscription_author_filter");
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut new_rng(42 + i), i, &pubkey_pool, 1))
        .collect();

    let query_pubkey = pubkey_pool[0];

    for size in [10, 50, 100] {
        group.throughput(Throughput::Elements(size as u64));

        group.bench_with_input(
            BenchmarkId::new("by_author_cached", size),
            &size,
            |b, &size| {
                let store = EventStore::new(make_config());
                let sub_mgr = SubscriptionManager::new(Arc::new(store));

                for event in &events {
                    sub_mgr.store.insert(Arc::clone(event));
                }

                let pubkey_hex = hex_encode(&query_pubkey);
                let filter = Filter {
                    authors: Some(vec![pubkey_hex.into()]),
                    limit: Some(size),
                    ..Default::default()
                };

                b.iter(|| {
                    for _ in 0..size {
                        black_box(sub_mgr.query_filter(&filter));
                    }
                });
            },
        );
    }

    group.finish();
}

fn bench_cache_invalidation_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache_invalidation");
    let pubkey_pool = generate_pubkey_pool(&mut new_rng(42), 1000);

    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut new_rng(42 + i), i, &pubkey_pool, 1))
        .collect();

    group.throughput(Throughput::Elements(1000));

    group.bench_function("invalidate_cache", |b| {
        b.iter_with_setup(
            || {
                let store = EventStore::new(make_config());
                let sub_mgr = SubscriptionManager::new(Arc::new(store));

                for event in &events {
                    sub_mgr.store.insert(Arc::clone(event));
                }

                for _ in 0..100 {
                    let filter = Filter {
                        kinds: Some(vec![1, 3, 7]),
                        limit: Some(100),
                        ..Default::default()
                    };
                    sub_mgr.query_filter(&filter);
                }

                sub_mgr
            },
            |sub_mgr| {
                for _ in 0..1000 {
                    sub_mgr.invalidate_cache();
                }
            },
        );
    });

    group.finish();
}

fn bench_repeated_identical_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("repeated_queries");
    let pubkey_pool = generate_pubkey_pool(&mut new_rng(42), 1000);

    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut new_rng(42 + i), i, &pubkey_pool, 1))
        .collect();

    for repeat_count in [10, 100, 1000] {
        group.throughput(Throughput::Elements(repeat_count));

        group.bench_with_input(
            BenchmarkId::new("same_filter", repeat_count),
            &repeat_count,
            |b, &repeat_count| {
                let store = EventStore::new(make_config());
                let sub_mgr = SubscriptionManager::new(Arc::new(store));

                for event in &events {
                    sub_mgr.store.insert(Arc::clone(event));
                }

                let filter = Filter {
                    kinds: Some(vec![1]),
                    limit: Some(100),
                    ..Default::default()
                };

                b.iter(|| {
                    for _ in 0..repeat_count {
                        black_box(sub_mgr.query_filter(&filter));
                    }
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_query_without_cache,
    bench_query_with_cache,
    bench_query_cache_hit_vs_miss,
    bench_query_with_author_filter,
    bench_cache_invalidation_overhead,
    bench_repeated_identical_queries,
);

criterion_main!(benches);
