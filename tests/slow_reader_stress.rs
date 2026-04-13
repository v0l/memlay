//! Test for slow reader issue that causes server to stop accepting connections

use memlay::{config::Config, relay::Relay};
use nostr_sdk::prelude::*;
use std::net::TcpListener;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;
use std::time::Instant;

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

/// Test: connect/disconnect repeatedly while adding slow readers and querying
#[tokio::test]
async fn test_connect_disconnect_with_slow_readers_and_queries() {
    let url = spawn_relay().await;
    
    println!("\n=== Phase 1: Populate relay ===");
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..1000 {
        let builder = EventBuilder::text_note(&format!("Event {}", i));
        let _ = client.send_event_builder(builder).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("  Populated 1000 events");

    // Phase 2: Add slow readers (subscribe but don't drain)
    println!("\n=== Phase 2: Adding 30 slow readers ===");
    let mut slow_readers = Vec::new();
    
    for i in 0..30 {
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            
            // Subscribe to everything - will receive all events
            let filter = Filter::new().limit(1000);
            client.subscribe(filter, None).await.unwrap();
            
            // Don't read - just sleep (slow reader)
            tokio::time::sleep(Duration::from_secs(10)).await;
            
            (i, client)
        });
        slow_readers.push(handle);
        tokio::time::sleep(Duration::from_millis(20)).await; // Stagger
    }
    
    tokio::time::sleep(Duration::from_millis(200)).await;
    println!("  Added 30 slow readers");

    // Phase 3: Publish rapidly to fill slow readers' buffers
    println!("\n=== Phase 3: Publishing 3000 events ===");
    let pub_keys = Keys::generate();
    let pub_client = Client::new(pub_keys);
    pub_client.add_relay(&url).await.unwrap();
    pub_client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..3000 {
        let builder = EventBuilder::text_note(&format!("Rapid event {}", i));
        let _ = pub_client.send_event_builder(builder).await;
    }
    println!("  Published 3000 events");

    // Phase 4: While slow readers are backed up, do rapid connect/disconnect with queries
    println!("\n=== Phase 4: Rapid connect/disconnect with queries (50 cycles) ===");
    let start = Instant::now();
    
    let mut successful_cycles = 0;
    for cycle in 0..50 {
        let url = url.clone();
        let keys = Keys::generate();
        let client = Client::new(keys);
        client.add_relay(&url).await.unwrap();
        
        let connect_start = Instant::now();
        match tokio::time::timeout(Duration::from_secs(2), client.connect()).await {
            Ok(_) => {
                // Do a query
                let filter = Filter::new().limit(100);
                match tokio::time::timeout(Duration::from_secs(2), client.fetch_events(filter, Duration::from_secs(2))).await {
                    Ok(Ok(_)) => {
                        successful_cycles += 1;
                        if successful_cycles % 10 == 0 {
                            println!("  Cycle {} OK (connect: {:?})", successful_cycles, connect_start.elapsed());
                        }
                    }
                    _ => {
                        println!("  Cycle {} query failed", cycle);
                    }
                }
                client.shutdown().await;
            }
            Err(_) => {
                println!("  Cycle {} CONNECT TIMEOUT - possible deadlock!", cycle);
                break; // Stop if we can't connect
            }
        }
    }

    let elapsed = start.elapsed();
    println!("\n  Completed {} cycles in {:?}", successful_cycles, elapsed);
    
    assert!(successful_cycles >= 40, "Only {} cycles completed - possible deadlock", successful_cycles);
}

/// Test: many concurrent REQs while publishing
#[tokio::test]
async fn test_concurrent_reqs_while_publishing() {
    let url = spawn_relay().await;
    
    println!("\n=== Phase 1: Populate relay ===");
    let keys = Keys::generate();
    let client = Client::new(keys.clone());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..2000 {
        let builder = EventBuilder::text_note(&format!("Event {}", i));
        let _ = client.send_event_builder(builder).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("  Populated 2000 events");

    // Phase 2: Start 50 concurrent REQs
    println!("\n=== Phase 2: Starting 50 concurrent REQs ===");
    let mut handles = Vec::new();
    
    for i in 0..50 {
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            
            let filter = Filter::new().limit(500);
            match tokio::time::timeout(Duration::from_secs(5), client.fetch_events(filter, Duration::from_secs(5))).await {
                Ok(Ok(events)) => (i, events.len(), 0),
                Ok(Err(_)) => (i, 0, 1),
                Err(_) => (i, 0, 2),
            }
        });
        handles.push(handle);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Phase 3: Publish while REQs are processing
    println!("\n=== Phase 3: Publishing 2000 events while REQs process ===");
    let pub_keys = Keys::generate();
    let pub_client = Client::new(pub_keys);
    pub_client.add_relay(&url).await.unwrap();
    pub_client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..2000 {
        let builder = EventBuilder::text_note(&format!("Rapid event {}", i));
        let _ = pub_client.send_event_builder(builder).await;
    }
    println!("  Published 2000 events");

    // Phase 4: Wait for REQs
    println!("\n=== Phase 4: Waiting for REQs ===");
    let mut completed = 0;
    let mut timeouts = 0;
    for handle in handles {
        match handle.await {
            Ok((_, events, status)) => {
                completed += 1;
                if status == 2 {
                    timeouts += 1;
                }
            }
            Err(_) => {}
        }
    }

    println!("  Completed {}/50 REQs, {} timeouts", completed, timeouts);
    assert!(timeouts < 10, "Too many timeouts: {}", timeouts);
}
