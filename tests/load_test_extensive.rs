use memlay::{config::Config, relay::Relay};
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::net::TcpListener;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpListener as TokioTcpListener;

async fn spawn_relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let url = format!("ws://127.0.0.1:{}", addr.port());
    let relay = Relay::new(Config::default());
    let router = relay.router();

    let tcp = TokioTcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(
            tcp,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    url
}

/// Generate a pool of keys for testing
fn generate_key_pool(count: usize) -> Vec<Keys> {
    (0..count).map(|_| Keys::generate()).collect()
}

/// Load test data structure
struct LoadTestData {
    total_events: usize,
    by_kind: HashMap<u16, usize>,
    authors: Vec<Keys>,
    #[allow(dead_code)]
    start_time: u64,
    #[allow(dead_code)]
    end_time: u64,
}

/// Populate the relay with diverse test data
async fn populate_relay_with_data(url: &str, target_events: usize) -> LoadTestData {
    println!(
        "\n=== Phase 1: Populating relay with {} events ===",
        target_events
    );
    let start_time = Instant::now();

    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut by_kind: HashMap<u16, usize> = HashMap::new();
    let now = Timestamp::now().as_secs();
    let start_time_filter = now - 86400; // 24 hours ago

    // Define event distribution - using available kinds
    let kind_distribution = vec![
        (Kind::Metadata, 500),  // Profile data
        (Kind::TextNote, 7000), // Regular notes
        (Kind::Repost, 1000),   // Reposts
        (Kind::Reaction, 1500), // Reactions
    ];

    let mut total_sent = 0;

    for (kind, count) in &kind_distribution {
        for _ in 0..*count {
            let event_builder = match kind {
                Kind::Metadata => {
                    let metadata = Metadata::new()
                        .name(format!("user_{}", total_sent))
                        .display_name(format!("Display {}", total_sent))
                        .about(format!("Bio for user {}", total_sent));
                    EventBuilder::metadata(&metadata)
                }
                Kind::TextNote => {
                    let content = format!(
                        "Test note {} by author. Hashtags: #memlay #nostr #test",
                        total_sent
                    );
                    EventBuilder::text_note(&content)
                }
                Kind::Repost => EventBuilder::text_note(&format!("Repost event {}", total_sent)),
                Kind::Reaction => {
                    EventBuilder::text_note(&format!("Reaction event {}", total_sent))
                }
                _ => EventBuilder::text_note(&format!("Generic event {}", total_sent)),
            };

            // Send the event
            let _ = client.send_event_builder(event_builder).await;
            total_sent += 1;

            // Progress update
            if total_sent % 2000 == 0 {
                println!("  Sent {} events...", total_sent);
            }

            if total_sent >= target_events {
                break;
            }
        }

        if total_sent >= target_events {
            break;
        }
    }

    // Update by_kind counts
    for (kind, count) in &kind_distribution {
        let actual_count = *count.min(&(target_events / kind_distribution.len()));
        by_kind.insert(kind.as_u16(), actual_count);
    }

    // Wait for all events to be processed
    println!("Waiting for events to be processed...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    let elapsed = start_time.elapsed();
    println!(
        "Populated {} events in {:?} ({:.0} events/sec)",
        total_sent,
        elapsed,
        total_sent as f64 / elapsed.as_secs_f64()
    );

    LoadTestData {
        total_events: total_sent,
        by_kind,
        authors: generate_key_pool(50),
        start_time: start_time_filter,
        end_time: now,
    }
}

/// Run query load tests
async fn run_query_load_tests(url: &str, data: &LoadTestData, num_clients: usize) {
    println!(
        "\n=== Phase 2: Query load test with {} concurrent clients ===",
        num_clients
    );
    let start_time = Instant::now();

    // Define various query patterns
    let query_patterns = vec![
        ("All notes", Filter::new().kind(Kind::TextNote).limit(100)),
        (
            "Recent notes",
            Filter::new()
                .kind(Kind::TextNote)
                .since(Timestamp::from_secs(Timestamp::now().as_secs() - 3600))
                .limit(50),
        ),
        ("Metadata", Filter::new().kind(Kind::Metadata).limit(50)),
        ("Reactions", Filter::new().kind(Kind::Reaction).limit(50)),
        ("Reposts", Filter::new().kind(Kind::Repost).limit(50)),
        ("Any kind", Filter::new().limit(50)),
        (
            "Author based",
            Filter::new().author(data.authors[0].public_key()).limit(50),
        ),
    ];

    let mut handles = vec![];

    for client_id in 0..num_clients {
        let url = url.to_string();
        let pattern_idx = client_id % query_patterns.len();
        let pattern = query_patterns[pattern_idx].clone();

        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut ops = 0;
            let mut total_events = 0;
            let num_queries = 5; // 5 queries per client

            for _ in 0..num_queries {
                match client
                    .fetch_events(pattern.1.clone(), Duration::from_secs(3))
                    .await
                {
                    Ok(events) => {
                        ops += 1;
                        total_events += events.len();
                    }
                    Err(e) => {
                        eprintln!("Client {} query error: {:?}", client_id, e);
                    }
                }
            }

            client.shutdown().await;
            (client_id, ops, total_events)
        });

        handles.push(handle);
    }

    // Wait for all clients
    let mut total_ops = 0;
    let mut total_events = 0;
    let mut completed = 0;

    for handle in handles {
        match handle.await {
            Ok((client_id, ops, events)) => {
                total_ops += ops;
                total_events += events;
                completed += 1;
                if client_id % 100 == 0 {
                    println!(
                        "Client {} completed {} queries, got {} events",
                        client_id, ops, events
                    );
                }
            }
            Err(e) => {
                eprintln!("Task failed: {:?}", e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n=== Query Results ===");
    println!("Completed clients: {}/{}", completed, num_clients);
    println!("Total queries: {}", total_ops);
    println!("Total events returned: {}", total_events);
    println!("Time: {:.2?}", elapsed);
    println!("Queries/sec: {}", total_ops as f64 / elapsed.as_secs_f64());
    println!(
        "Events/sec: {}",
        total_events as f64 / elapsed.as_secs_f64()
    );
}

/// Run insert load tests
async fn run_insert_load_tests(url: &str, num_clients: usize, inserts_per_client: usize) {
    println!("\n=== Phase 3: Insert load test ===");
    let start_time = Instant::now();

    let mut handles = vec![];

    for client_id in 0..num_clients {
        let url = url.to_string();

        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut inserted = 0;

            for i in 0..inserts_per_client {
                let content = format!(
                    "Insert test {} by client {} at timestamp {}",
                    i,
                    client_id,
                    Timestamp::now().as_secs()
                );
                let builder = EventBuilder::text_note(&content);

                match client.send_event_builder(builder).await {
                    Ok(_) => inserted += 1,
                    Err(e) => {
                        eprintln!("Client {} insert error: {:?}", client_id, e);
                    }
                }
            }

            client.shutdown().await;
            (client_id, inserted)
        });

        handles.push(handle);
    }

    let mut total_inserted = 0;
    let mut completed = 0;

    for handle in handles {
        match handle.await {
            Ok((client_id, inserted)) => {
                total_inserted += inserted;
                completed += 1;
                if client_id % 100 == 0 {
                    println!("Client {} inserted {} events", client_id, inserted);
                }
            }
            Err(e) => {
                eprintln!("Task failed: {:?}", e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n=== Insert Results ===");
    println!("Completed clients: {}/{}", completed, num_clients);
    println!("Total inserted: {}", total_inserted);
    println!("Time: {:.2?}", elapsed);
    println!(
        "Inserts/sec: {}",
        total_inserted as f64 / elapsed.as_secs_f64()
    );
}

/// Run mixed read/write load test
async fn run_mixed_load_test(url: &str, num_clients: usize) {
    println!("\n=== Phase 4: Mixed read/write load test ===");
    let start_time = Instant::now();

    let mut handles = vec![];

    for client_id in 0..num_clients {
        let url = url.to_string();

        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let mut ops = 0;
            let num_rounds = 10;

            for _ in 0..num_rounds {
                // Query
                let filter = Filter::new().kind(Kind::TextNote).limit(10);
                if let Ok(_events) = client.fetch_events(filter, Duration::from_secs(2)).await {
                    ops += 1;
                }

                // Insert
                let builder = EventBuilder::text_note(&format!(
                    "Mixed load event {} by client {}",
                    ops, client_id
                ));
                if let Ok(_) = client.send_event_builder(builder).await {
                    ops += 1;
                }
            }

            client.shutdown().await;
            (client_id, ops)
        });

        handles.push(handle);
    }

    let mut total_ops = 0;
    let mut completed = 0;

    for handle in handles {
        match handle.await {
            Ok((client_id, ops)) => {
                total_ops += ops;
                completed += 1;
                if client_id % 100 == 0 {
                    println!("Client {} completed {} ops", client_id, ops);
                }
            }
            Err(e) => {
                eprintln!("Task failed: {:?}", e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n=== Mixed Load Results ===");
    println!("Completed clients: {}/{}", completed, num_clients);
    println!("Total operations: {}", total_ops);
    println!("Time: {:.2?}", elapsed);
    println!("Ops/sec: {}", total_ops as f64 / elapsed.as_secs_f64());
}

#[tokio::test]
async fn test_extensive_load() {
    let url = spawn_relay().await;

    // Populate with 10k events
    let data = populate_relay_with_data(&url, 10000).await;

    println!("\n=== Data Summary ===");
    println!("Total events: {}", data.total_events);
    println!("By kind: {:?}", data.by_kind);
    println!("Authors in pool: {}", data.authors.len());

    // Run query load test with 500 clients
    run_query_load_tests(&url, &data, 500).await;

    // Run insert load test with 200 clients
    run_insert_load_tests(&url, 200, 50).await;

    // Run mixed load test with 300 clients
    run_mixed_load_test(&url, 300).await;

    println!("\n=== Full Load Test Complete ===");
}

#[tokio::test]
async fn test_stress_10k_clients() {
    let url = spawn_relay().await;

    // Quick population with 10k events
    println!("\n=== Quick Population ===");
    let keys = Keys::generate();
    let client = Client::new(keys);
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let now = Timestamp::now().as_secs();

    for i in 0..10000 {
        let content = format!("Stress test event {} with timestamp {}", i, now);
        let builder = EventBuilder::text_note(&content);
        let _ = client.send_event_builder(builder).await;

        if i % 2000 == 0 {
            println!("  Sent {} events...", i);
        }
    }

    println!("Waiting for events to be processed...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Stress test with 1000 concurrent clients
    println!("\n=== Stress Test: 1000 concurrent clients ===");
    let start_time = Instant::now();

    let mut handles = vec![];

    for client_id in 0..1000 {
        let url = url.clone();

        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(20)).await;

            let mut ops = 0;

            // Do 3 queries and 2 inserts
            for _ in 0..3 {
                let filter = Filter::new().kind(Kind::TextNote).limit(50);
                if let Ok(_events) = client.fetch_events(filter, Duration::from_secs(2)).await {
                    ops += 1;
                }
            }

            for _ in 0..2 {
                let builder =
                    EventBuilder::text_note(&format!("Stress event by client {}", client_id));
                if let Ok(_) = client.send_event_builder(builder).await {
                    ops += 1;
                }
            }

            client.shutdown().await;
            (client_id, ops)
        });

        handles.push(handle);
    }

    let mut total_ops = 0;
    let mut completed = 0;

    for handle in handles {
        match handle.await {
            Ok((_client_id, ops)) => {
                total_ops += ops;
                completed += 1;
            }
            Err(e) => {
                eprintln!("Task failed: {:?}", e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n=== Stress Test Results ===");
    println!("Completed clients: {}/{}", completed, 1000);
    println!("Total operations: {}", total_ops);
    println!("Time: {:.2?}", elapsed);
    println!("Ops/sec: {}", total_ops as f64 / elapsed.as_secs_f64());

    assert!(
        elapsed.as_secs() < 120,
        "Stress test took too long: {:?}",
        elapsed
    );
}

/// Test with varying filter combinations
#[tokio::test]
async fn test_varying_filters() {
    let url = spawn_relay().await;

    // Populate with 10k events
    println!("\n=== Populating with 10k events ===");
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let now = Timestamp::now().as_secs();
    for i in 0..10000 {
        let content = format!("Filter test event {} kind_{}", i, i % 4); // Mix of kinds
        let kind = Kind::TextNote; // Use text notes for simplicity
        let builder = EventBuilder::new(kind, &content);
        let _ = client.send_event_builder(builder).await;

        if i % 2000 == 0 {
            println!("  Sent {} events...", i);
        }
    }

    println!("Waiting for events to be processed...");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Test various filter combinations
    let filter_tests = vec![
        ("Single kind", Filter::new().kind(Kind::TextNote).limit(100)),
        (
            "Multiple kinds",
            Filter::new()
                .kinds([Kind::TextNote, Kind::Metadata])
                .limit(100),
        ),
        (
            "Time range",
            Filter::new()
                .since(Timestamp::from_secs(now - 3600))
                .limit(100),
        ),
        ("Limit only", Filter::new().limit(50)),
        (
            "Authors",
            Filter::new().authors([keys.public_key()]).limit(100),
        ),
    ];

    println!("\n=== Testing filter combinations ===");
    for (name, filter) in filter_tests {
        let test_client = Client::new(Keys::generate());
        test_client.add_relay(&url).await.unwrap();
        test_client.connect().await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let start = Instant::now();
        match test_client
            .fetch_events(filter, Duration::from_secs(3))
            .await
        {
            Ok(events) => {
                println!(
                    "  {}: {} events in {:?}",
                    name,
                    events.len(),
                    start.elapsed()
                );
            }
            Err(e) => {
                eprintln!("  {} error: {:?}", name, e);
            }
        }
        test_client.shutdown().await;
    }

    println!("\n=== Filter test complete ===");
}

/// Test with 2000 concurrent clients doing simple operations
#[tokio::test]
async fn test_scale_2000_clients() {
    let url = spawn_relay().await;

    // Quick population
    println!("\n=== Quick Population ===");
    let keys = Keys::generate();
    let client = Client::new(keys);
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..5000 {
        let builder = EventBuilder::text_note(&format!("Scale test event {}", i));
        let _ = client.send_event_builder(builder).await;
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Scale test with 2000 clients
    println!("\n=== Scale Test: 2000 concurrent clients ===");
    let start_time = Instant::now();

    let mut handles = vec![];

    for client_id in 0..2000 {
        let url = url.clone();

        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(10)).await;

            let mut ops = 0;

            // 2 queries
            for _ in 0..2 {
                let filter = Filter::new().kind(Kind::TextNote).limit(10);
                if let Ok(_) = client.fetch_events(filter, Duration::from_secs(1)).await {
                    ops += 1;
                }
            }

            client.shutdown().await;
            (client_id, ops)
        });

        handles.push(handle);
    }

    let mut total_ops = 0;
    let mut completed = 0;

    for handle in handles {
        match handle.await {
            Ok((_, ops)) => {
                total_ops += ops;
                completed += 1;
            }
            Err(e) => {
                eprintln!("Task failed: {:?}", e);
            }
        }
    }

    let elapsed = start_time.elapsed();
    println!("\n=== Scale Test Results ===");
    println!("Completed clients: {}/{}", completed, 2000);
    println!("Total operations: {}", total_ops);
    println!("Time: {:.2?}", elapsed);
    println!("Ops/sec: {}", total_ops as f64 / elapsed.as_secs_f64());

    assert!(
        elapsed.as_secs() < 120,
        "Scale test took too long: {:?}",
        elapsed
    );
}

/// Test that specifically targets the deadlock pattern: many concurrent REQs
/// with high event counts that fill the channel buffer
#[tokio::test]
async fn test_no_deadlock_under_high_load() {
    let url = spawn_relay().await;

    // Populate with events first
    println!("\n=== Populating relay ===");
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..5000 {
        let builder = EventBuilder::text_note(&format!("Load test event {}", i));
        let _ = client.send_event_builder(builder).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Now send many concurrent REQs that will try to fetch all events
    // This is the pattern that causes deadlock
    println!("\n=== Testing concurrent REQs (deadlock trigger) ===");
    let start = Instant::now();

    let mut handles = vec![];
    for i in 0..100 {
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Fetch ALL events - this will generate many messages
            let filter = Filter::new().limit(1000);
            match tokio::time::timeout(
                Duration::from_secs(10),
                client.fetch_events(filter, Duration::from_secs(10)),
            )
            .await
            {
                Ok(Ok(events)) => (i, events.len(), 0),
                Ok(Err(_)) => (i, 0, 1),
                Err(_) => (i, 0, 2),
            }
        });
        handles.push(handle);
    }

    let mut completed = 0;
    let mut total_events = 0;
    for handle in handles {
        match handle.await {
            Ok((_, events, status)) => {
                completed += 1;
                total_events += events;
                if status != 0 {
                    eprintln!("Client got status: {}", status);
                }
            }
            Err(e) => eprintln!("Task failed: {:?}", e),
        }
    }

    let elapsed = start.elapsed();
    println!(
        "Completed {} clients in {:?}, {} total events",
        completed, elapsed, total_events
    );

    // If we deadlock, this will timeout (120s is the test timeout)
    // If fixed, it should complete in reasonable time
    assert!(
        elapsed.as_secs() < 60,
        "Deadlock suspected: took {:?}",
        elapsed
    );
    assert_eq!(completed, 100, "Not all clients completed");
}

/// Test for deadlock with extreme concurrent REQs - more aggressive than test_no_deadlock_under_high_load
/// This test sends many concurrent REQs that each fetch large result sets, filling the send channel
#[tokio::test]
async fn test_extreme_concurrent_reqs() {
    let url = spawn_relay().await;

    // Populate with events first
    println!("\n=== Populating relay with 10k events ===");
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send 10,000 events
    for i in 0..10000 {
        let builder = EventBuilder::text_note(&format!("Extreme load test event {}", i));
        let _ = client.send_event_builder(builder).await;
        if i % 2000 == 0 {
            println!("  Sent {} events...", i);
        }
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("  Done populating");

    // Now send many concurrent REQs that will try to fetch all events
    // This is the pattern that causes deadlock when channel fills up
    println!("\n=== Testing extreme concurrent REQs (50 clients, 5k events each) ===");
    let start = Instant::now();

    let mut handles = vec![];
    for i in 0..50 {
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Fetch ALL events - this will generate many messages and fill the channel
            let filter = Filter::new().limit(5000);
            match tokio::time::timeout(
                Duration::from_secs(30),
                client.fetch_events(filter, Duration::from_secs(30)),
            )
            .await
            {
                Ok(Ok(events)) => (i, events.len(), 0),
                Ok(Err(_)) => (i, 0, 1),
                Err(_) => (i, 0, 2),
            }
        });
        handles.push(handle);
    }

    let mut completed = 0;
    let mut total_events = 0;
    let mut timeouts = 0;
    for handle in handles {
        match handle.await {
            Ok((_, events, status)) => {
                completed += 1;
                total_events += events;
                if status == 2 {
                    timeouts += 1;
                }
            }
            Err(e) => eprintln!("Task failed: {:?}", e),
        }
    }

    let elapsed = start.elapsed();
    println!(
        "Completed {} clients in {:?}, {} total events, {} timeouts",
        completed, elapsed, total_events, timeouts
    );

    // If we deadlock, this will timeout (120s is the test timeout)
    // With the fix, it should complete within reasonable time even if some clients timeout
    assert!(
        elapsed.as_secs() < 90,
        "Possible deadlock: took {:?}",
        elapsed
    );
    assert!(
        completed > 40,
        "Most clients should complete, got {}",
        completed
    );
}

/// Test for broadcast channel backpressure under high event volume
/// This reproduces the production issue where many concurrent clients + rapid event publishing
/// saturates the broadcast channel (8192 capacity), causing dropped events and apparent lockup
#[tokio::test]
async fn test_broadcast_backpressure() {
    let url = spawn_relay().await;

    // Phase 1: Connect many clients with subscriptions
    println!("\n=== Phase 1: Connecting 50 clients with subscriptions ===");
    let mut clients = Vec::new();
    let mut handles = Vec::new();

    for i in 0..50 {
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;

            // Subscribe to everything - this makes them all broadcast subscribers
            let filter = Filter::new().limit(100);
            client.subscribe(filter, None).await.unwrap();

            // Give time for subscription to register
            tokio::time::sleep(Duration::from_millis(50)).await;

            (i, client)
        });
        handles.push(handle);
    }

    // Wait for all clients to connect and subscribe
    for handle in handles {
        let (id, client) = handle.await.unwrap();
        clients.push((id, client));
    }
    println!("  Connected {} clients with subscriptions", clients.len());

    // Phase 2: Rapid event publishing to saturate broadcast channel
    println!("\n=== Phase 2: Publishing 2k events rapidly (backpressure trigger) ===");
    let start = Instant::now();

    let pub_keys = Keys::generate();
    let pub_client = Client::new(pub_keys);
    pub_client.add_relay(&url).await.unwrap();
    pub_client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut sent = 0;
    let mut dropped = 0;
    let mut failed = 0;

    // Publish events as fast as possible
    for i in 0..2000 {
        let builder = EventBuilder::text_note(&format!("Backpressure test event {}", i));
        match pub_client.send_event_builder(builder).await {
            Ok(_) => {
                sent += 1;
                if sent % 500 == 0 {
                    println!("  Sent {} events...", sent);
                }
            }
            Err(e) => {
                // Check if it's a timeout (backpressure indicator)
                if format!("{:?}", e).contains("timeout") {
                    dropped += 1;
                } else {
                    failed += 1;
                }
                if dropped % 100 == 0 && dropped > 0 {
                    println!(
                        "  Timeout/dropped: {} (sent: {}, failed: {})",
                        dropped, sent, failed
                    );
                }
            }
        }
    }

    let elapsed = start.elapsed();
    println!("  Published {} events in {:?}", sent, elapsed);
    println!("  Timeouts/dropped: {}, Failed: {}", dropped, failed);

    // Phase 3: Check if clients are still responsive
    println!("\n=== Phase 3: Checking client responsiveness ===");
    let mut responsive = 0;
    for (_id, client) in clients.into_iter() {
        // Try to fetch a small set - if client is deadlocked, this will timeout
        let filter = Filter::new().limit(10);
        match tokio::time::timeout(
            Duration::from_secs(5),
            client.fetch_events(filter, Duration::from_secs(5)),
        )
        .await
        {
            Ok(Ok(_events)) => {
                responsive += 1;
                if responsive % 10 == 0 {
                    println!("  Responsive clients: {}", responsive);
                }
            }
            _ => {
                // Client is unresponsive (deadlocked or overwhelmed)
            }
        }
    }

    println!("  Responsive clients: {}/50", responsive);

    // Assertions
    let total_elapsed = start.elapsed();
    println!("\nTotal test duration: {:?}", total_elapsed);

    // If we have severe backpressure issues, clients should become unresponsive
    // A healthy system should keep most clients responsive
    assert!(
        responsive > 25,
        "Only {} clients responsive - severe backpressure detected",
        responsive
    );

    // Test should complete in reasonable time (not hang forever)
    assert!(
        total_elapsed.as_secs() < 120,
        "Test took too long: {:?} - possible deadlock",
        total_elapsed
    );
}
