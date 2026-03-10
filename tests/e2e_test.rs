use memlay::{config::Config, relay::Relay};
use nostr_sdk::prelude::*;
use std::net::TcpListener;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;

/// Spawn a relay on a random available port and return the ws:// URL.
async fn spawn_relay() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let url = format!("ws://127.0.0.1:{}", addr.port());
    let relay = Relay::new(Config::default());
    let router = relay.router();

    let tcp = TokioTcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(tcp, router).await.unwrap();
    });

    url
}

/// Build a connected nostr-sdk client pointed at the given relay URL.
async fn make_client(url: &str) -> Client {
    let keys = Keys::generate();
    let client = Client::new(keys);
    client.add_relay(url).await.unwrap();
    client.connect().await;
    // Give the WebSocket connection a moment to establish.
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
}

#[tokio::test]
async fn test_relay_creation() {
    let config = Config::default();
    let relay = Relay::new(config);
    assert_eq!(relay.events.len(), 0);
}

#[tokio::test]
async fn test_subscription_manager() {
    let config = Config::default();
    let relay = Relay::new(config);
    let filter = memlay::subscription::Filter::default();
    let events = relay.subscriptions.query_filter(&filter);
    assert!(events.is_empty());
}

/// Publish a text note and fetch it back; the relay must return the event.
#[tokio::test]
async fn test_publish_and_fetch() {
    let url = spawn_relay().await;
    let client = make_client(&url).await;

    let builder = EventBuilder::text_note("hello memlay");
    let output = client.send_event_builder(builder).await.unwrap();
    let sent_id = output.val;

    // Brief pause so the relay processes the EVENT before we query.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let filter = Filter::new().id(sent_id);
    let events = client
        .fetch_events(filter, Duration::from_secs(3))
        .await
        .unwrap();

    assert_eq!(events.len(), 1, "expected exactly one event back");
    let first = events.into_iter().next().unwrap();
    assert_eq!(first.id, sent_id);

    client.shutdown().await;
}

/// Publish several events and verify that filter-by-kind works correctly.
#[tokio::test]
async fn test_filter_by_kind() {
    let url = spawn_relay().await;
    let client = make_client(&url).await;

    // Publish 3 text notes (kind 1) and 1 metadata (kind 0).
    for i in 0..3 {
        let builder = EventBuilder::text_note(format!("note {i}"));
        client.send_event_builder(builder).await.unwrap();
    }
    let metadata = Metadata::new().name("test");
    client.set_metadata(&metadata).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fetch only kind-1 events.
    let filter = Filter::new().kind(Kind::TextNote);
    let events = client
        .fetch_events(filter, Duration::from_secs(3))
        .await
        .unwrap();

    assert_eq!(events.len(), 3, "expected 3 kind-1 events");
    for event in events {
        assert_eq!(event.kind, Kind::TextNote);
    }

    client.shutdown().await;
}

/// Two independent clients: one publishes, the other fetches by author pubkey.
#[tokio::test]
async fn test_filter_by_author() {
    let url = spawn_relay().await;

    let alice_keys = Keys::generate();
    let alice = Client::new(alice_keys.clone());
    alice.add_relay(&url).await.unwrap();
    alice.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let bob = make_client(&url).await;

    // Alice publishes two notes.
    for i in 0..2 {
        let builder = EventBuilder::text_note(format!("alice note {i}"));
        alice.send_event_builder(builder).await.unwrap();
    }
    // Bob publishes one note (different author).
    let builder = EventBuilder::text_note("bob note");
    bob.send_event_builder(builder).await.unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Fetch events authored only by Alice.
    let filter = Filter::new()
        .author(alice_keys.public_key())
        .kind(Kind::TextNote);
    let events = bob
        .fetch_events(filter, Duration::from_secs(3))
        .await
        .unwrap();

    assert_eq!(events.len(), 2, "expected 2 events from Alice");
    for event in events {
        assert_eq!(event.pubkey, alice_keys.public_key());
    }

    alice.shutdown().await;
    bob.shutdown().await;
}

/// An empty REQ (no matching events) must still receive an EOSE and return
/// an empty result set rather than hanging.
#[tokio::test]
async fn test_empty_query_returns_eose() {
    let url = spawn_relay().await;
    let client = make_client(&url).await;

    // Query for a specific non-existent event id.
    let fake_id = EventId::all_zeros();
    let filter = Filter::new().id(fake_id);
    let events = client
        .fetch_events(filter, Duration::from_secs(3))
        .await
        .unwrap();

    assert!(events.is_empty(), "expected no events for non-existent id");

    client.shutdown().await;
}
