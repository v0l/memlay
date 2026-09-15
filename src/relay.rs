use crate::config::Config;
use crate::event::Event;
use crate::fanout::{BroadcastEvent, Fanout};
use crate::message::NostrMessage;
use crate::proxy::TrustedProxies;
use crate::store::{EventStore, InsertResult, StoreConfig};
use crate::subscription::{Filter, SubscriptionManager};
use axum::{
    Router,
    extract::{ConnectInfo, State, ws::WebSocket},
    http::{HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, stream::StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

/// Capacity of per-connection send channels (backpressure for slow clients).
/// Sized to absorb a sync/import burst without dropping. Live delivery is
/// routed per-connection by `Fanout`, so this is the only queue an event sits
/// in and it is never shared with unrelated connections.
const CONN_SEND_CAP: usize = 16384;

/// Maximum inbound WebSocket text frame size (bytes) we will parse. Larger
/// frames are rejected to bound per-message CPU/allocation.
const MAX_MESSAGE_BYTES: usize = 512 * 1024;

/// Maximum number of filters accepted in a single REQ.
const MAX_FILTERS_PER_REQ: usize = 20;

/// Global cap on concurrent signature verifications. Bounds blocking-pool
/// usage under an ingest flood (each verify is ~1.4ms of CPU) without throttling
/// normal concurrent load. Kept well below tokio's default 512 blocking threads.
fn verify_permits() -> usize {
    (num_cpus::get() * 32).clamp(64, 256)
}

/// Shared state threaded through axum via `State<Arc<AppState>>`.
struct AppState {
    events: Arc<EventStore>,
    subscriptions: Arc<SubscriptionManager>,
    fanout: Arc<Fanout>,
    config: Config,
    connection_count: Arc<AtomicUsize>,
    verify_sem: Arc<Semaphore>,
    trusted_proxies: TrustedProxies,
}

pub struct Relay {
    pub events: Arc<EventStore>,
    pub subscriptions: Arc<SubscriptionManager>,
    /// Live-event router: only connections with matching subscriptions are
    /// touched when an event is accepted.
    fanout: Arc<Fanout>,
    config: Config,
    /// Count of active WebSocket connections
    connection_count: Arc<AtomicUsize>,
}

impl Relay {
    pub fn new(config: Config) -> Self {
        // Export every metric at zero from startup.
        crate::metrics::init();

        // Create store config with persistence if enabled
        let store_config = if let Some(ref path) = config.persistence_path {
            StoreConfig::with_persistence(config.target_ram_percent, path.clone())
        } else {
            StoreConfig::from_target_ram_percent(config.target_ram_percent)
        };

        let events = Arc::new(EventStore::new(store_config));

        // Load events from disk if persistence is enabled
        if let Err(e) = events.load_from_disk() {
            tracing::warn!(error = %e, "failed to load events from disk");
        }

        // Start background eviction task
        events.start_eviction_task();

        // Start background WAL group-commit task (durability without per-event fsync)
        events.start_wal_sync_task();

        // Start background persistence task if enabled
        if config.persistence_path.is_some() {
            events.start_persistence_task(config.persistence_interval);
        }

        let subscriptions = Arc::new(SubscriptionManager::new(events.clone()));
        let fanout = Arc::new(Fanout::new());
        let connection_count = Arc::new(AtomicUsize::new(0));

        // Always start metrics collection for active connections
        let conn_count = connection_count.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                crate::metrics::ACTIVE_CONNECTIONS.set(conn_count.load(Ordering::Relaxed) as f64);
            }
        });

        Self {
            events,
            subscriptions,
            fanout,
            config,
            connection_count,
        }
    }

    pub fn router(self) -> Router {
        let app_state = Arc::new(AppState {
            events: self.events.clone(),
            subscriptions: self.subscriptions.clone(),
            fanout: self.fanout.clone(),
            config: self.config.clone(),
            connection_count: self.connection_count.clone(),
            verify_sem: Arc::new(Semaphore::new(verify_permits())),
            trusted_proxies: TrustedProxies::parse(&self.config.trusted_proxies),
        });

        Router::new()
            .route("/stats", get(stats_handler))
            .route("/", get(root_handler))
            .route("/metrics", get(metrics_handler))
            .with_state(app_state)
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

#[axum::debug_handler]
async fn root_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: Result<
        axum::extract::ws::WebSocketUpgrade,
        axum::extract::ws::rejection::WebSocketUpgradeRejection,
    >,
) -> axum::response::Response {
    let wants_info = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/nostr+json"))
        .unwrap_or(false);

    if wants_info {
        return nip11_handler(&state.config).into_response();
    }

    let addr = state.trusted_proxies.client_addr(addr, &headers);

    match ws {
        Ok(ws) => {
            let subscriptions = state.subscriptions.clone();
            let fanout = state.fanout.clone();
            let conn_count = state.connection_count.clone();
            let verify_sem = state.verify_sem.clone();
            let config = state.config.clone();
            ws.on_failed_upgrade(move |err| {
                tracing::warn!(%addr, "WebSocket upgrade failed: {}", err);
            })
            .on_upgrade(move |socket| {
                handle_socket(
                    socket,
                    addr,
                    subscriptions,
                    fanout,
                    conn_count,
                    verify_sem,
                    config,
                )
            })
            .into_response()
        }
        Err(_) => landing_page(&state.config).into_response(),
    }
}

