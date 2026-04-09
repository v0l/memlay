use crate::event::{Event, ReplacementKey};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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
    by_id: DashMap<[u8; 32], Arc<Event>>,

    // Pubkey index: all events from a pubkey (64-bit hash of pubkey)
    by_pubkey: DashMap<TagIdHash, RwLock<BTreeSet<EventRef>>>,
    by_kind: DashMap<u32, RwLock<BTreeSet<EventRef>>>,

    // Dedicated fast indexes for the two most-common tag types (64-bit hash)
    by_tag_e: DashMap<TagIdHash, RwLock<BTreeSet<EventRef>>>,
    by_tag_p: DashMap<TagIdHash, RwLock<BTreeSet<EventRef>>>,

    // Generic index for every other single-letter tag (NIP-01 §tags)
    // Outer key: tag letter ('t', 'a', 'd', …)
    // Inner key: raw tag value string
    by_tag_other: DashMap<TagLetter, DashMap<String, RwLock<BTreeSet<EventRef>>>>,

    // Oldest-first index for memory-based eviction
    by_oldest: RwLock<BTreeSet<EventRef>>,

    // Replaceable events index: maps (pubkey, kind) or (pubkey, kind, d-tag) to latest event id
    by_replaceable: DashMap<ReplacementKey, [u8; 32]>,

    // Track actual memory usage (bytes of raw event JSON)
    memory_bytes: AtomicUsize,
    
    // Track event count for lock-free stats
    event_count: AtomicUsize,
}

impl EventIndex {
    pub fn new() -> Self {
        Self {
            by_id: DashMap::new(),
            by_pubkey: DashMap::new(),
            by_kind: DashMap::new(),
            by_tag_e: DashMap::new(),
            by_tag_p: DashMap::new(),
            by_tag_other: DashMap::new(),
            by_oldest: RwLock::new(BTreeSet::new()),
            by_replaceable: DashMap::new(),
            memory_bytes: AtomicUsize::new(0),
            event_count: AtomicUsize::new(0),
        }
    }

    /// Insert an event into all indexes
    /// Returns the old event if this is a replaceable event that replaces an existing one
    pub fn insert(&self, event: Arc<Event>) -> Option<Arc<Event>> {
        let mut replaced = None;

        // Handle replaceable events: atomically get and remove old version
        if event.is_replaceable() {
            let key = event.replacement_key();
            if let ReplacementKey::Replaceable { .. } | ReplacementKey::Addressable { .. } = &key {
                // Atomically swap the old ID and get it
                if let Some(old_id) = self.by_replaceable.insert(key.clone(), event.id) {
                    // Remove old event from all indexes except by_replaceable (already updated)
                    if let Some(old_event) = self.internal_remove(&old_id, true) {
                        replaced = Some(old_event);
                    }
                }
            }
        }

        // Track memory usage (raw JSON bytes)
        self.memory_bytes.fetch_add(event.raw.len(), AtomicOrdering::Relaxed);
        self.event_count.fetch_add(1, AtomicOrdering::Relaxed);

        // Primary index
        self.by_id.insert(event.id, event.clone());

        // Pubkey index
        {
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            self.by_pubkey
                .entry(pubkey_hash)
                .or_insert_with(|| RwLock::new(BTreeSet::new()))
                .write()
                .insert(EventRef::new(event.clone()));
        }

        // Kind index
        {
            self.by_kind
                .entry(event.kind)
                .or_insert_with(|| RwLock::new(BTreeSet::new()))
                .write()
                .insert(EventRef::new(event.clone()));
        }

        // E-tag index
        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                self.by_tag_e
                    .entry(e_tag_hash)
                    .or_insert_with(|| RwLock::new(BTreeSet::new()))
                    .write()
                    .insert(EventRef::new(event.clone()));
            }
        }

        // P-tag index
        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                self.by_tag_p
                    .entry(p_tag_hash)
                    .or_insert_with(|| RwLock::new(BTreeSet::new()))
                    .write()
                    .insert(EventRef::new(event.clone()));
            }
        }

        // Generic tag index for all other single-letter tags
        {
            for tag in &event.tags {
                let mut chars = tag.name.chars();
                if let (Some(letter), None) = (chars.next(), chars.next())
                    && letter != 'e'
                    && letter != 'p'
                    && let Some(value) = tag.value()
                {
                    let inner_map = self.by_tag_other.entry(letter).or_insert_with(DashMap::new);
                    inner_map
                        .entry(value.to_string())
                        .or_insert_with(|| RwLock::new(BTreeSet::new()))
                        .write()
                        .insert(EventRef::new(event.clone()));
                }
            }
        }

        // Oldest-first index for eviction (newest first in EventRef ordering)
        {
            let mut by_oldest = self.by_oldest.write();
            by_oldest.insert(EventRef::new(event));
        }

        replaced
    }

