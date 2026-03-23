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
    Close {
        id: String,
    },
    EndOfStoredEvents {
        id: String,
    },
    Notification {
        message: String,
    },
    /// ["OK", "<event_id>", <accepted: bool>, "<message>"]
    Ok {
        id: String,
        accepted: bool,
        message: String,
    },
}

impl NostrMessage {
    /// Parse a Nostr message from a JSON string
    pub fn from_json(json: &str) -> Result<Self, String> {
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
                            .map_err(|e| format!("Event serialise error: {}", e))?,
                    )
                    .map_err(|e| format!("{}", e))?;
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
                    // NIP-01: ["REQ", "<id>", <filter1>, <filter2>, ...]
                    let mut filters = Vec::new();
                    for filter_json in &arr[2..] {
                        let filter: crate::subscription::Filter =
                            serde_json::from_value(filter_json.clone())
                                .map_err(|e| format!("Invalid filter: {}", e))?;
                        filters.push(filter);
                    }
                    Ok(NostrMessage::Request { id, filters })
                }
                "CLOSE" => {
                    if arr.len() < 2 {
                        return Err("Missing subscription ID".to_string());
                    }
                    let id = arr[1]
                        .as_str()
                        .ok_or("Invalid subscription ID")?
                        .to_string();
                    Ok(NostrMessage::Close { id })
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
                    if arr.len() < 4 {
                        return Err("Missing OK fields".to_string());
                    }
                    let id = arr[1].as_str().ok_or("Invalid OK id")?.to_string();
                    let accepted = arr[2].as_bool().ok_or("Invalid OK accepted field")?;
                    let message = arr[3].as_str().ok_or("Invalid OK message")?.to_string();
                    Ok(NostrMessage::Ok {
                        id,
                        accepted,
                        message,
                    })
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
                if let Some(id) = sub_id {
                    format!(
                        r#"["EVENT","{}",{}]"#,
                        id,
                        String::from_utf8_lossy(&event.raw)
                    )
                } else {
                    String::from_utf8_lossy(&event.raw).into_owned()
                }
            }
            NostrMessage::Request { id, filters } => {
                let filters_json = serde_json::to_string(filters).unwrap_or_default();
                format!(r#"["REQ","{}",{}]"#, id, filters_json)
            }
            NostrMessage::Close { id } => {
                format!(r#"["CLOSE","{}"]"#, id)
            }
            NostrMessage::EndOfStoredEvents { id } => {
                format!(r#"["EOSE","{}"]"#, id)
            }
            NostrMessage::Notification { message } => {
                format!(r#"["NOTICE","{}"]"#, message)
            }
            NostrMessage::Ok {
                id,
                accepted,
                message,
            } => {
                format!(r#"["OK","{}",{},"{}"]"#, id, accepted, message)
            }
        }
    }
}
