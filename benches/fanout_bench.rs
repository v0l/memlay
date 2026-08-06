//! Live-event fanout benchmark.
//!
//! Compares the old relay-wide `tokio::sync::broadcast` fanout (every
//! connection receives every event and matches it itself) against the
//! publisher-side `Fanout` router (only connections whose subscriptions could
//! match are touched).
//!
//! The realistic shape is modelled directly: a large pool of connections of
//! which most are idle or subscribed to kinds unrelated to the published event.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use memlay::event::Event;
use memlay::fanout::{BroadcastEvent, Fanout};
use memlay::subscription::{Filter, FilterMatch};
use std::hint::black_box;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

const SEND_CAP: usize = 16384;

/// One simulated connection under the old broadcast model.
type OldConn = (broadcast::Receiver<Arc<Event>>, Option<Filter>, String);

fn make_event(kind: u32) -> Arc<Event> {
    let json = format!(
        r#"{{"id":"{id}","pubkey":"{pk}","created_at":1700000000,"kind":{kind},"tags":[],"content":"{content}","sig":"{sig}"}}"#,
        id = "aa".repeat(32),
        pk = "bb".repeat(32),
        kind = kind,
        content = "x".repeat(280),
        sig = "cc".repeat(64),
    );
    Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
}

fn kind_filter(kinds: Vec<u32>) -> Filter {
    let mut f = Filter {
        kinds: Some(kinds),
        ..Default::default()
    };
    f.limit = Some(500);
    f
}

/// Build a fanout with `conns` connections: `idle_pct` percent have no
/// subscription at all, the rest subscribe to a single kind drawn round-robin
/// from `kind_pool`.
fn build_fanout(
    conns: usize,
    idle_pct: usize,
    kind_pool: &[u32],
) -> (Arc<Fanout>, Vec<mpsc::Receiver<String>>) {
    let fanout = Arc::new(Fanout::new());
    let mut rxs = Vec::with_capacity(conns);
    for i in 0..conns {
        let (tx, rx) = mpsc::channel(SEND_CAP);
        let state = fanout.register(tx);
        if i * 100 / conns >= idle_pct {
            let kind = kind_pool[i % kind_pool.len()];
            fanout.set_sub(&state, &format!("sub{i}"), vec![kind_filter(vec![kind])]);
        }
        rxs.push(rx);
    }
    (fanout, rxs)
}

/// The old model: every connection holds a broadcast receiver and does its own
/// matching + message construction. Modelled synchronously (`try_recv`) so the
/// bench measures the per-event CPU cost without runtime scheduling noise; the
/// real version additionally paid a task wakeup per connection per event.
fn broadcast_fanout(
    conns: usize,
    idle_pct: usize,
    kind_pool: &[u32],
) -> (broadcast::Sender<Arc<Event>>, Vec<OldConn>) {
    let (tx, _) = broadcast::channel(SEND_CAP);
    let mut receivers = Vec::with_capacity(conns);
    for i in 0..conns {
        let filter = if i * 100 / conns >= idle_pct {
            Some(kind_filter(vec![kind_pool[i % kind_pool.len()]]))
        } else {
            None
        };
        receivers.push((tx.subscribe(), filter, format!("sub{i}")));
    }
    (tx, receivers)
}

fn bench_fanout(c: &mut Criterion) {
    // Kinds subscribers care about. The published event is kind 1, so with this
    // pool roughly 1/6 of non-idle connections are actually interested.
    let kind_pool = [1u32, 3, 4, 7, 1984, 30023];
    let event = make_event(1);

    let mut group = c.benchmark_group("live_fanout_per_event");

    for &conns in &[100usize, 1000, 5000] {
        group.bench_with_input(BenchmarkId::new("broadcast_old", conns), &conns, |b, &n| {
            let (tx, mut receivers) = broadcast_fanout(n, 50, &kind_pool);
            b.iter(|| {
                let _ = tx.send(event.clone());
                for (rx, filter, sub_id) in receivers.iter_mut() {
                    // Every connection is woken and must drain the event.
                    while let Ok(ev) = rx.try_recv() {
                        let Some(f) = filter else { continue };
                        // Per-connection UTF-8 validation of the event body.
                        let json = String::from_utf8_lossy(&ev.raw);
                        if f.matches_event(&ev) {
                            let mut msg = String::with_capacity(sub_id.len() + json.len() + 14);
                            msg.push_str("[\"EVENT\",\"");
                            msg.push_str(sub_id);
                            msg.push_str("\",");
                            msg.push_str(&json);
                            msg.push(']');
                            black_box(msg);
                        }
                    }
                }
            });
        });

        group.bench_with_input(BenchmarkId::new("router_new", conns), &conns, |b, &n| {
            let (fanout, mut rxs) = build_fanout(n, 50, &kind_pool);
            b.iter(|| {
                // JSON validated once, shared by all recipients.
                fanout.publish(BroadcastEvent::new(event.clone()));
                for rx in rxs.iter_mut() {
                    while let Ok(msg) = rx.try_recv() {
                        black_box(msg);
                    }
                }
            });
        });
    }

    group.finish();

    // Worst case for the old design and best case for the new one: a flood of
    // connections that are connected but have opened no subscription.
    let mut idle = c.benchmark_group("live_fanout_all_idle");
    for &conns in &[1000usize, 5000] {
        idle.bench_with_input(BenchmarkId::new("broadcast_old", conns), &conns, |b, &n| {
            let (tx, mut receivers) = broadcast_fanout(n, 100, &kind_pool);
            b.iter(|| {
                let _ = tx.send(event.clone());
                for (rx, _, _) in receivers.iter_mut() {
                    while let Ok(ev) = rx.try_recv() {
                        black_box(ev);
                    }
                }
            });
        });

        idle.bench_with_input(BenchmarkId::new("router_new", conns), &conns, |b, &n| {
            let (fanout, _rxs) = build_fanout(n, 100, &kind_pool);
            b.iter(|| {
                fanout.publish(BroadcastEvent::new(event.clone()));
            });
        });
    }
    idle.finish();
}

criterion_group!(benches, bench_fanout);
criterion_main!(benches);