/// Remove an event from all indexes
    /// If skip_replaceable is true, skip removing from the replaceable index (used during insert to avoid deadlock)
    pub fn remove(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        self.internal_remove(id, true)
    }

    fn internal_remove(&self, id: &[u8; 32], skip_replaceable: bool) -> Option<Arc<Event>> {
        let (_, event) = self.by_id.remove(id)?;

        let event_size = event.raw.len();
        self.memory_bytes.fetch_sub(event_size, AtomicOrdering::Relaxed);
        self.event_count.fetch_sub(1, AtomicOrdering::Relaxed);

        if !skip_replaceable && event.is_replaceable() {
            let key = event.replacement_key();
            if let ReplacementKey::Replaceable { .. } | ReplacementKey::Addressable { .. } = &key {
                self.by_replaceable.remove(&key);
            }
        }

        let er = EventRef::new(event.clone());

        {
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            let mut should_remove_key = false;
            if let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) {
                let mut set = set_lock.write();
                set.remove(&er);
                should_remove_key = set.is_empty();
            }
            if should_remove_key {
                self.by_pubkey.remove(&pubkey_hash);
            }
        }

        {
            let mut should_remove_key = false;
            if let Some(set_lock) = self.by_kind.get(&event.kind) {
                let mut set = set_lock.write();
                set.remove(&er);
                should_remove_key = set.is_empty();
            }
            if should_remove_key {
                self.by_kind.remove(&event.kind);
            }
        }

        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                let mut should_remove_key = false;
                if let Some(set_lock) = self.by_tag_e.get(&e_tag_hash) {
                    let mut set = set_lock.write();
                    set.remove(&er);
                    should_remove_key = set.is_empty();
                }
                if should_remove_key {
                    self.by_tag_e.remove(&e_tag_hash);
                }
            }
        }

        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                let mut should_remove_key = false;
                if let Some(set_lock) = self.by_tag_p.get(&p_tag_hash) {
                    let mut set = set_lock.write();
                    set.remove(&er);
                    should_remove_key = set.is_empty();
                }
                if should_remove_key {
                    self.by_tag_p.remove(&p_tag_hash);
                }
            }
        }

        {
            for tag in &event.tags {
                let mut chars = tag.name.chars();
                if let (Some(letter), None) = (chars.next(), chars.next())
                    && letter != 'e'
                    && letter != 'p'
                    && let Some(value) = tag.value()
                {
                    let mut should_remove_value = false;
                    let mut should_remove_letter = false;
                    if let Some(inner_map) = self.by_tag_other.get(&letter) {
                        if let Some(set_lock) = inner_map.get(value) {
                            let mut set = set_lock.write();
                            set.remove(&er);
                            should_remove_value = set.is_empty();
                        }
                        if should_remove_value {
                            inner_map.remove(value);
                        }
                        should_remove_letter = inner_map.is_empty();
                    }
                    if should_remove_letter {
                        self.by_tag_other.remove(&letter);
                    }
                }
            }
        }

        {
            let mut by_oldest = self.by_oldest.write();
            by_oldest.remove(&er);
        }

        Some(event)
    }

    /// Get the oldest events for eviction (returns oldest first)
    pub fn get_oldest(&self, count: usize) -> Vec<EventRef> {
        // BTreeSet is sorted newest-first (EventRef ordering), so iterate from end
        let by_oldest = self.by_oldest.read();
        let oldest: Vec<EventRef> = by_oldest
            .iter()
            .rev()
            .take(count)
            .cloned()
            .collect();
        
        oldest
    }

    /// Get current memory usage in bytes (raw JSON only)
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes.load(AtomicOrdering::Relaxed)
    }
    
    /// Get event count (lock-free)
    pub fn event_count(&self) -> usize {
        self.event_count.load(AtomicOrdering::Relaxed)
    }
}

