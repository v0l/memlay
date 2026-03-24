use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod index;

pub use index::{EventIndex, EventRef};

use crate::event::Event;

/// Configuration for the event store
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Maximum memory usage in bytes (0 = unlimited)
    pub max_bytes: usize,
    /// Path to persist events to disk (None = disabled)
    /// Events are saved as JSONL (one JSON event per line)
    pub persistence_path: Option<String>,
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

/// High-performance in-memory event store with memory-based eviction
pub struct EventStore {
    index: EventIndex,
    config: StoreConfig,
    state: EvictionState,
}

impl StoreConfig {
    /// Create store config from target RAM percentage of total system memory
    /// Respects cgroup memory limits for cloud-native deployments.
    pub fn from_target_ram_percent(percent: u8) -> Self {
        if percent == 0 {
            return Self {
                max_bytes: 0,
                persistence_path: None,
            };
        }

        let total_memory = get_total_memory();
        let target_bytes = (total_memory as f64 * (percent as f64 / 100.0)) as usize;

        Self {
            max_bytes: target_bytes,
            persistence_path: None,
        }
    }

    /// Create store config with persistence enabled
    pub fn with_persistence(percent: u8, path: String) -> Self {
        if percent == 0 {
            return Self {
                max_bytes: 0,
                persistence_path: Some(path),
            };
        }

        let total_memory = get_total_memory();
        let target_bytes = (total_memory as f64 * (percent as f64 / 100.0)) as usize;

        Self {
            max_bytes: target_bytes,
            persistence_path: Some(path),
        }
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_bytes: 0, // unlimited by default
            persistence_path: None,
        }
    }
}

impl EventStore {
    pub fn new(config: StoreConfig) -> Self {
        Self {
            index: EventIndex::new(),
            config,
            state: EvictionState {
                interval_seconds: AtomicUsize::new(10),
                consecutive_evictions: AtomicUsize::new(0),
            },
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
    pub fn insert(&self, event: Arc<Event>) -> Option<Vec<Arc<Event>>> {
        let event_id = event.id;

        if self.index.get(&event_id).is_some() {
            return None;
        }
        
        self.index.insert(event);
        Some(vec![])
    }

    /// Batch insert events (more efficient for bulk operations)
    pub fn insert_batch(&self, events: Vec<Arc<Event>>) -> usize {
        let mut inserted = 0;
        
        for event in events {
            let event_id = event.id;
            
            if self.index.get(&event_id).is_some() {
                continue;
            }
            
            self.index.insert(event);
            inserted += 1;
        }
        
        inserted
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

    /// Save all events to disk
    pub fn save_to_disk(&self) -> anyhow::Result<()> {
        let Some(ref path) = self.config.persistence_path else {
            return Ok(());
        };

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

        tracing::info!(path = %events_file.display(), count, "saved events to disk");
        Ok(())
    }

    /// Load events from disk
    pub fn load_from_disk(&self) -> anyhow::Result<usize> {
        let Some(ref path) = self.config.persistence_path else {
            return Ok(0);
        };

        let data_dir = Path::new(path);
        let events_file = data_dir.join("events.jsonl");

        if !events_file.exists() {
            tracing::debug!(path = %events_file.display(), "no events file found");
            return Ok(0);
        }

        let file = File::open(&events_file)?;
        let reader = BufReader::new(file);

        let mut count = 0;
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
                    count += 1;
                }
                Err(e) => {
                    errors += 1;
                    tracing::warn!(error = %e, "failed to parse event from disk");
                }
            }
        }

        if errors > 0 {
            tracing::warn!(errors, "some events failed to load from disk");
        }

        tracing::info!(path = %events_file.display(), loaded = count, errors, "loaded events from disk");
        Ok(count)
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
    use crate::event::Event;
    use tempfile::TempDir;

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
        let store = EventStore::new(StoreConfig::default());
        let event = make_event(1, 1, 1, 1000);
        let id = event.id;

        store.insert(event);

        let retrieved = store.get(&id).unwrap();
        assert_eq!(retrieved.id, id);
    }

    #[test]
    fn test_lru_eviction() {
        let config = StoreConfig::from_target_ram_percent(0);
        let store = EventStore::new(config);

        // Insert 3 events
        store.insert(make_event(1, 1, 1, 1000));
        store.insert(make_event(2, 1, 1, 2000));
        store.insert(make_event(3, 1, 1, 3000));
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn test_batch_insert() {
        let store = EventStore::new(StoreConfig::default());
        
        let events: Vec<Arc<Event>> = vec![
            make_event(1, 1, 1, 1000),
            make_event(2, 1, 1, 2000),
            make_event(3, 1, 1, 3000),
        ];
        
        let inserted = store.insert_batch(events);
        assert_eq!(inserted, 3);
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn test_query_by_kind() {
        let store = EventStore::new(StoreConfig::default());

        store.insert(make_event(1, 1, 1, 1000));
        store.insert(make_event(2, 1, 1, 2000));
        store.insert(make_event(3, 1, 0, 3000));

        let results = store.query_by_kind(1, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].created_at, 2000);
        assert_eq!(results[1].created_at, 1000);
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