async fn metrics_handler(State(_state): State<Arc<AppState>>) -> impl IntoResponse {
    let metrics = crate::metrics::gather_metrics();
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    (headers, metrics)
}

async fn stats_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let events = &state.events;
    let cfg = events.config();
    let store_memory = events.memory_bytes();
    let body = serde_json::json!({
        "events": events.len(),
        "tombstones": events.tombstone_count(),
        "store_bytes": store_memory,
        "max_bytes": cfg.max_bytes,
        "process_memory": events.cached_process_memory(),
        "memory_limit": cfg.max_bytes,
        "memory_used": store_memory,
    });
    let mut headers = HeaderMap::new();
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    (headers, axum::Json(body))
}

/// NIPs this relay implements, advertised in NIP-11 and on the landing page.
const SUPPORTED_NIPS: &[u16] = &[1, 9, 11];

fn nip11_handler(config: &Config) -> impl IntoResponse {
    let body = serde_json::json!({
        "name": "memlay",
        "description": "High-performance in-memory Nostr relay",
        "software": "https://github.com/v0l/memlay",
        "version": env!("CARGO_PKG_VERSION"),
        "supported_nips": SUPPORTED_NIPS,
        "limitation": {
            "max_subscriptions": config.max_subscriptions,
            "max_limit": config.max_limit,
            "max_message_length": MAX_MESSAGE_BYTES,
            "idle_timeout": config.idle_timeout
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("application/nostr+json"),
    );
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("*"),
    );
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET"),
    );

    (headers, axum::Json(body)).into_response()
}

fn landing_page(_config: &Config) -> impl IntoResponse {
    let mut html = include_str!("index.html").replace("{{VERSION}}", env!("CARGO_PKG_VERSION"));

    // Always show metrics section since /metrics is always available
    let metrics_section = r#"<section>
    <h2>Metrics</h2>
    <div class="row">
      <span>Graphs</span>
      <a href="/metrics" target="_blank">View Prometheus Metrics</a>
    </div>
  </section>"#;
    html = html.replace("{{METRICS_SECTION}}", metrics_section);

    let nips: String = SUPPORTED_NIPS
        .iter()
        .map(|nip| format!("<span class=\"pill\">NIP-{nip:02}</span>"))
        .collect();
    html = html.replace("{{NIPS}}", &nips);

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    (headers, html)
}

// ── WebSocket connection handler ──────────────────────────────────────────────

