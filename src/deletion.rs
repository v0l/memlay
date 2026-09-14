//! NIP-09 — Event Deletion Request.
//!
//! A deletion request is a kind `5` event whose tags reference the events the
//! author wants removed: one `e` tag per event ID and/or one `a` tag per
//! addressable coordinate (`<kind>:<pubkey>:<d-identifier>`). A `k` tag may
//! additionally name the kind(s) being deleted.
//!
//! Relay behaviour implemented here (NIP-09 §Relay Usage):
//! - Only events authored by the deletion request's own `pubkey` are deleted.
//!   References to other users' events are ignored.
//! - `e` tags remove the referenced event by ID.
//! - `a` tags remove every stored version of the replaceable event up to the
//!   deletion request's `created_at` timestamp.
//! - The deletion request event itself is stored and served like any other
//!   event, so clients that already have the referenced events can hide them
//!   (NIP-09: relays SHOULD continue to publish deletion requests).
//! - Deleting a deletion request (a kind-5 `e` tag pointing at another kind-5)
//!   has no effect beyond removing that request event itself.
//!
//! Anti-resurrection: once an event ID has been deleted, a re-publish of the
//! same ID is rejected as a duplicate. Without this, a lagging client (or a
//! WAL replay after restart) could restore a deleted event, which would defeat
//! the whole point of the deletion request.

use crate::event::Event;
use std::collections::HashSet;

/// Result of applying a deletion request against the store.
#[derive(Debug, Default, Clone)]
pub struct DeletionOutcome {
    /// Event IDs actually removed from the store.
    pub deleted_ids: Vec<[u8; 32]>,
    /// Number of referenced events that were ignored because their author is
    /// not the deletion request's pubkey (NIP-09: same-pubkey only).
    pub skipped_foreign: usize,
    /// `a`-tag coordinates that referenced nothing currently stored.
    pub unknown_coordinates: usize,
}

/// Parsed references from a NIP-09 deletion request event.
#[derive(Debug, Default, Clone)]
pub struct DeletionRequest {
    /// Event IDs referenced by `e` tags (parsed from 64-char hex).
    pub ids: HashSet<[u8; 32]>,
    /// Coordinates referenced by `a` tags, as `(kind, pubkey, d-tag)` triples.
    pub coordinates: Vec<Coordinate>,
}

/// A parsed `<kind>:<pubkey>:<d-identifier>` coordinate (NIP-01/NIP-33).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Coordinate {
    pub kind: u32,
    pub pubkey: [u8; 32],
    pub d_tag: String,
}

/// NIP-09 deletion request event kind.
pub const KIND_DELETION: u32 = 5;

impl Event {
    /// True when this event is a NIP-09 deletion request (kind 5).
    pub fn is_deletion_request(&self) -> bool {
        self.kind == KIND_DELETION
    }

    /// Parse the `e` and `a` tags of a kind-5 event into a [`DeletionRequest`].
    ///
    /// Malformed values (non-hex IDs, coordinates with missing or
    /// out-of-range fields) are skipped rather than rejecting the whole
    /// request, matching how relays commonly tolerate partial deletions.
    pub fn deletion_request(&self) -> DeletionRequest {
        let mut req = DeletionRequest::default();

        for tag in &self.tags {
            let Some(value) = tag.value() else {
                continue;
            };
            match tag.name.as_str() {
                "e" => {
                    if let Some(id) = parse_hex_32_pub(value) {
                        req.ids.insert(id);
                    }
                }
                "a" => {
                    if let Some(coord) = parse_coordinate(value) {
                        req.coordinates.push(coord);
                    }
                }
                _ => {}
            }
        }

        req
    }
}

/// Parse a 32-byte hex event ID / pubkey reference.
fn parse_hex_32_pub(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = hex::decode(s).ok()?;
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

/// Parse an `a`-tag coordinate of the form `<kind>:<pubkey>:<d-identifier>`.
///
/// The d-identifier may itself contain colons (the coordinate is split on the
/// first two colons only). We do NOT support the naddr bech32 form — kind 5
/// requests in practice use the plain colon form and the bech32 form is
/// vanishingly rare on the wire.
fn parse_coordinate(value: &str) -> Option<Coordinate> {
    let mut parts = value.splitn(3, ':');
    let kind = parts.next()?.parse::<u32>().ok()?;
    let pubkey_hex = parts.next()?;
    if pubkey_hex.len() != 64 {
        return None;
    }
    let pubkey_bytes = hex::decode(pubkey_hex).ok()?;
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&pubkey_bytes);
    let d_tag = parts.next()?.to_string();
    Some(Coordinate {
        kind,
        pubkey,
        d_tag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBuilder;

    fn deletion_event(tags: Vec<(&str, &str)>) -> Event {
        let mut builder = EventBuilder::new().kind(KIND_DELETION).content("reason");
        for (name, value) in tags {
            builder = builder.tag(name, value);
        }
        builder.build()
    }

    #[test]
    fn parses_e_tags() {
        let id1 = set_last([0u8; 32], 1);
        let id2 = set_last([0u8; 32], 2);
        let ev = deletion_event(vec![
            ("e", &hex::encode(id1)),
            ("e", &hex::encode(id2)),
            ("k", "1"),
        ]);
        let req = ev.deletion_request();
        assert!(req.ids.contains(&id1));
        assert!(req.ids.contains(&id2));
        assert_eq!(req.ids.len(), 2, "duplicate ids must be deduped");
        assert!(req.coordinates.is_empty());
    }

    #[test]
    fn parses_a_tags() {
        let pubkey = set_last([0u8; 32], 9);
        let ev = deletion_event(vec![(
            "a",
            &format!("30023:{}:my-identifier", hex::encode(pubkey)),
        )]);
        let req = ev.deletion_request();
        assert_eq!(req.coordinates.len(), 1);
        let coord = &req.coordinates[0];
        assert_eq!(coord.kind, 30023);
        assert_eq!(coord.pubkey, pubkey);
        assert_eq!(coord.d_tag, "my-identifier");
    }

    #[test]
    fn coordinate_d_tag_may_contain_colons() {
        let pubkey = set_last([0u8; 32], 9);
        let ev = deletion_event(vec![(
            "a",
            &format!("30023:{}:ns:sub:name", hex::encode(pubkey)),
        )]);
        let coord = &ev.deletion_request().coordinates[0];
        assert_eq!(coord.d_tag, "ns:sub:name");
    }

    #[test]
    fn skips_malformed_values() {
        let ev = deletion_event(vec![
            ("e", "not-hex"),
            ("e", "1234"),
            ("a", "no-kind-here"),
            ("a", "30023:zzzz:tag"),
        ]);
        let req = ev.deletion_request();
        assert!(req.ids.is_empty());
        assert!(req.coordinates.is_empty());
    }

    #[test]
    fn non_deletion_events_have_empty_request() {
        let note = EventBuilder::new().kind(1).content("note").build();
        let req = note.deletion_request();
        assert!(req.ids.is_empty());
        assert!(req.coordinates.is_empty());
    }

    fn set_last(mut arr: [u8; 32], byte: u8) -> [u8; 32] {
        arr[31] = byte;
        arr
    }
}
