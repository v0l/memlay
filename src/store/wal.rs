use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;

use crate::event::Event;

/// Maximum size for a single WAL record (16MB)
/// This prevents OOM from corrupted/malicious WAL files
const MAX_WAL_RECORD_SIZE: usize = 16 * 1024 * 1024;

/// WAL operation types
#[derive(Debug, Clone)]
pub enum WalOp {
    /// Insert an event
    Insert(Vec<u8>),
    /// Delete an event by ID (32 bytes)
    Delete([u8; 32]),
}

/// Write-Ahead Log for persistent event storage
///
/// Instead of writing the entire database snapshot, WAL only writes
/// individual operations (inserts, deletes) as they happen.
pub struct WriteAheadLog {
    path: String,
    write_file: std::sync::Mutex<BufWriter<File>>,
    read_path: String,
}

impl WriteAheadLog {
    /// Open or create a new WAL at the given path
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let data_dir = Path::new(path);
        if !data_dir.exists() {
            fs::create_dir_all(data_dir)?;
        }

        let wal_path = data_dir.join("wal.log");
        let wal_path_str = wal_path.to_string_lossy().to_string();

        // Open file for writing (appending)
        let write_file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&wal_path)?;

        Ok(Self {
            path: wal_path_str.clone(),
            write_file: std::sync::Mutex::new(BufWriter::new(write_file)),
            read_path: wal_path_str,
        })
    }

    /// Append an operation to the WAL
    pub fn append(&self, op: &WalOp) -> anyhow::Result<()> {
        let mut file = self.write_file.lock().unwrap();

        // Write operation type (1 byte)
        match op {
            WalOp::Insert(_) => file.write_all(&[0u8])?,
            WalOp::Delete(_) => file.write_all(&[1u8])?,
        }

        match op {
            WalOp::Insert(data) => {
                // Write length (4 bytes, little-endian)
                let len = data.len() as u32;
                file.write_all(&len.to_le_bytes())?;
                // Write data
                file.write_all(data)?;
            }
            WalOp::Delete(id) => {
                // Write 32-byte ID
                file.write_all(id)?;
            }
        }

        // Flush and sync to disk to ensure durability
        file.flush()?;
        file.get_ref().sync_data()?;

        Ok(())
    }

    /// Append an insert operation
    pub fn insert(&self, event: &Arc<Event>) -> anyhow::Result<()> {
        self.append(&WalOp::Insert(event.raw.to_vec()))
    }

    /// Append a delete operation
    pub fn delete(&self, id: &[u8; 32]) -> anyhow::Result<()> {
        self.append(&WalOp::Delete(*id))
    }

    /// Replay all operations from the WAL
    /// Returns the number of operations replayed
    pub fn replay<F>(&self, mut f: F) -> anyhow::Result<usize>
    where
        F: FnMut(WalOp),
    {
        // Open file fresh for reading
        let file = File::open(&self.read_path)?;
        let mut reader = BufReader::new(file);

        let mut count = 0;
        loop {
            // Read operation type
            let mut op_type = [0u8; 1];
            match reader.read(&mut op_type) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read WAL op type");
                    break;
                }
            }

            match op_type[0] {
                0 => {
                    // Insert operation
                    let mut len_buf = [0u8; 4];
                    reader.read_exact(&mut len_buf)?;
                    let len = u32::from_le_bytes(len_buf) as usize;

                    // Validate record size to prevent OOM from corrupted WAL
                    if len > MAX_WAL_RECORD_SIZE {
                        tracing::error!(
                            len = len,
                            max = MAX_WAL_RECORD_SIZE,
                            "WAL record exceeds maximum size, possible corruption"
                        );
                        return Err(anyhow::anyhow!(
                            "WAL record size {} exceeds maximum {}",
                            len,
                            MAX_WAL_RECORD_SIZE
                        ));
                    }

                    let mut data = vec![0u8; len];
                    reader.read_exact(&mut data)?;

                    f(WalOp::Insert(data));
                }
                1 => {
                    // Delete operation
                    let mut id = [0u8; 32];
                    reader.read_exact(&mut id)?;

                    f(WalOp::Delete(id));
                }
                _ => {
                    tracing::warn!(op_type = op_type[0], "unknown WAL operation type");
                    break;
                }
            }

            count += 1;
        }

        Ok(count)
    }

    /// Get the path to the WAL file
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Flush the WAL to disk (ensure durability)
    pub fn flush(&self) -> anyhow::Result<()> {
        let mut file = self.write_file.lock().unwrap();
        file.flush()?;
        file.get_ref().sync_data()?;
        Ok(())
    }

    /// Truncate the WAL (after checkpoint)
    pub fn truncate(&self) -> anyhow::Result<()> {
        // Close the write handle and truncate the file
        {
            let mut file = self.write_file.lock().unwrap();
            file.flush()?;
            file.get_ref().sync_data()?;
            drop(file);
        }

        // Truncate the file
        let file = OpenOptions::new().write(true).open(&self.read_path)?;
        file.set_len(0)?;
        file.sync_data()?;

        Ok(())
    }

    /// Compact the WAL by keeping only inserts for events that are still live.
    ///
    /// Rewrites the WAL to contain only insert operations for event IDs present
    /// in `live_ids`. Delete operations and inserts for evicted events are dropped.
    /// Returns the number of operations removed.
    pub fn compact(&self, live_ids: &HashSet<[u8; 32]>) -> anyhow::Result<usize> {
        let file = File::open(&self.read_path)?;
        let mut reader = BufReader::new(&file);

        let temp_path = format!("{}.tmp", self.read_path);
        let temp_file = File::create(&temp_path)?;
        let mut writer = BufWriter::new(temp_file);

        let mut kept = 0usize;
        let mut removed = 0usize;
        let mut seen = HashSet::new();

        // Stream through WAL: copy only live inserts to temp file
        loop {
            let mut op_type = [0u8; 1];
            match reader.read(&mut op_type) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }

            match op_type[0] {
                0 => {
                    let mut len_buf = [0u8; 4];
                    if reader.read_exact(&mut len_buf).is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes(len_buf) as usize;

                    if len > MAX_WAL_RECORD_SIZE {
                        return Err(anyhow::anyhow!(
                            "WAL record size {} exceeds maximum {}",
                            len,
                            MAX_WAL_RECORD_SIZE
                        ));
                    }

                    let mut data = vec![0u8; len];
                    if reader.read_exact(&mut data).is_err() {
                        break;
                    }

                    // Parse event to get its ID
                    let keep = match Event::from_json_unchecked(&data) {
                        Ok(event) => live_ids.contains(&event.id) && seen.insert(event.id),
                        Err(_) => false,
                    };

                    if keep {
                        writer.write_all(&[0u8])?;
                        writer.write_all(&len_buf)?;
                        writer.write_all(&data)?;
                        kept += 1;
                    } else {
                        removed += 1;
                    }
                }
                1 => {
                    // Delete ops are no longer needed after compaction
                    let mut id = [0u8; 32];
                    if reader.read_exact(&mut id).is_err() {
                        break;
                    }
                    removed += 1;
                }
                _ => break,
            }
        }

        writer.flush()?;
        writer.get_ref().sync_data()?;
        drop(writer);
        drop(reader);
        drop(file);

        // Close the current write handle before renaming
        {
            let mut file = self.write_file.lock().unwrap();
            file.flush()?;
        }

        std::fs::rename(&temp_path, &self.read_path)?;

        let new_write_file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.read_path)?;

        *self.write_file.lock().unwrap() = BufWriter::new(new_write_file);

        tracing::info!(kept, removed, "compacted WAL");

        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBuilder;
    use tempfile::TempDir;

    #[test]
    fn test_wal_append_and_replay() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();

        let wal = WriteAheadLog::open(&path).unwrap();

        // Create a test event
        let event = Arc::new(
            EventBuilder::new()
                .pubkey([1u8; 32])
                .kind(1)
                .created_at(1000)
                .content("test")
                .build(),
        );

        let event_id = event.id;

        // Append insert
        wal.insert(&event).unwrap();

        // Append delete
        wal.delete(&event_id).unwrap();

        // Replay
        let mut inserts = 0;
        let mut deletes = 0;

        wal.replay(|op| match op {
            WalOp::Insert(_) => inserts += 1,
            WalOp::Delete(_) => deletes += 1,
        })
        .unwrap();

        assert_eq!(inserts, 1);
        assert_eq!(deletes, 1);
    }

    #[test]
    fn test_wal_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();

        // Create WAL and add data
        {
            let wal = WriteAheadLog::open(&path).unwrap();
            let event = Arc::new(
                EventBuilder::new()
                    .pubkey([1u8; 32])
                    .kind(1)
                    .created_at(1000)
                    .content("test")
                    .build(),
            );

            wal.insert(&event).unwrap();
        }

        // Open new WAL instance and replay
        let wal2 = WriteAheadLog::open(&path).unwrap();

        let mut count = 0;
        wal2.replay(|op| {
            if matches!(op, WalOp::Insert(_)) {
                count += 1;
            }
        })
        .unwrap();

        assert_eq!(count, 1);
    }

    #[test]
    fn test_wal_compact_removes_old_entries() {
        use std::collections::HashSet;

        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();

        let wal = WriteAheadLog::open(&path).unwrap();

        let mut event_ids = Vec::new();
        for i in 0..50 {
            let event = Arc::new(
                EventBuilder::new()
                    .pubkey([i as u8; 32])
                    .kind(1)
                    .created_at(i as u64)
                    .content("test event data that adds some size")
                    .build(),
            );
            wal.insert(&event).unwrap();
            event_ids.push(event.id);
        }

        let wal_path = std::path::Path::new(&path).join("wal.log");
        let size_before = wal_path.metadata().unwrap().len();
        assert!(size_before > 0);

        // Keep only the last 20 events as "live"
        let live_ids: HashSet<[u8; 32]> = event_ids[30..].iter().copied().collect();
        let removed = wal.compact(&live_ids).unwrap();

        assert_eq!(removed, 30, "Should have removed 30 WAL entries");

        let size_after = wal_path.metadata().unwrap().len();
        assert!(
            size_after < size_before,
            "WAL should be smaller after compaction"
        );

        let mut count = 0;
        wal.replay(|_| {
            count += 1;
        })
        .unwrap();
        assert_eq!(count, 20, "Should have 20 entries after compaction");
    }
}
