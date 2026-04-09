use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use memlay::event::Event;
use memlay::store::{EventStore, StoreConfig};
use rand::prelude::*;
use std::hint::black_box;
use std::sync::Arc;

fn new_rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

fn random_event(rng: &mut StdRng, id: u64, pubkey_pool: &[[u8; 32]]) -> Arc<Event> {
    let mut id_bytes = [0u8; 32];
    id_bytes[..8].copy_from_slice(&id.to_be_bytes());
    rng.fill_bytes(&mut id_bytes[8..]);

    let idx = rng.random_range(0..pubkey_pool.len());
    let pubkey = pubkey_pool[idx];

    let kind_choices = [0, 1, 3, 4, 7, 1984, 30023];
    let kind = kind_choices[rng.random_range(0..kind_choices.len())];

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

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    let pubkey_pool = generate_pubkey_pool(&mut new_rng(42), 1000);

    for size in [1000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("sequential", size), &size, |b, &size| {
            b.iter_with_setup(
                || {
                    let mut rng = new_rng(42);
                    let store = EventStore::new(StoreConfig {
                        max_bytes: 0,
                        persistence_path: None,
                        use_wal: true,
                    });
                    let events: Vec<_> = (0..size)
                        .map(|i| random_event(&mut rng, i as u64, &pubkey_pool))
                        .collect();
                    (store, events)
                },
                |(store, events)| {
                    for event in events {
                        store.insert(black_box(event));
                    }
                },
            );
        });
    }

    group.finish();
}

fn bench_get_by_id(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_by_id");

    for size in [1000, 10_000, 100_000] {
        let mut rng = new_rng(42);
        let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

        let store = EventStore::new(StoreConfig {
            max_bytes: 0,
            persistence_path: None,
            use_wal: true,
        });
        let mut ids = Vec::with_capacity(size);
        for i in 0..size {
            let event = random_event(&mut rng, i as u64, &pubkey_pool);
            ids.push(event.id);
            store.insert(event);
        }

        group.throughput(Throughput::Elements(1000));
        group.bench_with_input(BenchmarkId::new("random_lookup", size), &size, |b, _| {
            let mut lookup_rng = new_rng(123);
            b.iter(|| {
                for _ in 0..1000 {
                    let idx = lookup_rng.random_range(0..ids.len());
                    let id = ids[idx];
                    black_box(store.get(&id));
                }
            });
        });
    }

    group.finish();
}

fn bench_query_by_kind(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_by_kind");
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    let store = EventStore::new(StoreConfig {
        max_bytes: 0,
        persistence_path: None,
        use_wal: true,
    });
    for i in 0..100_000u64 {
        store.insert(random_event(&mut rng, i, &pubkey_pool));
    }

    for limit in [10, 100, 1000] {
        group.bench_with_input(BenchmarkId::new("kind_1", limit), &limit, |b, &limit| {
            b.iter(|| {
                black_box(store.query_by_kind(1, limit));
            });
        });
    }

    group.finish();
}

fn bench_query_by_pubkey(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_by_pubkey");
    let mut rng = new_rng(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_bytes: 0,
        persistence_path: None,
        use_wal: true,
    });
    for i in 0..100_000u64 {
        store.insert(random_event(&mut rng, i, &pubkey_pool));
    }

    let pubkey = pubkey_pool[0];

    for limit in [10, 100, 1000] {
        group.bench_with_input(
            BenchmarkId::new("single_pubkey", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_pubkey(&pubkey, limit));
                });
            },
        );
    }

    group.finish();
}

fn bench_concurrent_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_reads");
    let pubkey_pool = generate_pubkey_pool(&mut new_rng(42), 100);
    let num_cpus = num_cpus::get();

    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| {
            let mut local_rng = new_rng(i);
            random_event(&mut local_rng, i, &pubkey_pool)
        })
        .collect();
    let ids: Vec<[u8; 32]> = events.iter().map(|e| e.id).collect();

    for num_threads in [1, 4, 8, 16, 32] {
        if num_threads > num_cpus {
            continue;
        }

        let ops_per_thread = 10_000usize;
        let total_ops = num_threads * ops_per_thread;
        group.throughput(Throughput::Elements(total_ops as u64));

        group.bench_with_input(
            BenchmarkId::new("get_by_id", num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter_with_setup(
                    || {
                        let store = Arc::new(EventStore::new(StoreConfig {
                            max_bytes: 0,
                            persistence_path: None,
                            use_wal: true,
                        }));
                        for event in &events {
                            store.insert(Arc::clone(event));
                        }
                        store
                    },
                    |store| {
                        std::thread::scope(|s| {
                            for t in 0..num_threads {
                                let store = Arc::clone(&store);
                                let ids = &ids;
                                s.spawn(move || {
                                    let mut rng = new_rng(t as u64);
                                    for _ in 0..ops_per_thread {
                                        let idx = rng.random_range(0..ids.len());
                                        let id = ids[idx];
                                        black_box(store.get(&id));
                                    }
                                });
                            }
                        });
                    },
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_mixed(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_mixed");
    let pubkey_pool = generate_pubkey_pool(&mut new_rng(42), 100);
    let num_cpus = num_cpus::get();

    let preload_events: Vec<Arc<Event>> = (0..50_000u64)
        .map(|i| {
            let mut local_rng = new_rng(i);
            random_event(&mut local_rng, i, &pubkey_pool)
        })
        .collect();
    let insert_events: Vec<Arc<Event>> = (50_000..100_000u64)
        .map(|i| {
            let mut local_rng = new_rng(i);
            random_event(&mut local_rng, i, &pubkey_pool)
        })
        .collect();

    for num_threads in [4, 8, 16, 32] {
        if num_threads > num_cpus {
            continue;
        }

        let writers = num_threads / 2;
        let readers = num_threads - writers;
        let inserts_per_writer = 1_000usize;
        let reads_per_reader = 5_000usize;
        let total_ops = (writers * inserts_per_writer + readers * reads_per_reader) as u64;
        group.throughput(Throughput::Elements(total_ops));

        group.bench_with_input(
            BenchmarkId::new("read_write", num_threads),
            &num_threads,
            |b, &num_threads| {
                let writers = num_threads / 2;
                let readers = num_threads - writers;
                b.iter_with_setup(
                    || {
                        let store = Arc::new(EventStore::new(StoreConfig {
                            max_bytes: 0,
                            persistence_path: None,
                            use_wal: true,
                        }));
                        for event in &preload_events {
                            store.insert(Arc::clone(event));
                        }
                        store
                    },
                    |store| {
                        std::thread::scope(|s| {
                            for w in 0..writers {
                                let store = Arc::clone(&store);
                                let events = &insert_events;
                                s.spawn(move || {
                                    let start = w * inserts_per_writer;
                                    let end = (start + inserts_per_writer).min(events.len());
                                    for event in &events[start..end] {
                                        store.insert(Arc::clone(event));
                                    }
                                });
                            }
                            for _ in 0..readers {
                                let store = Arc::clone(&store);
                                s.spawn(move || {
                                    for _ in 0..reads_per_reader {
                                        black_box(store.query_by_kind(1, 50));
                                    }
                                });
                            }
                        });
                    },
                );
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_get_by_id,
    bench_query_by_kind,
    bench_query_by_pubkey,
    bench_concurrent_reads,
    bench_concurrent_mixed,
);

criterion_main!(benches);
