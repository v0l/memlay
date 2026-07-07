use crate::event::Event;

/// Serialize a string as a JSON string literal (with surrounding quotes),
/// properly escaping quotes, backslashes and control characters.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

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
    /// Parse a Nostr message from a JSON string.
    ///
    /// Uses `RawValue` so the embedded event object is sliced out of the
    /// original bytes and parsed exactly once (no intermediate `Value` tree,
    /// no re-serialization on the hot ingest path).
    pub fn from_json(json: &str) -> Result<Self, String> {
        use serde_json::value::RawValue;

        let arr: Vec<&RawValue> =
            serde_json::from_str(json).map_err(|e| format!("JSON parse error: {}", e))?;

        if arr.is_empty() {
            return Err("Empty array".to_string());
        }

        let msg_type: &str =
            serde_json::from_str(arr[0].get()).map_err(|_| "Invalid message type".to_string())?;

        match msg_type {
            "EVENT" => {
                if arr.len() < 2 {
                    return Err("Missing event data".to_string());
                }
                // Parse straight from the event's original JSON bytes.
                let event =
                    Event::from_json(arr[1].get().as_bytes()).map_err(|e| format!("{}", e))?;
                Ok(NostrMessage::Event {
                    event,
                    sub_id: None,
                })
            }
            "REQ" => {
                if arr.len() < 3 {
                    return Err("Missing filters".to_string());
                }
                let id: String = serde_json::from_str(arr[1].get())
                    .map_err(|_| "Missing subscription ID".to_string())?;
                // NIP-01: ["REQ", "<id>", <filter1>, <filter2>, ...]
                let mut filters = Vec::with_capacity(arr.len() - 2);
                for filter_json in &arr[2..] {
                    let filter: crate::subscription::Filter =
                        serde_json::from_str(filter_json.get())
                            .map_err(|e| format!("Invalid filter: {}", e))?;
                    filters.push(filter);
                }
                Ok(NostrMessage::Request { id, filters })
            }
            "CLOSE" => {
                if arr.len() < 2 {
                    return Err("Missing subscription ID".to_string());
                }
                let id: String = serde_json::from_str(arr[1].get())
                    .map_err(|_| "Invalid subscription ID".to_string())?;
                Ok(NostrMessage::Close { id })
            }
            "EOSE" => {
                if arr.len() < 2 {
                    return Err("Missing EOSE id".to_string());
                }
                let id: String = serde_json::from_str(arr[1].get())
                    .map_err(|_| "Invalid EOSE id".to_string())?;
                Ok(NostrMessage::EndOfStoredEvents { id })
            }
            "NOTICE" => {
                if arr.len() < 2 {
                    return Err("Missing notice message".to_string());
                }
                let message: String = serde_json::from_str(arr[1].get())
                    .map_err(|_| "Invalid notice message".to_string())?;
                Ok(NostrMessage::Notification { message })
            }
            "OK" => {
                if arr.len() < 4 {
                    return Err("Missing OK fields".to_string());
                }
                let id: String =
                    serde_json::from_str(arr[1].get()).map_err(|_| "Invalid OK id".to_string())?;
                let accepted: bool = serde_json::from_str(arr[2].get())
                    .map_err(|_| "Invalid OK accepted field".to_string())?;
                let message: String = serde_json::from_str(arr[3].get())
                    .map_err(|_| "Invalid OK message".to_string())?;
                Ok(NostrMessage::Ok {
                    id,
                    accepted,
                    message,
                })
            }
            _ => Err(format!("Unknown message type: {}", msg_type)),
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
                format!(r#"["REQ",{},{}]"#, json_str(id), filters_json)
            }
            NostrMessage::Close { id } => {
                format!(r#"["CLOSE",{}]"#, json_str(id))
            }
            NostrMessage::EndOfStoredEvents { id } => {
                format!(r#"["EOSE",{}]"#, json_str(id))
            }
            NostrMessage::Notification { message } => {
                // message may contain client-supplied text (e.g. parse errors);
                // escape it via serde to avoid emitting malformed JSON.
                format!(r#"["NOTICE",{}]"#, json_str(message))
            }
            NostrMessage::Ok {
                id,
                accepted,
                message,
            } => {
                format!(
                    r#"["OK",{},{},{}]"#,
                    json_str(id),
                    accepted,
                    json_str(message)
                )
            }
        }
    }
}
