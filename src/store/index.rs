use crate::event::Event;
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// Tag letter used as key in `by_tag_other`.
type TagLetter = char;

/// 64-bit hash for tag index keys.
/// Computes a hash from the full 32-byte ID using XOR folding.
/// Collision probability: ~0.000003% for 1M events (essentially zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TagIdHash(u64);

impl TagIdHash {
    pub fn from_id(id: &[u8; 32]) -> Self {
        // Fold all 32 bytes into 8 bytes using XOR (uniform distribution)
        let mut hash = [0u8; 8];
        for (i, &byte) in id.iter().enumerate() {
            hash[i % 8] ^= byte;
        }
        Self(u64::from_be_bytes(hash))
    }
}

/// Reference to an event, used in sorted indexes.
/// Stores the Arc<Event> pointer for zero-copy event access.
#[derive(Clone)]
pub struct EventRef {
    pub created_at: u64,
    pub id: [u8; 32],
    pub event: Arc<Event>,
}

impl EventRef {
    pub fn new(event: Arc<Event>) -> Self {
        Self {
            created_at: event.created_at,
            id: event.id,
            event,
        }
    }
}

// Only compare by created_at and id
impl PartialEq for EventRef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for EventRef {}

impl Ord for EventRef {
    fn cmp(&self, other: &Self) -> Ordering {
        // Descending by time (newest first)
        other
            .created_at
            .cmp(&self.created_at)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for EventRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// For BTreeSet removal - we need to find by id regardless of created_at
impl std::hash::Hash for EventRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// In-memory index for fast event lookups.
/// Each index has its own RwLock for fine-grained concurrency.
pub struct EventIndex {
    // Primary index: O(1) lookup by id
    by_id: RwLock<HashMap<[u8; 32], Arc<Event>>>,

    // Pubkey index: all events from a pubkey (64-bit hash of pubkey)
    by_pubkey: RwLock<HashMap<TagIdHash, BTreeSet<EventRef>>>,
    by_kind: RwLock<HashMap<u32, BTreeSet<EventRef>>>,

    // Dedicated fast indexes for the two most-common tag types (64-bit hash)
    by_tag_e: RwLock<HashMap<TagIdHash, BTreeSet<EventRef>>>,
    by_tag_p: RwLock<HashMap<TagIdHash, BTreeSet<EventRef>>>,

    // Generic index for every other single-letter tag (NIP-01 §tags)
    // Outer key: tag letter ('t', 'a', 'd', …)
    // Inner key: raw tag value string
    by_tag_other: RwLock<HashMap<TagLetter, HashMap<String, BTreeSet<EventRef>>>>,
    
    // Oldest-first index for memory-based eviction
    by_oldest: RwLock<BTreeSet<EventRef>>,
}

impl EventIndex {
    pub fn new() -> Self {
        Self {
            by_id: RwLock::new(HashMap::new()),
            by_pubkey: RwLock::new(HashMap::new()),
            by_kind: RwLock::new(HashMap::new()),
            by_tag_e: RwLock::new(HashMap::new()),
            by_tag_p: RwLock::new(HashMap::new()),
            by_tag_other: RwLock::new(HashMap::new()),
            by_oldest: RwLock::new(BTreeSet::new()),
        }
    }

    /// Insert an event into all indexes
    pub fn insert(&self, event: Arc<Event>) {
        // Primary index
        self.by_id.write().insert(event.id, event.clone());

        // Pubkey index
        {
            let mut by_pubkey = self.by_pubkey.write();
            let event_ref = EventRef::new(event.clone());
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            by_pubkey.entry(pubkey_hash).or_default().insert(event_ref);
        }

        // Kind index
        {
            let mut by_kind = self.by_kind.write();
            let event_ref = EventRef::new(event.clone());
            by_kind.entry(event.kind).or_default().insert(event_ref);
        }

        // E-tag index
        {
            let mut by_tag_e = self.by_tag_e.write();
            for e_tag in event.e_tags() {
                let event_ref = EventRef::new(event.clone());
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                by_tag_e.entry(e_tag_hash).or_default().insert(event_ref);
            }
        }

        // P-tag index
        {
            let mut by_tag_p = self.by_tag_p.write();
            for p_tag in event.p_tags() {
                let event_ref = EventRef::new(event.clone());
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                by_tag_p.entry(p_tag_hash).or_default().insert(event_ref);
            }
        }

        // Generic tag index for all other single-letter tags
        {
            let mut by_tag_other = self.by_tag_other.write();
            for tag in &event.tags {
                let mut chars = tag.name.chars();
                if let (Some(letter), None) = (chars.next(), chars.next())
                    && letter != 'e'
                    && letter != 'p'
                    && let Some(value) = tag.value()
                {
                    let event_ref = EventRef::new(event.clone());
                    by_tag_other
                        .entry(letter)
                        .or_default()
                        .entry(value.to_string())
                        .or_default()
                        .insert(event_ref);
                }
            }
        }
        
        // Oldest-first index for eviction (newest first in EventRef ordering)
        {
            let mut by_oldest = self.by_oldest.write();
            let event_ref = EventRef::new(event);
            by_oldest.insert(event_ref);
        }
    }

    /// Remove an event from all indexes
    pub fn remove(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        let event = self.by_id.write().remove(id)?;

        // Create a dummy EventRef for removal (only id matters for equality)
        let dummy_ref = EventRef {
            created_at: event.created_at,
            id: event.id,
            event: Arc::clone(&event),
        };

        // Delete from pubkey index
        {
            let mut by_pubkey = self.by_pubkey.write();
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            if let Some(set) = by_pubkey.get_mut(&pubkey_hash) {
                set.remove(&dummy_ref);
                if set.is_empty() {
                    by_pubkey.remove(&pubkey_hash);
                }
            }
        }

        // Delete from kind index
        {
            let mut by_kind = self.by_kind.write();
            if let Some(set) = by_kind.get_mut(&event.kind) {
                set.remove(&dummy_ref);
                if set.is_empty() {
                    by_kind.remove(&event.kind);
                }
            }
        }

        // Remove from e-tag index
        {
            let mut by_tag_e = self.by_tag_e.write();
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                if let Some(set) = by_tag_e.get_mut(&e_tag_hash) {
                    set.remove(&dummy_ref);
                    if set.is_empty() {
                        by_tag_e.remove(&e_tag_hash);
                    }
                }
            }
        }

        // Remove from p-tag index
        {
            let mut by_tag_p = self.by_tag_p.write();
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                if let Some(set) = by_tag_p.get_mut(&p_tag_hash) {
                    set.remove(&dummy_ref);
                    if set.is_empty() {
                        by_tag_p.remove(&p_tag_hash);
                    }
                }
            }
        }

        // Remove from generic tag index
        {
            let mut by_tag_other = self.by_tag_other.write();
            for tag in &event.tags {
                let mut chars = tag.name.chars();
                if let (Some(letter), None) = (chars.next(), chars.next())
                    && letter != 'e'
                    && letter != 'p'
                    && let Some(value) = tag.value()
                    && let Some(inner) = by_tag_other.get_mut(&letter)
                {
                    if let Some(set) = inner.get_mut(value) {
                        set.remove(&dummy_ref);
                        if set.is_empty() {
                            inner.remove(value);
                        }
                    }
                    if inner.is_empty() {
                        by_tag_other.remove(&letter);
                    }
                }
            }
        }
        
        // Remove from oldest index
        {
            let mut by_oldest = self.by_oldest.write();
            by_oldest.remove(&dummy_ref);
        }
        
        Some(event)
    }
    
