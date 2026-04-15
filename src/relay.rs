use crate::config::Config;
use crate::message::NostrMessage;
use crate::store::{EventStore, InsertResult, StoreConfig};
use crate::subscription::{Filter, Subscription, SubscriptionManager};
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
use tokio::sync::broadcast;

/// Capacity of the relay-wide new-event broadcast channel.
/// Increased to handle high event volume with many concurrent clients.
const BROADCAST_CAP: usize = 262144;

/// Capacity of per-connection send channels (backpressure for slow clients).
/// Increased to handle burst traffic without dropping events.
const CONN_SEND_CAP: usize = 4096;

/// Maximum consecutive dropped events before disconnecting a slow peer.
const SLOW_PEER_MAX_DROPPED: usize = 100;

/// Shared state threaded through axum via `State<Arc<AppState>>`.
struct AppState {
    events: Arc<EventStore>,
    subscriptions: Arc<SubscriptionManager>,
    tx: broadcast::Sender<Arc<crate::event::Event>>,
    config: Config,
    connection_count: Arc<AtomicUsize>,
}

pub struct Relay {
    pub events: Arc<EventStore>,
    pub subscriptions: Arc<SubscriptionManager>,
    /// Sender side of the relay-wide broadcast for newly accepted events.
    tx: broadcast::Sender<Arc<crate::event::Event>>,
    config: Config,
    /// Count of active WebSocket connections
    connection_count: Arc<AtomicUsize>,
}

