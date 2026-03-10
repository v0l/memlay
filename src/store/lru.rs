use std::collections::VecDeque;

/// Tracks event insertion order for LRU eviction.
/// Uses a simple FIFO queue - oldest events are evicted first.
pub struct LruTracker {
    /// Insertion order queue: (event_id, byte_size)
    order: VecDeque<([u8; 32], usize)>,
    /// Maximum number of events
    max_events: usize,
    /// Current total bytes
    current_bytes: usize,
    /// Maximum bytes (0 = unlimited)
    max_bytes: usize,
}

impl LruTracker {
    pub fn new(max_events: usize, max_bytes: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(max_events.min(1024)),
            max_events,
            current_bytes: 0,
            max_bytes,
        }
    }

    /// Insert an event, returns IDs of events that should be evicted.
    /// Evicts the oldest event(s) to make room.
    pub fn insert(&mut self, id: [u8; 32], size: usize) -> Vec<[u8; 32]> {
        let mut to_evict = Vec::new();

        // Evict by count
        while self.order.len() >= self.max_events {
            if let Some((evict_id, evict_size)) = self.order.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(evict_size);
                to_evict.push(evict_id);
            }
        }

        // Evict by bytes (if max_bytes is set)
        if self.max_bytes > 0 {
            while self.current_bytes + size > self.max_bytes && !self.order.is_empty() {
                if let Some((evict_id, evict_size)) = self.order.pop_front() {
                    self.current_bytes = self.current_bytes.saturating_sub(evict_size);
                    to_evict.push(evict_id);
                }
            }
        }

        // Add new event
        self.order.push_back((id, size));
        self.current_bytes += size;

        to_evict
    }

    /// Touch an event (move to back of queue for true LRU).
    /// Note: This is O(n) - for high performance, you might want to skip this
    /// or use a different data structure (linked hash map).
    pub fn touch(&mut self, id: &[u8; 32]) {
        // For now, we use FIFO (insertion order) rather than true LRU
        // because moving elements in VecDeque is O(n).
        // If true LRU is needed, switch to linked_hash_map crate.
        let _ = id; // Intentionally unused for FIFO behavior
    }

    /// Remove an event from tracking (called when event is explicitly deleted)
    pub fn remove(&mut self, id: &[u8; 32]) {
        if let Some(pos) = self.order.iter().position(|(eid, _)| eid == id) {
            if let Some((_, size)) = self.order.remove(pos) {
                self.current_bytes = self.current_bytes.saturating_sub(size);
            }
        }
    }

    /// Current number of tracked events
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Check if tracker is empty
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Current bytes used
    pub fn bytes_used(&self) -> usize {
        self.current_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_within_limit() {
        let mut lru = LruTracker::new(10, 0);

        let evicted = lru.insert([1; 32], 100);
        assert!(evicted.is_empty());
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.bytes_used(), 100);
    }

    #[test]
    fn test_eviction_by_count() {
        let mut lru = LruTracker::new(3, 0);

        lru.insert([1; 32], 100);
        lru.insert([2; 32], 100);
        lru.insert([3; 32], 100);
        assert_eq!(lru.len(), 3);

        // Should evict oldest (id=1)
        let evicted = lru.insert([4; 32], 100);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], [1; 32]);
        assert_eq!(lru.len(), 3);
    }

    #[test]
    fn test_eviction_by_bytes() {
        let mut lru = LruTracker::new(1000, 300); // max 300 bytes

        lru.insert([1; 32], 100);
        lru.insert([2; 32], 100);
        assert_eq!(lru.bytes_used(), 200);

        // This should evict [1] to make room
        let evicted = lru.insert([3; 32], 150);
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0], [1; 32]);
        assert_eq!(lru.bytes_used(), 250); // 100 (id=2) + 150 (id=3)
    }

    #[test]
    fn test_remove() {
        let mut lru = LruTracker::new(10, 0);

        lru.insert([1; 32], 100);
        lru.insert([2; 32], 200);
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.bytes_used(), 300);

        lru.remove(&[1; 32]);
        assert_eq!(lru.len(), 1);
        assert_eq!(lru.bytes_used(), 200);
    }
}
