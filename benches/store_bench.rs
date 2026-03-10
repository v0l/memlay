use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use memlay::event::Event;
use memlay::store::{EventStore, StoreConfig};
use nostr_archive_cursor::NostrCursor;
use rand::prelude::*;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

/// Generate a random event for benchmarking
fn random_event(rng: &mut impl Rng, id: u64, pubkey_pool: &[[u8; 32]]) -> Arc<Event> {
    let mut id_bytes = [0u8; 32];
    id_bytes[..8].copy_from_slice(&id.to_be_bytes());
    rng.fill_bytes(&mut id_bytes[8..]);

    let pubkey = pubkey_pool[rng.gen_range(0..pubkey_pool.len())];
    let kind = *[0, 1, 3, 4, 7, 1984, 30023].choose(rng).unwrap();
    let created_at = rng.gen_range(1700000000u64..1710000000u64);

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

    Arc::new(Event::from_json(json.as_bytes()).unwrap())
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

/// Generate a pool of pubkeys for realistic distribution
fn generate_pubkey_pool(rng: &mut impl Rng, size: usize) -> Vec<[u8; 32]> {
    (0..size)
        .map(|_| {
            let mut pk = [0u8; 32];
            rng.fill_bytes(&mut pk);
            pk
        })
        .collect()
}

/// Load real events from a directory using nostr-archive-cursor
fn load_events_from_archive(dir: &PathBuf, limit: usize) -> Vec<Arc<Event>> {
    let cursor = NostrCursor::new(dir.clone())
        .with_dedupe(true)
        .with_max_parallelism();

    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(limit)));
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let limit_reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let events_clone = events.clone();
    let count_clone = count.clone();
    let limit_clone = limit_reached.clone();

    cursor.walk_with_chunked_sync(
        move |chunk| {
            if limit_clone.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }

            for borrowed_event in chunk {
                let current = count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if current >= limit {
                    limit_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                    return;
                }

                // Serialize borrowed event directly (serde handles Cow<str>)
                if let Ok(json) = serde_json::to_vec(&borrowed_event) {
                    if let Ok(event) = Event::from_json(&json) {
                        let mut events = events_clone.lock().unwrap();
                        events.push(Arc::new(event));
                    }
                }
            }
        },
        1000,
    );

    let result = events.lock().unwrap();
    result.clone()
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    for size in [1000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("sequential", size), &size, |b, &size| {
            b.iter_with_setup(
                || {
                    let store = EventStore::new(StoreConfig {
                        max_events: size * 2,
                        max_bytes: 0,
                    });
                    let events: Vec<_> = (0..size)
                        .map(|i| random_event(&mut rng.clone(), i as u64, &pubkey_pool))
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

fn bench_insert_with_eviction(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_with_eviction");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    // Pre-fill store to max capacity, then benchmark inserting more
    let max_events = 10_000;
    let insert_count = 1000;

    group.throughput(Throughput::Elements(insert_count as u64));
    group.bench_function("evicting_oldest", |b| {
        b.iter_with_setup(
            || {
                let store = EventStore::new(StoreConfig {
                    max_events,
                    max_bytes: 0,
                });
                // Pre-fill
                for i in 0..max_events {
                    store.insert(random_event(&mut rng.clone(), i as u64, &pubkey_pool));
                }
                // Events to insert (will trigger eviction)
                let events: Vec<_> = (max_events..max_events + insert_count)
                    .map(|i| random_event(&mut rng.clone(), i as u64, &pubkey_pool))
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

    group.finish();
}

fn bench_get_by_id(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_by_id");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    for size in [1000, 10_000, 100_000] {
        // Setup: create store with events
        let store = EventStore::new(StoreConfig {
            max_events: size,
            max_bytes: 0,
        });
        let mut ids = Vec::with_capacity(size);
        for i in 0..size {
            let event = random_event(&mut rng.clone(), i as u64, &pubkey_pool);
            ids.push(event.id);
            store.insert(event);
        }

        group.throughput(Throughput::Elements(1000));
        group.bench_with_input(BenchmarkId::new("random_lookup", size), &size, |b, _| {
            let mut lookup_rng = StdRng::seed_from_u64(123);
            b.iter(|| {
                for _ in 0..1000 {
                    let id = ids[lookup_rng.gen_range(0..ids.len())];
                    black_box(store.get(&id));
                }
            });
        });
    }

    group.finish();
}

fn bench_query_by_kind(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_by_kind");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 1000);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });
    // Use advancing rng, not cloned
    for i in 0..100_000u64 {
        store.insert(random_event(&mut rng, i, &pubkey_pool));
    }

    // Verify we have events of kind 1
    let sample = store.query_by_kind(1, 10);
    eprintln!(
        "query_by_kind: found {} events of kind 1 (sample)",
        sample.len()
    );

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
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100); // Smaller pool = more events per pubkey

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });
    // Use advancing rng
    for i in 0..100_000u64 {
        store.insert(random_event(&mut rng, i, &pubkey_pool));
    }

    let pubkey = pubkey_pool[0];
    // Verify we have events for this pubkey
    let sample = store.query_by_pubkey(&pubkey, 10);
    eprintln!(
        "query_by_pubkey: found {} events for pubkey[0] (sample)",
        sample.len()
    );

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

fn bench_query_by_pubkey_since(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_by_pubkey_since");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });
    // Use advancing rng
    for i in 0..100_000u64 {
        store.insert(random_event(&mut rng, i, &pubkey_pool));
    }

    let pubkey = pubkey_pool[0];
    let since = 1705000000u64; // Middle of the timestamp range

    // Verify we have events
    let sample = store.query_by_pubkey_since(&pubkey, since, 10);
    eprintln!(
        "query_by_pubkey_since: found {} events for pubkey[0] since {} (sample)",
        sample.len(),
        since
    );

    for limit in [10, 100, 1000] {
        group.bench_with_input(
            BenchmarkId::new("with_since_filter", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_pubkey_since(&pubkey, since, limit));
                });
            },
        );
    }

    group.finish();
}

// ============== CONCURRENT BENCHMARKS ==============

fn bench_concurrent_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_reads");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);
    let num_cpus = num_cpus::get();

    // Pre-generate events
    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut rng.clone(), i, &pubkey_pool))
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
                            max_events: 200_000,
                            max_bytes: 0,
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
                                    let mut rng = StdRng::seed_from_u64(t as u64);
                                    for _ in 0..ops_per_thread {
                                        let id = ids[rng.gen_range(0..ids.len())];
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

fn bench_concurrent_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_queries");
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);
    let num_cpus = num_cpus::get();

    // Pre-generate events
    let events: Vec<Arc<Event>> = (0..100_000u64)
        .map(|i| random_event(&mut rng.clone(), i, &pubkey_pool))
        .collect();

    for num_threads in [1, 4, 8, 16, 32] {
        if num_threads > num_cpus {
            continue;
        }

        let ops_per_thread = 1_000usize;
        let total_ops = num_threads * ops_per_thread;
        group.throughput(Throughput::Elements(total_ops as u64));

        group.bench_with_input(
            BenchmarkId::new("query_by_kind", num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter_with_setup(
                    || {
                        let store = Arc::new(EventStore::new(StoreConfig {
                            max_events: 200_000,
                            max_bytes: 0,
                        }));
                        for event in &events {
                            store.insert(Arc::clone(event));
                        }
                        store
                    },
                    |store| {
                        std::thread::scope(|s| {
                            for _ in 0..num_threads {
                                let store = Arc::clone(&store);
                                s.spawn(move || {
                                    for _ in 0..ops_per_thread {
                                        black_box(store.query_by_kind(1, 100));
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
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);
    let num_cpus = num_cpus::get();

    // Pre-generate events - half for pre-loading, half for inserting during benchmark
    let preload_events: Vec<Arc<Event>> = (0..50_000u64)
        .map(|i| random_event(&mut rng.clone(), i, &pubkey_pool))
        .collect();
    let insert_events: Vec<Arc<Event>> = (50_000..100_000u64)
        .map(|i| random_event(&mut rng.clone(), i, &pubkey_pool))
        .collect();

    for num_threads in [4, 8, 16, 32] {
        if num_threads > num_cpus {
            continue;
        }

        // Mixed workload: half threads write, half threads read
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
                            max_events: 200_000,
                            max_bytes: 0,
                        }));
                        for event in &preload_events {
                            store.insert(Arc::clone(event));
                        }
                        store
                    },
                    |store| {
                        std::thread::scope(|s| {
                            // Writer threads
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
                            // Reader threads
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

fn bench_real_events(c: &mut Criterion) {
    // Try to load real events from directories
    // Check NOSTR_ARCHIVE_DIR env var first, then fallback paths
    let possible_dirs: Vec<PathBuf> = env::var("NOSTR_ARCHIVE_DIR")
        .map(|d| vec![PathBuf::from(d)])
        .unwrap_or_else(|_| {
            vec![
                PathBuf::from("./events"),
                PathBuf::from("../events"),
                PathBuf::from("/tmp/nostr_events"),
                PathBuf::from(env::var("HOME").unwrap_or_default()).join("nostr-archive"),
            ]
        });

    let events: Vec<Arc<Event>> = possible_dirs
        .iter()
        .filter(|p| p.is_dir())
        .find_map(|dir| {
            eprintln!("Trying to load events from: {}", dir.display());
            let events = load_events_from_archive(dir, 100_000);
            if events.is_empty() {
                None
            } else {
                eprintln!("Loaded {} real events from {}", events.len(), dir.display());
                Some(events)
            }
        })
        .unwrap_or_default();

    if events.is_empty() {
        eprintln!("No real events found. Skipping real_events benchmark.");
        eprintln!(
            "Set NOSTR_ARCHIVE_DIR to a directory containing JSON-L files, or place files in:"
        );
        for p in &possible_dirs {
            eprintln!("  - {}", p.display());
        }
        return;
    }

    let mut group = c.benchmark_group("real_events");
    group.throughput(Throughput::Elements(events.len() as u64));

    group.bench_function("insert_all", |b| {
        b.iter_with_setup(
            || {
                EventStore::new(StoreConfig {
                    max_events: events.len() * 2,
                    max_bytes: 0,
                })
            },
            |store| {
                for event in &events {
                    store.insert(Arc::clone(event));
                }
            },
        );
    });

    // Additional benchmark: query performance on real data
    let store = EventStore::new(StoreConfig {
        max_events: events.len() * 2,
        max_bytes: 0,
    });
    for event in &events {
        store.insert(Arc::clone(event));
    }

    // Find a pubkey that has multiple events
    let sample_pubkey = events.first().map(|e| e.pubkey).unwrap_or([0u8; 32]);
    let pubkey_event_count = store.query_by_pubkey(&sample_pubkey, 10000).len();
    eprintln!("Sample pubkey has {} events in store", pubkey_event_count);

    group.bench_function("query_by_pubkey_real", |b| {
        b.iter(|| {
            black_box(store.query_by_pubkey(&sample_pubkey, 100));
        });
    });

    group.bench_function("query_by_kind_1_real", |b| {
        b.iter(|| {
            black_box(store.query_by_kind(1, 100));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_insert_with_eviction,
    bench_get_by_id,
    bench_query_by_kind,
    bench_query_by_pubkey,
    bench_query_by_pubkey_since,
    bench_concurrent_reads,
    bench_concurrent_queries,
    bench_concurrent_mixed,
    bench_real_events,
);

criterion_main!(benches);
