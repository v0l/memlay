use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use memlay::event::{Event, Tag};
use memlay::message::NostrMessage;
use memlay::store::{EventStore, StoreConfig};
use memlay::subscription::{Filter, FilterMatch, Subscription, SubscriptionManager};
use rand::prelude::*;
use std::sync::Arc;

/// Generate a random event for benchmarking
fn generate_random_event(rng: &mut impl Rng, id: u64, pubkey_pool: &[[u8; 32]]) -> Event {
    let mut id_bytes = [0u8; 32];
    id_bytes[..8].copy_from_slice(&id.to_be_bytes());
    rng.fill_bytes(&mut id_bytes[8..]);

    let pubkey = pubkey_pool[rng.gen_range(0..pubkey_pool.len())];
    let kind = *[0, 1, 3, 4, 7, 1984, 30023].choose(rng).unwrap();
    let created_at = rng.gen_range(1700000000u64..1710000000u64);

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

fn generate_pubkey_pool(rng: &mut impl Rng, size: usize) -> Vec<[u8; 32]> {
    (0..size)
        .map(|_| {
            let mut pk = [0u8; 32];
            rng.fill_bytes(&mut pk);
            pk
        })
        .collect()
}

// ── Message Parsing Benchmarks ────────────────────────────────────────────────

fn bench_message_parse_event(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let events: Vec<String> = (0..1000)
        .map(|i| {
            let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
            let json = String::from_utf8(event.raw.to_vec()).unwrap();
            format!(r#"["EVENT",{}]"#, json)
        })
        .collect();

    c.bench_function("message_parse_event", |b| {
        b.iter(|| {
            for msg in &events {
                let _ = black_box(NostrMessage::from_json(msg));
            }
        });
    });
}

fn bench_message_parse_req(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 10_000,
        max_bytes: 0,
    });

    for i in 0..10_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        store.insert(std::sync::Arc::new(event));
    }

    let filter = Filter {
        kinds: Some(vec![1, 2, 3]),
        authors: Some(vec![
            hex_encode(&pubkey_pool[0]),
            hex_encode(&pubkey_pool[1]),
        ]),
        limit: Some(100),
        ..Default::default()
    };

    let filter_json = serde_json::to_string(&filter).unwrap();
    let req = format!(r#"["REQ","sub1",{}]"#, filter_json);

    c.bench_function("message_parse_req", |b| {
        b.iter(|| {
            let _ = black_box(NostrMessage::from_json(&req));
        });
    });
}

fn bench_message_parse_req_complex(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let filters_json: Vec<String> = (0..10)
        .map(|i| {
            let filter = Filter {
                kinds: Some(vec![i as u32, (i + 1) as u32]),
                authors: Some(vec![hex_encode(&pubkey_pool[(i % 50) as usize])]),
                since: Some(1700000000 + i as u64 * 1000),
                limit: Some(50),
                ..Default::default()
            };
            serde_json::to_string(&filter).unwrap()
        })
        .collect();

    let filters_combined = filters_json.join(",");
    let req = format!(r#"["REQ","sub1",{}]"#, filters_combined);

    c.bench_function("message_parse_req_complex", |b| {
        b.iter(|| {
            let _ = black_box(NostrMessage::from_json(&req));
        });
    });
}

fn bench_message_to_json(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let event = generate_random_event(&mut rng, 1, &pubkey_pool);
    let event_arc = std::sync::Arc::new(event);

    let msg = NostrMessage::Event {
        sub_id: Some("sub1".to_string()),
        event: (*event_arc).clone(),
    };

    c.bench_function("message_to_json_event", |b| {
        b.iter(|| {
            black_box(msg.to_json());
        });
    });

    let filter = Filter {
        kinds: Some(vec![1, 2, 3]),
        limit: Some(100),
        ..Default::default()
    };
    let msg_req = NostrMessage::Request {
        id: "sub1".to_string(),
        filters: vec![filter],
    };

    c.bench_function("message_to_json_req", |b| {
        b.iter(|| {
            black_box(msg_req.to_json());
        });
    });
}

// ── Filter Benchmarks ─────────────────────────────────────────────────────────

fn bench_filter_deserialize(c: &mut Criterion) {
    let filter_json =
        r#"{"kinds":[1,2,3],"authors":["abcd","efgh"],"since":1700000000,"limit":100}"#;

    c.bench_function("filter_deserialize_simple", |b| {
        b.iter(|| {
            let _ = black_box(serde_json::from_str::<Filter>(filter_json));
        });
    });

    c.bench_function("filter_deserialize_complex", |b| {
        let complex_filter = r#"{"kinds":[1,2,3,4,5],"authors":["abcd","efgh","ijkl","mnop","qrst"],"since":1700000000,"until":1710000000,"limit":100}"#;
        b.iter(|| {
            let _ = black_box(serde_json::from_str::<Filter>(complex_filter));
        });
    });
}

fn bench_filter_match_event(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let event = generate_random_event(&mut rng, 1, &pubkey_pool);

    let filter = Filter {
        kinds: Some(vec![event.kind]),
        authors: Some(vec![hex_encode(&event.pubkey)]),
        since: Some(event.created_at - 1000),
        until: Some(event.created_at + 1000),
        ..Default::default()
    };

    c.bench_function("filter_match_event_matches", |b| {
        b.iter(|| {
            black_box(filter.matches_event(&event));
        });
    });

    let non_matching_filter = Filter {
        kinds: Some(vec![999]),
        ..Default::default()
    };

    c.bench_function("filter_match_event_no_match", |b| {
        b.iter(|| {
            black_box(non_matching_filter.matches_event(&event));
        });
    });
}

// ── Store Query Benchmarks ────────────────────────────────────────────────────

fn bench_store_query_by_kind(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });

    for i in 0..100_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        store.insert(std::sync::Arc::new(event));
    }

    for limit in [10, 100, 1000] {
        c.bench_with_input(
            BenchmarkId::new("store_query_by_kind", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_kind(1, limit));
                });
            },
        );
    }
}

