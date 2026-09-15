use crate::deletion::Coordinate;
use crate::event::{Event, ReplacementKey};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// Number of shards for the by_oldest eviction index.
/// Each shard has its own RwLock, so inserts/removes to different shards
/// never contend. Events are assigned to shards by hashing the event ID.
const OLDEST_SHARDS: usize = 64;

/// Upper bound on NIP-09 tombstones kept in memory. Tombstones are not covered
/// by `max_bytes`, and a client can mint one per publish+delete cycle, so the
/// set is a FIFO with a hard cap: roughly 80 bytes per entry across the map and
/// the queue, i.e. ~20 MiB at this size. Evicting the oldest tombstone only
/// means a very old deleted event could be re-published.
const MAX_TOMBSTONES: usize = 262_144;

/// Hasher for keys that are already uniformly random (event IDs). Takes the
/// first 8 bytes instead of running SipHash over all 32.
#[derive(Default)]
struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() >= 8 {
            self.0 = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type IdHashBuilder = std::hash::BuildHasherDefault<IdHasher>;

/// Bounded FIFO set of deleted event IDs (NIP-09 anti-resurrection).
///
/// `contains` sits on the insert hot path, so the common case (no deletions
/// yet) is a single relaxed load and the rest is an identity-hashed lookup.
struct Tombstones {
    count: AtomicUsize,
    ids: DashMap<[u8; 32], (), IdHashBuilder>,
    order: Mutex<VecDeque<[u8; 32]>>,
}

impl Tombstones {
    fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            ids: DashMap::with_hasher(IdHashBuilder::default()),
            order: Mutex::new(VecDeque::new()),
        }
    }

    #[inline]
    fn contains(&self, id: &[u8; 32]) -> bool {
        self.count.load(AtomicOrdering::Relaxed) != 0 && self.ids.contains_key(id)
    }

    fn insert(&self, id: &[u8; 32]) {
        if self.ids.insert(*id, ()).is_some() {
            return;
        }
        let mut order = self.order.lock();
        order.push_back(*id);
        while order.len() > MAX_TOMBSTONES {
            if let Some(evicted) = order.pop_front() {
                self.ids.remove(&evicted);
            }
        }
        self.count.store(self.ids.len(), AtomicOrdering::Relaxed);
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Tag letter used as key in `by_tag_other`.
type TagLetter = char;

/// 128-bit hash for tag index keys.
/// Computes a hash from the full 32-byte ID by XOR-folding its two 16-byte
/// halves. IDs are SHA-256 outputs and pubkeys are uniformly random, so the
/// result is uniformly distributed.
///
/// At 128 bits, finding *any* accidental or adversarial collision requires on
/// the order of 2^64 work (birthday bound) — cryptographically infeasible — so
/// callers can trust the bucket contents without re-verifying the full key.
/// (The previous 64-bit fold was forgeable: an attacker controlling a tag value
/// could grind a colliding value in ~2^32 work and pollute another query.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TagIdHash(u128);

impl TagIdHash {
    pub fn from_id(id: &[u8; 32]) -> Self {
        let hi = u128::from_be_bytes(id[0..16].try_into().unwrap());
        let lo = u128::from_be_bytes(id[16..32].try_into().unwrap());
        Self(hi ^ lo)
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
    /// Event was previously removed by a NIP-09 deletion request and must not
    /// be resurrected by re-publishing the same ID.
    Deleted,
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

    // NIP-09 tombstones: event IDs removed by a deletion request. Re-publishing
    // the exact same event (same ID = same sig) is rejected while its tombstone
    // exists, so a lagging client or WAL replay cannot resurrect deleted data.
    tombstones: Tombstones,

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
            tombstones: Tombstones::new(),
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

        // NIP-09: an event that was removed by a deletion request must not be
        // resurrected by re-publishing the identical event bytes (same ID).
        if self.tombstones.contains(&event.id) {
            return InsertOutcome::Deleted;
        }

        // Handle replaceable events: atomically swap old ID in by_replaceable,
        // but only if the incoming event is newer than the one we already hold.
        // NIP-01: keep the LATEST version per (pubkey, kind[, d-tag]); a
        // late-arriving older event must NOT delete a newer stored one.
        if event.is_replaceable() {
            let key = event.replacement_key();
            if let ReplacementKey::Replaceable { .. } | ReplacementKey::Addressable { .. } = &key {
                use dashmap::mapref::entry::Entry;
                match self.by_replaceable.entry(key.clone()) {
                    Entry::Occupied(mut occ) => {
                        let old_id = *occ.get();
                        // Compare against the currently-stored event. Newer wins;
                        // ties broken by lexicographically-lower id (deterministic).
                        let incoming_wins = match self.by_id.get(&old_id) {
                            Some(old) => {
                                event.created_at > old.created_at
                                    || (event.created_at == old.created_at && event.id < old.id)
                            }
                            // Stored id already gone (evicted/removed) — take over.
                            None => true,
                        };
                        if !incoming_wins {
                            // Incoming event is stale; reject without storing.
                            return InsertOutcome::LostRace;
                        }
                        occ.insert(event.id);
                        if let Some(old_event) = self.internal_remove(&old_id, true) {
                            replaced = Some(old_event);
                        }
                    }
                    Entry::Vacant(vac) => {
                        vac.insert(event.id);
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
            let set_lock = self
                .by_pubkey
                .entry(pubkey_hash)
                .or_insert_with(|| RwLock::new(BTreeSet::new()));
            set_lock.write().insert(EventRef::new(event.clone()));
        }

        // Kind index
        {
            let set_lock = self
                .by_kind
                .entry(event.kind)
                .or_insert_with(|| RwLock::new(BTreeSet::new()));
            set_lock.write().insert(EventRef::new(event.clone()));
        }

        // E-tag index
        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                let set_lock = self
                    .by_tag_e
                    .entry(e_tag_hash)
                    .or_insert_with(|| RwLock::new(BTreeSet::new()));
                set_lock.write().insert(EventRef::new(event.clone()));
            }
        }

        // P-tag index
        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                let set_lock = self
                    .by_tag_p
                    .entry(p_tag_hash)
                    .or_insert_with(|| RwLock::new(BTreeSet::new()));
                set_lock.write().insert(EventRef::new(event.clone()));
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
                    let set_lock = inner_map
                        .entry(value.to_string())
                        .or_insert_with(|| RwLock::new(BTreeSet::new()));
                    set_lock.write().insert(EventRef::new(event.clone()));
                }
            }
        }

        // Oldest-first index for eviction (newest first in EventRef ordering)
        {
            let shard = self.oldest_shard(&event.id);
            let mut by_oldest = self.by_oldest[shard].write();
            by_oldest.insert(EventRef::new(event.clone()));
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

        {
            let pubkey_hash = TagIdHash::from_id(&event.pubkey);
            if let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) {
                set_lock.write().remove(&er);
            }
        }

        {
            if let Some(set_lock) = self.by_kind.get(&event.kind) {
                set_lock.write().remove(&er);
            }
        }

        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                if let Some(set_lock) = self.by_tag_e.get(&e_tag_hash) {
                    set_lock.write().remove(&er);
                }
            }
        }

        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                if let Some(set_lock) = self.by_tag_p.get(&p_tag_hash) {
                    set_lock.write().remove(&er);
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
                            set_lock.write().remove(&er);
                        }
                    }
                }
            }
        }

        {
            let shard = self.oldest_shard(&event.id);
            let mut by_oldest = self.by_oldest[shard].write();
            by_oldest.remove(&er);
        }
    }

    /// Remove an event from all indexes
    /// If skip_replaceable is true, skip removing from the replaceable index (used during insert to avoid deadlock)
    pub fn remove(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        self.internal_remove(id, true)
    }

    /// NIP-09: delete an event by ID on behalf of its author, recording a
    /// tombstone so the same event bytes cannot be re-inserted later.
    /// Returns the removed event, if it was present.
    pub fn delete_by_id(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        let removed = self.internal_remove(id, false);
        self.tombstones.insert(id);
        if removed.is_some() {
            crate::metrics::inc_events_deleted();
        }
        removed
    }

    /// NIP-09: stored versions of a replaceable event
    /// (`<kind>:<pubkey>:<d-tag>`) whose `created_at` is not newer than
    /// `until`, without removing anything.
    ///
    /// A version newer than the deletion request is left alone: NIP-09 deletes
    /// versions "up to the created_at timestamp of the deletion request event",
    /// so a repost that is newer than a late-arriving delete must survive.
    ///
    /// The caller removes these via [`EventIndex::delete_by_id`] after writing
    /// the WAL delete records, so a crash mid-delete replays the removal.
    pub fn coordinate_targets(&self, coord: &Coordinate, until: u64) -> Vec<[u8; 32]> {
        let replacement_key = if coord.d_tag.is_empty() {
            ReplacementKey::Replaceable {
                pubkey: coord.pubkey,
                kind: coord.kind,
            }
        } else {
            ReplacementKey::Addressable {
                pubkey: coord.pubkey,
                kind: coord.kind,
                d_tag: coord.d_tag.clone(),
            }
        };

        // Snapshot candidate IDs under the DashMap shard lock and drop it
        // before the caller takes the slow per-event locks.
        let candidate_ids: Vec<[u8; 32]> = match self.by_replaceable.get(&replacement_key) {
            Some(entry) => vec![*entry.value()],
            None => Vec::new(),
        };

        candidate_ids
            .into_iter()
            .filter(|id| {
                self.by_id
                    .get(id)
                    .is_some_and(|event| event.created_at <= until)
            })
            .collect()
    }

    /// True if this event ID has a NIP-09 tombstone (deleted, do not re-accept).
    pub fn is_tombstoned(&self, id: &[u8; 32]) -> bool {
        self.tombstones.contains(id)
    }

    /// Record a NIP-09 tombstone without requiring the event to be present
    /// (used by WAL replay where the event may only exist in the snapshot).
    pub fn tombstone(&self, id: &[u8; 32]) {
        self.tombstones.insert(id);
    }

    /// Number of NIP-09 tombstones currently held (capped at [`MAX_TOMBSTONES`]).
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.len()
    }

    fn internal_remove(&self, id: &[u8; 32], skip_replaceable: bool) -> Option<Arc<Event>> {
        let (_, event) = self.by_id.remove(id)?;

        let event_size = event.raw.len();
        self.memory_bytes
            .fetch_sub(event_size, AtomicOrdering::Relaxed);
        self.event_count.fetch_sub(1, AtomicOrdering::Relaxed);

        if !skip_replaceable && event.is_replaceable() {
            let key = event.replacement_key();
            self.by_replaceable.remove(&key);
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
                set_lock.write().remove(&er);
            }
        }

        {
            if let Some(set_lock) = self.by_kind.get(&event.kind) {
                set_lock.write().remove(&er);
            }
        }

        {
            for e_tag in event.e_tags() {
                let e_tag_hash = TagIdHash::from_id(&e_tag);
                if let Some(set_lock) = self.by_tag_e.get(&e_tag_hash) {
                    set_lock.write().remove(&er);
                }
            }
        }

        {
            for p_tag in event.p_tags() {
                let p_tag_hash = TagIdHash::from_id(&p_tag);
                if let Some(set_lock) = self.by_tag_p.get(&p_tag_hash) {
                    set_lock.write().remove(&er);
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
                            set_lock.write().remove(&er);
                        }
                    }
                }
            }
        }

        {
            let shard = self.oldest_shard(&event.id);
            let mut by_oldest = self.by_oldest[shard].write();
            by_oldest.remove(&er);
        }

        Some(event)
    }

    /// Get the oldest events for eviction (returns oldest first).
    /// Scans all shards, merges results, and returns the N oldest.
    pub fn get_oldest(&self, count: usize) -> Vec<EventRef> {
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

        oldest
    }

    /// Get the newest `count` events across all data (created_at DESC).
    /// Used to satisfy filters with no selective index (e.g. `{}` or a bare
    /// `since`/`until`), bounded by the caller's limit.
    pub fn get_newest(&self, count: usize) -> Vec<Arc<Event>> {
        if count == 0 {
            return Vec::new();
        }
        // Min-heap of the newest `count` seen so far. EventRef sorts newest
        // first, so Reverse(er) makes the heap's peek() the OLDEST kept event,
        // which is the one to drop when a newer candidate arrives.
        use std::cmp::Reverse;
        // Cap preallocation: callers may pass usize::MAX to mean "no limit".
        let cap = count.min(4096);
        let mut heap: std::collections::BinaryHeap<Reverse<EventRef>> =
            std::collections::BinaryHeap::with_capacity(cap);

        for shard in &self.by_oldest {
            let set = shard.read();
            // Iterate newest-first; stop early once the newest remaining is not
            // better than our current oldest-kept.
            for er in set.iter() {
                if heap.len() < count {
                    heap.push(Reverse(er.clone()));
                } else if er.created_at > heap.peek().unwrap().0.created_at {
                    heap.pop();
                    heap.push(Reverse(er.clone()));
                } else {
                    break;
                }
            }
        }

        let mut newest: Vec<EventRef> = Vec::with_capacity(heap.len());
        while let Some(Reverse(er)) = heap.pop() {
            newest.push(er);
        }
        // Heap pops oldest-first; reverse to newest-first.
        newest.reverse();
        newest.into_iter().map(|er| er.event).collect()
    }

    /// Get current memory usage in bytes (raw JSON only)
    pub fn memory_bytes(&self) -> usize {
        self.memory_bytes.load(AtomicOrdering::Relaxed)
    }

    /// Inflate the payload accounting without storing events, so memory-budget
    /// behaviour can be exercised without allocating gigabytes.
    #[cfg(test)]
    pub fn add_memory_bytes_for_test(&self, bytes: usize) {
        self.memory_bytes.fetch_add(bytes, AtomicOrdering::Relaxed);
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

    /// Query by pubkey, returns events sorted by created_at DESC
    pub fn query_by_pubkey(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        let pubkey_hash = TagIdHash::from_id(pubkey);

        // Fast path: check if entry exists without holding lock
        let Some(set_lock) = self.by_pubkey.get(&pubkey_hash) else {
            return Vec::new();
        };

        // Clone Arcs quickly while holding read lock, then return.
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
            // The set is ordered created_at DESC, so every event with
            // created_at >= since is a contiguous prefix. take_while stops at
            // the first older event: O(log n + k) instead of scanning all.
            set.iter()
                .take_while(|r| r.created_at >= since)
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
    fn test_replaceable_older_does_not_replace_newer() {
        // kind 10000 is replaceable; same pubkey.
        let index = EventIndex::new();
        let newer = make_event(2, 1, 10000, 2000);
        let newer_id = newer.id;
        index.insert(newer);
        assert_eq!(index.len(), 1);

        // A late-arriving OLDER event must not evict the newer stored one.
        let older = make_event(1, 1, 10000, 1000);
        let older_id = older.id;
        let outcome = index.insert(older);
        assert!(matches!(outcome, InsertOutcome::LostRace));
        assert_eq!(index.len(), 1);
        assert!(index.get(&newer_id).is_some());
        assert!(index.get(&older_id).is_none());

        // A newer event DOES replace.
        let newest = make_event(3, 1, 10000, 3000);
        let newest_id = newest.id;
        let outcome = index.insert(newest);
        assert!(matches!(outcome, InsertOutcome::Inserted { .. }));
        assert_eq!(index.len(), 1);
        assert!(index.get(&newest_id).is_some());
        assert!(index.get(&newer_id).is_none());
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

    fn id_bytes(byte: u8) -> [u8; 32] {
        let mut arr = [0u8; 32];
        arr[31] = byte;
        arr
    }

    #[test]
    fn test_delete_by_id_blocks_reinsert_and_clears_replaceable() {
        let index = EventIndex::new();
        let event = make_event(1, 7, 10002, 5000);
        index.insert(event.clone());

        assert!(index.delete_by_id(&event.id).is_some());
        assert!(index.is_tombstoned(&event.id));
        assert!(matches!(
            index.insert(event.clone()),
            InsertOutcome::Deleted
        ));
        assert!(index.get(&event.id).is_none());

        // The replaceable mapping must not keep pointing at the deleted id.
        let newer = make_event(2, 7, 10002, 6000);
        assert!(matches!(
            index.insert(newer.clone()),
            InsertOutcome::Inserted { .. }
        ));
        assert!(index.get(&newer.id).is_some());
    }

    #[test]
    fn test_coordinate_targets_spares_newer_versions() {
        let index = EventIndex::new();
        let event = make_event(1, 7, 10002, 5000);
        index.insert(event.clone());

        let coord = Coordinate {
            kind: 10002,
            pubkey: id_bytes(7),
            d_tag: String::new(),
        };

        // A deletion request older than the stored version deletes nothing.
        assert!(index.coordinate_targets(&coord, 4999).is_empty());
        assert_eq!(index.coordinate_targets(&coord, 5000), vec![event.id]);
    }

    #[test]
    fn test_tombstones_are_capped() {
        let tombstones = Tombstones::new();
        // Event IDs are SHA-256 outputs; spread the test ids the same way so
        // the identity hasher behaves as it does in production.
        let first = {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&u64::MAX.to_le_bytes());
            id
        };
        tombstones.insert(&first);
        for i in 0..MAX_TOMBSTONES as u64 {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
            tombstones.insert(&id);
            tombstones.insert(&id); // repeats must not consume capacity
        }

        assert_eq!(tombstones.len(), MAX_TOMBSTONES);
        assert!(!tombstones.contains(&first), "oldest entry should age out");
    }
}
