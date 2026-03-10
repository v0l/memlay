mod index;
mod lru;

pub use index::{EventIndex, EventRef};
pub use lru::LruTracker;

use crate::event::Event;
use std::sync::Arc;

/// Configuration for the event store
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Maximum number of events to store
    pub max_events: usize,
    /// Maximum memory usage in bytes (0 = unlimited)
    pub max_bytes: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_events: 1_000_000,
            max_bytes: 0, // unlimited by default
        }
    }
}

/// High-performance in-memory event store with LRU eviction
pub struct EventStore {
    index: EventIndex,
    lru: parking_lot::RwLock<LruTracker>,
    config: StoreConfig,
}

impl EventStore {
    pub fn new(config: StoreConfig) -> Self {
        Self {
            index: EventIndex::new(),
            lru: parking_lot::RwLock::new(LruTracker::new(config.max_events, config.max_bytes)),
            config,
        }
    }

    /// Insert an event, returning any evicted events
    pub fn insert(&self, event: Arc<Event>) -> Vec<Arc<Event>> {
        let event_size = event.size();
        let event_id = event.id;

        // Check if event already exists
        if self.index.get(&event_id).is_some() {
            return vec![];
        }

        // Determine which events need to be evicted
        let to_evict = {
            let mut lru = self.lru.write();
            lru.insert(event_id, event_size)
        };

        // Evict old events first
        let mut evicted = Vec::with_capacity(to_evict.len());
        for id in to_evict {
            if let Some(old_event) = self.index.remove(&id) {
                evicted.push(old_event);
            }
        }

        // Insert the new event
        self.index.insert(event);

        evicted
    }

    /// Get an event by ID
    pub fn get(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        let event = self.index.get(id)?;
        // Update LRU on access
        self.lru.write().touch(id);
        Some(event)
    }

    /// Query events by pubkey, ordered by created_at DESC
    pub fn query_by_pubkey(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_pubkey(pubkey, limit)
    }

    /// Query events by kind, ordered by created_at DESC
    pub fn query_by_kind(&self, kind: u32, limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_kind(kind, limit)
    }

    /// Query events by pubkey with time filter, ordered by created_at DESC
    pub fn query_by_pubkey_since(
        &self,
        pubkey: &[u8; 32],
        since: u64,
        limit: usize,
    ) -> Vec<Arc<Event>> {
        self.index.query_by_pubkey_since(pubkey, since, limit)
    }

    /// Query events by e-tag (referenced event ID)
    pub fn query_by_e_tag(&self, event_id: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_e_tag(event_id, limit)
    }

    /// Query events by p-tag (referenced pubkey)
    pub fn query_by_p_tag(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_p_tag(pubkey, limit)
    }

    /// Number of events in the store
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Check if store is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total bytes used by events
    pub fn bytes_used(&self) -> usize {
        self.lru.read().bytes_used()
    }

    /// Get store configuration
    pub fn config(&self) -> &StoreConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn make_event(id: u8, pubkey: u8, kind: u32, created_at: u64) -> Arc<Event> {
        let json = format!(
            r#"{{"id":"{:0>64}","pubkey":"{:0>64}","created_at":{},"kind":{},"tags":[],"content":"test","sig":"{:0>128}"}}"#,
            format!("{:x}", id),
            format!("{:x}", pubkey),
            created_at,
            kind,
            "0"
        );
        Arc::new(Event::from_json(json.as_bytes()).unwrap())
    }

    #[test]
    fn test_insert_and_get() {
        let store = EventStore::new(StoreConfig::default());
        let event = make_event(1, 1, 1, 1000);
        let id = event.id;

        store.insert(event);

        let retrieved = store.get(&id).unwrap();
        assert_eq!(retrieved.id, id);
    }

    #[test]
    fn test_lru_eviction() {
        let config = StoreConfig {
            max_events: 3,
            max_bytes: 0,
        };
        let store = EventStore::new(config);

        // Insert 3 events
        store.insert(make_event(1, 1, 1, 1000));
        store.insert(make_event(2, 1, 1, 2000));
        store.insert(make_event(3, 1, 1, 3000));
        assert_eq!(store.len(), 3);

        // Insert 4th event, should evict oldest (id=1)
        let evicted = store.insert(make_event(4, 1, 1, 4000));
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].id[31], 1);
        assert_eq!(store.len(), 3);

        // Verify event 1 is gone
        let mut id1 = [0u8; 32];
        id1[31] = 1;
        assert!(store.get(&id1).is_none());
    }

    #[test]
    fn test_query_by_kind() {
        let store = EventStore::new(StoreConfig::default());

        store.insert(make_event(1, 1, 1, 1000));
        store.insert(make_event(2, 1, 1, 2000));
        store.insert(make_event(3, 1, 0, 3000)); // different kind

        let results = store.query_by_kind(1, 10);
        assert_eq!(results.len(), 2);
        // Should be ordered by created_at DESC
        assert_eq!(results[0].created_at, 2000);
        assert_eq!(results[1].created_at, 1000);
    }

    #[test]
    fn test_query_by_pubkey_since() {
        let store = EventStore::new(StoreConfig::default());

        store.insert(make_event(1, 1, 1, 1000));
        store.insert(make_event(2, 1, 1, 2000));
        store.insert(make_event(3, 1, 1, 3000));
        store.insert(make_event(4, 2, 1, 4000)); // different pubkey

        let mut pubkey = [0u8; 32];
        pubkey[31] = 1;

        let results = store.query_by_pubkey_since(&pubkey, 1500, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].created_at, 3000);
        assert_eq!(results[1].created_at, 2000);
    }
}
