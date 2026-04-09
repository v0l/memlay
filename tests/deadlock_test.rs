use memlay::store::{EventStore, StoreConfig};
use memlay::subscription::{Filter, SubscriptionManager};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;

fn make_event(id: u32, pubkey: u32, kind: u32, created_at: u64) -> Arc<memlay::event::Event> {
    let json = format!(
        r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
        format!("{:x}", id),
        format!("{:x}", pubkey),
        created_at,
        kind,
        "0"
    );
    Arc::new(memlay::event::Event::from_json_unchecked(json.as_bytes()).unwrap())
}

fn make_event_with_tag(
    id: u32,
    pubkey: u32,
    kind: u32,
    created_at: u64,
    tag_type: &str,
    tag_value: &str,
) -> Arc<memlay::event::Event> {
    let json = format!(
        r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[["{}","{}"]],"content":"test","sig":"{:0>128}"}}"#,
        format!("{:x}", id),
        format!("{:x}", pubkey),
        created_at,
        kind,
        tag_type,
        tag_value,
        "0"
    );
    Arc::new(memlay::event::Event::from_json_unchecked(json.as_bytes()).unwrap())
}

/// Test concurrent insert and query operations to detect deadlocks
#[tokio::test]
async fn test_concurrent_insert_and_query() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    let store_clone = store.clone();
    let insert_handle = tokio::spawn(async move {
        for i in 0..1000 {
            let event = make_event(i, i, 1, i as u64);
            store_clone.insert(event);
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    });

    let store_clone = store.clone();
    let query_handle = tokio::spawn(async move {
        for i in 0..100 {
            let events = store_clone.query_by_kind(1, 100);
            assert!(events.len() <= 100);
            tokio::time::sleep(Duration::from_micros(500)).await;
        }
    });

    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(insert_handle, query_handle);
    })
    .await
    .expect("Test timed out - possible deadlock");
}

/// Test concurrent batch insert operations
#[tokio::test]
async fn test_concurrent_batch_insert() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    let mut handles = vec![];

    for thread_id in 0..10 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for batch in 0..100 {
                let events: Vec<Arc<_>> = (0..10)
                    .map(|i| {
                        make_event(
                            thread_id * 100 + batch * 10 + i,
                            thread_id * 100 + batch * 10 + i,
                            1,
                            (batch * 10 + i) as u64,
                        )
                    })
                    .collect();

                let (inserted, _) = store_clone.insert_batch(events);
                assert!(inserted <= 10);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }
}

/// Test concurrent insert and remove (eviction pattern)
#[tokio::test]
async fn test_concurrent_insert_and_eviction() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().to_str().unwrap().to_string();

    let config = StoreConfig {
        max_bytes: 1024 * 1024, // 1MB limit
        persistence_path: Some(path),
        use_wal: true,
    };

    let store = Arc::new(EventStore::new(config));

    let store_clone = store.clone();
    let insert_handle = tokio::spawn(async move {
        for i in 0..5000 {
            let event = make_event(i, i, 1, i as u64);
            store_clone.insert(event);
        }
    });

    let store_clone = store.clone();
    let eviction_handle = tokio::spawn(async move {
        // Simulate eviction by manually calling maybe_evict
        for _ in 0..100 {
            tokio::task::spawn_blocking({
                let store_clone = store_clone.clone();
                move || {
                    // Access the index directly for testing
                    let oldest = store_clone.index.get_oldest(100);
                    for event_ref in oldest.iter() {
                        store_clone.index.remove(&event_ref.id);
                    }
                }
            })
            .await
            .ok();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(insert_handle, eviction_handle);
    })
    .await
    .expect("Test timed out - possible deadlock");
}

/// Test concurrent replaceable event insertions
#[tokio::test]
async fn test_concurrent_replaceable_events() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    let mut handles = vec![];

    // Multiple threads inserting replaceable events for the same pubkey
    for pubkey_byte in 0..5 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for i in 0..200 {
                let event = make_event(pubkey_byte * 200 + i, pubkey_byte, 10000, i as u64);
                store_clone.insert(event);
                tokio::time::sleep(Duration::from_micros(50)).await;
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }

    // Verify only one event per pubkey
    for pubkey_byte in 0..5 {
        let events = store.query_by_pubkey(&[pubkey_byte; 32], 100);
        assert_eq!(
            events.len(),
            1,
            "Should have exactly 1 replaceable event per pubkey"
        );
    }
}

