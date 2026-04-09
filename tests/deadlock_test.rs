use memlay::store::{EventStore, StoreConfig};
use std::sync::Arc;
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

fn make_event_with_tag(id: u32, pubkey: u32, kind: u32, created_at: u64, tag_type: &str, tag_value: &str) -> Arc<memlay::event::Event> {
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
    }).await.expect("Test timed out - possible deadlock");
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
                        make_event(thread_id * 100 + batch * 10 + i, thread_id * 100 + batch * 10 + i, 1, (batch * 10 + i) as u64)
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
            }).await.ok();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(insert_handle, eviction_handle);
    }).await.expect("Test timed out - possible deadlock");
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
        assert_eq!(events.len(), 1, "Should have exactly 1 replaceable event per pubkey");
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
                let event = make_event_with_tag(thread_id * 200 + i, thread_id, 30000, i as u64, "d", "profile");
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
                }).await.ok();
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
    }).await.expect("Test timed out - possible deadlock");
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
                        let event = make_event(thread_id * 1000 + i, thread_id, (i % 10) as u32, i as u64);
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
