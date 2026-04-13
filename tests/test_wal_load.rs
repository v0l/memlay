use memlay::{config::Config, relay::Relay};
use nostr_sdk::prelude::*;
use std::net::TcpListener;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;
use std::time::Instant;
use std::path::PathBuf;

#[tokio::test]
async fn test_wal_load_and_accept_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("ws://127.0.0.1:{}", addr.port());
    
    // Use the WAL from production
    let wal_path = PathBuf::from("/home/kieran/memlay/wal.log");
    let mut config = Config::default();
    config.persistence_path = Some(wal_path.to_string_lossy().to_string());
    
    println!("Starting relay with WAL from production...");
    let relay = Relay::new(config);
    let router = relay.router();
    let tcp = TokioTcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(tcp, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await.unwrap();
    });
    
    // Give it time to load
    println!("Waiting for WAL to load...");
    tokio::time::sleep(Duration::from_secs(5)).await;
    
    let keys = Keys::generate();
    let client = Client::new(keys);
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("Publishing 100 events...");
    let start = Instant::now();
    for i in 0..100 {
        let builder = EventBuilder::text_note(&format!("Test after WAL load {}", i));
        match client.send_event_builder(builder).await {
            Ok(_) => {
                if i % 20 == 0 {
                    println!("  Sent {} events", i);
                }
            }
            Err(e) => {
                panic!("Error at event {}: {:?}", i, e);
            }
        }
    }
    let elapsed = start.elapsed();
    println!("Done in {:?} ({:.0} events/sec)", elapsed, 100.0/elapsed.as_secs_f64());
    
    assert!(elapsed.as_secs() < 60, "Took too long: {:?}", elapsed);
}