/// Test concurrent addressable event insertions (with d-tags)
#[tokio::test]
async fn test_concurrent_addressable_events() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    let mut handles = vec![];

    // Multiple threads inserting addressable events with same d-tag
    for thread_id in 0..5 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for i in 0..200 {
                let event = make_event_with_tag(
                    thread_id * 200 + i,
                    thread_id,
                    30000,
                    i as u64,
                    "d",
                    "profile",
                );
                store_clone.insert(event);
                tokio::time::sleep(Duration::from_micros(50)).await;
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }
}

/// Test WAL concurrent operations
#[tokio::test]
async fn test_concurrent_wal_operations() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().to_str().unwrap().to_string();

    let config = StoreConfig::with_persistence(0, path.clone());
    let store = Arc::new(EventStore::new(config));

    let mut insert_handles = vec![];

    // Concurrent inserts
    for i in 0..5 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for j in 0..100 {
                let event = make_event(i * 100 + j, i * 100 + j, 1, (i * 100 + j) as u64);
                store_clone.insert(event);
            }
        });
        insert_handles.push(handle);
    }

    // Concurrent save operations
    let save_handle = tokio::spawn({
        let store_clone = store.clone();
        async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let store_for_save = store_clone.clone();
                tokio::task::spawn_blocking(move || {
                    store_for_save.save_to_disk().ok();
                })
                .await
                .ok();
            }
        }
    });

    for handle in insert_handles {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }

    tokio::time::timeout(Duration::from_secs(5), save_handle)
        .await
        .expect("Test timed out - possible deadlock")
        .expect("Task failed");
}

/// Test heavy concurrent read/write with different index types
#[tokio::test]
async fn test_concurrent_multi_index_access() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    // Pre-populate with some events
    for i in 0..100 {
        let event = make_event_with_tag(i, i, i as u32 % 10, i as u64, "t", "test");
        store.insert(event);
    }

    let mut handles = vec![];

    // Concurrent queries on different indexes
    for i in 0..10 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for j in 0..100 {
                match j % 5 {
                    0 => {
                        let _ = store_clone.query_by_pubkey(&[i as u8; 32], 10);
                    }
                    1 => {
                        let _ = store_clone.query_by_kind(i as u32 % 10, 10);
                    }
                    2 => {
                        let _ = store_clone.query_by_tag('t', "test", 10);
                    }
                    3 => {
                        let event = make_event(200 + i * 100 + j, 200 + i, 1, j as u64);
                        store_clone.insert(event);
                    }
                    _ => {
                        let _ = store_clone.len();
                    }
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }
}

/// Test stress scenario: rapid insert/delete cycles
#[tokio::test]
async fn test_rapid_insert_delete_cycles() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    let mut handles = vec![];

    for thread_id in 0..8 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for i in 0..500 {
                let event = make_event(thread_id * 500 + i, thread_id, 1, i as u64);

                let event_id = event.id;
                store_clone.insert(event);

                // Delete immediately after insert
                store_clone.index.remove(&event_id);

                if i % 50 == 0 {
                    tokio::time::sleep(Duration::from_micros(100)).await;
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }
}

/// Test concurrent eviction task with heavy writes
#[tokio::test]
async fn test_eviction_task_concurrent_writes() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().to_str().unwrap().to_string();

    let config = StoreConfig {
        max_bytes: 512 * 1024, // 512KB limit
        persistence_path: Some(path),
        use_wal: true,
    };

    let store = Arc::new(EventStore::new(config));

    // Start eviction task
    store.start_eviction_task();

    let store_clone = store.clone();
    let write_handle = tokio::spawn(async move {
        for i in 0..10000 {
            let event = make_event(i, i, 1, i as u64);
            store_clone.insert(event);

            if i % 100 == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });

    tokio::time::timeout(Duration::from_secs(20), write_handle)
        .await
        .expect("Test timed out - possible deadlock")
        .expect("Task failed");
}