fn bench_store_query_by_pubkey(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });

    for i in 0..100_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        store.insert(std::sync::Arc::new(event));
    }

    let sample_pubkey = pubkey_pool[0];

    for limit in [10, 100, 1000] {
        c.bench_with_input(
            BenchmarkId::new("store_query_by_pubkey", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_pubkey(&sample_pubkey, limit));
                });
            },
        );
    }
}

fn bench_store_query_by_e_tag(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });

    // Create events with e-tags
    let mut e_tag_events: Vec<Arc<Event>> = Vec::new();
    for i in 0..10_000 {
        let mut event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        // Add an e-tag referencing a specific event
        let e_tag_event = generate_random_event(&mut rng.clone(), 99999 - i, &pubkey_pool);
        event.tags.push(Tag {
            name: "e".to_string(),
            values: vec![hex_encode(&e_tag_event.id)],
        });
        let arc = std::sync::Arc::new(event);
        store.insert(arc.clone());
        if i < 100 {
            e_tag_events.push(arc);
        }
    }

    let sample_e_tag = if !e_tag_events.is_empty() {
        e_tag_events[0].id
    } else {
        [0u8; 32]
    };

    for limit in [10, 100, 1000] {
        c.bench_with_input(
            BenchmarkId::new("store_query_by_e_tag", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_e_tag(&sample_e_tag, limit));
                });
            },
        );
    }
}

fn bench_store_query_by_p_tag(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });

    for i in 0..100_000 {
        let mut event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        // Add p-tags
        event.tags.push(Tag {
            name: "p".to_string(),
            values: vec![hex_encode(&pubkey_pool[(i % 50) as usize])],
        });
        store.insert(std::sync::Arc::new(event));
    }

    let sample_p_tag = pubkey_pool[0];

    for limit in [10, 100, 1000] {
        c.bench_with_input(
            BenchmarkId::new("store_query_by_p_tag", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_p_tag(&sample_p_tag, limit));
                });
            },
        );
    }
}

fn bench_store_query_by_tag(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });

    let tag_values: [&str; 5] = ["nostr", "bitcoin", "lightning", "defi", "crypto"];

    for i in 0..100_000 {
        let mut event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        event.tags.push(Tag {
            name: "t".to_string(),
            values: vec![tag_values[(i % 5) as usize].to_string()],
        });
        store.insert(std::sync::Arc::new(event));
    }

    let sample_tag_value = "nostr";

    for limit in [10, 100, 1000] {
        c.bench_with_input(
            BenchmarkId::new("store_query_by_tag", limit),
            &limit,
            |b, &limit| {
                b.iter(|| {
                    black_box(store.query_by_tag('t', sample_tag_value, limit));
                });
            },
        );
    }
}

fn bench_store_get_by_id(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = EventStore::new(StoreConfig {
        max_events: 100_000,
        max_bytes: 0,
    });

    let mut ids = Vec::new();
    for i in 0..100_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        ids.push(event.id);
        store.insert(std::sync::Arc::new(event));
    }

    c.bench_function("store_get_by_id", |b| {
        b.iter(|| {
            for id in &ids {
                black_box(store.get(id));
            }
        });
    });
}

// ── Subscription Manager Benchmarks ───────────────────────────────────────────

