use bytes::Bytes;
use hex::FromHex;
use secp256k1::{schnorr::Signature, XOnlyPublicKey};
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
    /// Only for internal tests, benchmarks, and WAL replay — do not use on untrusted input.
    /// WAL replay is safe because we're reading our own persisted data.
    #[cfg(any(test, feature = "unchecked", feature = "wal"))]
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
            XOnlyPublicKey::from_byte_array(pubkey).map_err(|_| EventParseError::InvalidPubkey)?;
        let schnorr_sig = Signature::from_byte_array(sig);
        secp256k1::global::SECP256K1
            .verify_schnorr(&schnorr_sig, &id, &xonly)
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

    /// Check if this is an ephemeral event (kind 20000-29999)
    /// Ephemeral events should NOT be stored, only broadcast to active subscribers
    pub fn is_ephemeral(&self) -> bool {
        self.kind >= 20000 && self.kind < 30000
    }

    /// Check if this is a replaceable event (kind 10000-19999 or 30000-39999)
    /// Only the latest version should be kept per (pubkey, kind) or (pubkey, kind, d-tag)
    pub fn is_replaceable(&self) -> bool {
        (self.kind >= 10000 && self.kind < 20000) || (self.kind >= 30000 && self.kind < 40000)
    }

    /// Check if this is an addressable event (kind 30000-39999)
    /// Addressable events are identified by (pubkey, kind, d-tag)
    pub fn is_addressable(&self) -> bool {
        self.kind >= 30000 && self.kind < 40000
    }

    /// Get the d-tag value for addressable events (kind 30000-39999)
    /// Returns None for non-addressable events or if no d-tag is present
    pub fn d_tag(&self) -> Option<&str> {
        if !self.is_addressable() {
            return None;
        }
        self.tags
            .iter()
            .find_map(|tag| if tag.name == "d" { tag.value() } else { None })
    }

    /// Get the replacement key for this event
    /// For regular replaceable events (0-9999): (pubkey, kind)
    /// For addressable events (30000-39999): (pubkey, kind, d-tag)
    pub fn replacement_key(&self) -> ReplacementKey {
        if self.is_addressable() {
            if let Some(d_tag) = self.d_tag() {
                ReplacementKey::Addressable {
                    pubkey: self.pubkey,
                    kind: self.kind,
                    d_tag: d_tag.to_string(),
                }
            } else {
                // Fallback to regular replaceable if no d-tag
                ReplacementKey::Replaceable {
                    pubkey: self.pubkey,
                    kind: self.kind,
                }
            }
        } else if self.is_replaceable() {
            ReplacementKey::Replaceable {
                pubkey: self.pubkey,
                kind: self.kind,
            }
        } else {
            ReplacementKey::None
        }
    }
}

