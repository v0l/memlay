use memlay::{config::Config, relay::Relay};
use std::net::TcpListener;
use std::time::Duration;
use tokio::net::TcpListener as TokioTcpListener;

fn rss_mb() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    (pages * 4096) >> 20
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_readers_cannot_buffer_the_store_in_memory() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let config = Config {
        max_subscriptions: 300,
        ..Config::default()
    };
    let relay = Relay::new(config);
    for i in 0..4_000u32 {
        let json = format!(
            r#"{{"id":"{i:064x}","pubkey":"{:064x}","created_at":{},"kind":1,"tags":[],"content":"{}","sig":"{:0128}"}}"#,
            1,
            1_700_000_000 + i,
            "x".repeat(4_000),
            0
        );
        let event = memlay::event::Event::from_json_unchecked(json.as_bytes()).unwrap();
        relay.events.insert(std::sync::Arc::new(event));
    }
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
    tokio::time::sleep(Duration::from_millis(100)).await;

    let before = rss_mb();
    let url = format!("ws://{addr}");
    let mut stalled = Vec::new();
    for c in 0..40 {
        let url = url.clone();
        stalled.push(
            tokio::task::spawn_blocking(move || {
                let (mut ws, _) = tungstenite::connect(&url).unwrap();
                for s in 0..10 {
                    ws.send(tungstenite::Message::Text(
                        format!(r#"["REQ","s{c}-{s}",{{"limit":5000}}]"#).into(),
                    ))
                    .unwrap();
                }
                ws
            })
            .await
            .unwrap(),
        );
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let grown = rss_mb().saturating_sub(before);

    assert!(
        grown < 400,
        "40 stalled readers grew the relay by {grown} MB; each asked for 10 x 16 MB of history"
    );
    drop(stalled);
}
