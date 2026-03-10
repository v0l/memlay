//! End-to-end ingest + query benchmark.
//!
//! 1. Loads events from a JSONL archive
//! 2. Spawns a real memlay relay
//! 3. Pushes all events over a plain WebSocket and measures ingest throughput
//! 4. Runs a set of query scenarios (REQ → collect events → EOSE) and reports
//!    latency and result-set sizes
//!
//! Usage:
//!   NOSTR_ARCHIVE_DIR=/path/to/jsonl cargo bench --bench ingest_bench
//!
//! Optional env vars:
//!   INGEST_LIMIT=100000   how many events to load (default 100 000)
//!   QUERY_ROUNDS=20       REQ repetitions per scenario (default 20)

use memlay::{config::Config, relay::Relay};
use nostr_archive_cursor::NostrCursor;
use std::env;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener as TokioListener;
use tungstenite::{connect, Message};

// ── data model ───────────────────────────────────────────────────────────────

/// Lightweight summary of a parsed event used for building query filters.
#[derive(Debug)]
struct EventMeta {
    id: String,
    pubkey: String,
    kind: u64,
    created_at: u64,
    /// First `e` tag value if present
    e_tag: Option<String>,
}

// ── archive loading ──────────────────────────────────────────────────────────

/// Load up to `limit` events, returning both the raw `["EVENT",…]` wire
/// frames and lightweight metadata for filter construction.
fn load_events(dir: &str, limit: usize) -> (Vec<String>, Vec<EventMeta>) {
    let cursor = NostrCursor::new(dir.into()).with_dedupe(true).with_max_parallelism();

    let frames: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::with_capacity(limit)));
    let metas: Arc<Mutex<Vec<EventMeta>>> = Arc::new(Mutex::new(Vec::with_capacity(limit)));
    let done = Arc::new(AtomicBool::new(false));

    let frames2 = frames.clone();
    let metas2 = metas.clone();
    let done2 = done.clone();

    cursor.walk_with_chunked_sync(
        move |chunk| {
            if done2.load(Ordering::Relaxed) {
                return;
            }
            for ev in chunk {
                let mut f = frames2.lock().unwrap();
                if f.len() >= limit {
                    done2.store(true, Ordering::Relaxed);
                    return;
                }
                if let Ok(json) = serde_json::to_string(&ev) {
                    // Extract metadata from the raw JSON value for filter building.
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                        let id = v["id"].as_str().unwrap_or("").to_string();
                        let pubkey = v["pubkey"].as_str().unwrap_or("").to_string();
                        let kind = v["kind"].as_u64().unwrap_or(0);
                        let e_tag = v["tags"]
                            .as_array()
                            .and_then(|tags| {
                                tags.iter().find(|t| {
                                    t.as_array()
                                        .and_then(|a| a.first())
                                        .and_then(|v| v.as_str())
                                        == Some("e")
                                })
                            })
                            .and_then(|t| t[1].as_str())
                            .map(|s| s.to_string());

                        let created_at = v["created_at"].as_u64().unwrap_or(0);
                        metas2.lock().unwrap().push(EventMeta { id, pubkey, kind, created_at, e_tag });
                    }
                    f.push(format!(r#"["EVENT",{}]"#, json));
                }
            }
        },
        1000,
    );

    let frames = Arc::try_unwrap(frames).unwrap().into_inner().unwrap();
    let metas = Arc::try_unwrap(metas).unwrap().into_inner().unwrap();
    (frames, metas)
}

// ── relay helpers ────────────────────────────────────────────────────────────

async fn spawn_relay() -> String {
    let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    drop(std_listener);

    let relay = Relay::new(Config::default());
    let router = relay.router();
    let listener = TokioListener::bind(addr).await.unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    format!("ws://127.0.0.1:{}", addr.port())
}

// ── WebSocket helpers ────────────────────────────────────────────────────────

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

/// Send a PING and drain until PONG — ensures all previously sent frames have
/// been processed by the relay.
fn flush(ws: &mut Ws) {
    ws.send(Message::Ping(vec![].into())).ok();
    loop {
        match ws.read() {
            Ok(Message::Pong(_)) => break,
            Ok(_) | Err(_) => {}
        }
    }
}

