use crate::config::Config;
use crate::store::{EventStore, StoreConfig};
use crate::subscription::SubscriptionManager;
use axum::{extract::ws::{WebSocket, WebSocketUpgrade}, response::IntoResponse, routing::get, Router};
use std::sync::Arc;

pub struct Relay {
    pub events: Arc<EventStore>,
    pub subscriptions: Arc<SubscriptionManager>,
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
        }
    }

    pub fn router(self) -> Router {
        let subscriptions = self.subscriptions.clone();
        Router::new()
            .route("/", get(move |ws: WebSocketUpgrade| async move {
                ws.on_failed_upgrade(|err| {
                    tracing::warn!("WebSocket failed to upgrade: {}", err);
                })
                .on_upgrade(move |socket: WebSocket| {
                    handle_socket(socket, subscriptions.clone())
                })
            }))
            .route("/info", get(info_handler))
    }
}

async fn info_handler() -> impl IntoResponse {
    let json = serde_json::json!({
        "name": "memlay",
        "description": "High Performance In-Memory Nostr Relay",
        "pubkey": "",
        "supported_nips": [1, 9, 11, 20, 22]
    });
    (axum::response::Json(json)).into_response()
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