/// Cheaply peek the Nostr message type (first array element) without a full
/// JSON parse or signature verification. Tolerant of leading whitespace, so it
/// is robust where a naive `starts_with("[\"REQ\"")` check would fail.
fn message_type(text: &str) -> Option<&str> {
    let s = text.trim_start().strip_prefix('[')?.trim_start();
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

/// Clamp each filter's `limit` to `max_limit` and pre-parse hex tag values so
/// both the initial query and live matching use the fast 32-byte index path.
fn prepare_filters(filters: &mut [Filter], max_limit: usize) {
    for f in filters {
        f.limit = Some(f.limit.map_or(max_limit, |l| l.min(max_limit)));
        f.parse_hex_values();
    }
}

async fn handle_socket(
    socket: WebSocket,
    addr: SocketAddr,
    subscriptions: Arc<SubscriptionManager>,
    fanout: Arc<Fanout>,
    connection_count: Arc<AtomicUsize>,
    verify_sem: Arc<Semaphore>,
    config: Config,
) {
    connection_count.fetch_add(1, Ordering::Relaxed);
    crate::metrics::ACTIVE_CONNECTIONS.inc();

    tracing::info!(%addr, "client connected");

    let (mut ws_send, mut ws_recv) = socket.split();

    // Per-connection send channel for async event delivery (prevents slow clients from blocking).
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<String>(CONN_SEND_CAP);

    // Register with the live-event router. No events are routed here until a
    // REQ opens a subscription, so idle connections cost the publisher nothing.
    let conn = fanout.register(send_tx.clone());

    let mut subs_opened = 0usize;
    let mut subs_closed = 0usize;

    // Idle reaper: a connection that never sends a REQ or an EVENT is holding a
    // slot, a task and a send buffer for nothing. Production logs showed heavy
    // churn of exactly these (`sent=0, subs_opened=0`). Transport-level
    // ping/pong does not count as activity — only client-originated frames do.
    let idle_enabled = config.idle_timeout > 0;
    let mut client_active = false;
    let idle_timer = tokio::time::sleep(std::time::Duration::from_secs(if idle_enabled {
        config.idle_timeout
    } else {
        0
    }));
    tokio::pin!(idle_timer);

    // Track spawned REQ task handles for cleanup on disconnect / CLOSE.
    let mut req_task_handles: std::collections::HashMap<String, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();

    // Sender task: drains from send_rx and writes to the socket.
    let mut sender_handle = tokio::spawn(async move {
        while let Some(msg) = send_rx.recv().await {
            if ws_send
                .send(axum::extract::ws::Message::Text(msg.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            // ── inbound message from client ───────────────────────────────
            msg = ws_recv.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break,
                };

                match msg {
                    axum::extract::ws::Message::Text(text) => {
                        client_active = true;
                        if text.len() > MAX_MESSAGE_BYTES {
                            let notice = NostrMessage::Notification {
                                message: "message too large".to_string(),
                            };
                            let _ = send_tx.try_send(notice.to_json());
                            continue;
                        }

                        match message_type(&text) {
                            Some("EVENT") => {
                                // Offload parse + signature verification (the dominant
                                // CPU cost) to the blocking pool. A global semaphore
                                // caps concurrent verifications so an ingest flood
                                // can't exhaust the pool, but the permit is acquired
                                // inside the spawned task so the select loop stays
                                // responsive to REQ/CLOSE/broadcast traffic.
                                let text_owned = text.to_string();
                                let store = subscriptions.store.clone();
                                let fanout2 = fanout.clone();
                                let send_tx2 = send_tx.clone();
                                let sem = verify_sem.clone();
                                tokio::spawn(async move {
                                    let _permit = match sem.acquire_owned().await {
                                        Ok(p) => p,
                                        Err(_) => return, // semaphore closed → shutting down
                                    };
                                    let outcome = tokio::task::spawn_blocking(move || {
                                        process_event_message(&text_owned, &store, &fanout2)
                                    })
                                    .await
                                    .ok()
                                    .flatten();
                                    if let Some(reply) = outcome {
                                        let _ = send_tx2.send(reply).await;
                                    }
                                });
                            }
                            Some("REQ") => {
                                match NostrMessage::from_json(&text) {
                                    Ok(NostrMessage::Request { id, mut filters }) => {
                                        if filters.len() > MAX_FILTERS_PER_REQ {
                                            let notice = NostrMessage::Notification {
                                                message: format!(
                                                    "too many filters (max {})",
                                                    MAX_FILTERS_PER_REQ
                                                ),
                                            };
                                            let _ = send_tx.try_send(notice.to_json());
                                            continue;
                                        }
                                        // Enforce max concurrent subscriptions per connection.
                                        if !fanout.has_sub(&conn, &id)
                                            && fanout.sub_count(&conn) >= config.max_subscriptions
                                        {
                                            let closed = NostrMessage::Notification {
                                                message: format!(
                                                    "subscription limit reached (max {})",
                                                    config.max_subscriptions
                                                ),
                                            };
                                            let _ = send_tx.try_send(closed.to_json());
                                            continue;
                                        }

                                        prepare_filters(&mut filters, config.max_limit);
                                        fanout.set_sub(&conn, &id, filters.clone());
                                        subs_opened += 1;

                                        // Abort any prior task reusing this sub id.
                                        if let Some(h) = req_task_handles.remove(&id) {
                                            h.abort();
                                        }

                                        let send_tx = send_tx.clone();
                                        let subs = subscriptions.clone();
                                        let id_clone = id.clone();
                                        let sub_start = std::time::Instant::now();

                                        let handle = tokio::spawn(async move {
                                            // Stream matched events directly from stored
                                            // raw bytes (no deep clone, no Vec buffering).
                                            for filter in &filters {
                                                for event in subs.query_filter(filter) {
                                                    let raw = String::from_utf8_lossy(&event.raw);
                                                    let mut msg = String::with_capacity(
                                                        id_clone.len() + raw.len() + 16,
                                                    );
                                                    msg.push_str("[\"EVENT\",\"");
                                                    msg.push_str(&id_clone);
                                                    msg.push_str("\",");
                                                    msg.push_str(&raw);
                                                    msg.push(']');
                                                    if send_tx.send(msg).await.is_err() {
                                                        return;
                                                    }
                                                    crate::metrics::inc_events_output();
                                                }
                                            }
                                            crate::metrics::observe_tteose(sub_start.elapsed());
                                            let eose =
                                                NostrMessage::EndOfStoredEvents { id: id_clone };
                                            let _ = send_tx.send(eose.to_json()).await;
                                        });

                                        req_task_handles.insert(id, handle);
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        let notice = NostrMessage::Notification { message: e };
                                        let _ = send_tx.try_send(notice.to_json());
                                    }
                                }
                            }
                            Some("CLOSE") => {
                                if let Ok(NostrMessage::Close { id }) =
                                    NostrMessage::from_json(&text)
                                {
                                    fanout.remove_sub(&conn, &id);
                                    if let Some(handle) = req_task_handles.remove(&id) {
                                        handle.abort();
                                    }
                                    subs_closed += 1;
                                }
                            }
                            _ => {
                                // Unknown / client-side frame (OK, EOSE, NOTICE) or
                                // malformed. Reply with a NOTICE on parse error only.
                                if let Err(e) = NostrMessage::from_json(&text) {
                                    let notice = NostrMessage::Notification { message: e };
                                    let _ = send_tx.try_send(notice.to_json());
                                }
                            }
                        }
                    }
                    axum::extract::ws::Message::Binary(_) => {
                        client_active = true;
                        let notice = NostrMessage::Notification {
                            message: "binary messages are not supported".to_string(),
                        };
                        let _ = send_tx.try_send(notice.to_json());
                    }
                    axum::extract::ws::Message::Ping(_) => {}
                    axum::extract::ws::Message::Pong(_) => {}
                    axum::extract::ws::Message::Close(_) => break,
                }
            }

            // ── live delivery overflowed for one or more subscriptions ────
            _ = conn.notified() => {
                // A full send queue means this client cannot keep up. Rather
                // than silently dropping events (the old broadcast-lag
                // behaviour), close the affected subscriptions per NIP-01 so
                // the client knows it must resubscribe to resync.
                for sub_id in conn.take_overflowed() {
                    fanout.remove_sub(&conn, &sub_id);
                    if let Some(handle) = req_task_handles.remove(&*sub_id) {
                        handle.abort();
                    }
                    subs_closed += 1;
                    let closed = NostrMessage::Closed {
                        id: sub_id.to_string(),
                        message: "error: client too slow, resubscribe to resync".to_string(),
                    };
                    tracing::warn!(%addr, sub = %sub_id, "subscription overflowed; sent CLOSED");
                    let _ = send_tx.try_send(closed.to_json());
                }

                if conn.should_disconnect() {
                    tracing::warn!(
                        %addr,
                        dropped = conn.dropped(),
                        "disconnecting slow peer (drop rate exceeded)"
                    );
                    break;
                }
            }

            // ── idle connection reaper ──────────────────────────────────
            _ = &mut idle_timer, if idle_enabled && !client_active => {
                tracing::debug!(
                    %addr,
                    "closing idle connection: no REQ or EVENT within {}s",
                    config.idle_timeout
                );
                crate::metrics::inc_idle_disconnects();
                let notice = NostrMessage::Notification {
                    message: format!(
                        "idle: no REQ or EVENT within {}s, closing",
                        config.idle_timeout
                    ),
                };
                let _ = send_tx.try_send(notice.to_json());
                // Let the sender task flush the NOTICE before we tear it down.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                break;
            }

            // ── sender task failure ───────────────────────────────────────
            res = &mut sender_handle => {
                let _ = res;
                break;
            }
        }
    }

    // Unregister first so no further events are routed to this connection.
    fanout.unregister(&conn);

    // Abort all spawned REQ tasks and the sender task.
    for (_, handle) in req_task_handles {
        handle.abort();
    }
    sender_handle.abort();

    connection_count.fetch_sub(1, Ordering::Relaxed);
    crate::metrics::ACTIVE_CONNECTIONS.dec();

    tracing::info!(
        %addr,
        "client disconnected: sent={}, dropped={}, subs_opened={}, subs_closed={}",
        conn.sent(),
        conn.dropped(),
        subs_opened,
        subs_closed
    );
}

/// Parse, verify, store and broadcast an EVENT message. Runs on the blocking
/// thread pool (signature verification is CPU-heavy). Returns the OK/NOTICE
/// reply JSON to send back to the client, if any.
fn process_event_message(
    text: &str,
    store: &Arc<EventStore>,
    fanout: &Arc<Fanout>,
) -> Option<String> {
    match NostrMessage::from_json(text) {
        Ok(NostrMessage::Event { event, .. }) => {
            let event_id = hex::encode(event.id);
            let ev = Arc::new(event);

            // NIP-09: a kind-5 event is a deletion request, not content. Apply
            // it (delete referenced same-pubkey events), store the request
            // itself so clients that already hold the referenced events can
            // hide them, and publish it to live subscribers.
            if ev.is_deletion_request() {
                return Some(apply_deletion_request(&ev, store, fanout));
            }

            let ok = match store.insert(ev.clone()) {
                InsertResult::Ephemeral => {
                    tracing::debug!(id = %event_id, kind = ev.kind, "ephemeral event");
                    fanout.publish(BroadcastEvent::new(ev));
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: "ephemeral: will not be stored".to_string(),
                    }
                }
                InsertResult::Duplicate => {
                    tracing::debug!(id = %event_id, "duplicate event");
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: "duplicate: already have this event".to_string(),
                    }
                }
                InsertResult::Deleted => {
                    // NIP-09: the identical event bytes were removed by a
                    // deletion request. Reject with OK=false so the client
                    // learns the event is gone.
                    tracing::debug!(id = %event_id, "rejected: tombstoned by NIP-09 deletion");
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: false,
                        message: "deleted: this event was removed by a deletion request"
                            .to_string(),
                    }
                }
                InsertResult::Stored { event, replaced } => {
                    tracing::debug!(
                        id = %event_id,
                        kind = event.kind,
                        replaced = replaced.len(),
                        "event stored"
                    );
                    fanout.publish(BroadcastEvent::new(event));
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: String::new(),
                    }
                }
            };
            Some(ok.to_json())
        }
        Ok(_) => None,
        Err(e) => Some(NostrMessage::Notification { message: e }.to_json()),
    }
}

