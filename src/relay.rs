use crate::config::Config;
use crate::store::{EventStore, StoreConfig};
use crate::subscription::SubscriptionManager;
use axum::{
    extract::ws::{WebSocket, WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::Arc;

pub struct Relay {
    pub events: Arc<EventStore>,
    pub subscriptions: Arc<SubscriptionManager>,
    config: Config,
}

impl Relay {
    pub fn new(config: Config) -> Self {
        let events = Arc::new(EventStore::new(StoreConfig {
            max_events: config.max_events,
            max_bytes: config.max_bytes,
        }));
        let subscriptions = Arc::new(SubscriptionManager::new(events.clone()));
        Self {
            events,
            subscriptions,
            config,
        }
    }

    pub fn router(self) -> Router {
        let subscriptions = self.subscriptions.clone();
        let config = self.config.clone();
        Router::new().route(
            "/",
            get(move |headers: HeaderMap, ws: Option<WebSocketUpgrade>| {
                let subscriptions = subscriptions.clone();
                let config = config.clone();
                async move {
                    // NIP-11: serve relay info document when Accept header is
                    // application/nostr+json, regardless of WebSocket upgrade.
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
                            .on_failed_upgrade(|err| {
                                tracing::warn!("WebSocket upgrade failed: {}", err);
                            })
                            .on_upgrade(move |socket| {
                                handle_socket(socket, subscriptions)
                            })
                            .into_response(),
                        None => (StatusCode::BAD_REQUEST, "Expected WebSocket upgrade or Accept: application/nostr+json").into_response(),
                    }
                }
            }),
        )
    }
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
            "max_limit": config.max_limit,
        }
    });

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/nostr+json"));
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    headers.insert("Access-Control-Allow-Headers", HeaderValue::from_static("*"));
    headers.insert("Access-Control-Allow-Methods", HeaderValue::from_static("GET"));

    (headers, axum::Json(body)).into_response()
}

async fn handle_socket(mut socket: WebSocket, subscriptions: Arc<SubscriptionManager>) {
    loop {
        let msg = match socket.recv().await {
            Some(Ok(msg)) => msg,
            _ => break,
        };
        
        match msg {
            axum::extract::ws::Message::Text(text) => {
                let msg = crate::message::NostrMessage::from_json(&text);
                match msg {
                    Ok(crate::message::NostrMessage::Event { event, .. }) => {
                        let ev = Arc::new(event);
                        let _ = subscriptions.store.insert(ev);
                    }
                    Ok(crate::message::NostrMessage::Request { id, filters }) => {
                        for filter in filters {
                            let events = subscriptions.query_filter(&filter);
                            for event in events {
                                let msg = crate::message::NostrMessage::Event {
                                    sub_id: Some(id.clone()),
                                    event: (*event).clone(),
                                };
                                let json = msg.to_json();
                                let _ = socket.send(axum::extract::ws::Message::Text(json)).await;
                            }
                        }
                        let eose = crate::message::NostrMessage::EndOfStoredEvents { id };
                        let _ = socket
                            .send(axum::extract::ws::Message::Text(eose.to_json()))
                            .await;
                    }
                    Err(e) => {
                        let _ = socket.send(axum::extract::ws::Message::Text(format!(
                            r#"["ERROR","{}"]"#,
                            e
                        )));
                    }
                    _ => {}
                }
            }
            axum::extract::ws::Message::Binary(_) => {
                let _ = socket.send(axum::extract::ws::Message::Text(
                    r#"["ERROR","Binary messages not supported"]"#
                        .to_string()
                )).await;
            }
            axum::extract::ws::Message::Ping(_) => {}
            axum::extract::ws::Message::Pong(_) => {}
            axum::extract::ws::Message::Close(_) => break,
        }
    }
}
