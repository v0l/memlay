use crate::event::Event;

/// Nostr protocol message types
#[derive(Debug, Clone)]
pub enum NostrMessage {
    Event {
        sub_id: Option<String>,
        event: Event,
    },
    Request {
        id: String,
        filters: Vec<crate::subscription::Filter>,
    },
    EndOfStoredEvents {
        id: String,
    },
    Notification {
        message: String,
    },
    Success {
        id: String,
        message: String,
    },
    Error {
        id: String,
        message: String,
    },
}

impl NostrMessage {
    /// Parse a Nostr message from a JSON string
    pub fn from_json(json: &str) -> Result<Self, String> {
        // Try to parse as array first (Nostr protocol format)
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("JSON parse error: {}", e))?;

        if let serde_json::Value::Array(arr) = value {
            if arr.is_empty() {
                return Err("Empty array".to_string());
            }

            let msg_type = arr[0].as_str().ok_or("Invalid message type")?;

            match msg_type {
                "EVENT" => {
                    if arr.len() < 2 {
                        return Err("Missing event data".to_string());
                    }
                    let event_json = &arr[1];
                    let event = Event::from_json(
                        &serde_json::to_vec(event_json)
                            .map_err(|e| format!("Event parse error: {}", e))?,
                    )
                    .map_err(|e| format!("Invalid event: {}", e))?;
                    Ok(NostrMessage::Event {
                        event,
                        sub_id: None,
                    })
                }
                "REQ" => {
                    if arr.len() < 3 {
                        return Err("Missing filters".to_string());
                    }
                    let id = arr[1]
                        .as_str()
                        .ok_or("Missing subscription ID")?
                        .to_string();
                    let filters_json = &arr[2];
                    let filters: Vec<crate::subscription::Filter> =
                        serde_json::from_value(filters_json.clone())
                            .map_err(|e| format!("Invalid filters: {}", e))?;
                    Ok(NostrMessage::Request { id, filters })
                }
                "EOSE" => {
                    if arr.len() < 2 {
                        return Err("Missing EOSE id".to_string());
                    }
                    let id = arr[1].as_str().ok_or("Invalid EOSE id")?.to_string();
                    Ok(NostrMessage::EndOfStoredEvents { id })
                }
                "NOTICE" => {
                    if arr.len() < 2 {
                        return Err("Missing notice message".to_string());
                    }
                    let message = arr[1].as_str().ok_or("Invalid notice message")?.to_string();
                    Ok(NostrMessage::Notification { message })
                }
                "OK" => {
                    if arr.len() < 3 {
                        return Err("Missing OK fields".to_string());
                    }
                    let id = arr[1].as_str().ok_or("Invalid OK id")?.to_string();
                    let message = arr[2].as_str().ok_or("Invalid OK message")?.to_string();
                    Ok(NostrMessage::Success { id, message })
                }
                "CLOSED" | "ERROR" => {
                    if arr.len() < 3 {
                        return Err("Missing CLOSED/ERROR fields".to_string());
                    }
                    let id = arr[1].as_str().ok_or("Invalid id")?.to_string();
                    let message = arr[2].as_str().ok_or("Invalid message")?.to_string();
                    Ok(NostrMessage::Error { id, message })
                }
                _ => Err(format!("Unknown message type: {}", msg_type)),
            }
        } else {
            Err("Expected JSON array".to_string())
        }
    }

    /// Serialize the message to JSON
    pub fn to_json(&self) -> String {
        match self {
            NostrMessage::Event { sub_id, event } => {
                let json = String::from_utf8(event.raw.to_vec())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if let Some(id) = sub_id {
                    format!(r#"["EVENT","{}",{}]"#, id, json)
                } else {
                    json
                }
            }
            NostrMessage::Request { id, filters } => {
                let filters_json = serde_json::to_string(filters).unwrap_or_default();
                format!(r#"["REQ","{}",{}]"#, id, filters_json)
            }
            NostrMessage::EndOfStoredEvents { id } => {
                format!(r#"["EOSE","{}"]"#, id)
            }
            NostrMessage::Notification { message } => {
                format!(r#"["NOTICE","{}"]"#, message)
            }
            NostrMessage::Success { id, message } => {
                format!(r#"["OK","{}","{}"]"#, id, message)
            }
            NostrMessage::Error { id, message } => {
                format!(r#"["ERROR","{}","{}"]"#, id, message)
            }
        }
    }
}