/// Apply a NIP-09 kind-5 deletion request, store the request event itself and
/// publish it to live subscribers. Returns the OK reply JSON.
fn apply_deletion_request(
    ev: &Arc<Event>,
    store: &Arc<EventStore>,
    fanout: &Arc<Fanout>,
) -> String {
    let event_id = hex::encode(ev.id);

    // Replaying a deletion request must not re-run the (unbounded) reference
    // walk: once we hold it, or once it has itself been deleted, answer from
    // the store alone.
    if store.contains(&ev.id) {
        return NostrMessage::Ok {
            id: event_id,
            accepted: true,
            message: "duplicate: already have this event".to_string(),
        }
        .to_json();
    }
    if store.is_tombstoned(&ev.id) {
        return NostrMessage::Ok {
            id: event_id,
            accepted: false,
            message: "deleted: this event was removed by a deletion request".to_string(),
        }
        .to_json();
    }

    let req = ev.deletion_request();
    let outcome = store.apply_deletion(&req, &ev.pubkey, ev.created_at);

    crate::metrics::inc_deletion_requests();

    tracing::debug!(
        id = %event_id,
        deleted = outcome.deleted_ids.len(),
        foreign = outcome.skipped_foreign,
        unknown_coords = outcome.unknown_coordinates,
        "applied NIP-09 deletion request"
    );

    // Persist + broadcast the deletion request itself so clients that already
    // hold the referenced events learn about the deletion (NIP-09: relays
    // SHOULD continue to publish deletion requests indefinitely).
    let ok = match store.insert(ev.clone()) {
        InsertResult::Deleted => NostrMessage::Ok {
            id: event_id,
            accepted: false,
            message: "deleted: this event was removed by a deletion request".to_string(),
        },
        InsertResult::Ephemeral | InsertResult::Duplicate => {
            fanout.publish(BroadcastEvent::new(ev.clone()));
            NostrMessage::Ok {
                id: event_id,
                accepted: true,
                message: String::new(),
            }
        }
        InsertResult::Stored { event, .. } => {
            fanout.publish(BroadcastEvent::new(event));
            NostrMessage::Ok {
                id: event_id,
                accepted: true,
                message: format!("deleted {} event(s)", outcome.deleted_ids.len()),
            }
        }
    };
    ok.to_json()
}
