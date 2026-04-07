use criterion::{criterion_group, criterion_main, Criterion};
use memlay::event::Event;
use memlay::store::{EventStore, StoreConfig};
use memlay::subscription::{Filter, SubscriptionManager};
use std::hint::black_box;
use std::sync::Arc;

fn make_event(id: u64, kind: u32) -> Arc<Event> {
    let mut id_bytes = [0u8; 32];
    id_bytes[..8].copy_from_slice(&id.to_be_bytes());

    let json = format!(
        r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":1700000000,"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
        hex_encode(&id_bytes),
        "0",
        kind,
        "0"
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

/// Compare cache hit vs cache miss performance
fn bench_cache_hit_vs_miss(c: &mut Criterion) {
    let num_events = 10000;
    let events: Vec<Arc<Event>> = (0..num_events).map(|i| make_event(i, 1)).collect();

    let filter = Filter {
        kinds: Some(vec![1]),
        limit: Some(100),
        ..Default::default()
    };

    c.bench_function("cache_miss", &mut |b: &mut criterion::Bencher| {
        b.iter(|| {
            let store = EventStore::new(StoreConfig::default());
            let sub_mgr = SubscriptionManager::new(Arc::new(store));

            for event in &events {
                sub_mgr.store.insert(Arc::clone(event));
            }

            // New filter each time = cache miss
            let f = Filter {
                kinds: Some(vec![1]),
                limit: Some(100),
                ..Default::default()
            };
            black_box(sub_mgr.query_filter(&f));
        });
    });

    c.bench_function("cache_hit", &mut |b: &mut criterion::Bencher| {
        b.iter(|| {
            let store = EventStore::new(StoreConfig::default());
            let sub_mgr = SubscriptionManager::new(Arc::new(store));

            for event in &events {
                sub_mgr.store.insert(Arc::clone(event));
            }

            // Warm up cache
            let _ = sub_mgr.query_filter(&filter);

            // Cache hit
            black_box(sub_mgr.query_filter(&filter));
        });
    });
}

criterion_group!(benches, bench_cache_hit_vs_miss);
criterion_main!(benches);
