use crate::event::{Event, ReplacementKey};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// Number of shards for the by_oldest eviction index.
/// Each shard has its own RwLock, so inserts/removes to different shards
/// never contend. Events are assigned to shards by hashing the event ID.
const OLDEST_SHARDS: usize = 64;

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

/// Result of inserting an event into the index.
#[derive(Debug)]
pub enum InsertOutcome {
    /// Event was successfully inserted
    Inserted {
        /// Old event that was replaced (for replaceable events)
        replaced: Option<Arc<Event>>,
    },
    /// Event was a duplicate (same ID already exists)
    Duplicate,
    /// Insert lost a replaceable event race (another thread won)
    LostRace,
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

    // Sharded oldest-first index for memory-based eviction.
    // Uses OLDEST_SHARDS independent locks so inserts/removes to different
    // shards never contend — eliminates the single-lock bottleneck that
    // caused lockup under high insert + eviction load.
    by_oldest: [RwLock<BTreeSet<EventRef>>; OLDEST_SHARDS],

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
            by_oldest: std::array::from_fn(|_| RwLock::new(BTreeSet::new())),
            by_replaceable: DashMap::new(),
            memory_bytes: AtomicUsize::new(0),
            event_count: AtomicUsize::new(0),
        }
    }

    /// Pick the by_oldest shard for a given event ID.
    /// Uses a simple hash of the first 8 bytes modulo shard count.
    #[inline]
    fn oldest_shard(&self, id: &[u8; 32]) -> usize {
        let mut hash: usize = 0;
        for i in 0..8 {
            hash = hash.wrapping_mul(31).wrapping_add(id[i] as usize);
        }
        hash % OLDEST_SHARDS
    }

    /// Insert an event into all indexes
    /// Returns InsertOutcome indicating whether the event was inserted, was a duplicate,
    /// or lost a replaceable event race.
    pub fn insert(&self, event: Arc<Event>) -> InsertOutcome {
        let mut replaced = None;

        // Handle replaceable events: atomically swap old ID in by_replaceable
        if event.is_replaceable() {
            let key = event.replacement_key();
            if let ReplacementKey::Replaceable { .. } | ReplacementKey::Addressable { .. } = &key {
                if let Some(old_id) = self.by_replaceable.insert(key.clone(), event.id) {
                    if let Some(old_event) = self.internal_remove(&old_id, true) {
                        replaced = Some(old_event);
                    }
                }
            }
        }

        // Primary index: atomic insert — detect duplicate IDs
        if self.by_id.insert(event.id, event.clone()).is_some() {
            return InsertOutcome::Duplicate;
        }

        // Early verification: check if we're still the winner for replaceable events.
        // Between by_replaceable.insert() and by_id.insert(), another thread could
        // have replaced our ID in by_replaceable and removed our event from by_id
        // via internal_remove. Our by_id.insert() would then succeed (since we were
        // just removed), creating an orphaned event.
        if event.is_replaceable() && !self.is_still_winner(&event) {
            self.by_id.remove(&event.id);
            return InsertOutcome::LostRace;
        }

        // Track memory usage (raw JSON bytes) — only after confirmed new insert
        self.memory_bytes
            .fetch_add(event.raw.len(), AtomicOrdering::Relaxed);
        self.event_count.fetch_add(1, AtomicOrdering::Relaxed);

        // Pubkey index
        {
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            let start = std::time::Instant::now();
            let set_lock = self
                .by_pubkey
                .entry(pubkey_hash)
                .or_insert_with(|| RwLock::new(BTreeSet::new()));
            let entry_duration = start.elapsed();
            tracing::trace!(event_id = %hex::encode(event.id), index = "pubkey", entry_duration_us = %entry_duration.as_micros(), "acquired pubkey index entry");

            let write_start = std::time::Instant::now();
            set_lock.write().insert(EventRef::new(event.clone()));
            let write_duration = write_start.elapsed();
            tracing::trace!(event_id = %hex::encode(event.id), index = "pubkey", write_duration_us = %write_duration.as_micros(), "pubkey index write completed");
        }

        // Kind index
        {
            let start = std::time::Instant::now();
            let set_lock = self
                .by_kind
                .entry(event.kind)
                .or_insert_with(|| RwLock::new(BTreeSet::new()));
            let entry_duration = start.elapsed();
            tracing::trace!(event_id = %hex::encode(event.id), index = "kind", kind = event.kind, entry_duration_us = %entry_duration.as_micros(), "acquired kind index entry");

            let write_start = std::time::Instant::now();
            set_lock.write().insert(EventRef::new(event.clone()));
            let write_duration = write_start.elapsed();
            tracing::trace!(event_id = %hex::encode(event.id), index = "kind", kind = event.kind, write_duration_us = %write_duration.as_micros(), "kind index write completed");
        }

        // E-tag index
        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                let start = std::time::Instant::now();
                let set_lock = self
                    .by_tag_e
                    .entry(e_tag_hash)
                    .or_insert_with(|| RwLock::new(BTreeSet::new()));
                let entry_duration = start.elapsed();
                tracing::trace!(event_id = %hex::encode(event.id), index = "e-tag", e_tag = %hex::encode(e_tag), entry_duration_us = %entry_duration.as_micros(), "acquired e-tag index entry");

                let write_start = std::time::Instant::now();
                set_lock.write().insert(EventRef::new(event.clone()));
                let write_duration = write_start.elapsed();
                tracing::trace!(event_id = %hex::encode(event.id), index = "e-tag", e_tag = %hex::encode(e_tag), write_duration_us = %write_duration.as_micros(), "e-tag index write completed");
            }
        }

        // P-tag index
        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                let start = std::time::Instant::now();
                let set_lock = self
                    .by_tag_p
                    .entry(p_tag_hash)
                    .or_insert_with(|| RwLock::new(BTreeSet::new()));
                let entry_duration = start.elapsed();
                tracing::trace!(event_id = %hex::encode(event.id), index = "p-tag", p_tag = %hex::encode(p_tag), entry_duration_us = %entry_duration.as_micros(), "acquired p-tag index entry");

                let write_start = std::time::Instant::now();
                set_lock.write().insert(EventRef::new(event.clone()));
                let write_duration = write_start.elapsed();
                tracing::trace!(event_id = %hex::encode(event.id), index = "p-tag", p_tag = %hex::encode(p_tag), write_duration_us = %write_duration.as_micros(), "p-tag index write completed");
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
                    let start = std::time::Instant::now();
                    let inner_map = self.by_tag_other.entry(letter).or_insert_with(DashMap::new);
                    let inner_entry_duration = start.elapsed();
                    tracing::trace!(event_id = %hex::encode(event.id), index = "generic-tag", letter = %letter, value = %value, inner_entry_duration_us = %inner_entry_duration.as_micros(), "acquired generic tag inner map entry");

                    let start = std::time::Instant::now();
                    let set_lock = inner_map
                        .entry(value.to_string())
                        .or_insert_with(|| RwLock::new(BTreeSet::new()));
                    let entry_duration = start.elapsed();
                    tracing::trace!(event_id = %hex::encode(event.id), index = "generic-tag", letter = %letter, value = %value, entry_duration_us = %entry_duration.as_micros(), "acquired generic tag entry");

                    let write_start = std::time::Instant::now();
                    set_lock.write().insert(EventRef::new(event.clone()));
                    let write_duration = write_start.elapsed();
                    tracing::trace!(event_id = %hex::encode(event.id), index = "generic-tag", letter = %letter, value = %value, write_duration_us = %write_duration.as_micros(), "generic tag index write completed");
                }
            }
        }

        // Oldest-first index for eviction (newest first in EventRef ordering)
        {
            let shard = self.oldest_shard(&event.id);
            let start = std::time::Instant::now();
            let mut by_oldest = self.by_oldest[shard].write();
            let lock_duration = start.elapsed();
            tracing::trace!(event_id = %hex::encode(event.id), index = "by_oldest", shard = shard, lock_duration_us = %lock_duration.as_micros(), "acquired by_oldest shard write lock");

            let insert_start = std::time::Instant::now();
            by_oldest.insert(EventRef::new(event.clone()));
            let insert_duration = insert_start.elapsed();
            tracing::trace!(event_id = %hex::encode(event.id), index = "by_oldest", shard = shard, insert_duration_us = %insert_duration.as_micros(), "by_oldest shard insert completed");
        }

        // Final consistency check: verify the event is still in by_id.
        // A concurrent remove() could have removed from by_id while we were
        // populating secondary indexes, leaving orphaned entries.
        // For replaceable events, also check we're still the winner.
        if !self.by_id.contains_key(&event.id) {
            self.cleanup_secondary_indexes(&event);
            if event.is_replaceable() {
                return InsertOutcome::LostRace;
            }
            return InsertOutcome::Duplicate;
        }

        if event.is_replaceable() && !self.is_still_winner(&event) {
            self.cleanup_event(&event);
            return InsertOutcome::LostRace;
        }

        InsertOutcome::Inserted { replaced }
    }

    /// Check if a replaceable event is still the current winner in by_replaceable
    fn is_still_winner(&self, event: &Event) -> bool {
        let key = event.replacement_key();
        match &key {
            ReplacementKey::Replaceable { .. } | ReplacementKey::Addressable { .. } => self
                .by_replaceable
                .get(&key)
                .map(|guard| *guard == event.id)
                .unwrap_or(false),
            _ => true,
        }
    }

    /// Remove an event from by_id and all secondary indexes, handling the case
    /// where by_id.remove() returns None (another thread already removed from by_id
    /// while we were adding to secondary indexes, leaving orphaned entries).
    fn cleanup_event(&self, event: &Arc<Event>) {
        if self.by_id.remove(&event.id).is_some() {
            self.memory_bytes
                .fetch_sub(event.raw.len(), AtomicOrdering::Relaxed);
            self.event_count.fetch_sub(1, AtomicOrdering::Relaxed);
        }

        self.cleanup_secondary_indexes(event);
    }

    /// Remove an event from all secondary indexes (but NOT by_id or counters).
    /// Used to clean up orphaned entries when a concurrent remove() already
    /// removed from by_id while we were adding to secondary indexes.
    /// The caller is responsible for counter adjustments.
    fn cleanup_secondary_indexes(&self, event: &Arc<Event>) {
        let er = EventRef::new(event.clone());
        let event_id = hex::encode(event.id);

        {
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            if let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) {
                let start = std::time::Instant::now();
                set_lock.write().remove(&er);
                let duration = start.elapsed();
                tracing::trace!(event_id = %event_id, index = "pubkey", cleanup = true, duration_us = %duration.as_micros(), "pubkey index cleanup completed");
            }
        }

        {
            if let Some(set_lock) = self.by_kind.get(&event.kind) {
                let start = std::time::Instant::now();
                set_lock.write().remove(&er);
                let duration = start.elapsed();
                tracing::trace!(event_id = %event_id, index = "kind", kind = event.kind, cleanup = true, duration_us = %duration.as_micros(), "kind index cleanup completed");
            }
        }

        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                if let Some(set_lock) = self.by_tag_e.get(&e_tag_hash) {
                    let start = std::time::Instant::now();
                    set_lock.write().remove(&er);
                    let duration = start.elapsed();
                    tracing::trace!(event_id = %event_id, index = "e-tag", e_tag = %hex::encode(e_tag), cleanup = true, duration_us = %duration.as_micros(), "e-tag index cleanup completed");
                }
            }
        }

        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                if let Some(set_lock) = self.by_tag_p.get(&p_tag_hash) {
                    let start = std::time::Instant::now();
                    set_lock.write().remove(&er);
                    let duration = start.elapsed();
                    tracing::trace!(event_id = %event_id, index = "p-tag", p_tag = %hex::encode(p_tag), cleanup = true, duration_us = %duration.as_micros(), "p-tag index cleanup completed");
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
                    if let Some(inner_map) = self.by_tag_other.get(&letter) {
                        if let Some(set_lock) = inner_map.get(value) {
                            let start = std::time::Instant::now();
                            set_lock.write().remove(&er);
                            let duration = start.elapsed();
                            tracing::trace!(event_id = %event_id, index = "generic-tag", letter = %letter, value = %value, cleanup = true, duration_us = %duration.as_micros(), "generic tag index cleanup completed");
                        }
                    }
                }
            }
        }

        {
            let shard = self.oldest_shard(&event.id);
            let start = std::time::Instant::now();
            let mut by_oldest = self.by_oldest[shard].write();
            let lock_duration = start.elapsed();
            tracing::trace!(event_id = %event_id, index = "by_oldest", shard = shard, cleanup = true, lock_duration_us = %lock_duration.as_micros(), "acquired by_oldest shard write lock for cleanup");

            let remove_start = std::time::Instant::now();
            by_oldest.remove(&er);
            let remove_duration = remove_start.elapsed();
            tracing::trace!(event_id = %event_id, index = "by_oldest", shard = shard, cleanup = true, remove_duration_us = %remove_duration.as_micros(), "by_oldest shard cleanup completed");
        }
    }

    /// Remove an event from all indexes
    /// If skip_replaceable is true, skip removing from the replaceable index (used during insert to avoid deadlock)
    pub fn remove(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        self.internal_remove(id, true)
    }

    fn internal_remove(&self, id: &[u8; 32], skip_replaceable: bool) -> Option<Arc<Event>> {
        let event_id = hex::encode(id);
        let start = std::time::Instant::now();
        tracing::trace!(event_id = %event_id, "acquiring by_id remove lock");
        let (_, event) = self.by_id.remove(id)?;
        let remove_duration = start.elapsed();
        tracing::trace!(event_id = %event_id, remove_duration_us = %remove_duration.as_micros(), "by_id remove completed");

        let event_size = event.raw.len();
        self.memory_bytes
            .fetch_sub(event_size, AtomicOrdering::Relaxed);
        self.event_count.fetch_sub(1, AtomicOrdering::Relaxed);

        if !skip_replaceable && event.is_replaceable() {
            let key = event.replacement_key();
            let start = std::time::Instant::now();
            self.by_replaceable.remove(&key);
            let duration = start.elapsed();
            tracing::trace!(event_id = %event_id, index = "replaceable", cleanup = true, duration_us = %duration.as_micros(), "replaceable index cleanup completed");
        }

        let er = EventRef::new(event.clone());

        // Remove from secondary indexes.
        // We no longer remove empty DashMap entries here because that creates
        // a TOCTOU race: between checking is_empty() and calling .remove(),
        // another thread could insert into the set. Leaving empty sets uses
        // slightly more memory but eliminates the race.
        {
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            if let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) {
                let start = std::time::Instant::now();
                set_lock.write().remove(&er);
                let duration = start.elapsed();
                tracing::trace!(event_id = %event_id, index = "pubkey", cleanup = true, duration_us = %duration.as_micros(), "pubkey index cleanup completed");
            }
        }

        {
            if let Some(set_lock) = self.by_kind.get(&event.kind) {
                let start = std::time::Instant::now();
                set_lock.write().remove(&er);
                let duration = start.elapsed();
                tracing::trace!(event_id = %event_id, index = "kind", kind = event.kind, cleanup = true, duration_us = %duration.as_micros(), "kind index cleanup completed");
            }
        }

        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                if let Some(set_lock) = self.by_tag_e.get(&e_tag_hash) {
                    let start = std::time::Instant::now();
                    set_lock.write().remove(&er);
                    let duration = start.elapsed();
                    tracing::trace!(event_id = %event_id, index = "e-tag", e_tag = %hex::encode(e_tag), cleanup = true, duration_us = %duration.as_micros(), "e-tag index cleanup completed");
                }
            }
        }

        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                if let Some(set_lock) = self.by_tag_p.get(&p_tag_hash) {
                    let start = std::time::Instant::now();
                    set_lock.write().remove(&er);
                    let duration = start.elapsed();
                    tracing::trace!(event_id = %event_id, index = "p-tag", p_tag = %hex::encode(p_tag), cleanup = true, duration_us = %duration.as_micros(), "p-tag index cleanup completed");
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
                    if let Some(inner_map) = self.by_tag_other.get(&letter) {
                        if let Some(set_lock) = inner_map.get(value) {
                            let start = std::time::Instant::now();
                            set_lock.write().remove(&er);
                            let duration = start.elapsed();
                            tracing::trace!(event_id = %event_id, index = "generic-tag", letter = %letter, value = %value, cleanup = true, duration_us = %duration.as_micros(), "generic tag index cleanup completed");
                        }
                    }
                }
            }
        }

        {
            let shard = self.oldest_shard(&event.id);
            let start = std::time::Instant::now();
            let mut by_oldest = self.by_oldest[shard].write();
            let lock_duration = start.elapsed();
            tracing::trace!(event_id = %event_id, index = "by_oldest", shard = shard, cleanup = true, lock_duration_us = %lock_duration.as_micros(), "acquired by_oldest shard write lock for removal");

            let remove_start = std::time::Instant::now();
            by_oldest.remove(&er);
            let remove_duration = remove_start.elapsed();
            tracing::trace!(event_id = %event_id, index = "by_oldest", shard = shard, cleanup = true, remove_duration_us = %remove_duration.as_micros(), "by_oldest shard removal completed");
        }

        Some(event)
    }

    /// Get the oldest events for eviction (returns oldest first).
    /// Scans all shards, merges results, and returns the N oldest.
    pub fn get_oldest(&self, count: usize) -> Vec<EventRef> {
        let start = std::time::Instant::now();

        // Use a min-heap (via reverse ordering) to merge N oldest from all shards.
        // EventRef sorts newest-first, so its Reverse sorts oldest-first.
        use std::cmp::Reverse;
        let mut heap: std::collections::BinaryHeap<Reverse<EventRef>> =
            std::collections::BinaryHeap::with_capacity(count);

        for shard in &self.by_oldest {
            let set = shard.read();
            // BTreeSet is sorted newest-first, so iterate from end (oldest)
            for er in set.iter().rev() {
                if heap.len() < count {
                    heap.push(Reverse(er.clone()));
                } else if er.created_at < heap.peek().unwrap().0.created_at {
                    heap.pop();
                    heap.push(Reverse(er.clone()));
                } else {
                    // This shard is sorted oldest→newest from .rev(), so once
                    // we see a newer event than our heap max, we can stop.
                    break;
                }
            }
        }

        // Extract from heap, sorted oldest-first
        let mut oldest: Vec<EventRef> = Vec::with_capacity(heap.len());
        while let Some(Reverse(er)) = heap.pop() {
            oldest.push(er);
        }
        oldest.reverse(); // oldest first

        let duration = start.elapsed();
        tracing::trace!(requested = %count, returned = %oldest.len(), duration_us = %duration.as_micros(), "get_oldest completed");

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

    fn make_event_with_content(
        id: u8,
        pubkey: u8,
        kind: u32,
        created_at: u64,
        content: &str,
    ) -> Arc<Event> {
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
