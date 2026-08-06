//! Publisher-side live event fanout.
//!
//! Replaces the previous "every connection subscribes to a relay-wide
//! `broadcast::channel`" model, which woke *every* connection task for *every*
//! event and re-validated the event JSON once per connection. Under a burst
//! that collapsed: 245 connections all lagged past the 16k channel buffer
//! within a single second and the runtime spent all its time on wakeups.
//!
//! Instead, connections register here and the publishing thread walks a small
//! inverted index (kind → interested connections) so an event only reaches
//! connections that could plausibly match it. The event JSON is validated and
//! shared once via `Arc<str>`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use tokio::sync::{Notify, mpsc};

use crate::event::Event;
use crate::subscription::{Filter, FilterMatch};

/// Sliding window over which dropped-event rate is measured for slow peers.
const DROP_WINDOW: Duration = Duration::from_secs(10);

/// Maximum events a connection may drop within `DROP_WINDOW` before it is
/// disconnected. Rate-based rather than a raw consecutive count, so a single
/// burst does not kill an otherwise healthy client.
const MAX_DROPS_PER_WINDOW: usize = 2048;

pub type ConnId = u64;

/// A registered subscription: its id and its (clamped, hex-preparsed) filters.
/// Both sides are `Arc` so the delivery path can snapshot the list cheaply.
type SubEntry = (Arc<str>, Arc<Vec<Filter>>);

/// An event ready for live delivery: the parsed event plus its JSON body,
/// UTF-8 validated exactly once and shared by every recipient.
#[derive(Clone)]
pub struct BroadcastEvent {
    pub event: Arc<Event>,
    pub json: Arc<str>,
}

impl BroadcastEvent {
    pub fn new(event: Arc<Event>) -> Self {
        let json: Arc<str> = Arc::from(String::from_utf8_lossy(&event.raw).as_ref());
        Self { event, json }
    }
}

/// The set of kinds a connection is interested in, used as its index key.
#[derive(Clone, PartialEq, Eq)]
enum KindKey {
    /// Not currently in the index (no subscriptions).
    None,
    /// Interested in every kind (at least one filter does not constrain kinds).
    Any,
    /// Interested only in these kinds.
    Some(HashSet<u32>),
}

/// Per-connection delivery state, shared between the connection's own task and
/// whichever thread is publishing an event.
pub struct ConnState {
    pub id: ConnId,
    tx: mpsc::Sender<String>,
    /// sub_id → filters. Small (bounded by `max_subscriptions`), read on every
    /// candidate delivery, written only on REQ/CLOSE.
    subs: RwLock<Vec<SubEntry>>,
    /// Key this connection is currently indexed under in `Fanout`.
    kind_key: Mutex<KindKey>,
    /// Subscriptions that overflowed and must be closed by the connection task.
    overflow: Mutex<Vec<Arc<str>>>,
    /// Signals the connection task that `overflow` and/or `kill` changed.
    notify: Notify,
    kill: AtomicBool,
    sent: AtomicUsize,
    dropped: AtomicUsize,
    /// (window start, drops in window)
    drop_window: Mutex<(Instant, usize)>,
}

impl ConnState {
    /// Total live events successfully queued to this connection.
    pub fn sent(&self) -> usize {
        self.sent.load(Ordering::Relaxed)
    }

    /// Total live events dropped because the send queue was full.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// True when this connection exceeded the drop rate limit and should be
    /// disconnected.
    pub fn should_disconnect(&self) -> bool {
        self.kill.load(Ordering::Relaxed)
    }

    /// Wait for an overflow / disconnect signal.
    pub async fn notified(&self) {
        self.notify.notified().await
    }

    /// Take the subscriptions that overflowed since the last call.
    pub fn take_overflowed(&self) -> Vec<Arc<str>> {
        std::mem::take(&mut *self.overflow.lock())
    }

    /// Deliver `ev` to every matching subscription on this connection.
    fn deliver(&self, ev: &BroadcastEvent) {
        // Held across delivery: `try_send` never blocks or awaits, and the only
        // writers are this connection's own task on REQ/CLOSE.
        let subs = self.subs.read();

        for (sub_id, filters) in subs.iter() {
            if !filters.iter().any(|f| f.matches_event(&ev.event)) {
                continue;
            }

            let mut msg = String::with_capacity(sub_id.len() + ev.json.len() + 14);
            msg.push_str("[\"EVENT\",\"");
            msg.push_str(sub_id);
            msg.push_str("\",");
            msg.push_str(&ev.json);
            msg.push(']');

            if self.tx.try_send(msg).is_ok() {
                self.sent.fetch_add(1, Ordering::Relaxed);
                crate::metrics::inc_events_output();
            } else {
                self.record_drop(sub_id);
            }
        }
    }