impl EventIndex {
    /// Get an event by ID
    pub fn get(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        self.by_id.get(id).map(|r| Arc::clone(r.value()))
    }

    /// Number of events in the index
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Get all events for persistence
    pub fn iter_all(&self) -> Vec<Arc<Event>> {
        self.by_id.iter().map(|r| Arc::clone(r.value())).collect()
    }

    /// Get all live event IDs
    pub fn all_ids(&self) -> std::collections::HashSet<[u8; 32]> {
        self.by_id.iter().map(|r| *r.key()).collect()
    }

    /// Query by pubkey, returns events sorted by created_at DESC
    pub fn query_by_pubkey(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let pubkey_hash = TagIdHash::from_id(pubkey);
        
        // Fast path: check if entry exists without holding lock
        let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) else {
            return Vec::new();
        };
        
        // Clone Arcs quickly while holding read lock, then return
        let events: Vec<Arc<Event>> = {
            let set = set_lock.read();
            set.iter()
                .take(limit)
                .map(|r| Arc::clone(&r.event))
                .collect()
        };
        
        events
    }

    /// Query by kind, returns events sorted by created_at DESC
    pub fn query_by_kind(&self, kind: u32, limit: usize) -> Vec<Arc<Event>> {
        let Some(set_lock) = self.by_kind.get(&kind) else {
            return Vec::new();
        };
        
        let events: Vec<Arc<Event>> = {
            let set = set_lock.read();
            set.iter()
                .take(limit)
                .map(|r| Arc::clone(&r.event))
                .collect()
        };
        
        events
    }

    /// Query by pubkey with since filter, returns events sorted by created_at DESC
    pub fn query_by_pubkey_since(
        &self,
        pubkey: &[u8; 32],
        since: u64,
        limit: usize,
    ) -> Vec<Arc<Event>> {
        let pubkey_hash = TagIdHash::from_id(pubkey);
        
        let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) else {
            return Vec::new();
        };
        
        let events: Vec<Arc<Event>> = {
            let set = set_lock.read();
            set.iter()
                .filter(|r| r.created_at >= since)
                .take(limit)
                .map(|r| Arc::clone(&r.event))
                .collect()
        };
        
        events
    }

    /// Query by e-tag (events referencing this event ID)
    pub fn query_by_e_tag(&self, event_id: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let e_tag_hash = TagIdHash::from_id(event_id);
        
        let Some(set_lock) = self.by_tag_e.get(&e_tag_hash) else {
            return Vec::new();
        };
        
        let events: Vec<Arc<Event>> = {
            let set = set_lock.read();
            set.iter()
                .take(limit)
                .map(|r| Arc::clone(&r.event))
                .collect()
        };
        
        events
    }

    /// Query by p-tag (events mentioning this pubkey)
    pub fn query_by_p_tag(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let p_tag_hash = TagIdHash::from_id(pubkey);
        
        let Some(set_lock) = self.by_tag_p.get(&p_tag_hash) else {
            return Vec::new();
        };
        
        let events: Vec<Arc<Event>> = {
            let set = set_lock.read();
            set.iter()
                .take(limit)
                .map(|r| Arc::clone(&r.event))
                .collect()
        };
        
        events
    }

    /// Query by any other single-letter tag value (events with `["x", "<value>"]`).
    pub fn query_by_tag(&self, letter: char, value: &str, limit: usize) -> Vec<Arc<Event>> {
        let Some(inner_map) = self.by_tag_other.get(&letter) else {
            return Vec::new();
        };
        
        let Some(set_lock) = inner_map.get(value) else {
            return Vec::new();
        };
        
        let events: Vec<Arc<Event>> = {
            let set = set_lock.read();
            set.iter()
                .take(limit)
                .map(|r| Arc::clone(&r.event))
                .collect()
        };
        
        events
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

    fn make_event_with_content(id: u8, pubkey: u8, kind: u32, created_at: u64, content: &str) -> Arc<Event> {
        let json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"{}","sig":"{:0>128}"}}"#,
            format!("{:x}", id),
            format!("{:x}", pubkey),
            created_at,
            kind,
            content,
            "0"
        );
        Arc::new(Event::from_json_unchecked(json.as_bytes()).unwrap())
    }

    #[test]
    fn test_memory_tracking_on_insert() {
        let index = EventIndex::new();
        assert_eq!(index.memory_bytes(), 0);

        let event = make_event(1, 1, 1, 1000);
        let expected_size = event.raw.len();

        index.insert(event);
        
        // Memory should equal the raw JSON size
        assert_eq!(index.memory_bytes(), expected_size);
    }

    #[test]
    fn test_memory_tracking_on_remove() {
        let index = EventIndex::new();
        
        let event = make_event(1, 1, 1, 1000);
        let event_size = event.raw.len();
        
        index.insert(event.clone());
        let mem_after_insert = index.memory_bytes();
        assert_eq!(mem_after_insert, event_size);

        index.remove(&event.id);
        let mem_after_remove = index.memory_bytes();
        
        // Memory should be back to 0
        assert_eq!(mem_after_remove, 0);
    }

    #[test]
    fn test_memory_tracking_multiple_events() {
        let index = EventIndex::new();
        
        let mut total_size = 0;
        for i in 0..10 {
            let event = make_event(i, 1, 1, 1000 + i as u64);
            total_size += event.raw.len();
            index.insert(event);
        }
        
        let mem_used = index.memory_bytes();
        assert_eq!(mem_used, total_size);
        assert_eq!(index.len(), 10);

        // Remove half
        for i in 0..5 {
            let event = make_event(i, 1, 1, 1000 + i as u64);
            let removed_size = event.raw.len();
            total_size -= removed_size;
            index.remove(&event.id);
        }
        
        let mem_after_remove = index.memory_bytes();
        assert_eq!(mem_after_remove, total_size);
        assert_eq!(index.len(), 5);
    }

    #[test]
    fn test_memory_tracking_with_large_content() {
        let index = EventIndex::new();
        
        // Create events with different content sizes
        let small_event = make_event_with_content(1, 1, 1, 1000, "small");
        let large_content = "x".repeat(1000);
        let large_event = make_event_with_content(2, 1, 1, 1001, &large_content);
        
        let small_size = small_event.raw.len();
        let large_size = large_event.raw.len();
        
        index.insert(small_event.clone());
        let mem_after_small = index.memory_bytes();
        assert_eq!(mem_after_small, small_size);

        index.insert(large_event.clone());
        let mem_after_large = index.memory_bytes();
        assert_eq!(mem_after_large, small_size + large_size);

        // Remove small event
        index.remove(&small_event.id);
        let mem_after_remove = index.memory_bytes();
        assert_eq!(mem_after_remove, large_size);

        // Remove large event
        index.remove(&large_event.id);
        let mem_final = index.memory_bytes();
        assert_eq!(mem_final, 0);
    }

    #[test]
    fn test_memory_tracking_with_replaceable_events() {
        let index = EventIndex::new();
        
        // Create two replaceable events (kind 10000) with same pubkey
        let event1 = make_event(1, 1, 10000, 1000);
        let size1 = event1.raw.len();
        
        index.insert(event1.clone());
        let mem_after_first = index.memory_bytes();
        assert_eq!(mem_after_first, size1);

        let event2 = make_event(2, 1, 10000, 2000);
        let size2 = event2.raw.len();
        
        // Insert second event (should replace first)
        index.insert(event2.clone());
        let mem_after_replace = index.memory_bytes();
        
        // Memory should be roughly the same (one removed, one added)
        assert_eq!(mem_after_replace, size2);
        assert_eq!(index.len(), 1);
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
