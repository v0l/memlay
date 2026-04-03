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

    /// Compact the WAL by removing old entries that exceed the memory limit.
    ///
    /// This reads all operations, keeps only the most recent ones that fit
    /// within max_bytes, and rewrites the WAL. Returns the number of operations removed.
    pub fn compact(&self, max_bytes: usize, current_mem: usize) -> anyhow::Result<usize> {
        // If we're already under the limit, no need to compact
        if current_mem <= max_bytes {
            return Ok(0);
        }

        // Open file fresh for reading
        let file = File::open(&self.read_path)?;
        let mut reader = BufReader::new(&file);

        // Read all operations into memory
        let mut operations: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut total_size = 0;

        loop {
            let mut op_type = [0u8; 1];
            match reader.read(&mut op_type) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }

            match op_type[0] {
                0 => {
                    // Insert operation
                    let mut len_buf = [0u8; 4];
                    if reader.read_exact(&mut len_buf).is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes(len_buf) as usize;

                    if len > MAX_WAL_RECORD_SIZE {
                        tracing::error!(
                            len,
                            max = MAX_WAL_RECORD_SIZE,
                            "WAL record exceeds maximum during compact"
                        );
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

                    let record_size = 1 + 4 + len; // op_type + length + data
                    operations.push((0, data));
                    total_size += record_size;
                }
                1 => {
                    // Delete operation
                    let mut id = [0u8; 32];
                    if reader.read_exact(&mut id).is_err() {
                        break;
                    }

                    let record_size = 1 + 32; // op_type + id
                    operations.push((1, id.to_vec()));
                    total_size += record_size;
                }
                _ => {
                    tracing::warn!(
                        op_type = op_type[0],
                        "unknown WAL operation type during compact"
                    );
                    break;
                }
            }
        }

        if operations.is_empty() {
            return Ok(0);
        }

        // Count how many operations we need to remove from the beginning
        // to get under the limit
        let mut removed = 0;
        let mut removed_size = 0;

        for (op_type, data) in operations.iter() {
            let record_size = if *op_type == 0 {
                1 + 4 + data.len() // insert
            } else {
                1 + 32 // delete
            };

            // Stop removing when we'd be at 80% of the limit
            if total_size - removed_size <= (max_bytes * 80 / 100) {
                break;
            }

            removed_size += record_size;
            removed += 1;
        }

        // If nothing to remove, we're done
        if removed == 0 {
            return Ok(0);
        }

        tracing::info!(
            removed,
            removed_bytes = removed_size,
            total_ops = operations.len(),
            "compacting WAL to respect memory limit"
        );

        // Write compacted WAL to temp file
        let temp_path = format!("{}.tmp", self.read_path);
        {
            let temp_file = File::create(&temp_path)?;
            let mut writer = BufWriter::new(temp_file);

            for (op_type, data) in operations.iter().skip(removed) {
                writer.write_all(&[*op_type])?;

                if *op_type == 0 {
                    // Insert
                    let len = data.len() as u32;
                    writer.write_all(&len.to_le_bytes())?;
                    writer.write_all(data)?;
                } else {
                    // Delete
                    writer.write_all(data)?;
                }
            }

            writer.flush()?;
            writer.get_ref().sync_data()?;
        }

        // Close the current write handle before renaming
        {
            let mut file = self.write_file.lock().unwrap();
            file.flush()?;
            drop(file);
        }

        // Replace the old WAL with the compacted one
        std::fs::rename(&temp_path, &self.read_path)?;

        // Reopen the write handle
        let new_write_file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&self.read_path)?;

        *self.write_file.lock().unwrap() = BufWriter::new(new_write_file);

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
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap().to_string();

        // Create WAL and add many events
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

        // Verify WAL has content
        let wal_path = std::path::Path::new(&path).join("wal.log");
        let size_before = wal_path.metadata().unwrap().len();
        assert!(size_before > 0);

        // Estimate size per record: 1 (op) + 4 (len) + ~70 (event data) = ~75 bytes
        // Compact to keep about 20 events (20 * 75 = 1500 bytes target)
        let target_size = 1500;
        let removed = wal.compact(target_size, 10000).unwrap();

        // Should have removed some entries but not all
        assert!(removed > 0, "Should have removed some WAL entries");
        assert!(removed < 50, "Should not have removed all entries");

        // Verify WAL is smaller
        let size_after = wal_path.metadata().unwrap().len();
        assert!(
            size_after < size_before,
            "WAL should be smaller after compaction"
        );

        // Verify we can still replay the remaining entries
        let mut count = 0;
        wal.replay(|_| {
            count += 1;
        })
        .unwrap();

        // Should have fewer entries than before but at least some
        assert!(count < 50, "Should have fewer entries after compaction");
        assert!(count > 0, "Should still have some entries");
    }
}
