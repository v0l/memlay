use bytes::Bytes;
use hex::FromHex;
use secp256k1::{XOnlyPublicKey, schnorr::Signature};
use serde::Deserialize;
#[cfg(test)]
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
enum TagValue {
    Span { start: u32, len: u32 },
    Owned(Box<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedTag {
    letter: char,
    value: TagValue,
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
    tags: Box<[IndexedTag]>,

    // Original JSON for zero-copy response
    pub raw: Bytes,
}

impl Event {
    /// Parse an event from JSON bytes, skipping id/signature verification.
    /// Only for internal tests, benchmarks, and WAL replay — do not use on untrusted input.
    /// WAL replay is safe because we're reading our own persisted data.
    #[cfg(any(test, feature = "unchecked", feature = "wal"))]
    pub fn from_json_unchecked(json: &[u8]) -> Result<Self, EventParseError> {
        Self::parse(json, false)
    }

    /// Parse an event from JSON bytes and verify its id and signature.
    pub fn from_json(json: &[u8]) -> Result<Self, EventParseError> {
        Self::parse(json, true)
    }

    fn parse(json: &[u8], verify: bool) -> Result<Self, EventParseError> {
        let raw_event: RawEvent = serde_json::from_slice(json)?;

        let id = parse_hex_32(raw_event.id.as_bytes()).ok_or(EventParseError::InvalidId)?;
        let pubkey =
            parse_hex_32(raw_event.pubkey.as_bytes()).ok_or(EventParseError::InvalidPubkey)?;
        let sig = parse_hex_64(&raw_event.sig).ok_or(EventParseError::InvalidSignature)?;

        if verify {
            raw_event.verify(&id, &pubkey, &sig)?;
        }

        Ok(Event {
            id,
            pubkey,
            created_at: raw_event.created_at,
            kind: raw_event.kind,
            tags: index_tags(json, &raw_event.tags),
            raw: Bytes::copy_from_slice(json),
        })
    }

    fn tag_bytes<'a>(&'a self, tag: &'a IndexedTag) -> &'a [u8] {
        match &tag.value {
            TagValue::Span { start, len } => {
                let start = *start as usize;
                &self.raw[start..start + *len as usize]
            }
            TagValue::Owned(value) => value.as_bytes(),
        }
    }

    /// First value of every single-letter tag, as `(letter, value)`.
    pub fn tags(&self) -> impl Iterator<Item = (char, &str)> + '_ {
        self.tags.iter().filter_map(|tag| {
            std::str::from_utf8(self.tag_bytes(tag))
                .ok()
                .map(|value| (tag.letter, value))
        })
    }

    /// First value of every single-letter tag other than `e` and `p`.
    pub fn generic_tags(&self) -> impl Iterator<Item = (char, &str)> + '_ {
        self.tags()
            .filter(|(letter, _)| *letter != 'e' && *letter != 'p')
    }

    /// True when a single-letter tag `letter` has a first value equal to one of `values`.
    pub fn has_tag_value(&self, letter: char, values: &[String]) -> bool {
        self.tags.iter().any(|tag| {
            tag.letter == letter && {
                let bytes = self.tag_bytes(tag);
                values.iter().any(|v| v.as_bytes() == bytes)
            }
        })
    }

    fn hex_tags(&self, letter: char) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.tags
            .iter()
            .filter(move |tag| tag.letter == letter)
            .filter_map(|tag| parse_hex_32(self.tag_bytes(tag)))
    }

    /// Get e-tags (referenced event IDs) as bytes
    pub fn e_tags(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.hex_tags('e')
    }

    /// Get p-tags (referenced pubkeys) as bytes
    pub fn p_tags(&self) -> impl Iterator<Item = [u8; 32]> + '_ {
        self.hex_tags('p')
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
        self.tags()
            .find_map(|(letter, value)| (letter == 'd').then_some(value))
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
            .field("tags", &self.tags().collect::<Vec<_>>())
            .field("raw_len", &self.raw.len())
            .finish()
    }
}

/// Raw JSON structure for parsing. Strings borrow from the input unless they
/// contain escapes, which is what lets tag values be stored as spans of `raw`.
#[derive(Deserialize)]
struct RawEvent<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    #[serde(borrow)]
    pubkey: Cow<'a, str>,
    created_at: u64,
    kind: u32,
    #[serde(borrow)]
    tags: Vec<Vec<JsonStr<'a>>>,
    #[serde(borrow)]
    content: Cow<'a, str>,
    #[serde(borrow)]
    sig: Cow<'a, str>,
}

impl RawEvent<'_> {
    fn verify(
        &self,
        id: &[u8; 32],
        pubkey: &[u8; 32],
        sig: &[u8; 64],
    ) -> Result<(), EventParseError> {
        let serialised = serde_json::to_vec(&(
            0u8,
            &self.pubkey,
            self.created_at,
            self.kind,
            &self.tags,
            &self.content,
        ))
        .map_err(EventParseError::Json)?;
        let computed: [u8; 32] = Sha256::digest(&serialised).into();
        if &computed != id {
            return Err(EventParseError::IdMismatch);
        }

        let xonly =
            XOnlyPublicKey::from_byte_array(*pubkey).map_err(|_| EventParseError::InvalidPubkey)?;
        let schnorr_sig = Signature::from_byte_array(*sig);
        secp256k1::global::SECP256K1
            .verify_schnorr(&schnorr_sig, id, &xonly)
            .map_err(|_| EventParseError::BadSignature)
    }
}

#[derive(Deserialize, serde::Serialize)]
struct JsonStr<'a>(#[serde(borrow)] Cow<'a, str>);