/// Test concurrent WAL replay simulation
#[tokio::test]
async fn test_concurrent_wal_replay_pattern() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().to_str().unwrap().to_string();

    let config = StoreConfig::with_persistence(0, path.clone());
    let store = Arc::new(EventStore::new(config));

    // Phase 1: Insert events
    for i in 0..100 {
        let event = make_event(i, i, 1, i as u64);
        store.insert(event);
    }

    // Phase 2: Concurrent operations during simulated replay
    let store_clone = store.clone();
    let read_handle = tokio::spawn(async move {
        for _ in 0..100 {
            let _ = store_clone.query_by_kind(1, 100);
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
    });

    let store_clone = store.clone();
    let write_handle = tokio::spawn(async move {
        for i in 100..200 {
            let event = make_event(i, i, 1, i as u64);
            store_clone.insert(event);
            tokio::time::sleep(Duration::from_micros(50)).await;
        }
    });

    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(read_handle, write_handle);
    })
    .await
    .expect("Test timed out - possible deadlock");
}

/// Test extreme concurrent load with many threads
#[tokio::test]
async fn test_extreme_concurrent_load() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    let mut handles = vec![];

    // Launch 20 concurrent threads doing mixed operations
    for thread_id in 0..20 {
        let store_clone = store.clone();
        let handle = tokio::spawn(async move {
            for i in 0..1000 {
                let op = i % 10;
                match op {
                    0..=5 => {
                        // Insert
                        let event =
                            make_event(thread_id * 1000 + i, thread_id, (i % 10) as u32, i as u64);
                        store_clone.insert(event);
                    }
                    6 => {
                        // Query by kind
                        let _ = store_clone.query_by_kind(i as u32 % 10, 50);
                    }
                    7 => {
                        // Query by pubkey
                        let _ = store_clone.query_by_pubkey(&[thread_id as u8; 32], 50);
                    }
                    8 => {
                        // Get by ID
                        let event = make_event(thread_id * 1000 + i, thread_id, 1, i as u64);
                        let _ = store_clone.get(&event.id);
                    }
                    9 => {
                        // Check length
                        let _ = store_clone.len();
                    }
                    _ => {}
                }

                if i % 100 == 0 {
                    tokio::time::sleep(Duration::from_micros(50)).await;
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("Test timed out - possible deadlock")
            .expect("Task failed");
    }
}

// ── Phase 2: Targeted race condition tests (true multi-threaded) ────────────

/// TOCTOU race in internal_remove: between set.is_empty() and by_pubkey.remove(),
/// another thread can insert, and the remove wipes their event.
/// Uses std::thread for true OS-level concurrency.
#[test]
fn test_toctou_remove_empty_set_race() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let pubkey_byte: u8 = 0xAA;

    // Pre-populate: insert 2 events with the same pubkey
    let e1 = make_event(1, pubkey_byte as u32, 1, 1000);
    let e2 = make_event(2, pubkey_byte as u32, 1, 2000);
    store.insert(e1.clone());
    store.insert(e2.clone());

    let mut pubkey = [0u8; 32];
    pubkey[31] = pubkey_byte;

    // Verify pubkey has 2 events
    let before = store.query_by_pubkey(&pubkey, 100);
    assert_eq!(before.len(), 2);

    let n_threads = 16;
    let iterations = 200;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for _ in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..iterations {
                // Insert a new event with the same pubkey
                let id = 1000 + i;
                let event = make_event(id, pubkey_byte as u32, 1, 3000 + id as u64);
                let event_id = event.id;
                store.insert(event);
                // Immediately remove it — this creates contention on the same pubkey set
                store.index.remove(&event_id);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // After all operations, verify consistency:
    // The set for this pubkey should have exactly the events we expect.
    // e1 and e2 should still be there, plus any that weren't removed.
    let mut pubkey = [0u8; 32];
    pubkey[31] = pubkey_byte;
    let remaining = store.query_by_pubkey(&pubkey, 1000);

    // Every event still in the store should be findable by ID
    for event in &remaining {
        assert!(
            store.get(&event.id).is_some(),
            "event in pubkey index but not in by_id"
        );
    }

    // The reverse: every event in by_id with this pubkey should appear in the pubkey query
    let all = store.index.iter_all();
    let all_for_pubkey: Vec<_> = all.iter().filter(|e| e.pubkey[31] == pubkey_byte).collect();
    for event in &all_for_pubkey {
        assert!(
            remaining.iter().any(|r| r.id == event.id),
            "event in by_id with pubkey but not in pubkey query"
        );
    }

    // Most importantly: event_count should match the actual number of events
    let actual_count = store.index.iter_all().len();
    assert_eq!(
        store.len(),
        actual_count,
        "event_count counter ({}) doesn't match actual events ({})",
        store.len(),
        actual_count
    );
}

/// Double-insert counter race: two threads inserting the same event ID can both pass
/// the duplicate check, causing memory_bytes and event_count to over-count.
#[test]
fn test_double_insert_counter_race() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    // Create a single event that many threads will try to insert
    let event = make_event(42, 1, 1, 1000);
    let event_json_size = event.raw.len();
    let event_id = event.id;

    let n_threads = 20;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for _ in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let event_json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
            format!("{:x}", 42u32),
            format!("{:x}", 1u32),
            1000u64,
            1u32,
            "0"
        );
        let handle = std::thread::spawn(move || {
            barrier.wait();
            let event =
                Arc::new(memlay::event::Event::from_json_unchecked(event_json.as_bytes()).unwrap());
            store.insert(event);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // The event should be stored exactly once
    assert!(store.get(&event_id).is_some(), "event should exist");

    // Check event count — this is the key assertion
    let actual_events = store.index.iter_all().len();
    assert_eq!(
        actual_events, 1,
        "should have exactly 1 event, but found {}",
        actual_events
    );

    // The event_count counter should match reality
    let reported_count = store.len();
    assert_eq!(
        reported_count, actual_events,
        "event_count ({}) doesn't match actual events ({}) - over-counting from race!",
        reported_count, actual_events
    );

    // Memory bytes should match one event's size, not N
    let reported_mem = store.memory_bytes();
    assert_eq!(
        reported_mem, event_json_size,
        "memory_bytes ({}) doesn't match single event size ({}) - over-counting from race!",
        reported_mem, event_json_size
    );
}

/// Secondary index consistency: after heavy concurrent insert/remove/query,
/// verify all indexes agree with each other.
#[test]
fn test_secondary_index_consistency_after_stress() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 12;
    let ops_per_thread = 500;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..ops_per_thread {
                let base = thread_id * ops_per_thread + i;
                let kind = (base % 5) as u32 + 1;
                let event = make_event_with_tag(
                    base as u32,
                    thread_id as u32,
                    kind,
                    base as u64,
                    "t",
                    "nostr",
                );
                let event_id = event.id;
                store.insert(event);

                // Mix in queries
                if i % 3 == 0 {
                    let mut pk = [0u8; 32];
                    pk[31] = thread_id as u8;
                    let _ = store.query_by_pubkey(&pk, 50);
                    let _ = store.query_by_kind(kind, 50);
                    let _ = store.query_by_tag('t', "nostr", 50);
                }

                // Remove some events (every other one)
                if i % 2 == 0 {
                    store.index.remove(&event_id);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Consistency check: every event in by_id should be in its secondary indexes
    let all_events = store.index.iter_all();
    for event in &all_events {
        // Should be findable by pubkey
        let mut pk = [0u8; 32];
        pk[31] = event.pubkey[31];
        let pk_results = store.query_by_pubkey(&pk, 10000);
        assert!(
            pk_results.iter().any(|e| e.id == event.id),
            "event {} (pubkey byte {}) missing from pubkey index",
            hex::encode(event.id),
            event.pubkey[31]
        );

        // Should be findable by kind
        let kind_results = store.query_by_kind(event.kind, 10000);
        assert!(
            kind_results.iter().any(|e| e.id == event.id),
            "event {} (kind {}) missing from kind index",
            hex::encode(event.id),
            event.kind
        );
    }

    // Reverse check: every event in pubkey index should be in by_id
    for thread_id in 0..n_threads {
        let mut pk = [0u8; 32];
        pk[31] = thread_id as u8;
        let pk_results = store.query_by_pubkey(&pk, 10000);
        for event in &pk_results {
            assert!(
                store.get(&event.id).is_some(),
                "event in pubkey index but not in by_id (orphaned secondary index entry)"
            );
        }
    }

    // Kind index reverse check
    for kind in 1..=5 {
        let kind_results = store.query_by_kind(kind, 10000);
        for event in &kind_results {
            assert!(
                store.get(&event.id).is_some(),
                "event in kind index but not in by_id (orphaned secondary index entry)"
            );
        }
    }

    // Counter consistency
    let actual = store.index.iter_all().len();
    assert_eq!(
        store.len(),
        actual,
        "event_count ({}) doesn't match actual ({})",
        store.len(),
        actual
    );
}

/// Replaceable events + eviction: verify replaceable event handling is correct
/// when eviction removes the old event before the insert can.
#[tokio::test]
async fn test_replaceable_events_with_eviction() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().to_str().unwrap().to_string();

    let config = StoreConfig {
        max_bytes: 512 * 1024,
        persistence_path: Some(path),
        use_wal: true,
    };

    let store = Arc::new(EventStore::new(config));
    store.start_eviction_task();

    let n_threads = 8;
    let iterations = 300;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            // Each thread uses a different pubkey so replaceable events are per-thread
            for i in 0..iterations {
                // Insert regular events to fill memory
                let regular = make_event(
                    (10000 + thread_id * 1000 + i) as u32,
                    thread_id as u32,
                    1,
                    (10000 + i) as u64,
                );
                store.insert(regular);

                // Insert replaceable events (kind 10000+) with same key
                let replaceable = make_event(
                    (20000 + thread_id * 1000 + i) as u32,
                    thread_id as u32,
                    10000,
                    (20000 + i) as u64,
                );
                store.insert(replaceable);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked - possible deadlock");
    }

    // Verify: for each thread's pubkey, there should be exactly 1 replaceable event (kind 10000)
    for thread_id in 0..n_threads {
        let mut pk = [0u8; 32];
        pk[31] = thread_id as u8;
        let kind_10000 = store
            .query_by_pubkey(&pk, 10000)
            .into_iter()
            .filter(|e| e.kind == 10000)
            .collect::<Vec<_>>();

        // Due to eviction, some pubkeys might have 0 replaceable events left.
        // But they should NEVER have more than 1.
        assert!(
            kind_10000.len() <= 1,
            "pubkey {} has {} replaceable events (kind 10000) - should be at most 1",
            thread_id,
            kind_10000.len()
        );
    }
}

/// Concurrent WAL compact + inserts: verify no data loss when WAL compaction
/// runs during heavy writes.
#[test]
fn test_concurrent_wal_compact_and_inserts() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().to_str().unwrap().to_string();

    let config = StoreConfig::with_persistence(0, path);
    let store = Arc::new(EventStore::new(config));

    // Phase 1: Insert events
    for i in 0..200 {
        store.insert(make_event(i, i, 1, i as u64));
    }
    store.save_to_disk().unwrap();

    // Phase 2: Concurrent inserts + WAL compaction
    let n_threads = 6;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            if thread_id == 0 {
                // Compaction thread: do save_to_disk (flushes WAL)
                for _ in 0..10 {
                    store.save_to_disk().ok();
                    std::thread::sleep(Duration::from_millis(5));
                }
            } else {
                // Insert threads
                for i in 0..100u32 {
                    let id = 200 + thread_id * 100 + i as usize;
                    store.insert(make_event(id as u32, thread_id as u32, 1, id as u64));
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked - possible deadlock");
    }

    // Verify: the store should have events and be consistent
    assert!(store.len() > 0, "store should have events");
    let actual = store.index.iter_all().len();
    assert_eq!(store.len(), actual, "event_count doesn't match actual");
}

/// Subscription cache consistency: verify queries return correct results
/// even with concurrent cache invalidation.
#[test]
fn test_subscription_cache_concurrent_invalidation() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let sub_mgr = Arc::new(SubscriptionManager::new(store.clone()));

    // Pre-populate
    for i in 0..50 {
        store.insert(make_event(i, i, 1, i as u64));
    }

    let n_threads = 8;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));
    let query_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let sub_mgr = sub_mgr.clone();
        let barrier = barrier.clone();
        let query_count = query_count.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..200 {
                if thread_id % 2 == 0 {
                    // Insert + invalidate
                    let event = make_event(
                        (50 + thread_id * 200 + i) as u32,
                        thread_id as u32,
                        1,
                        (50 + i) as u64,
                    );
                    store.insert(event);
                    sub_mgr.invalidate_cache();
                } else {
                    // Query
                    let filter = Filter {
                        kinds: Some(vec![1]),
                        limit: Some(100),
                        ..Default::default()
                    };
                    let results = sub_mgr.query_filter(&filter);
                    // Results should always be non-negative (no panics)
                    assert!(results.len() <= 10000);
                    query_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // After all operations, verify the subscription manager still works correctly
    let filter = Filter {
        kinds: Some(vec![1]),
        limit: Some(1000),
        ..Default::default()
    };
    let results = sub_mgr.query_filter(&filter);
    let all_events = store.index.iter_all();
    let kind_1_count = all_events.iter().filter(|e| e.kind == 1).count();
    assert!(
        results.len() <= kind_1_count,
        "query returned {} results but only {} kind-1 events exist",
        results.len(),
        kind_1_count
    );
}

/// Insert during get_oldest: verify eviction + concurrent insert doesn't deadlock
/// or lose events unexpectedly.
#[test]
fn test_insert_during_get_oldest_eviction() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    // Pre-fill with events
    for i in 0..500 {
        store.insert(make_event(i, i, 1, i as u64));
    }

    let n_threads = 10;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..500 {
                if thread_id == 0 {
                    // Eviction thread: mimic what maybe_evict does
                    let oldest = store.index.get_oldest(50);
                    for event_ref in &oldest {
                        store.index.remove(&event_ref.id);
                    }
                    std::thread::sleep(Duration::from_micros(100));
                } else {
                    // Insert threads
                    let event = make_event(
                        (500 + thread_id * 500 + i) as u32,
                        thread_id as u32,
                        1,
                        (500 + i) as u64,
                    );
                    store.insert(event);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked - possible deadlock");
    }

    // Verify no counter drift
    let actual = store.index.iter_all().len();
    assert_eq!(
        store.len(),
        actual,
        "event_count ({}) doesn't match actual ({}) after eviction+insert stress",
        store.len(),
        actual
    );
}

/// Heavy lock contention: many threads doing insert+remove+query cycles
/// on overlapping keys to stress-test lock ordering.
#[test]
fn test_heavy_lock_contention_no_deadlock() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 24;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..300 {
                let base = thread_id * 300 + i;
                let kind = (base % 8) as u32;

                // All threads use the same small set of pubkeys (high contention)
                let pubkey = (thread_id % 4) as u32;

                let event = make_event(base as u32, pubkey, kind, base as u64);
                let event_id = event.id;

                store.insert(event);

                // Query overlapping indexes
                let mut pk = [0u8; 32];
                pk[31] = pubkey as u8;
                let _ = store.query_by_pubkey(&pk, 50);
                let _ = store.query_by_kind(kind, 50);

                // Remove some
                if i % 3 == 0 {
                    store.index.remove(&event_id);
                }
            }
        });
        handles.push(handle);
    }

    // If any thread deadlocks, the test framework will time out the whole process
    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|_| panic!("Thread {i} panicked - possible deadlock"));
    }
}