    /// Get the oldest events for eviction (returns oldest first)
    pub fn get_oldest(&self, count: usize) -> Vec<EventRef> {
        // BTreeSet is sorted newest-first (EventRef ordering), so iterate from end
        self.by_oldest.read()
            .iter()
            .rev()
            .take(count)
            .cloned()
            .collect()
    }
}

impl EventIndex {
    /// Get an event by ID
    pub fn get(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        self.by_id.read().get(id).cloned()
    }

    /// Number of events in the index
    pub fn len(&self) -> usize {
        self.by_id.read().len()
    }

    /// Query by pubkey, returns events sorted by created_at DESC
    pub fn query_by_pubkey(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let pubkey_hash = TagIdHash::from_id(pubkey);
        self.by_pubkey
            .read()
            .get(&pubkey_hash)
            .map(|set| {
                set.iter()
                    .take(limit)
                    .map(|r| Arc::clone(&r.event))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Query by kind, returns events sorted by created_at DESC
    pub fn query_by_kind(&self, kind: u32, limit: usize) -> Vec<Arc<Event>> {
        self.by_kind
            .read()
            .get(&kind)
            .map(|set| {
                set.iter()
                    .take(limit)
                    .map(|r| Arc::clone(&r.event))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Query by pubkey with since filter, returns events sorted by created_at DESC
    pub fn query_by_pubkey_since(
        &self,
        pubkey: &[u8; 32],
        since: u64,
        limit: usize,
    ) -> Vec<Arc<Event>> {
        let pubkey_hash = TagIdHash::from_id(pubkey);
        self.by_pubkey
            .read()
            .get(&pubkey_hash)
            .map(|set| {
                set.iter()
                    .filter(|r| r.created_at >= since)
                    .take(limit)
                    .map(|r| Arc::clone(&r.event))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Query by e-tag (events referencing this event ID)
    pub fn query_by_e_tag(&self, event_id: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let e_tag_hash = TagIdHash::from_id(event_id);
        self.by_tag_e
            .read()
            .get(&e_tag_hash)
            .map(|set| {
                set.iter()
                    .take(limit)
                    .map(|r| Arc::clone(&r.event))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Query by p-tag (events mentioning this pubkey)
    pub fn query_by_p_tag(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let p_tag_hash = TagIdHash::from_id(pubkey);
        self.by_tag_p
            .read()
            .get(&p_tag_hash)
            .map(|set| {
                set.iter()
                    .take(limit)
                    .map(|r| Arc::clone(&r.event))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Query by any other single-letter tag value (events with `["x", "<value>"]`).
    pub fn query_by_tag(&self, letter: char, value: &str, limit: usize) -> Vec<Arc<Event>> {
        self.by_tag_other
            .read()
            .get(&letter)
            .and_then(|inner| inner.get(value))
            .map(|set| {
                set.iter()
                    .take(limit)
                    .map(|r| Arc::clone(&r.event))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Default for EventIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(id: u8, pubkey: u8, kind: u32, created_at: u64) -> Arc<Event> {
        let json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
            format!("{:x}", id),
            format!("{:x}", pubkey),
            created_at,
            kind,
            "0"
        );
        Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
    }

    #[test]
    fn test_insert_and_get() {
        let index = EventIndex::new();
        let event = make_event(1, 1, 1, 1000);
        let id = event.id;

        index.insert(event);
        assert_eq!(index.len(), 1);

        let retrieved = index.get(&id).unwrap();
        assert_eq!(retrieved.id, id);
    }

    #[test]
    fn test_remove() {
        let index = EventIndex::new();
        let event = make_event(1, 1, 1, 1000);
        let id = event.id;

        index.insert(event);
        assert_eq!(index.len(), 1);

        let removed = index.remove(&id).unwrap();
        assert_eq!(removed.id, id);
        assert_eq!(index.len(), 0);
        assert!(index.get(&id).is_none());
    }

    #[test]
    fn test_query_ordering() {
        let index = EventIndex::new();

        // Insert in random order
        index.insert(make_event(2, 1, 1, 2000));
        index.insert(make_event(1, 1, 1, 1000));
        index.insert(make_event(3, 1, 1, 3000));

        // Query should return in created_at DESC order
        let results = index.query_by_kind(1, 10);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].created_at, 3000);
        assert_eq!(results[1].created_at, 2000);
        assert_eq!(results[2].created_at, 1000);
    }
}
