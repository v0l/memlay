//! End-to-end ingest throughput benchmark.
//!
//! Spawns a real memlay relay, connects over a plain WebSocket (tungstenite),
//! and pushes every event from a JSONL archive as fast as the TCP socket
//! allows.  Reports events/sec and MB/sec at the end.
//!
//! Usage:
//!   NOSTR_ARCHIVE_DIR=/path/to/jsonl/dir cargo bench --bench ingest_bench
//!
//! If NOSTR_ARCHIVE_DIR is not set the benchmark is skipped.

use memlay::{config::Config, relay::Relay};
use nostr_archive_cursor::NostrCursor;
use std::env;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::TcpListener as TokioListener;
use tungstenite::{connect, Message};

// ── archive loading ──────────────────────────────────────────────────────────

/// Read up to `limit` raw EVENT JSON strings from the archive directory.
/// Returns the raw bytes ready to wrap in `["EVENT", <event>]`.
fn load_raw_events(dir: &str, limit: usize) -> Vec<Vec<u8>> {
    let cursor = NostrCursor::new(dir.into()).with_dedupe(true).with_max_parallelism();

    let out = Arc::new(Mutex::new(Vec::with_capacity(limit)));
    let done = Arc::new(AtomicBool::new(false));

    let out2 = out.clone();
    let done2 = done.clone();

    cursor.walk_with_chunked_sync(
        move |chunk| {
            if done2.load(Ordering::Relaxed) {
                return;
            }
            for ev in chunk {
                let mut guard = out2.lock().unwrap();
                if guard.len() >= limit {
                    done2.store(true, Ordering::Relaxed);
                    return;
                }
                // Serialise the borrowed event back to JSON, then wrap it.
                if let Ok(event_json) = serde_json::to_string(&ev) {
                    let msg = format!(r#"["EVENT",{}]"#, event_json);
                    guard.push(msg.into_bytes());
                }
            }
        },
        1000,
    );

    Arc::try_unwrap(out).unwrap().into_inner().unwrap()
}

// ── relay helpers ────────────────────────────────────────────────────────────

/// Bind a relay on a random OS-assigned port, return the `ws://` URL.
async fn spawn_relay() -> String {
    // Grab an ephemeral port then immediately release the std listener so
    // Tokio can bind the same address.
    let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    drop(std_listener);

    let relay = Relay::new(Config::default());
    let router = relay.router();
    let listener = TokioListener::bind(addr).await.unwrap();

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    format!("ws://127.0.0.1:{}", addr.port())
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let archive_dir = match env::var("NOSTR_ARCHIVE_DIR") {
        Ok(d) => d,
        Err(_) => {
            eprintln!(
                "ingest_bench: NOSTR_ARCHIVE_DIR not set — skipping.\n\
                 Set it to a directory containing .jsonl files to run this benchmark."
            );
            return;
        }
    };

    // How many events to load; override with INGEST_LIMIT env var.
    let limit: usize = env::var("INGEST_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);

    // ── load events from disk ────────────────────────────────────────────────
    eprint!("Loading up to {} events from {} … ", limit, archive_dir);
    let t_load = Instant::now();
    let events = load_raw_events(&archive_dir, limit);
    eprintln!("loaded {} in {:.2}s", events.len(), t_load.elapsed().as_secs_f64());

    if events.is_empty() {
        eprintln!("No events found — check NOSTR_ARCHIVE_DIR.");
        return;
    }

    // ── start relay ──────────────────────────────────────────────────────────
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = rt.block_on(spawn_relay());

    // Small pause to let the relay finish binding.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // ── connect WebSocket ────────────────────────────────────────────────────
    let (mut ws, _) = connect(&url).expect("WebSocket connect failed");

    // ── push events ─────────────────────────────────────────────────────────
    let total_bytes: usize = events.iter().map(|e| e.len()).sum();
    let total_events = events.len();

    eprintln!("Sending {} events ({:.1} MB) …", total_events, total_bytes as f64 / 1e6);

    let t_start = Instant::now();

    for msg_bytes in &events {
        // SAFETY: we built these from valid UTF-8 above
        let text = unsafe { std::str::from_utf8_unchecked(msg_bytes) };
        ws.send(Message::Text(text.to_owned().into())).expect("ws send failed");
    }

    // Flush any buffered frames by sending a PING and waiting for PONG.
    ws.send(Message::Ping(vec![].into())).ok();
    loop {
        match ws.read() {
            Ok(Message::Pong(_)) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("ws read error during flush: {e}");
                break;
            }
        }
    }

    let elapsed = t_start.elapsed();

    ws.close(None).ok();

    // ── report ───────────────────────────────────────────────────────────────
    let events_per_sec = total_events as f64 / elapsed.as_secs_f64();
    let mb_per_sec = total_bytes as f64 / elapsed.as_secs_f64() / 1e6;

    println!(
        "\n── ingest_bench results ──────────────────────────────────────\n\
         events     : {}\n\
         payload    : {:.2} MB\n\
         elapsed    : {:.3}s\n\
         throughput : {:.0} events/s  |  {:.2} MB/s\n\
         ──────────────────────────────────────────────────────────────",
        total_events,
        total_bytes as f64 / 1e6,
        elapsed.as_secs_f64(),
        events_per_sec,
        mb_per_sec,
    );
}