fn bench_subscription_add_remove(c: &mut Criterion) {
    let store = Arc::new(EventStore::new(StoreConfig {
        max_events: 10_000,
        max_bytes: 0,
    }));
    let sm = SubscriptionManager::new(store);

    let filter = Filter {
        kinds: Some(vec![1]),
        ..Default::default()
    };

    c.bench_function("subscription_add_remove", |b| {
        b.iter(|| {
            let sub = Subscription {
                id: "test_sub".to_string(),
                filters: vec![filter.clone()],
            };
            sm.add_subscription(sub);
            sm.remove_subscription("test_sub");
        });
    });
}

fn bench_subscription_query_filter(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = Arc::new(EventStore::new(StoreConfig {
        max_events: 50_000,
        max_bytes: 0,
    }));

    for i in 0..50_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        store.insert(std::sync::Arc::new(event));
    }

    let sm = SubscriptionManager::new(store);

    let filter = Filter {
        kinds: Some(vec![1, 2]),
        authors: Some(vec![hex_encode(&pubkey_pool[0])]),
        since: Some(1700000000),
        limit: Some(100),
        ..Default::default()
    };

    c.bench_function("subscription_query_filter", |b| {
        b.iter(|| {
            black_box(sm.query_filter(&filter));
        });
    });
}

fn bench_subscription_query_multiple_filters(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = Arc::new(EventStore::new(StoreConfig {
        max_events: 50_000,
        max_bytes: 0,
    }));

    for i in 0..50_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        store.insert(std::sync::Arc::new(event));
    }

    let sm = SubscriptionManager::new(store);

    let filters: Vec<Filter> = (0..10)
        .map(|i| Filter {
            kinds: Some(vec![i as u32]),
            limit: Some(50),
            ..Default::default()
        })
        .collect();

    let sub = Subscription {
        id: "multi_filter_sub".to_string(),
        filters,
    };

    c.bench_function("subscription_query_multiple_filters", |b| {
        b.iter(|| {
            black_box(sm.query_subscriptions());
        });
    });
}

// ── End-to-End Pipeline Benchmarks ────────────────────────────────────────────

fn bench_full_request_pipeline(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let store = Arc::new(EventStore::new(StoreConfig {
        max_events: 10_000,
        max_bytes: 0,
    }));

    for i in 0..10_000 {
        let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
        store.insert(std::sync::Arc::new(event));
    }

    let sm = SubscriptionManager::new(store);

    // Simulate REQ message
    let filter = Filter {
        kinds: Some(vec![1, 2, 3]),
        limit: Some(50),
        ..Default::default()
    };

    let req_json = format!(
        r#"["REQ","sub1",{}]"#,
        serde_json::to_string(&filter).unwrap()
    );

    c.bench_function("full_request_pipeline", |b| {
        b.iter(|| {
            // 1. Parse message
            let msg = NostrMessage::from_json(&req_json).unwrap();

            // 2. Extract request
            let (_sub_id, filters) = match msg {
                NostrMessage::Request { id, filters } => (id, filters),
                _ => panic!("Expected REQ message"),
            };

            // 3. Process each filter
            let mut result_count = 0;
            for filter in filters {
                let events = sm.query_filter(&filter);
                result_count += events.len();

                // 4. Serialize responses (simulated)
                for event in &events {
                    let _ = black_box(event.id);
                }
            }

            black_box(result_count);
        });
    });
}

fn bench_full_event_ingest_pipeline(c: &mut Criterion) {
    let mut rng = StdRng::seed_from_u64(42);
    let pubkey_pool = generate_pubkey_pool(&mut rng, 100);

    let events: Vec<String> = (0..1000)
        .map(|i| {
            let event = generate_random_event(&mut rng.clone(), i, &pubkey_pool);
            let json = String::from_utf8(event.raw.to_vec()).unwrap();
            format!(r#"["EVENT",{}]"#, json)
        })
        .collect();

    let store = Arc::new(EventStore::new(StoreConfig {
        max_events: 10_000,
        max_bytes: 0,
    }));

    c.bench_function("full_event_ingest_pipeline", |b| {
        b.iter(|| {
            for msg_json in &events {
                // 1. Parse message
                let msg = match NostrMessage::from_json(msg_json) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                // 2. Extract event
                let event = match msg {
                    NostrMessage::Event { event, .. } => event,
                    _ => continue,
                };

                // 3. Insert into store
                let arc_event = Arc::new(event);
                let _ = store.insert(arc_event);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_message_parse_event,
    bench_message_parse_req,
    bench_message_parse_req_complex,
    bench_message_to_json,
    bench_filter_deserialize,
    bench_filter_match_event,
    bench_store_query_by_kind,
    bench_store_query_by_pubkey,
    bench_store_query_by_e_tag,
    bench_store_query_by_p_tag,
    bench_store_query_by_tag,
    bench_store_get_by_id,
    bench_subscription_add_remove,
    bench_subscription_query_filter,
    bench_subscription_query_multiple_filters,
    bench_full_request_pipeline,
    bench_full_event_ingest_pipeline,
);

criterion_main!(benches);
