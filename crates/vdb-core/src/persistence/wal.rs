//! Writing the write-ahead log.
//!
//! The frame format lives in `vdb-format`; this is the part that puts frames into a file
//! through the [`Storage`] abstraction, and it is where the durability policy is applied.
//!
//! # The one rule that makes batches atomic
//!
//! A transaction group is written with **a single `append`**. The commit record is the last
//! frame in that buffer, so any interruption — a torn write, a full disk, a process kill —
//! leaves the commit record absent, and replay discards the whole group. If the group were
//! appended frame by frame, a crash between the last operation and the commit record would be
//! indistinguishable at the storage layer from a crash before it, but the intermediate frames
//! would already be durable and every future reader would have to reason about them.

use vdb_format::{WalFrame, WalOp, Writer};

use crate::error::{Result, StorageError, StorageOp};
use crate::path::DbPath;
use crate::persistence::Durability;
use crate::storage::{File, OpenMode, Storage};

/// Appends frames to one log file.
#[derive(Debug)]
pub struct WalWriter {
    file: Box<dyn File>,
    path: DbPath,
    next_sequence: u64,
    next_txn: u64,
    bytes: u64,
    durability: Durability,
    /// Whether anything has been appended since the last successful sync.
    unsynced: bool,
}

impl WalWriter {
    /// Open a log for appending, creating it if it does not exist.
    ///
    /// `first_sequence` is the sequence the next frame will carry; recovery passes one past the
    /// highest sequence it replayed, so numbering stays monotonic across restarts.
    ///
    /// # Errors
    /// Any [`StorageError`](crate::error::StorageError).
    pub fn open(
        storage: &dyn Storage,
        path: &DbPath,
        first_sequence: u64,
        durability: Durability,
    ) -> Result<Self> {
        let file = storage.open_file(path, OpenMode::Create)?;
        let bytes = file.len()?;
        Ok(Self {
            file,
            path: path.clone(),
            next_sequence: first_sequence,
            next_txn: 1,
            bytes,
            durability,
            unsynced: false,
        })
    }

    /// The sequence the next frame will carry.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Bytes in the log.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether anything is pending a sync.
    pub fn is_unsynced(&self) -> bool {
        self.unsynced
    }

    /// Append one standalone operation.
    ///
    /// # Errors
    /// A format error if the operation is too large to frame, or any storage error.
    pub fn append(&mut self, op: WalOp) -> Result<u64> {
        let sequence = self.next_sequence;
        let frame = WalFrame::standalone(sequence, op);
        let mut buf = Writer::new();
        frame
            .append_to(&mut buf)
            .map_err(|e| crate::error::from_format_at(e, &self.path))?;
        self.write(buf.as_slice())?;
        self.next_sequence += 1;
        if self.durability.syncs_every_write() {
            self.sync()?;
        }
        Ok(sequence)
    }

    /// Append a group of operations followed by their commit record, as one write.
    ///
    /// Returns the transaction id. Replay applies the group only if the commit record survived,
    /// which is what makes a batch all-or-nothing across a crash.
    ///
    /// # Errors
    /// A format error if a frame cannot be built, or any storage error.
    pub fn append_group(&mut self, ops: Vec<WalOp>) -> Result<u64> {
        let txn_id = self.next_txn;
        self.next_txn += 1;
        let op_count = ops.len() as u64;

        let mut buf = Writer::new();
        let mut sequence = self.next_sequence;
        for op in ops {
            WalFrame::in_txn(sequence, txn_id, op)
                .append_to(&mut buf)
                .map_err(|e| crate::error::from_format_at(e, &self.path))?;
            sequence += 1;
        }
        WalFrame::in_txn(sequence, txn_id, WalOp::Commit { op_count })
            .append_to(&mut buf)
            .map_err(|e| crate::error::from_format_at(e, &self.path))?;
        sequence += 1;

        // One append: the commit record cannot become durable separately from what it commits.
        self.write(buf.as_slice())?;
        self.next_sequence = sequence;
        if self.durability.syncs_on_commit() {
            self.sync()?;
        }
        Ok(txn_id)
    }

    /// Make everything appended so far durable.
    ///
    /// # Errors
    /// [`StorageError::Io`] if the sync fails.
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        self.unsynced = false;
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let offset = self.file.append(bytes)?;
        // A log whose append landed somewhere other than the end means another writer is active
        // on the same file, which the single-writer lock is supposed to prevent. Detect it here
        // rather than discovering interleaved frames during recovery.
        if offset != self.bytes {
            return Err(StorageError::Io {
                path: self.path.clone(),
                operation: StorageOp::Append,
                detail: format!(
                    "append landed at {offset} but the log is {} bytes; another writer?",
                    self.bytes
                ),
            }
            .into());
        }
        self.bytes += bytes.len() as u64;
        self.unsynced = true;
        Ok(())
    }
}
