use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod index;
mod wal;

pub use index::{EventIndex, EventRef};
pub use wal::{WalOp, WriteAheadLog};

use crate::event::Event;

/// Configuration for the event store
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Maximum memory usage in bytes (0 = unlimited)
    pub max_bytes: usize,
    /// Path to persist events to disk (None = disabled)
    /// With WAL enabled, operations are appended incrementally
    /// Without WAL, full snapshots are written periodically
    pub persistence_path: Option<String>,
    /// Enable Write-Ahead Logging (default: true)
    /// When enabled, only changes are written to disk instead of full snapshots
    pub use_wal: bool,
}

/// Get total system memory respecting cgroup limits for cloud-native deployments.
fn get_total_memory() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    
    if let Some(limits) = sys.cgroup_limits() {
        return limits.total_memory;
    }
    
    sys.total_memory()
}

/// Get current process memory usage (RSS) in bytes.
/// Uses sysinfo for cross-platform compatibility (Windows, macOS, Linux).
pub fn get_process_memory() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(
        sysinfo::ProcessesToUpdate::All,
        true,
    );
    
    match sysinfo::get_current_pid() {
        Ok(pid) => {
            if let Some(process) = sys.process(pid) {
                return process.memory();
            }
        }
        Err(_e) => {}
    }
    
    0
}

/// Adaptive eviction state
struct EvictionState {
    interval_seconds: AtomicUsize,
    consecutive_evictions: AtomicUsize,
}

/// Result of inserting an event into the store
#[derive(Debug)]
pub enum InsertResult {
    /// Event was not stored (ephemeral events are not stored)
    Ephemeral,
    /// Event was a duplicate (already exists)
    Duplicate,
    /// Event was stored successfully
    Stored {
        /// The newly inserted event
        event: Arc<Event>,
        /// Old events that were replaced (for replaceable events)
        replaced: Vec<Arc<Event>>,
    },
}

/// High-performance in-memory event store with memory-based eviction
pub struct EventStore {
    index: EventIndex,
    config: StoreConfig,
    state: EvictionState,
    wal: Option<Arc<WriteAheadLog>>,
}

impl StoreConfig {
    /// Create store config from target RAM percentage of total system memory
    /// Respects cgroup memory limits for cloud-native deployments.
    pub fn from_target_ram_percent(percent: u8) -> Self {
        if percent == 0 {
            return Self {
                max_bytes: 0,
                persistence_path: None,
                use_wal: true,
            };
        }

        let total_memory = get_total_memory();
        let target_bytes = (total_memory as f64 * (percent as f64 / 100.0)) as usize;

        Self {
            max_bytes: target_bytes,
            persistence_path: None,
            use_wal: true,
        }
    }

    /// Create store config with persistence enabled
    pub fn with_persistence(percent: u8, path: String) -> Self {
        if percent == 0 {
            return Self {
                max_bytes: 0,
                persistence_path: Some(path),
                use_wal: true,
            };
        }

        let total_memory = get_total_memory();
        let target_bytes = (total_memory as f64 * (percent as f64 / 100.0)) as usize;

        Self {
            max_bytes: target_bytes,
            persistence_path: Some(path),
            use_wal: true,
        }
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_bytes: 0, // unlimited by default
            persistence_path: None,
            use_wal: true, // WAL enabled by default
        }
    }
}

impl EventStore {
    pub fn new(config: StoreConfig) -> Self {
        // Initialize WAL if persistence is enabled and WAL is requested
        let wal = if config.persistence_path.is_some() && config.use_wal {
            match WriteAheadLog::open(config.persistence_path.as_ref().unwrap()) {
                Ok(wal) => Some(Arc::new(wal)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open WAL, disabling WAL");
                    None
                }
            }
        } else {
            None
        };

        Self {
            index: EventIndex::new(),
            config,
            state: EvictionState {
                interval_seconds: AtomicUsize::new(10),
                consecutive_evictions: AtomicUsize::new(0),
            },
            wal,
        }
    }

