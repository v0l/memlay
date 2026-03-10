use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A parsed tag from a Nostr event.
/// First element is the tag type (e.g., "e", "p", "t"), rest are values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub values: Vec<String>,
}

impl Tag {
    /// Get the first value (most common case: ["e", "<event_id>"])
    pub fn value(&self) -> Option<&str> {
        self.values.first().map(|s| s.as_str())
    }
}

/// A Nostr event with both parsed fields and raw JSON bytes.
///
/// The raw bytes are kept for zero-copy responses - we can send the original
/// JSON directly to clients without re-serialization.
#[derive(Clone)]
pub struct Event {
    // Parsed fields for indexing
    pub id: [u8; 32],
    pub pubkey: [u8; 32],
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Tag>,
    pub content: String,
    pub sig: [u8; 64],

    // Original JSON for zero-copy response
    pub raw: Bytes,
}

impl Event {
    /// Parse an event from JSON bytes.
    pub fn from_json(json: &[u8]) -> Result<Self, EventParseError> {
        let raw_event: RawEvent = serde_json::from_slice(json)?;

        let id = parse_hex_32(&raw_event.id).ok_or(EventParseError::InvalidId)?;
        let pubkey = parse_hex_32(&raw_event.pubkey).ok_or(EventParseError::InvalidPubkey)?;
        let sig = parse_hex_64(&raw_event.sig).ok_or(EventParseError::InvalidSignature)?;

        let tags = raw_event
            .tags
            .into_iter()
            .filter_map(|tag_arr| {
                if tag_arr.is_empty() {
                    return None;
                }
                Some(Tag {
                    name: tag_arr[0].clone(),
                    values: tag_arr[1..].to_vec(),
                })
            })
            .collect();

        Ok(Event {
            id,
            pubkey,
            created_at: raw_event.created_at,
            kind: raw_event.kind,
            tags,
            content: raw_event.content,
            sig,
            raw: Bytes::copy_from_slice(json),
        })
    }

    /// Get e-tags (referenced event IDs) as bytes
    pub fn e_tags(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.tags.iter().filter_map(|tag| {
            if tag.name == "e" {
                tag.value().and_then(parse_hex_32)
            } else {
                None
            }
        })
    }

    /// Get p-tags (referenced pubkeys) as bytes
    pub fn p_tags(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.tags.iter().filter_map(|tag| {
            if tag.name == "p" {
                tag.value().and_then(parse_hex_32)
            } else {
                None
            }
        })
    }

    /// Approximate memory size of this event
    pub fn size(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.raw.len()
            + self.content.len()
            + self
                .tags
                .iter()
                .map(|t| t.name.len() + t.values.iter().map(|v| v.len()).sum::<usize>())
                .sum::<usize>()
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("id", &hex::encode(self.id))
            .field("pubkey", &hex::encode(self.pubkey))
            .field("created_at", &self.created_at)
            .field("kind", &self.kind)
            .field("tags", &self.tags)
            .field("content_len", &self.content.len())
            .field("raw_len", &self.raw.len())
            .finish()
    }
}

/// Raw JSON structure for parsing
#[derive(Deserialize, Serialize)]
struct RawEvent {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EventParseError {
    #[error("Invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid event id (expected 32-byte hex)")]
    InvalidId,
    #[error("Invalid pubkey (expected 32-byte hex)")]
    InvalidPubkey,
    #[error("Invalid signature (expected 64-byte hex)")]
    InvalidSignature,
}

/// Parse a 32-byte hex string into bytes
fn parse_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hex_str = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(hex_str, 16).ok()?;
    }
    Some(bytes)
}

/// Parse a 64-byte hex string into bytes
fn parse_hex_64(s: &str) -> Option<[u8; 64]> {
    if s.len() != 128 {
        return None;
    }
    let mut bytes = [0u8; 64];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hex_str = std::str::from_utf8(chunk).ok()?;
        bytes[i] = u8::from_str_radix(hex_str, 16).ok()?;
    }
    Some(bytes)
}

/// Helper for hex encoding (we'll add hex crate later if needed)
mod hex {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX_CHARS[(b >> 4) as usize] as char);
            s.push(HEX_CHARS[(b & 0xf) as usize] as char);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_event() {
        let json = r#"{
            "id": "0000000000000000000000000000000000000000000000000000000000000001",
            "pubkey": "0000000000000000000000000000000000000000000000000000000000000002",
            "created_at": 1234567890,
            "kind": 1,
            "tags": [["e", "0000000000000000000000000000000000000000000000000000000000000003"], ["p", "0000000000000000000000000000000000000000000000000000000000000004"]],
            "content": "hello world",
            "sig": "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000005"
        }"#;

        let event = Event::from_json(json.as_bytes()).unwrap();
        assert_eq!(event.id[31], 1);
        assert_eq!(event.pubkey[31], 2);
        assert_eq!(event.created_at, 1234567890);
        assert_eq!(event.kind, 1);
        assert_eq!(event.content, "hello world");
        assert_eq!(event.tags.len(), 2);

        let e_tags: Vec<_> = event.e_tags().collect();
        assert_eq!(e_tags.len(), 1);
        assert_eq!(e_tags[0][31], 3);

        let p_tags: Vec<_> = event.p_tags().collect();
        assert_eq!(p_tags.len(), 1);
        assert_eq!(p_tags[0][31], 4);
    }
}