    /// Record a dropped delivery. The affected subscription is queued for a
    /// `CLOSED` reply so the client knows to resubscribe rather than silently
    /// missing events, and a sustained drop rate disconnects the peer.
    fn record_drop(&self, sub_id: &Arc<str>) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        crate::metrics::inc_events_dropped();

        {
            let mut overflow = self.overflow.lock();
            if !overflow.iter().any(|s| s == sub_id) {
                overflow.push(sub_id.clone());
                crate::metrics::inc_subs_overflowed();
            }
        }

        let over_limit = {
            let mut w = self.drop_window.lock();
            let now = Instant::now();
            if now.duration_since(w.0) > DROP_WINDOW {
                *w = (now, 1);
                false
            } else {
                w.1 += 1;
                w.1 > MAX_DROPS_PER_WINDOW
            }
        };

        if over_limit {
            self.kill.store(true, Ordering::Relaxed);
        }
        self.notify.notify_one();
    }
}

#[derive(Default)]
struct Index {
    conns: HashMap<ConnId, Arc<ConnState>>,
    /// Connections whose subscriptions all constrain `kinds`.
    by_kind: HashMap<u32, HashSet<ConnId>>,
    /// Connections with at least one filter that matches any kind.
    any_kind: HashSet<ConnId>,
    next_id: ConnId,
}

/// Registry of live connections plus the kind inverted index used to route
/// newly accepted events to only the connections that might want them.
#[derive(Default)]
pub struct Fanout {
    inner: RwLock<Index>,
}

impl Fanout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connection. It receives no events until it opens a
    /// subscription, so idle connections cost nothing on the publish path.
    pub fn register(&self, tx: mpsc::Sender<String>) -> Arc<ConnState> {
        let mut inner = self.inner.write();
        inner.next_id += 1;
        let id = inner.next_id;
        let state = Arc::new(ConnState {
            id,
            tx,
            subs: RwLock::new(Vec::new()),
            kind_key: Mutex::new(KindKey::None),
            overflow: Mutex::new(Vec::new()),
            notify: Notify::new(),
            kill: AtomicBool::new(false),
            sent: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            drop_window: Mutex::new((Instant::now(), 0)),
        });
        inner.conns.insert(id, state.clone());
        state
    }

    pub fn unregister(&self, conn: &ConnState) {
        let mut inner = self.inner.write();
        inner.conns.remove(&conn.id);
        let mut key = conn.kind_key.lock();
        inner.deindex(conn.id, &key);
        *key = KindKey::None;
    }

    /// Add or replace a subscription. Filters must already be clamped and
    /// hex-preparsed.
    pub fn set_sub(&self, conn: &Arc<ConnState>, sub_id: &str, filters: Vec<Filter>) {
        let sub_id: Arc<str> = Arc::from(sub_id);
        let filters = Arc::new(filters);
        {
            let mut subs = conn.subs.write();
            match subs.iter_mut().find(|(id, _)| *id == sub_id) {
                Some(slot) => slot.1 = filters,
                None => subs.push((sub_id, filters)),
            }
        }
        self.reindex(conn);
    }

    pub fn remove_sub(&self, conn: &Arc<ConnState>, sub_id: &str) -> bool {
        let removed = {
            let mut subs = conn.subs.write();
            let before = subs.len();
            subs.retain(|(id, _)| &**id != sub_id);
            subs.len() != before
        };
        if removed {
            self.reindex(conn);
        }
        removed
    }

    pub fn sub_count(&self, conn: &ConnState) -> usize {
        conn.subs.read().len()
    }

    pub fn has_sub(&self, conn: &ConnState, sub_id: &str) -> bool {
        conn.subs.read().iter().any(|(id, _)| &**id == sub_id)
    }

    /// Recompute a connection's index key from its current subscriptions.
    fn reindex(&self, conn: &Arc<ConnState>) {
        let new_key = {
            let subs = conn.subs.read();
            if subs.is_empty() {
                KindKey::None
            } else {
                let mut kinds = HashSet::new();
                let mut any = false;
                'outer: for (_, filters) in subs.iter() {
                    for f in filters.iter() {
                        match &f.kinds {
                            Some(k) if !k.is_empty() => kinds.extend(k.iter().copied()),
                            _ => {
                                any = true;
                                break 'outer;
                            }
                        }
                    }
                }
                if any {
                    KindKey::Any
                } else {
                    KindKey::Some(kinds)
                }
            }
        };

        let mut key = conn.kind_key.lock();
        if *key == new_key {
            return;
        }
        let mut inner = self.inner.write();
        inner.deindex(conn.id, &key);
        inner.index(conn.id, &new_key);
        *key = new_key;
    }

    /// Route an event to every connection that might match it.
    pub fn publish(&self, ev: BroadcastEvent) {
        let targets: Vec<Arc<ConnState>> = {
            let inner = self.inner.read();
            if inner.any_kind.is_empty() && !inner.by_kind.contains_key(&ev.event.kind) {
                return;
            }
            let by_kind = inner.by_kind.get(&ev.event.kind);
            let cap = inner.any_kind.len() + by_kind.map_or(0, |s| s.len());
            let mut targets = Vec::with_capacity(cap);
            // `any_kind` and `by_kind` are disjoint by construction (a
            // connection's key is either Any or Some(..)), so no dedup needed.
            for id in inner.any_kind.iter().chain(by_kind.into_iter().flatten()) {
                if let Some(c) = inner.conns.get(id) {
                    targets.push(c.clone());
                }
            }
            targets
        };

        for conn in targets {
            conn.deliver(&ev);
        }
    }

    pub fn conn_count(&self) -> usize {
        self.inner.read().conns.len()
    }
}