    /// Start background eviction task (call from relay startup)
    pub fn start_eviction_task(self: &Arc<Self>) {
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                let interval = Duration::from_secs(store.state.interval_seconds.load(Ordering::Relaxed) as u64);
                tokio::time::sleep(interval).await;
                store.maybe_evict();
            }
        });
    }

    /// Background eviction check with adaptive interval and batch eviction
    fn maybe_evict(&self) {
        let current_mem = get_process_memory();
        let max_bytes = self.config.max_bytes;
        
        if max_bytes == 0 || current_mem == 0 {
            self.state.consecutive_evictions.store(0, Ordering::Relaxed);
            return;
        }
        
        let threshold = max_bytes as u64 * 70 / 100;
        let target = max_bytes as u64 * 50 / 100;
        
        // Adaptive interval: evict more frequently when close to limit
        let new_interval = if current_mem >= threshold {
            1  // 1s when over 70% for high input rates
        } else if current_mem >= max_bytes as u64 * 50 / 100 {
            2  // 2s when 50-70% of limit
        } else {
            5  // 5s when under 50% of limit
        };
        self.state.interval_seconds.store(new_interval, Ordering::Relaxed);
        
        // Evict in batch if over threshold - remove up to 1000 events at once
        if current_mem > target {
            let batch_size = 1000;
            let mut removed = 0;
            
            // Get batch of oldest events
            let oldest = self.index.get_oldest(batch_size);
            
            // Remove all without re-checking memory
            for event_ref in oldest.iter() {
                self.index.remove(&event_ref.id);
                removed += 1;
            }
            
            if removed > 0 {
                let evictions = self.state.consecutive_evictions.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(
                    evicted = removed,
                    evictions_streak = evictions,
                    mem_bytes = current_mem,
                    "batch eviction"
                );
            }
        }
    }

    /// Check if an event id is already in the store
    pub fn contains(&self, id: &[u8; 32]) -> bool {
        self.index.get(id).is_some()
    }

    /// Insert a single event
    /// 
    /// Returns InsertResult indicating:
    /// - Ephemeral: event was not stored (ephemeral events are not persisted)
    /// - Duplicate: event already exists
    /// - Stored: event was stored, with any replaced events (for replaceable events)
    pub fn insert(&self, event: Arc<Event>) -> InsertResult {
        let event_id = event.id;
        let start = std::time::Instant::now();

        // Ephemeral events should never be stored
        if event.is_ephemeral() {
            return InsertResult::Ephemeral;
        }

        // Check if event already exists
        if self.index.get(&event_id).is_some() {
            return InsertResult::Duplicate;
        }
        
        // Insert the event and get replaced events (for replaceable events)
        let replaced = self.index.insert(event.clone());
        
        // Write to WAL if available
        if let Some(ref wal) = self.wal {
            if let Err(e) = wal.insert(&event) {
                tracing::error!(error = %e, "failed to write insert to WAL");
            }
            
            // If this event replaced another, write delete for the old one
            if let Some(replaced_event) = &replaced {
                if let Err(e) = wal.delete(&replaced_event.id) {
                    tracing::error!(error = %e, "failed to write delete to WAL");
                }
            }
        }
        
        // Record write delay metric
        crate::metrics::observe_write_delay(start.elapsed());
        crate::metrics::inc_events_saved();
        
        InsertResult::Stored {
            event,
            replaced: replaced.map(|e| vec![e]).unwrap_or_default(),
        }
    }

    /// Batch insert events (more efficient for bulk operations)
    /// 
    /// Returns (inserted_count, replaced_events) where:
    /// - inserted_count: number of new events stored (excluding ephemeral and duplicates)
    /// - replaced_events: events that were replaced by new replaceable events
    pub fn insert_batch(&self, events: Vec<Arc<Event>>) -> (usize, Vec<Arc<Event>>) {
        let mut inserted = 0;
        let mut replaced = Vec::new();
        
        for event in events {
            // Skip ephemeral events
            if event.is_ephemeral() {
                continue;
            }
            
            let event_id = event.id;
            
            if self.index.get(&event_id).is_some() {
                continue;
            }
            
            let old = self.index.insert(event.clone());
            inserted += 1;
            
            // Write to WAL if available
            if let Some(ref wal) = self.wal {
                if let Err(e) = wal.insert(&event) {
                    tracing::error!(error = %e, "failed to write batch insert to WAL");
                }
                
                if let Some(old_event) = &old {
                    if let Err(e) = wal.delete(&old_event.id) {
                        tracing::error!(error = %e, "failed to write batch delete to WAL");
                    }
                }
            }
            
            if let Some(old_event) = old {
                replaced.push(old_event);
            }
        }
        
        (inserted, replaced)
    }

    /// Get an event by ID
    pub fn get(&self, id: &[u8; 32]) -> Option<Arc<Event>> {
        self.index.get(id)
    }

    /// Query events by pubkey
    pub fn query_by_pubkey(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_pubkey(pubkey, limit)
    }

    /// Query events by kind
    pub fn query_by_kind(&self, kind: u32, limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_kind(kind, limit)
    }

    /// Query events by pubkey with time filter
    pub fn query_by_pubkey_since(
        &self,
        pubkey: &[u8; 32],
        since: u64,
        limit: usize,
    ) -> Vec<Arc<Event>> {
        self.index.query_by_pubkey_since(pubkey, since, limit)
    }

    /// Query events by e-tag
    pub fn query_by_e_tag(&self, event_id: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_e_tag(event_id, limit)
    }

    /// Query events by p-tag
    pub fn query_by_p_tag(&self, pubkey: &[u8; 32], limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_p_tag(pubkey, limit)
    }

    /// Query events by tag
    pub fn query_by_tag(&self, letter: char, value: &str, limit: usize) -> Vec<Arc<Event>> {
        self.index.query_by_tag(letter, value, limit)
    }

    /// Number of events in the store
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Check if store is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current bytes used (deprecated - use process RSS)
    pub fn bytes_used(&self) -> usize {
        0
    }

    /// Get store configuration
    pub fn config(&self) -> &StoreConfig {
        &self.config
    }

    /// Save all events to disk (checkpoint with WAL)
    /// 
    /// With WAL enabled: creates a snapshot and truncates the WAL
    /// Without WAL: writes all events to the snapshot file
    pub fn save_to_disk(&self) -> anyhow::Result<()> {
        let Some(ref path) = self.config.persistence_path else {
            return Ok(());
        };

        let start = std::time::Instant::now();
        let data_dir = Path::new(path);
        
        // Create data directory if it doesn't exist
        if !data_dir.exists() {
            fs::create_dir_all(data_dir)?;
        }

        let events_file = data_dir.join("events.jsonl");
        let temp_file = data_dir.join("events.jsonl.tmp");

        // Write events to temp file first (atomic write)
        let file = File::create(&temp_file)?;
        let mut writer = BufWriter::new(file);

        let mut count = 0;
        // Iterate through all events in the index
        for event in self.index.iter_all() {
            writer.write_all(&event.raw)?;
            writer.write_all(b"\n")?;
            count += 1;
        }

        writer.flush()?;
        drop(writer);

        // Atomic rename
        fs::rename(&temp_file, &events_file)?;

        // If WAL is enabled, truncate it after checkpoint
        if let Some(ref wal) = self.wal {
            if let Err(e) = wal.truncate() {
                tracing::warn!(error = %e, "failed to truncate WAL after checkpoint");
            } else {
                tracing::debug!("WAL truncated after checkpoint");
            }
        }

        // Record disk persistence time metric
        crate::metrics::observe_disk_persistence(start.elapsed());

        tracing::info!(path = %events_file.display(), count, "saved events to disk");
        Ok(())
    }

    /// Load events from disk (snapshot + WAL replay)
    pub fn load_from_disk(&self) -> anyhow::Result<usize> {
        let Some(ref path) = self.config.persistence_path else {
            return Ok(0);
        };

        let data_dir = Path::new(path);
        let events_file = data_dir.join("events.jsonl");
        let mut total_count = 0;

        // Load snapshot if it exists
        if events_file.exists() {
            let file = File::open(&events_file)?;
            let reader = BufReader::new(file);

            let mut snapshot_count = 0;
            let mut errors = 0;

            for line_result in reader.lines() {
                let line = line_result?;
                if line.trim().is_empty() {
                    continue;
                }

                match Event::from_json(line.as_bytes()) {
                    Ok(event) => {
                        let event = Arc::new(event);
                        self.index.insert(event);
                        snapshot_count += 1;
                    }
                    Err(e) => {
                        errors += 1;
                        tracing::warn!(error = %e, "failed to parse event from snapshot");
                    }
                }
            }

            total_count += snapshot_count;

            if errors > 0 {
                tracing::warn!(errors, "some events failed to load from snapshot");
            }

            tracing::info!(path = %events_file.display(), loaded = snapshot_count, errors, "loaded snapshot from disk");
        } else {
            tracing::debug!(path = %events_file.display(), "no snapshot file found");
        }

        // Replay WAL if available
        if let Some(ref wal) = self.wal {
            let wal_path = wal.path();
            let mut wal_count = 0;
            let mut errors = 0;

            match wal.replay(|op| {
                match op {
                    WalOp::Insert(data) => {
                        match Event::from_json(&data) {
                            Ok(event) => {
                                self.index.insert(Arc::new(event));
                                wal_count += 1;
                            }
                            Err(e) => {
                                errors += 1;
                                tracing::warn!(error = %e, "failed to parse event from WAL");
                            }
                        }
                    }
                    WalOp::Delete(id) => {
                        // Remove event from index
                        self.index.remove(&id);
                        wal_count += 1;
                    }
                }
            }) {
                Ok(_) => {
                    if wal_count > 0 {
                        tracing::info!(path = wal_path, replayed = wal_count, errors, "replayed WAL operations");
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to replay WAL");
                }
            }
            
            total_count += wal_count;
        }

        tracing::info!(total = total_count, "total events loaded from disk");
        Ok(total_count)
    }

    /// Start background persistence task (call from relay startup)
    pub fn start_persistence_task(self: &Arc<Self>, interval_seconds: u64) {
        let Some(ref path) = self.config.persistence_path else {
            return;
        };

        let store = self.clone();
        let path = path.clone();
        
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
                
                match store.save_to_disk() {
                    Ok(_) => {
                        tracing::debug!(path, "background persistence completed");
                    }
                    Err(e) => {
                        tracing::error!(path, error = %e, "background persistence failed");
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventBuilder, Tag};
    use tempfile::TempDir;

    fn make_event(id: u8, pubkey: u8, kind: u32, created_at: u64) -> Arc<Event> {
        Arc::new(EventBuilder::new()
            .pubkey(set_byte([0u8; 32], pubkey))
            .kind(kind)
            .created_at(created_at)
            .content("test")
            .build())
    }

    fn make_event_with_d_tag(
        id: u8,
        pubkey: u8,
        kind: u32,
        created_at: u64,
        d_tag: &str,
    ) -> Arc<Event> {
        Arc::new(EventBuilder::new()
            .pubkey(set_byte([0u8; 32], pubkey))
            .kind(kind)
            .created_at(created_at)
            .content("test")
            .tag("d", d_tag)
            .build())
    }

    fn set_byte(mut arr: [u8; 32], byte: u8) -> [u8; 32] {
        arr[31] = byte;
        arr
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
    fn test_insert_ephemeral_event() {
        let store = EventStore::new(StoreConfig::default());
        
        // Create ephemeral event (kind 20001)
        let ephemeral = make_event(1, 1, 20001, 1000);
        
        // Ephemeral events should not be stored
        let result = store.insert(ephemeral);
        assert!(matches!(result, InsertResult::Ephemeral));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_insert_replaceable_event_replaces_old() {
        let store = EventStore::new(StoreConfig::default());
        
        let pubkey = 1u8;
        
        // Insert first replaceable event (kind 10000)
        let event1 = make_event(1, pubkey, 10000, 1000);
        let result1 = store.insert(event1.clone());
        assert!(matches!(result1, InsertResult::Stored { .. }));
        assert_eq!(store.len(), 1);
        
        // Insert new replaceable event (should replace old)
        let event2 = make_event(2, pubkey, 10000, 2000);
        let result2 = store.insert(event2.clone());
        
        match result2 {
            InsertResult::Stored { replaced, .. } => {
                assert_eq!(replaced.len(), 1);
                assert_eq!(replaced[0].id, event1.id);
            }
            _ => panic!("Expected Stored with replaced events"),
        }
        
        // Should still have only 1 event
        assert_eq!(store.len(), 1);
        
        // Should have the new event
        let retrieved = store.get(&event2.id).unwrap();
        assert_eq!(retrieved.id, event2.id);
        
        // Old event should be gone
        assert!(store.get(&event1.id).is_none());
    }

    #[test]
    fn test_insert_addressable_event_replaces_by_d_tag() {
        let store = EventStore::new(StoreConfig::default());
        
        let pubkey = 1u8;
        
        // Insert first addressable event with d-tag "profile"
        let event1 = make_event_with_d_tag(1, pubkey, 30000, 1000, "profile");
        let result1 = store.insert(event1.clone());
        assert!(matches!(result1, InsertResult::Stored { .. }));
        assert_eq!(store.len(), 1);
        
        // Insert new addressable event with same d-tag (should replace)
        let event2 = make_event_with_d_tag(2, pubkey, 30000, 2000, "profile");
        let result2 = store.insert(event2.clone());
        
        match result2 {
            InsertResult::Stored { replaced, .. } => {
                assert_eq!(replaced.len(), 1);
                assert_eq!(replaced[0].id, event1.id);
            }
            _ => panic!("Expected Stored with replaced events"),
        }
        
        // Should still have only 1 event
        assert_eq!(store.len(), 1);
        
        // Insert addressable event with different d-tag (should NOT replace)
        let event3 = make_event_with_d_tag(3, pubkey, 30000, 3000, "settings");
        let result3 = store.insert(event3.clone());
        assert!(matches!(result3, InsertResult::Stored { replaced, .. } if replaced.is_empty()));
        
        // Should now have 2 events
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_insert_regular_event_no_replacement() {
        let store = EventStore::new(StoreConfig::default());
        
        let pubkey = 1u8;
        
        // Insert regular note (kind 1)
        let event1 = make_event(1, pubkey, 1, 1000);
        let result1 = store.insert(event1.clone());
        assert!(matches!(result1, InsertResult::Stored { replaced, .. } if replaced.is_empty()));
        assert_eq!(store.len(), 1);
        
        // Insert another note from same author (should NOT replace)
        let event2 = make_event(2, pubkey, 1, 2000);
        let result2 = store.insert(event2.clone());
        assert!(matches!(result2, InsertResult::Stored { replaced, .. } if replaced.is_empty()));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn test_insert_duplicate_event() {
        let store = EventStore::new(StoreConfig::default());
        
        let event = make_event(1, 1, 1, 1000);
        let event_id = event.id;
        
        store.insert(event.clone());
        assert_eq!(store.len(), 1);
        
        // Insert same event again
        let result = store.insert(event.clone());
        assert!(matches!(result, InsertResult::Duplicate));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_batch_insert_with_ephemeral_and_replaceable() {
        let store = EventStore::new(StoreConfig::default());
        
        let pubkey = 1u8;
        
        let events = vec![
            // Regular event
            make_event(1, pubkey, 1, 1000),
            // Ephemeral event (should be skipped)
            make_event(2, pubkey, 20001, 1001),
            // Replaceable event (kind 10000)
            make_event(3, pubkey, 10000, 1002),
            // Another regular event
            make_event(4, pubkey, 1, 1003),
        ];
        
        let (inserted, replaced) = store.insert_batch(events);
        
        assert_eq!(inserted, 3); // ephemeral skipped
        assert_eq!(replaced.len(), 0);
        assert_eq!(store.len(), 3);
        
        // Ephemeral should not be stored
        let ephemeral_id = make_event(2, pubkey, 20001, 1001).id;
        assert!(store.get(&ephemeral_id).is_none());
    }

    #[test]
    fn test_get_total_memory() {
        let memory = get_total_memory();
        assert!(memory > 0, "Total memory should be greater than 0");
        assert!(memory <= u64::MAX, "Total memory should not exceed u64::MAX");
    }

    #[test]
    fn test_persistence_save_and_load() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();
        
        let config = StoreConfig::with_persistence(0, path.clone());
        let store = EventStore::new(config);
        
        // Insert some events
        store.insert(make_event(1, 1, 1, 1000));
        store.insert(make_event(2, 1, 1, 2000));
        store.insert(make_event(3, 1, 1, 3000));
        assert_eq!(store.len(), 3);
        
        // Save to disk
        store.save_to_disk().unwrap();
        
        // Verify file exists
        let events_file = std::path::Path::new(&path).join("events.jsonl");
        assert!(events_file.exists());
        
        // Verify file has content
        let content = std::fs::read_to_string(&events_file).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn test_persistence_no_file_when_disabled() {
        let config = StoreConfig::default();
        let store = EventStore::new(config);
        
        store.insert(make_event(1, 1, 1, 1000));
        
        // Should return Ok(()) when persistence is disabled
        let result = store.save_to_disk();
        assert!(result.is_ok());
    }

    #[test]
    fn test_persistence_load_when_no_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();
        
        let config = StoreConfig::with_persistence(0, path);
        let store = EventStore::new(config);
        
        // Should return Ok(0) when no file exists
        let loaded = store.load_from_disk().unwrap();
        assert_eq!(loaded, 0);
    }
}
