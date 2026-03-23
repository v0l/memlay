use bytes::Bytes;
use hex::FromHex;
use secp256k1::{Message, XOnlyPublicKey, schnorr::Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// A parsed tag from a Nostr event.
/// First element is the tag type (e.g., "e", "p", "t"), rest are values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// Parse an event from JSON bytes, skipping id/signature verification.
    /// Only for internal tests and benchmarks — do not use on untrusted input.
    #[cfg(any(test, feature = "unchecked"))]
    pub fn from_json_unchecked(json: &[u8]) -> Result<Self, EventParseError> {
        let raw_event: RawEvent = serde_json::from_slice(json)?;
        let id = parse_hex_32(&raw_event.id).ok_or(EventParseError::InvalidId)?;
        let pubkey = parse_hex_32(&raw_event.pubkey).ok_or(EventParseError::InvalidPubkey)?;
        let sig = parse_hex_64(&raw_event.sig).ok_or(EventParseError::InvalidSignature)?;
        let tags = raw_event
            .tags
            .iter()
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

    /// Parse an event from JSON bytes and verify its id and signature.
    pub fn from_json(json: &[u8]) -> Result<Self, EventParseError> {
        let raw_event: RawEvent = serde_json::from_slice(json)?;

        let id = parse_hex_32(&raw_event.id).ok_or(EventParseError::InvalidId)?;
        let pubkey = parse_hex_32(&raw_event.pubkey).ok_or(EventParseError::InvalidPubkey)?;
        let sig = parse_hex_64(&raw_event.sig).ok_or(EventParseError::InvalidSignature)?;

        let tags: Vec<Tag> = raw_event
            .tags
            .iter()
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

        // Verify event id = SHA-256([0, pubkey, created_at, kind, tags, content])
        let serialised = serde_json::to_vec(&serde_json::json!([
            0,
            raw_event.pubkey,
            raw_event.created_at,
            raw_event.kind,
            raw_event.tags,
            raw_event.content,
        ]))
        .map_err(EventParseError::Json)?;
        let computed: [u8; 32] = Sha256::digest(&serialised).into();
        if computed != id {
            return Err(EventParseError::IdMismatch);
        }

        // Verify Schnorr signature
        let xonly =
            XOnlyPublicKey::from_slice(&pubkey).map_err(|_| EventParseError::InvalidPubkey)?;
        let schnorr_sig =
            Signature::from_slice(&sig).map_err(|_| EventParseError::InvalidSignature)?;
        let msg = Message::from_digest(id);
        secp256k1::global::SECP256K1
            .verify_schnorr(&schnorr_sig, &msg, &xonly)
            .map_err(|_| EventParseError::BadSignature)?;

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
    #[error("Event id does not match content hash")]
    IdMismatch,
    #[error("Signature verification failed")]
    BadSignature,
}

/// Parse a 32-byte hex string into bytes
fn parse_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    <[u8; 32]>::from_hex(s).ok()
}

/// Parse a 64-byte hex string into bytes
fn parse_hex_64(s: &str) -> Option<[u8; 64]> {
    if s.len() != 128 {
        return None;
    }
    <[u8; 64]>::from_hex(s).ok()
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

        let event = Event::from_json_unchecked(json.as_bytes()).unwrap();
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