impl Index {
    fn index(&mut self, id: ConnId, key: &KindKey) {
        match key {
            KindKey::None => {}
            KindKey::Any => {
                self.any_kind.insert(id);
            }
            KindKey::Some(kinds) => {
                for k in kinds {
                    self.by_kind.entry(*k).or_default().insert(id);
                }
            }
        }
    }

    fn deindex(&mut self, id: ConnId, key: &KindKey) {
        match key {
            KindKey::None => {}
            KindKey::Any => {
                self.any_kind.remove(&id);
            }
            KindKey::Some(kinds) => {
                for k in kinds {
                    if let Some(set) = self.by_kind.get_mut(k) {
                        set.remove(&id);
                        if set.is_empty() {
                            self.by_kind.remove(k);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_filter(kinds: Option<Vec<u32>>) -> Filter {
        Filter {
            kinds,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn idle_connections_receive_nothing() {
        let fanout = Fanout::new();
        let (tx, mut rx) = mpsc::channel(8);
        let _conn = fanout.register(tx);

        let ev = crate::event::EventBuilder::new().kind(1).build();
        fanout.publish(BroadcastEvent::new(Arc::new(ev)));

        assert!(rx.try_recv().is_err(), "idle conn must not be delivered to");
    }

    #[tokio::test]
    async fn kind_index_routes_only_to_interested() {
        let fanout = Fanout::new();

        let (tx1, mut rx1) = mpsc::channel(8);
        let c1 = fanout.register(tx1);
        fanout.set_sub(&c1, "a", vec![kind_filter(Some(vec![1]))]);

        let (tx2, mut rx2) = mpsc::channel(8);
        let c2 = fanout.register(tx2);
        fanout.set_sub(&c2, "b", vec![kind_filter(Some(vec![7]))]);

        let (tx3, mut rx3) = mpsc::channel(8);
        let c3 = fanout.register(tx3);
        fanout.set_sub(&c3, "c", vec![kind_filter(None)]);

        let ev = crate::event::EventBuilder::new().kind(1).build();
        fanout.publish(BroadcastEvent::new(Arc::new(ev)));

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
        assert!(rx3.try_recv().is_ok());
    }

    #[tokio::test]
    async fn removing_subs_deindexes() {
        let fanout = Fanout::new();
        let (tx, mut rx) = mpsc::channel(8);
        let conn = fanout.register(tx);
        fanout.set_sub(&conn, "a", vec![kind_filter(Some(vec![1]))]);
        assert!(fanout.remove_sub(&conn, "a"));

        let ev = crate::event::EventBuilder::new().kind(1).build();
        fanout.publish(BroadcastEvent::new(Arc::new(ev)));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn overflow_queues_closed_and_kills_slow_peer() {
        let fanout = Fanout::new();
        let (tx, _rx) = mpsc::channel(1);
        let conn = fanout.register(tx);
        fanout.set_sub(&conn, "a", vec![kind_filter(Some(vec![1]))]);

        for _ in 0..(MAX_DROPS_PER_WINDOW + 8) {
            let ev = crate::event::EventBuilder::new().kind(1).build();
            fanout.publish(BroadcastEvent::new(Arc::new(ev)));
        }

        assert!(conn.dropped() > 0);
        assert_eq!(conn.take_overflowed().len(), 1);
        assert!(conn.should_disconnect());
    }
}
