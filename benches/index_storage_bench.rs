use criterion::{Bencher, Criterion, black_box, criterion_group, criterion_main};
use memlay::event::Event;
use memlay::store::EventRef;
use parking_lot::RwLock;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

fn make_event(id: u8, pubkey: u8, kind: u32, created_at: u64) -> Arc<Event> {
    let json = format!(
        r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
        format!("{:x}", id),
        format!("{:x}", pubkey),
        created_at,
        kind,
        "0"
    );
    Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
}

/// Compare RwLock<HashMap> vs DashMap for kind index lookups (concurrent scenarios)
fn benchmark_kind_index_concurrent(c: &mut Criterion) {
    let num_events = 10000;

    // Generate events
    let events: Vec<_> = (0..num_events)
        .map(|i| make_event(i as u8, (i % 100) as u8, (i % 10) as u32, 1000 + i as u64))
        .collect();

    // Build RwLock<HashMap> index (current memlay implementation)
    let rwlock_index: Arc<RwLock<HashMap<u32, BTreeSet<EventRef>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    {
        let mut index = rwlock_index.write();
        for event in &events {
            let key = event.kind;
            index
                .entry(key)
                .or_default()
                .insert(EventRef::new(event.clone()));
        }
    }

    // Build DashMap index
    let dashmap_index: dashmap::DashMap<u32, BTreeSet<EventRef>> = dashmap::DashMap::new();
    for event in &events {
        let key = event.kind;
        dashmap_index
            .entry(key)
            .or_insert_with(BTreeSet::new)
            .insert(EventRef::new(event.clone()));
    }

    let test_kinds: Vec<u32> = (0..10).collect();

    c.bench_function("RwLock_HashMap_kind_lookup", &mut |b: &mut Bencher| {
        let index = &rwlock_index;
        let kinds = &test_kinds;
        b.iter(|| {
            let guard = index.read();
            for kind in kinds {
                let result = guard.get(kind);
                black_box(result);
            }
            drop(guard);
        });
    });

    c.bench_function("DashMap_kind_lookup", &mut |b: &mut Bencher| {
        let index = &dashmap_index;
        let kinds = &test_kinds;
        b.iter(|| {
            for kind in kinds {
                let result = index.get(kind);
                black_box(result);
            }
        });
    });
}

/// Compare RwLock<HashMap> vs DashMap for pubkey index lookups
fn benchmark_pubkey_index_concurrent(c: &mut Criterion) {
    let num_events = 10000;
    let num_pubkeys = 100;

    // Generate events
    let events: Vec<_> = (0..num_events)
        .map(|i| {
            make_event(
                i as u8,
                (i % num_pubkeys) as u8,
                (i % 10) as u32,
                1000 + i as u64,
            )
        })
        .collect();

    // Build RwLock<HashMap> pubkey index
    let rwlock_pubkey: Arc<RwLock<HashMap<[u8; 32], BTreeSet<EventRef>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    {
        let mut index = rwlock_pubkey.write();
        for event in &events {
            index
                .entry(event.pubkey)
                .or_default()
                .insert(EventRef::new(event.clone()));
        }
    }

    // Build DashMap pubkey index
    let dashmap_pubkey: dashmap::DashMap<[u8; 32], BTreeSet<EventRef>> = dashmap::DashMap::new();
    for event in &events {
        dashmap_pubkey
            .entry(event.pubkey)
            .or_insert_with(BTreeSet::new)
            .insert(EventRef::new(event.clone()));
    }

    let test_keys: Vec<[u8; 32]> = (0..100)
        .map(|i| {
            let mut key = [0u8; 32];
            key[31] = i as u8;
            key
        })
        .collect();

    c.bench_function("RwLock_HashMap_pubkey_lookup", &mut |b: &mut Bencher| {
        let index = &rwlock_pubkey;
        let keys = &test_keys;
        b.iter(|| {
            let guard = index.read();
            for key in keys {
                let result = guard.get(key);
                black_box(result);
            }
            drop(guard);
        });
    });

    c.bench_function("DashMap_pubkey_lookup", &mut |b: &mut Bencher| {
        let index = &dashmap_pubkey;
        let keys = &test_keys;
        b.iter(|| {
            for key in keys {
                let result = index.get(key);
                black_box(result);
            }
        });
    });
}

/// Benchmark insert performance comparison
fn benchmark_insert(c: &mut Criterion) {
    // RwLock<HashMap> insert (current memlay approach)
    c.bench_function("RwLock_HashMap_insert_10000", &mut |b: &mut Bencher| {
        b.iter(|| {
            let index: Arc<RwLock<HashMap<u32, BTreeSet<EventRef>>>> =
                Arc::new(RwLock::new(HashMap::new()));
            {
                let mut idx = index.write();
                for i in 0..10000 {
                    let event =
                        make_event(i as u8, (i % 100) as u8, (i % 10) as u32, 1000 + i as u64);
                    idx.entry(event.kind)
                        .or_default()
                        .insert(EventRef::new(event));
                }
            }
        });
    });

    // DashMap insert
    c.bench_function("DashMap_insert_10000", &mut |b: &mut Bencher| {
        b.iter(|| {
            let index: dashmap::DashMap<u32, BTreeSet<EventRef>> = dashmap::DashMap::new();
            for i in 0..10000 {
                let event = make_event(i as u8, (i % 100) as u8, (i % 10) as u32, 1000 + i as u64);
                index
                    .entry(event.kind)
                    .or_insert_with(BTreeSet::new)
                    .insert(EventRef::new(event));
            }
        });
    });
}

criterion_group!(
    benches,
    benchmark_kind_index_concurrent,
    benchmark_pubkey_index_concurrent,
    benchmark_insert,
);
criterion_main!(benches);