fn index_tags(json: &[u8], tags: &[Vec<JsonStr<'_>>]) -> Box<[IndexedTag]> {
    tags.iter()
        .filter_map(|tag| {
            let mut name = tag.first()?.0.chars();
            let letter = name.next()?;
            if name.next().is_some() {
                return None;
            }
            let value = &tag.get(1)?.0;
            let stored = match value {
                Cow::Borrowed(s) => span_in(json, s),
                Cow::Owned(_) => None,
            };
            Some(IndexedTag {
                letter,
                value: stored.unwrap_or_else(|| TagValue::Owned(value.as_ref().into())),
            })
        })
        .collect()
}

fn span_in(json: &[u8], s: &str) -> Option<TagValue> {
    let start = (s.as_ptr() as usize).checked_sub(json.as_ptr() as usize)?;
    if start.checked_add(s.len())? > json.len() {
        return None;
    }
    Some(TagValue::Span {
        start: u32::try_from(start).ok()?,
        len: u32::try_from(s.len()).ok()?,
    })
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
fn parse_hex_32(s: &[u8]) -> Option<[u8; 32]> {
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
    tags: Vec<Vec<String>>,
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
        self.tags.push(vec![name.to_string(), value.to_string()]);
        self
    }

    pub fn build(self) -> Event {
        use sha2::{Digest, Sha256};

        let tags = self.tags;

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
        assert_eq!(event.tags().count(), 2);

        let e_tags: Vec<_> = event.e_tags().collect();
        assert_eq!(e_tags.len(), 1);
        assert_eq!(e_tags[0][31], 3);

        let p_tags: Vec<_> = event.p_tags().collect();
        assert_eq!(p_tags.len(), 1);
        assert_eq!(p_tags[0][31], 4);
    }

    fn event_with_tags(tags: &str) -> Event {
        let json = format!(
            r#"{{ "id" : "{}", "pubkey":"{}", "created_at":1, "kind":1, "tags" : {tags} , "content":"x", "sig":"{}" }}"#,
            "0".repeat(64),
            "0".repeat(64),
            "0".repeat(128)
        );
        Event::from_json_unchecked(json.as_bytes()).unwrap()
    }

    #[test]
    fn tag_values_read_from_raw_with_whitespace() {
        let id = "ab".repeat(32);
        let event = event_with_tags(&format!(
            r#"[ [ "e" , "{id}" , "wss://r" ], ["t",  "nostr"] ]"#
        ));
        assert_eq!(
            event.tags().collect::<Vec<_>>(),
            vec![('e', id.as_str()), ('t', "nostr")]
        );
        assert_eq!(event.e_tags().next(), Some([0xab; 32]));
        assert!(event.has_tag_value('t', &["nostr".to_string()]));
        assert!(!event.has_tag_value('t', &["nost".to_string()]));
    }

    #[test]
    fn escaped_tag_values_are_unescaped() {
        let event = event_with_tags(r#"[["t","a\"b\u00e9"],["d","x\\y"]]"#);
        assert_eq!(
            event.tags().collect::<Vec<_>>(),
            vec![('t', "a\"bé"), ('d', "x\\y")]
        );
        assert!(event.has_tag_value('t', &["a\"bé".to_string()]));
        assert!(matches!(event.tags[0].value, TagValue::Owned(_)));
    }

    #[test]
    fn plain_tag_values_are_spans_into_raw() {
        let event = event_with_tags(r#"[["t","nostr"],["p","x"]]"#);
        assert!(
            event
                .tags
                .iter()
                .all(|t| matches!(t.value, TagValue::Span { .. }))
        );
        assert_eq!(std::mem::size_of::<IndexedTag>(), 24);
    }

    #[test]
    fn only_first_value_of_single_letter_tags_is_indexed() {
        let event =
            event_with_tags(r#"[["expiration","123"],["t"],["t","one","two"],[],["é","accent"]]"#);
        assert_eq!(
            event.tags().collect::<Vec<_>>(),
            vec![('t', "one"), ('é', "accent")]
        );
        assert!(!event.has_tag_value('t', &["two".to_string()]));
    }

    #[test]
    fn verified_parse_hashes_escaped_tags_and_content() {
        use secp256k1::{Keypair, Secp256k1, SecretKey};

        let secp = Secp256k1::new();
        let keypair =
            Keypair::from_secret_key(&secp, &SecretKey::from_byte_array([7; 32]).unwrap());
        let pubkey = hex::encode(keypair.x_only_public_key().0.serialize());
        let tags = serde_json::json!([["t", "q\"uote"], ["p", "cd".repeat(32)]]);
        let content = "line\nbreak \"quoted\" é";
        let id: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&serde_json::json!([0, pubkey, 5, 1, tags, content])).unwrap(),
        )
        .into();
        let sig = secp.sign_schnorr_no_aux_rand(&id, &keypair);
        let json = serde_json::json!({
            "id": hex::encode(id),
            "pubkey": pubkey,
            "created_at": 5,
            "kind": 1,
            "tags": tags,
            "content": content,
            "sig": hex::encode(sig.to_byte_array()),
        })
        .to_string();

        let event = Event::from_json(json.as_bytes()).unwrap();
        assert_eq!(event.tags().next(), Some(('t', "q\"uote")));
        assert_eq!(event.p_tags().next(), Some([0xcd; 32]));

        let tampered = json.replace("q\\\"uote", "q\\\"uotf");
        assert_ne!(tampered, json);
        assert!(matches!(
            Event::from_json(tampered.as_bytes()),
            Err(EventParseError::IdMismatch)
        ));
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