impl Relay {
    pub fn new(config: Config) -> Self {
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

        // Start background persistence task if enabled
        if config.persistence_path.is_some() {
            events.start_persistence_task(config.persistence_interval);
        }

        let subscriptions = Arc::new(SubscriptionManager::new(events.clone()));
        let (tx, _) = broadcast::channel(BROADCAST_CAP);
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
            tx,
            config,
            connection_count,
        }
    }

    pub fn router(self) -> Router {
        let app_state = Arc::new(AppState {
            events: self.events.clone(),
            subscriptions: self.subscriptions.clone(),
            tx: self.tx.clone(),
            config: self.config.clone(),
            connection_count: self.connection_count.clone(),
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

    match ws {
        Ok(ws) => {
            let subscriptions = state.subscriptions.clone();
            let tx = state.tx.clone();
            let conn_count = state.connection_count.clone();
            ws.on_failed_upgrade(move |err| {
                tracing::warn!(%addr, "WebSocket upgrade failed: {}", err);
            })
            .on_upgrade(move |socket| handle_socket(socket, addr, subscriptions, tx, conn_count))
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
        "store_bytes": store_memory,
        "max_bytes": cfg.max_bytes,
        "process_memory": crate::store::get_process_memory(),
        "memory_limit": cfg.max_bytes,
        "memory_used": store_memory,
    });
    let mut headers = HeaderMap::new();
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    (headers, axum::Json(body))
}

fn nip11_handler(config: &Config) -> impl IntoResponse {
    let body = serde_json::json!({
        "name": "memlay",
        "description": "High-performance in-memory Nostr relay",
        "software": "https://github.com/v0l/memlay",
        "version": env!("CARGO_PKG_VERSION"),
        "supported_nips": [1, 11],
        "limitation": {
            "max_subscriptions": config.max_subscriptions,
            "max_limit": config.max_limit
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

async fn handle_socket(
    socket: WebSocket,
    addr: SocketAddr,
    subscriptions: Arc<SubscriptionManager>,
    tx: broadcast::Sender<Arc<crate::event::Event>>,
    connection_count: Arc<AtomicUsize>,
) {
    // Increment connection count
    connection_count.fetch_add(1, Ordering::Relaxed);
    crate::metrics::ACTIVE_CONNECTIONS.inc();

    tracing::info!(%addr, "client connected");

    // Split socket into send/receive parts
    let (mut ws_send, mut ws_recv) = socket.split();

    // Per-connection subscription state: sub_id → filters
    let mut conn_subs: std::collections::HashMap<String, Vec<Filter>> =
        std::collections::HashMap::new();

    // Per-connection send channel for async event delivery (prevents slow clients from blocking)
    let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<String>(CONN_SEND_CAP);

    let mut rx = tx.subscribe();

    // Track consecutive dropped events for slow peer detection
    let mut consecutive_dropped = 0;

    // Track event statistics for this connection
    let mut events_sent = 0;
    let mut events_dropped = 0;

    // Track subscription statistics for this connection
    let mut subs_opened = 0;
    let mut subs_closed = 0;

    // Track spawned REQ task handles for cleanup on disconnect
    let mut req_task_handles: std::collections::HashMap<String, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();

    // Spawn sender task - drains from send_rx and sends to socket
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
                                // Handle REQ asynchronously to keep main loop responsive
                                if text.starts_with("[\"REQ\"") {
                                    let text_str = text.as_ref();
                                    if let Ok(NostrMessage::Request { id, filters }) = NostrMessage::from_json(text_str) {
                                        // Update conn_subs in main loop immediately so broadcast events are matched
                                        conn_subs.insert(id.clone(), filters.clone());

                                        let send_tx = send_tx.clone();
                                        let subs = subscriptions.clone();
                                        let filters = filters.clone();
                                        let id_clone = id.clone();

                                        let handle = tokio::spawn(async move {
                                            let query_start = std::time::Instant::now();

                                            // Register subscription
                                            subs.add_subscription(crate::subscription::Subscription {
                                                id: id_clone.clone(),
                                                filters: filters.clone(),
                                            });

                                            // Query events directly (no spawn_blocking - queries are fast)
                                            let mut events = Vec::new();
                                            for filter in &filters {
                                                let filter_query_start = std::time::Instant::now();
                                                let matched = subs.query_filter(filter);
                                                let filter_query_duration = filter_query_start.elapsed();

                                                tracing::trace!(
                                                    sub_id = %id_clone,
                                                    filter_idx = 0,
                                                    matched_events = matched.len(),
                                                    query_duration_us = filter_query_duration.as_micros(),
                                                    "query_filter completed"
                                                );

                                                for event in matched {
                                                    events.push(NostrMessage::Event {
                                                        sub_id: Some(id_clone.clone()),
                                                        event: event.as_ref().clone(),
                                                    });
                                                    crate::metrics::inc_events_output();
                                                }
                                            }

                                            let query_duration = query_start.elapsed();
                                            tracing::trace!(
                                                sub_id = %id_clone,
                                                total_events = events.len(),
                                                query_duration_us = query_duration.as_micros(),
                                                "REQ query completed"
                                            );

                                            // Send events from async context
                                            let mut send_count = 0;
                                            let mut send_duration = std::time::Duration::ZERO;
                                            for msg in events {
                                                let send_start = std::time::Instant::now();
                                                let _ = send_tx.send(msg.to_json()).await;
                                                send_duration += send_start.elapsed();
                                                send_count += 1;
                                            }

                                            tracing::trace!(
                                                sub_id = %id_clone,
                                                send_count = send_count,
                                                send_duration_us = send_duration.as_micros(),
                                                "REQ send completed"
                                            );

                                            let eose = NostrMessage::EndOfStoredEvents { id: id_clone };
                                            let _ = send_tx.send(eose.to_json()).await;
                                        });

                                        // Track the task handle for cleanup
                                        req_task_handles.insert(id, handle);
                                    }
                        } else if text.starts_with("[\"CLOSE\"") {
                            // Handle CLOSE for async subscriptions
                            let text_str = text.as_ref();
                            if let Ok(NostrMessage::Close { id }) = NostrMessage::from_json(text_str) {
                                subscriptions.remove_subscription(&id);
                                conn_subs.remove(&id);
                                // Abort the spawned task if it exists
                                if let Some(handle) = req_task_handles.remove(&id) {
                                    handle.abort();
                                }
                            }
                        } else {
                            handle_text(&text, addr, &send_tx, &subscriptions, &tx, &mut conn_subs, &mut events_sent, &mut events_dropped, &mut subs_opened, &mut subs_closed).await;
                        }
                    }
                    axum::extract::ws::Message::Binary(_) => {
                        let notice = NostrMessage::Notification {
                            message: "binary messages are not supported".to_string(),
                        };
                        let _ = send_tx.send(notice.to_json()).await;
                    }
                    axum::extract::ws::Message::Ping(_) => {
                        // axum automatically responds to pings
                    }
                    axum::extract::ws::Message::Pong(_) => {}
                    axum::extract::ws::Message::Close(_) => break,
                }
            }

            // ── new event broadcast from another connection ───────────────
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        // Pre-compute event JSON once
                        let event_json = String::from_utf8_lossy(&event.raw);
                        let mut dropped_for_event = 0;

                        // Check every active subscription on this connection
                        for (sub_id, filters) in &conn_subs {
                            for filter in filters {
                                if filter_matches(filter, &*event) {
                                    // Construct message with pre-computed JSON
                                    let msg = format!(r#"["EVENT","{}",{}]"#, sub_id, event_json);
                                    // Send asynchronously via channel (non-blocking)
                                    if send_tx.try_send(msg).is_err() {
                                        dropped_for_event += 1;
                                        events_dropped += 1;
                                        consecutive_dropped += 1;

                                        // Disconnect slow peer
                                        if consecutive_dropped >= SLOW_PEER_MAX_DROPPED {
                                            tracing::warn!(%addr, "disconnecting slow peer after {} dropped events", consecutive_dropped);
                                            break;
                                        }
                                    } else {
                                        events_sent += 1;
                                    }
                                    // Track events output
                                    crate::metrics::inc_events_output();
                                    break; // only send once per subscription even if multiple filters match
                                }
                            }
                            if consecutive_dropped >= SLOW_PEER_MAX_DROPPED {
                                break;
                            }
                        }

                        if dropped_for_event > 0 {
                            tracing::debug!(%addr, "dropped {} events for this broadcast", dropped_for_event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%addr, "broadcast lagged by {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // ── sender task failure ───────────────────────────────────────
            res = &mut sender_handle => {
                let _ = res;
                break; // Sender task failed
            }
        }

        // Check for slow peer after each iteration
        if consecutive_dropped >= SLOW_PEER_MAX_DROPPED {
            tracing::warn!(%addr, "disconnecting slow peer ({} dropped events)", consecutive_dropped);
            break;
        }
    }

    // Clean up all subscriptions for this connection on disconnect
    for sub_id in conn_subs.keys() {
        subscriptions.remove_subscription(sub_id);
    }

    // Abort all spawned REQ tasks for this connection
    for (_, handle) in req_task_handles {
        handle.abort();
    }

    // Abort sender task
    sender_handle.abort();

    // Decrement connection count
    connection_count.fetch_sub(1, Ordering::Relaxed);
    crate::metrics::ACTIVE_CONNECTIONS.dec();

    tracing::info!(%addr, "client disconnected: sent={}, dropped={}, subs_opened={}, subs_closed={}", events_sent, events_dropped, subs_opened, subs_closed);
}

async fn handle_text(
    text: &str,
    addr: SocketAddr,
    send_tx: &tokio::sync::mpsc::Sender<String>,
    subscriptions: &Arc<SubscriptionManager>,
    tx: &broadcast::Sender<Arc<crate::event::Event>>,
    conn_subs: &mut std::collections::HashMap<String, Vec<Filter>>,
    _events_sent: &mut usize,
    _events_dropped: &mut usize,
    _subs_opened: &mut usize,
    subs_closed: &mut usize,
) {
    match NostrMessage::from_json(text) {
        Ok(NostrMessage::Event { event, .. }) => {
            let event_id = hex::encode(event.id);
            let ev = Arc::new(event);

            let ok = match subscriptions.store.insert(ev.clone()) {
                InsertResult::Ephemeral => {
                    tracing::debug!(%addr, id = %event_id, kind = ev.kind, "ephemeral event");
                    // Ephemeral events are not stored but still broadcast to active subscribers
                    subscriptions.invalidate_cache();
                    // Use try_send to prevent blocking when broadcast channel is full
                    let _ = tx.send(ev);
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: "ephemeral: will not be stored".to_string(),
                    }
                }
                InsertResult::Duplicate => {
                    tracing::debug!(%addr, id = %event_id, "duplicate event");
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: "duplicate: already have this event".to_string(),
                    }
                }
                InsertResult::Stored { event, replaced } => {
                    tracing::info!(
                        %addr,
                        id = %event_id,
                        kind = event.kind,
                        pubkey = %hex::encode(event.pubkey),
                        replaced = replaced.len(),
                        "event stored"
                    );
                    // Invalidate query cache on new event insertion
                    subscriptions.invalidate_cache();
                    // Broadcast to live subscriptions on other connections
                    // Use try_send to prevent blocking when broadcast channel is full
                    let _ = tx.send(event);
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: String::new(),
                    }
                }
            };
            let _ = send_tx.send(ok.to_json()).await;
        }

        Ok(NostrMessage::Request { id, filters }) => {
            // Register subscription and update conn_subs immediately
            // so broadcast events are matched while the query runs.
            conn_subs.insert(id.clone(), filters.clone());

            let send_tx = send_tx.clone();
            let subs = subscriptions.clone();
            let filters_clone = filters.clone();
            let id_clone = id.clone();
            let sub_start = std::time::Instant::now();

            tokio::spawn(async move {
                // Register subscription
                subs.add_subscription(Subscription {
                    id: id_clone.clone(),
                    filters: filters_clone.clone(),
                });

                // Query events
                let mut events = Vec::new();
                for filter in &filters_clone {
                    let matched = subs.query_filter(filter);
                    for event in matched {
                        events.push(NostrMessage::Event {
                            sub_id: Some(id_clone.clone()),
                            event: event.as_ref().clone(),
                        });
                        crate::metrics::inc_events_output();
                    }
                }

                // Send events
                for msg in events {
                    let _ = send_tx.send(msg.to_json()).await;
                }

                // Record TTEOSE
                crate::metrics::observe_tteose(sub_start.elapsed());

                let eose = NostrMessage::EndOfStoredEvents { id: id_clone };
                let _ = send_tx.send(eose.to_json()).await;
            });
        }

        Ok(NostrMessage::Close { id }) => {
            subscriptions.remove_subscription(&id);
            conn_subs.remove(&id);
            *subs_closed += 1;
        }

        Err(e) => {
            let notice = NostrMessage::Notification { message: e };
            let _ = send_tx.send(notice.to_json()).await;
        }

        _ => {} // client-side messages (OK, EOSE, NOTICE) — ignore
    }
}

// ── filter matching for live delivery ────────────────────────────────────────

/// Returns true if `event` matches `filter` (same AND logic as query_filter,
/// but applied to a single event without going through the store).
fn filter_matches(filter: &Filter, event: &crate::event::Event) -> bool {
    if let Some(since) = filter.since
        && event.created_at < since
    {
        return false;
    }
    if let Some(until) = filter.until
        && event.created_at > until
    {
        return false;
    }
    if let Some(kinds) = &filter.kinds
        && !kinds.contains(&event.kind)
    {
        return false;
    }
    if let Some(ids) = &filter.ids {
        let event_id_hex = hex::encode(event.id);
        if !ids.iter().any(|id| event_id_hex.starts_with(&id.as_hex())) {
            return false;
        }
    }
    if let Some(authors) = &filter.authors {
        let pubkey_hex = hex::encode(event.pubkey);
        if !authors.iter().any(|a| pubkey_hex.starts_with(&a.as_hex())) {
            return false;
        }
    }
    if !filter.tag_filters.is_empty() {
        for (&letter, values) in &filter.tag_filters {
            let matched = event.tags.iter().any(|tag| {
                let mut chars = tag.name.chars();
                matches!((chars.next(), chars.next()), (Some(l), None) if l == letter)
                    && tag.value().is_some_and(|v| values.iter().any(|fv| fv == v))
            });
            if !matched {
                return false;
            }
        }
    }
    true
}
