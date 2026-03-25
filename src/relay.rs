use crate::config::Config;
use crate::message::NostrMessage;
use crate::store::{EventStore, InsertResult, StoreConfig};
use crate::subscription::{Filter, Subscription, SubscriptionManager};
use axum::{
    Router,
    extract::{
        ConnectInfo,
        ws::{WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue},
    response::IntoResponse,
    routing::get,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Capacity of the relay-wide new-event broadcast channel.
const BROADCAST_CAP: usize = 8192;

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
        let subscriptions = self.subscriptions.clone();
        let tx = self.tx.clone();
        let config = self.config.clone();

        let events_for_stats = self.events.clone();
        let connection_count = self.connection_count.clone();
        
        let mut router = Router::new()
            .route(
                "/stats",
                get(move || {
                    let events = events_for_stats.clone();
                    async move { stats_handler(events) }
                }),
            )
            .route(
                "/",
                get(
                    move |ConnectInfo(addr): ConnectInfo<SocketAddr>,
                          headers: HeaderMap,
                          ws: Option<WebSocketUpgrade>| {
                        let subscriptions = subscriptions.clone();
                        let tx = tx.clone();
                        let config = config.clone();
                        let conn_count = connection_count.clone();
                        async move {
                            let wants_info = headers
                                .get("accept")
                                .and_then(|v| v.to_str().ok())
                                .map(|v| v.contains("application/nostr+json"))
                                .unwrap_or(false);

                            if wants_info {
                                return nip11_handler(&config).into_response();
                            }

                            match ws {
                                Some(ws) => ws
                                    .on_failed_upgrade(move |err| {
                                        tracing::warn!(%addr, "WebSocket upgrade failed: {}", err);
                                    })
                                    .on_upgrade(move |socket| {
                                        handle_socket(socket, addr, subscriptions, tx, conn_count)
                                    })
                                    .into_response(),
                                None => landing_page(&config).into_response(),
                            }
                        }
                    },
                ),
            );
        
        // Add metrics endpoint
        router = router.route(
            "/metrics",
            get(metrics_handler),
        );
        
        router
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn metrics_handler() -> impl IntoResponse {
    let metrics = crate::metrics::gather_metrics();
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    (headers, metrics)
}

fn stats_handler(events: Arc<crate::store::EventStore>) -> impl IntoResponse {
    let cfg = events.config();
    let process_memory = crate::store::get_process_memory();
    let body = serde_json::json!({
        "events": events.len(),
        "store_bytes": events.bytes_used(),
        "max_bytes": cfg.max_bytes,
        "process_memory": process_memory,
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
    let mut html = include_str!("index.html")
        .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"));
    
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
    mut socket: WebSocket,
    addr: SocketAddr,
    subscriptions: Arc<SubscriptionManager>,
    tx: broadcast::Sender<Arc<crate::event::Event>>,
    connection_count: Arc<AtomicUsize>,
) {
    // Increment connection count
    connection_count.fetch_add(1, Ordering::Relaxed);
    crate::metrics::ACTIVE_CONNECTIONS.inc();
    
    tracing::info!(%addr, "client connected");

    // Per-connection subscription state: sub_id → filters
    let mut conn_subs: std::collections::HashMap<String, Vec<Filter>> =
        std::collections::HashMap::new();

    let mut rx = tx.subscribe();

    loop {
        tokio::select! {
            // ── inbound message from client ───────────────────────────────
            msg = socket.recv() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break,
                };

                match msg {
                    axum::extract::ws::Message::Text(text) => {
                        handle_text(&text, addr, &mut socket, &subscriptions, &tx, &mut conn_subs).await;
                    }
                    axum::extract::ws::Message::Binary(_) => {
                        let notice = NostrMessage::Notification {
                            message: "binary messages are not supported".to_string(),
                        };
                        let _ = socket.send(axum::extract::ws::Message::Text(notice.to_json())).await;
                    }
                    axum::extract::ws::Message::Ping(data) => {
                        let _ = socket.send(axum::extract::ws::Message::Pong(data)).await;
                    }
                    axum::extract::ws::Message::Pong(_) => {}
                    axum::extract::ws::Message::Close(_) => break,
                }
            }

            // ── new event broadcast from another connection ───────────────
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        // Check every active subscription on this connection
                        for (sub_id, filters) in &conn_subs {
                            for filter in filters {
                                if filter_matches(filter, &event) {
                                    let msg = NostrMessage::Event {
                                        sub_id: Some(sub_id.clone()),
                                        event: (*event).clone(),
                                    };
                                    if socket
                                        .send(axum::extract::ws::Message::Text(msg.to_json()))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    // Track events output
                                    crate::metrics::inc_events_output();
                                    break; // only send once per subscription even if multiple filters match
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%addr, "broadcast lagged by {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    // Clean up all subscriptions for this connection on disconnect
    for sub_id in conn_subs.keys() {
        subscriptions.remove_subscription(sub_id);
    }
    
    // Decrement connection count
    connection_count.fetch_sub(1, Ordering::Relaxed);
    crate::metrics::ACTIVE_CONNECTIONS.dec();
    
    tracing::info!(%addr, "client disconnected");
}

async fn handle_text(
    text: &str,
    addr: SocketAddr,
    socket: &mut WebSocket,
    subscriptions: &Arc<SubscriptionManager>,
    tx: &broadcast::Sender<Arc<crate::event::Event>>,
    conn_subs: &mut std::collections::HashMap<String, Vec<Filter>>,
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
                    let _ = tx.send(event);
                    NostrMessage::Ok {
                        id: event_id,
                        accepted: true,
                        message: String::new(),
                    }
                }
            };
            let _ = socket
                .send(axum::extract::ws::Message::Text(ok.to_json()))
                .await;
        }

        Ok(NostrMessage::Request { id, filters }) => {
            // Track subscription start time for TTEOSE metric
            let sub_start = std::time::Instant::now();
            
            // Register subscription
            subscriptions.add_subscription(Subscription {
                id: id.clone(),
                filters: filters.clone(),
            });
            conn_subs.insert(id.clone(), filters.clone());

            // Send stored matching events
            for filter in &filters {
                let events = subscriptions.query_filter(filter);
                for event in events {
                    let msg = NostrMessage::Event {
                        sub_id: Some(id.clone()),
                        event: (*event).clone(),
                    };
                    if socket
                        .send(axum::extract::ws::Message::Text(msg.to_json()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    // Track events output
                    crate::metrics::inc_events_output();
                }
            }

            // Record TTEOSE (Time To End Of Stored Events)
            crate::metrics::observe_tteose(sub_start.elapsed());

            let eose = NostrMessage::EndOfStoredEvents { id };
            let _ = socket
                .send(axum::extract::ws::Message::Text(eose.to_json()))
                .await;
        }

        Ok(NostrMessage::Close { id }) => {
            subscriptions.remove_subscription(&id);
            conn_subs.remove(&id);
        }

        Err(e) => {
            let notice = NostrMessage::Notification { message: e };
            let _ = socket
                .send(axum::extract::ws::Message::Text(notice.to_json()))
                .await;
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