/// Stress test: concurrent replaceable events with identical replacement keys.
/// This specifically targets the atomic swap + internal_remove path.
#[test]
fn test_concurrent_identical_replaceable_keys() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 16;
    let iterations = 200;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..iterations {
                // All threads use pubkey=1, kind=10000 — identical replacement key
                let event = make_event(
                    (thread_id * 1000 + i) as u32,
                    1,     // same pubkey
                    10000, // same replaceable kind
                    (thread_id * 1000 + i) as u64,
                );
                store.insert(event);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // There should be at most 1 replaceable event for (pubkey=1, kind=10000)
    let mut pk = [0u8; 32];
    pk[31] = 1;
    let kind_10000_events: Vec<_> = store
        .query_by_pubkey(&pk, 10000)
        .into_iter()
        .filter(|e| e.kind == 10000)
        .collect();

    assert!(
        kind_10000_events.len() <= 1,
        "expected at most 1 replaceable event, got {} — race condition in replaceable handling",
        kind_10000_events.len()
    );
}

/// Verify memory_bytes stays consistent: no double-counting from concurrent operations.
#[test]
fn test_memory_bytes_consistency_under_concurrency() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 8;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..200 {
                let event = make_event(
                    (thread_id * 200 + i) as u32,
                    thread_id as u32,
                    1,
                    (thread_id * 200 + i) as u64,
                );
                store.insert(event);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Compute actual memory from stored events
    let actual_mem: usize = store.index.iter_all().iter().map(|e| e.raw.len()).sum();
    let reported_mem = store.memory_bytes();

    assert_eq!(
        reported_mem, actual_mem,
        "memory_bytes ({}) doesn't match sum of raw.len() ({}) — counter race!",
        reported_mem, actual_mem
    );
}

