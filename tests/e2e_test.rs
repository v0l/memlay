use memlay::{config::Config, relay::Relay};
use nostr_sdk::prelude::*;
use std::net::TcpListener;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;
use tungstenite::{Message, connect};

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
        axum::serve(
            tcp,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
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

/// Verify the raw wire protocol: OK response, correct format, EOSE.
/// Uses tungstenite directly so we can inspect exact relay responses.
#[tokio::test]
async fn test_wire_protocol() {
    let url = spawn_relay().await;

    // Generate a real signed event.
    let keys = Keys::generate();
    let event = EventBuilder::text_note("wire test")
        .build(keys.public_key())
        .sign(&keys)
        .await
        .unwrap();
    let event_json = event.as_json();
    let event_id = event.id.to_hex();

    // Run the blocking tungstenite assertions off the async executor.
    tokio::task::spawn_blocking(move || {
        let (mut ws, _) = connect(&url).unwrap();

        // ── publish event ─────────────────────────────────────────────────
        let msg = format!(r#"["EVENT",{}]"#, event_json);
        ws.send(Message::Text(msg.into())).unwrap();

        let text = read_text(&mut ws);
        let arr: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(arr[0], "OK", "relay must respond with OK, got: {}", text);
        assert_eq!(arr[1].as_str().unwrap(), event_id, "OK must echo the event id");
        assert!(arr[2].as_bool().unwrap(), "OK accepted must be true, got: {}", text);
        assert_eq!(arr[3].as_str().unwrap(), "", "OK message must be empty for new event");

        // ── fetch it back with REQ ─────────────────────────────────────────
        let req = format!(r#"["REQ","test-sub",{{"ids":["{}"]}}]"#, event_id);
        ws.send(Message::Text(req.into())).unwrap();

        let text = read_text(&mut ws);
        let arr: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(arr[0], "EVENT", "expected EVENT frame, got: {}", text);
        assert_eq!(arr[1], "test-sub");
        assert_eq!(arr[2]["id"].as_str().unwrap(), event_id);

        let text = read_text(&mut ws);
        let arr: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(arr[0], "EOSE", "expected EOSE, got: {}", text);
        assert_eq!(arr[1], "test-sub");

        // ── CLOSE ─────────────────────────────────────────────────────────
        ws.send(Message::Text(r#"["CLOSE","test-sub"]"#.into())).unwrap();

        // ── duplicate detection ────────────────────────────────────────────
        let msg = format!(r#"["EVENT",{}]"#, event_json);
        ws.send(Message::Text(msg.into())).unwrap();

        let text = read_text(&mut ws);
        let arr: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(arr[0], "OK", "duplicate must get OK, got: {}", text);
        assert!(arr[2].as_bool().unwrap(), "duplicate accepted must be true");
        assert!(
            arr[3].as_str().unwrap().starts_with("duplicate:"),
            "message must have duplicate: prefix, got: {}",
            arr[3]
        );

        // ── invalid event (bad id + sig) rejected ─────────────────────────
        let bad = r#"["EVENT",{"id":"0000000000000000000000000000000000000000000000000000000000000000","pubkey":"0000000000000000000000000000000000000000000000000000000000000000","created_at":1,"kind":1,"tags":[],"content":"bad","sig":"0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}]"#;
        ws.send(Message::Text(bad.into())).unwrap();

        let text = read_text(&mut ws);
        let arr: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            arr[0] == "NOTICE" || (arr[0] == "OK" && arr[2].as_bool() == Some(false)),
            "invalid event must produce NOTICE or OK false, got: {}",
            text
        );

        ws.close(None).ok();
    })
    .await
    .unwrap();
}

fn read_text(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
) -> String {
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => return t.to_string(),
            Message::Ping(d) => ws.send(Message::Pong(d)).unwrap(),
            other => panic!("unexpected frame: {:?}", other),
        }
    }
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