/// Send a REQ, collect all EVENT responses until EOSE, return (count, bytes).
fn run_req(ws: &mut Ws, sub_id: &str, filter_json: &str) -> (usize, usize) {
    let req = format!(r#"["REQ","{}",{}]"#, sub_id, filter_json);
    ws.send(Message::Text(req.into())).unwrap();

    let mut count = 0usize;
    let mut bytes = 0usize;
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                // ["EOSE","<sub_id>"] signals end of stored events
                if t.starts_with(r#"["EOSE""#) {
                    break;
                }
                bytes += t.len();
                count += 1;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("ws read error in REQ: {e}");
                break;
            }
        }
    }
    (count, bytes)
}

// ── query benchmark ──────────────────────────────────────────────────────────

struct QueryResult {
    name: String,
    rounds: usize,
    total_events: usize,
    total_bytes: usize,
    total_time: Duration,
}

impl QueryResult {
    fn avg_latency(&self) -> Duration {
        self.total_time / self.rounds as u32
    }
    fn avg_events(&self) -> f64 {
        self.total_events as f64 / self.rounds as f64
    }
    fn throughput_mb(&self) -> f64 {
        self.total_bytes as f64 / self.total_time.as_secs_f64() / 1e6
    }
}

fn bench_query(
    ws: &mut Ws,
    name: &str,
    rounds: usize,
    filter_fn: impl Fn(usize) -> String,
) -> QueryResult {
    let mut total_events = 0;
    let mut total_bytes = 0;
    let t = Instant::now();
    for i in 0..rounds {
        let sub_id = format!("bench-{}-{}", name, i);
        let filter = filter_fn(i);
        let (ev, by) = run_req(ws, &sub_id, &filter);
        total_events += ev;
        total_bytes += by;
    }
    QueryResult {
        name: name.to_string(),
        rounds,
        total_events,
        total_bytes,
        total_time: t.elapsed(),
    }
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

    let limit: usize = env::var("INGEST_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);

    let rounds: usize = env::var("QUERY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    // ── load ─────────────────────────────────────────────────────────────────
    eprint!("Loading up to {} events from {} … ", limit, archive_dir);
    let t_load = Instant::now();
    let (frames, metas) = load_events(&archive_dir, limit);
    eprintln!("loaded {} in {:.2}s", frames.len(), t_load.elapsed().as_secs_f64());

    if frames.is_empty() {
        eprintln!("No events found — check NOSTR_ARCHIVE_DIR.");
        return;
    }

    // Collect real values for filter sampling
    let pubkeys: Vec<&str> = metas.iter().map(|m| m.pubkey.as_str()).collect();
    let event_ids: Vec<&str> = metas.iter().map(|m| m.id.as_str()).collect();
    let e_tag_refs: Vec<&str> = metas.iter().filter_map(|m| m.e_tag.as_deref()).collect();

    // Kind distribution for realistic kind filters
    let mut kind_counts = std::collections::HashMap::<u64, usize>::new();
    for m in &metas {
        *kind_counts.entry(m.kind).or_default() += 1;
    }
    let mut kinds_by_freq: Vec<u64> = kind_counts.keys().copied().collect();
    kinds_by_freq.sort_by_key(|k| std::cmp::Reverse(kind_counts[k]));
    let top_kinds = &kinds_by_freq[..kinds_by_freq.len().min(5)];

    // ── start relay + ingest ─────────────────────────────────────────────────
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = rt.block_on(spawn_relay());
    std::thread::sleep(Duration::from_millis(50));

    let (mut ws, _) = connect(&url).expect("WebSocket connect failed");

    let total_bytes: usize = frames.iter().map(|f| f.len()).sum();
    let total_events = frames.len();
    eprintln!("Sending {} events ({:.1} MB) …", total_events, total_bytes as f64 / 1e6);

    let t_ingest = Instant::now();
    for frame in &frames {
        ws.send(Message::Text(frame.as_str().into())).expect("ws send failed");
    }
    flush(&mut ws);
    let ingest_elapsed = t_ingest.elapsed();

    println!(
        "\n── ingest results ────────────────────────────────────────────\n\
         events     : {}\n\
         payload    : {:.2} MB\n\
         elapsed    : {:.3}s\n\
         throughput : {:.0} events/s  |  {:.2} MB/s\n\
         ──────────────────────────────────────────────────────────────",
        total_events,
        total_bytes as f64 / 1e6,
        ingest_elapsed.as_secs_f64(),
        total_events as f64 / ingest_elapsed.as_secs_f64(),
        total_bytes as f64 / ingest_elapsed.as_secs_f64() / 1e6,
    );

    // ── query scenarios ───────────────────────────────────────────────────────
    eprintln!("\nRunning query scenarios ({} rounds each) …", rounds);

    let step = pubkeys.len() / rounds;
    let n = pubkeys.len();

    let mut results = Vec::new();

    // 1. Each top kind individually, with and without limit
    for &k in top_kinds {
        let count = kind_counts[&k];
        results.push(bench_query(&mut ws, &format!("kind {} (limit 100) [{} total]", k, count), rounds, move |_| {
            format!(r#"{{"kinds":[{}],"limit":100}}"#, k)
        }));
        results.push(bench_query(&mut ws, &format!("kind {} (no limit) [{} total]", k, count), rounds, move |_| {
            format!(r#"{{"kinds":[{}]}}"#, k)
        }));
    }

    // Pick a few authors with varying event counts for author scenarios.
    // Sort pubkeys by how many events they have and sample low/mid/high.
    let mut pubkey_counts: Vec<(&str, usize)> = {
        let mut counts = std::collections::HashMap::<&str, usize>::new();
        for m in &metas {
            *counts.entry(m.pubkey.as_str()).or_default() += 1;
        }
        counts.into_iter().collect()
    };
    pubkey_counts.sort_by_key(|&(_, c)| c);
    let sample_authors: Vec<(&str, usize)> = {
        let len = pubkey_counts.len();
        // low, mid-low, mid, mid-high, high event-count authors
        [len / 10, len / 4, len / 2, len * 3 / 4, len - 1]
            .iter()
            .map(|&i| pubkey_counts[i])
            .collect()
    };

    // 2. By author — each pinned author, all rounds query the same pubkey
    for (pk, total) in &sample_authors {
        let pk = *pk;
        let total = *total;
        results.push(bench_query(&mut ws, &format!("author (limit 100) [{} total]", total), rounds, move |_| {
            format!(r#"{{"authors":["{}"],"limit":100}}"#, pk)
        }));
        results.push(bench_query(&mut ws, &format!("author (no limit) [{} total]", total), rounds, move |_| {
            format!(r#"{{"authors":["{}"]}}"#, pk)
        }));
    }

    // 3. By event ID (single lookup — same cost every time)
    results.push(bench_query(&mut ws, "id lookup", rounds, |i| {
        let id = event_ids[(i * step) % n];
        format!(r#"{{"ids":["{}"]}}"#, id)
    }));

    // 4. By #e tag
    if !e_tag_refs.is_empty() {
        let e_step = (e_tag_refs.len() / rounds).max(1);
        let en = e_tag_refs.len();
        results.push(bench_query(&mut ws, "#e tag", rounds, |i| {
            let id = e_tag_refs[(i * e_step) % en];
            format!("{{\"#e\":[\"{}\"]}}", id)
        }));
    }

    // 5. Author + kind (pinned mid author, cycling top kinds)
    let (mid_pk, _) = sample_authors[2];
    results.push(bench_query(&mut ws, "author + kind", rounds, |i| {
        let k = top_kinds[i % top_kinds.len()];
        format!(r#"{{"authors":["{}"],"kinds":[{}]}}"#, mid_pk, k)
    }));

    // 6. Author + since (pinned mid author, midpoint timestamp)
    let mid_ts = {
        let mut ts: Vec<u64> = metas.iter().map(|m| m.created_at).collect();
        ts.sort_unstable();
        ts[ts.len() / 2]
    };
    results.push(bench_query(&mut ws, "author + since", rounds, |i| {
        let k = i; // unused but keeps closure consistent
        let _ = k;
        format!(r#"{{"authors":["{}"],"since":{}}}"#, mid_pk, mid_ts)
    }));

    // ── print results ─────────────────────────────────────────────────────────
    println!(
        "\n── query results ({} rounds each) ────────────────────────────",
        rounds
    );
    println!(
        "{:<25}  {:>10}  {:>12}  {:>10}  {:>10}",
        "scenario", "avg events", "avg latency", "MB/s", "rounds"
    );
    println!("{}", "─".repeat(75));
    for r in &results {
        println!(
            "{:<25}  {:>10.1}  {:>12}  {:>10.2}  {:>10}",
            r.name,
            r.avg_events(),
            format!("{:.2}ms", r.avg_latency().as_secs_f64() * 1000.0),
            r.throughput_mb(),
            r.rounds,
        );
    }
    println!("─────────────────────────────────────────────────────────────────────────");

    ws.close(None).ok();
}
