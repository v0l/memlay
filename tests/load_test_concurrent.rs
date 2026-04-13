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

#[tokio::test]
async fn test_concurrent_load_with_data() {
    let url = spawn_relay().await;
    println!("\n=== Phase 1: Populating store with events ===");
    
    // First, populate the store with events
    let populate_keys = Keys::generate();
    let populate_client = Client::new(populate_keys.clone());
    populate_client.add_relay(&url).await.unwrap();
    populate_client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    let populate_start = Instant::now();
    let num_populate_events = 1000;
    
    println!("Publishing {} events to populate store...", num_populate_events);
    for i in 0..num_populate_events {
        let builder = EventBuilder::text_note(format!("populate event {}", i));
        populate_client.send_event_builder(builder).await.unwrap();
    }
    
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("Populated {} events in {:?}", num_populate_events, populate_start.elapsed());
    
    // Now run concurrent load test with queries AND inserts
    println!("\n=== Phase 2: Concurrent load test (queries + inserts) ===");
    let load_start = Instant::now();
    
    let num_clients = 500;
    let ops_per_client = 10; // 5 queries + 5 inserts
    
    let mut handles = vec![];
    
    for i in 0..num_clients {
        let url = url.clone();
        let handle = tokio::spawn(async move {
            let keys = Keys::generate();
            let client = Client::new(keys);
            client.add_relay(&url).await.unwrap();
            client.connect().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            
            let mut ops = 0;
            
            for j in 0..ops_per_client {
                // Query operation
                let filter = Filter::new()
                    .kind(Kind::TextNote)
                    .limit(10);
                
                match client.fetch_events(filter, Duration::from_secs(2)).await {
                    Ok(events) => {
                        ops += 1;
                        if j == 0 && i % 100 == 0 {
                            println!("Client {} query got {} events", i, events.len());
                        }
                    }
                    Err(e) => {
                        eprintln!("Client {} query error: {:?}", i, e);
                    }
                }
                
                // Insert operation
                let builder = EventBuilder::text_note(format!("client {} event {}", i, j));
                match client.send_event_builder(builder).await {
                    Ok(_) => ops += 1,
                    Err(e) => eprintln!("Client {} insert error: {:?}", i, e),
                }
            }
            
            client.shutdown().await;
            (i, ops)
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
    
    let elapsed = load_start.elapsed();
    println!("\n=== Results ===");
    println!("Completed clients: {}/{}", completed, num_clients);
    println!("Total operations: {}", total_ops);
    println!("Time: {:.2?}", elapsed);
    println!("Ops/sec: {}", total_ops as f64 / elapsed.as_secs_f64());
    
    assert!(elapsed.as_secs() < 60, "Load test took too long: {:?}", elapsed);
}