/// Key used to identify replaceable events
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReplacementKey {
    /// No replacement key (regular event)
    None,
    /// Replaceable event: identified by (pubkey, kind)
    Replaceable { pubkey: [u8; 32], kind: u32 },
    /// Addressable event: identified by (pubkey, kind, d-tag)
    Addressable {
        pubkey: [u8; 32],
        kind: u32,
        d_tag: String,
    },
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

/// Test-only event structure for serialization
#[cfg(test)]
#[derive(Serialize)]
struct TestEvent {
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

/// Builder for constructing test events with proper ID computation and signing
#[cfg(test)]
pub struct EventBuilder {
    pubkey: [u8; 32],
    created_at: u64,
    kind: u32,
    tags: Vec<Tag>,
    content: String,
}

#[cfg(test)]
impl EventBuilder {
    pub fn new() -> Self {
        Self {
            pubkey: [0; 32],
            created_at: 0,
            kind: 0,
            tags: Vec::new(),
            content: String::new(),
        }
    }

    pub fn pubkey(mut self, pubkey: [u8; 32]) -> Self {
        self.pubkey = pubkey;
        self
    }

    pub fn kind(mut self, kind: u32) -> Self {
        self.kind = kind;
        self
    }

    pub fn created_at(mut self, created_at: u64) -> Self {
        self.created_at = created_at;
        self
    }

    pub fn content(mut self, content: &str) -> Self {
        self.content = content.to_string();
        self
    }

    pub fn tag(mut self, name: &str, value: &str) -> Self {
        self.tags.push(Tag {
            name: name.to_string(),
            values: vec![value.to_string()],
        });
        self
    }

    pub fn build(self) -> Event {
        use sha2::{Digest, Sha256};

        let tags: Vec<Vec<String>> = self
            .tags
            .iter()
            .map(|t| {
                let mut arr = vec![t.name.clone()];
                arr.extend(t.values.iter().cloned());
                arr
            })
            .collect();

        // Compute ID by serializing the content array
        let serialised = serde_json::to_vec(&serde_json::json!([
            0,
            hex::encode(self.pubkey),
            self.created_at,
            self.kind,
            tags,
            self.content,
        ]))
        .unwrap();
        let id = Sha256::digest(&serialised);
        let mut id_arr = [0u8; 32];
        id_arr.copy_from_slice(&id);

        // Use dummy signature for tests (we skip verification with from_json_unchecked)
        let sig = [0u8; 64];

        // Serialize the full event
        let test_event = TestEvent {
            id: hex::encode(id_arr),
            pubkey: hex::encode(self.pubkey),
            created_at: self.created_at,
            kind: self.kind,
            tags,
            content: self.content,
            sig: hex::encode(sig),
        };

        let json = serde_json::to_string(&test_event).unwrap();
        Event::from_json_unchecked(json.as_bytes()).unwrap()
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

    #[test]
    fn test_is_ephemeral() {
        let ephemeral = EventBuilder::new()
            .pubkey([0; 32])
            .kind(20001)
            .created_at(1234567890)
            .content("ephemeral")
            .build();
        assert!(ephemeral.is_ephemeral());

        let not_ephemeral = EventBuilder::new()
            .pubkey([0; 32])
            .kind(19999)
            .created_at(1234567890)
            .content("not ephemeral")
            .build();
        assert!(!not_ephemeral.is_ephemeral());

        let not_ephemeral2 = EventBuilder::new()
            .pubkey([0; 32])
            .kind(30000)
            .created_at(1234567890)
            .content("not ephemeral")
            .build();
        assert!(!not_ephemeral2.is_ephemeral());
    }

    #[test]
    fn test_is_replaceable() {
        let replaceable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(10000)
            .created_at(1234567890)
            .content("replaceable")
            .build();
        assert!(replaceable.is_replaceable());

        let replaceable2 = EventBuilder::new()
            .pubkey([0; 32])
            .kind(19999)
            .created_at(1234567890)
            .content("replaceable")
            .build();
        assert!(replaceable2.is_replaceable());

        let addressable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(30000)
            .created_at(1234567890)
            .content("addressable")
            .tag("d", "profile")
            .build();
        assert!(addressable.is_replaceable());

        let not_replaceable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(0)
            .created_at(1234567890)
            .content("not replaceable")
            .build();
        assert!(!not_replaceable.is_replaceable());
    }

    #[test]
    fn test_is_addressable() {
        let addressable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(30000)
            .created_at(1234567890)
            .content("addressable")
            .tag("d", "profile")
            .build();
        assert!(addressable.is_addressable());

        let not_addressable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(29999)
            .created_at(1234567890)
            .content("not addressable")
            .build();
        assert!(!not_addressable.is_addressable());

        let not_addressable2 = EventBuilder::new()
            .pubkey([0; 32])
            .kind(40000)
            .created_at(1234567890)
            .content("not addressable")
            .build();
        assert!(!not_addressable2.is_addressable());
    }

    #[test]
    fn test_d_tag() {
        let addressable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(30000)
            .created_at(1234567890)
            .content("addressable")
            .tag("d", "profile")
            .build();
        assert_eq!(addressable.d_tag(), Some("profile"));

        let no_d_tag = EventBuilder::new()
            .pubkey([0; 32])
            .kind(30000)
            .created_at(1234567890)
            .content("no d-tag")
            .build();
        assert_eq!(no_d_tag.d_tag(), None);

        let not_addressable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(1)
            .created_at(1234567890)
            .content("not addressable")
            .tag("d", "ignored")
            .build();
        assert_eq!(not_addressable.d_tag(), None);
    }

    #[test]
    fn test_replacement_key() {
        let regular = EventBuilder::new()
            .pubkey([0; 32])
            .kind(1)
            .created_at(1234567890)
            .content("regular")
            .build();
        assert!(matches!(regular.replacement_key(), ReplacementKey::None));

        let replaceable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(10000)
            .created_at(1234567890)
            .content("replaceable")
            .build();
        assert!(matches!(
            replaceable.replacement_key(),
            ReplacementKey::Replaceable { .. }
        ));

        let addressable = EventBuilder::new()
            .pubkey([0; 32])
            .kind(30000)
            .created_at(1234567890)
            .content("addressable")
            .tag("d", "profile")
            .build();
        assert!(matches!(
            addressable.replacement_key(),
            ReplacementKey::Addressable { .. }
        ));
    }
}