/// Stress test: concurrent insert+remove on events with generic tags (by_tag_other).
/// The nested DashMap<char, DashMap<String, RwLock<BTreeSet>>> is especially prone
/// to races: outer entry creation, inner entry creation, and BTreeSet modification
/// can interleave with removes.
#[test]
fn test_concurrent_generic_tag_index_consistency() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 16;
    let iterations = 200;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..iterations {
                let id = (thread_id * 1000 + i) as u32;
                let tag_value = format!("topic{}", i % 5);
                let event =
                    make_event_with_tag(id, thread_id as u32, 1, id as u64, "t", &tag_value);
                let event_id = event.id;
                store.insert(event);

                if i % 3 == 0 {
                    store.index.remove(&event_id);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    for topic_idx in 0..5 {
        let tag_value = format!("topic{}", topic_idx);
        let results = store.query_by_tag('t', &tag_value, 100000);
        for event in &results {
            assert!(
                store.get(&event.id).is_some(),
                "event in tag index 't'='{}' but not in by_id — orphaned tag entry",
                tag_value,
            );
        }
    }

    let actual = store.index.iter_all().len();
    assert_eq!(
        store.len(),
        actual,
        "event_count ({}) doesn't match actual ({}) after tag stress",
        store.len(),
        actual
    );
}

/// Stress test: concurrent insert+remove with e-tags and p-tags.
/// Verifies that the dedicated e-tag and p-tag indexes stay consistent
/// under concurrent modification.
#[test]
fn test_concurrent_e_p_tag_index_consistency() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 12;
    let iterations = 150;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..iterations {
                let id = (thread_id * 1000 + i) as u32;
                let ref_id = format!("{:0>64x}", i % 3);
                let tag_type = if thread_id % 2 == 0 { "e" } else { "p" };
                let event =
                    make_event_with_tag(id, thread_id as u32, 1, id as u64, tag_type, &ref_id);
                let event_id = event.id;
                store.insert(event);

                if i % 4 == 0 {
                    store.index.remove(&event_id);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    for ref_idx in 0..3u32 {
        let mut ref_id = [0u8; 32];
        ref_id[31] = ref_idx as u8;

        let e_results = store.query_by_e_tag(&ref_id, 100000);
        for event in &e_results {
            assert!(
                store.get(&event.id).is_some(),
                "event in e-tag index but not in by_id"
            );
        }

        let p_results = store.query_by_p_tag(&ref_id, 100000);
        for event in &p_results {
            assert!(
                store.get(&event.id).is_some(),
                "event in p-tag index but not in by_id"
            );
        }
    }

    let actual = store.index.iter_all().len();
    assert_eq!(
        store.len(),
        actual,
        "event_count ({}) doesn't match actual ({})",
        store.len(),
        actual
    );
}

/// Stress test: subscription cache consistency under heavy concurrent load.
/// After all mutations stop and cache is invalidated, a fresh query must
/// return only events that actually exist in the store.
#[test]
fn test_subscription_cache_post_quiesce_consistency() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let sub_mgr = Arc::new(SubscriptionManager::new(store.clone()));

    for i in 0..100 {
        store.insert(make_event(i, i % 4, 1, i as u64));
    }

    let n_threads = 12;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let sub_mgr = sub_mgr.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..300 {
                match thread_id % 3 {
                    0 => {
                        let event = make_event(
                            (100 + thread_id * 300 + i) as u32,
                            (thread_id % 4) as u32,
                            1,
                            (100 + i) as u64,
                        );
                        store.insert(event);
                    }
                    1 => {
                        let id_to_remove = (100 + ((thread_id - 1) * 300) + i) as u32;
                        let event = make_event(
                            id_to_remove,
                            ((thread_id - 1) % 4) as u32,
                            1,
                            (100 + i) as u64,
                        );
                        store.index.remove(&event.id);
                        sub_mgr.invalidate_cache();
                    }
                    _ => {
                        let filter = Filter {
                            kinds: Some(vec![1]),
                            limit: Some(500),
                            ..Default::default()
                        };
                        let _ = sub_mgr.query_filter(&filter);
                    }
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // After quiescence: invalidate cache and verify a fresh query is consistent
    sub_mgr.invalidate_cache();
    let filter = Filter {
        kinds: Some(vec![1]),
        limit: Some(100000),
        ..Default::default()
    };
    let results = sub_mgr.query_filter(&filter);
    for event in &results {
        assert!(
            store.get(&event.id).is_some(),
            "fresh query after quiescence returned event not in store"
        );
    }
}

/// Stress test: by_oldest contention with simultaneous get_oldest reads and inserts.
/// This tests the global RwLock<BTreeSet> under heavy write+read contention.
#[test]
fn test_by_oldest_heavy_contention() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));

    for i in 0..200 {
        store.insert(make_event(i, i, 1, i as u64));
    }

    let n_threads = 16;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..500 {
                match thread_id % 4 {
                    0 => {
                        let oldest = store.index.get_oldest(20);
                        for er in &oldest {
                            store.index.remove(&er.id);
                        }
                    }
                    1 => {
                        let _ = store.index.get_oldest(100);
                    }
                    _ => {
                        let event = make_event(
                            (200 + thread_id * 500 + i) as u32,
                            thread_id as u32,
                            1,
                            (200 + i) as u64,
                        );
                        store.insert(event);
                    }
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle
            .join()
            .expect("Thread panicked — possible deadlock on by_oldest");
    }

    let actual = store.index.iter_all().len();
    assert_eq!(
        store.len(),
        actual,
        "event_count ({}) doesn't match actual ({}) after by_oldest contention",
        store.len(),
        actual
    );
}

/// Concurrent addressable events (kind 30000+) with same d-tag.
/// Tests the ReplacementKey::Addressable path.
#[test]
fn test_concurrent_addressable_same_d_tag() {
    let store = Arc::new(EventStore::new(StoreConfig::default()));
    let n_threads = 12;
    let iterations = 100;
    let barrier = Arc::new(std::sync::Barrier::new(n_threads));

    let mut handles = vec![];
    for thread_id in 0..n_threads {
        let store = store.clone();
        let barrier = barrier.clone();
        let handle = std::thread::spawn(move || {
            barrier.wait();
            for i in 0..iterations {
                // Same pubkey, same kind, same d-tag — identical addressable key
                let event = make_event_with_tag(
                    (thread_id * 1000 + i) as u32,
                    1,     // same pubkey
                    30000, // addressable kind
                    (thread_id * 1000 + i) as u64,
                    "d",
                    "profile", // same d-tag
                );
                store.insert(event);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Should have at most 1 addressable event for (pubkey=1, kind=30000, d="profile")
    let mut pk = [0u8; 32];
    pk[31] = 1;
    let addressable: Vec<_> = store
        .query_by_pubkey(&pk, 10000)
        .into_iter()
        .filter(|e| e.kind == 30000)
        .collect();

    assert!(
        addressable.len() <= 1,
        "expected at most 1 addressable event, got {} — race condition in addressable handling",
        addressable.len()
    );
}
