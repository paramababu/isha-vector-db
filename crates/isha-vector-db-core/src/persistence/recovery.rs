//! Replaying a log back into a memtable.
//!
//! The rule this implements, from `docs/architecture/05-storage-and-persistence.md` §5.6:
//! a log that ends mid-frame was interrupted by process death, which is expected and is
//! repaired by truncating the tail. A frame that is entirely present and fails its checksum
//! means the bytes reached storage and are wrong, which is damage and is reported.
//!
//! Getting that distinction wrong in either direction is bad: too strict and every unclean
//! shutdown looks like data loss to the user; too lax and real corruption is silently discarded.

use isha_vector_db_format::{wal, WalOp, WalTail};

use crate::document::DocId;
use crate::error::{CorruptionError, DbError, Result};
use crate::metadata::Metadata;
use crate::path::DbPath;
use crate::storage::{OpenMode, Storage};
use crate::write::memtable::Memtable;
use isha_vector_db_format::IdKind;

/// What a replay did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Frames applied to the memtable.
    pub applied: usize,
    /// Frames present in the log but discarded because their transaction never committed.
    pub rolled_back: usize,
    /// Highest sequence seen, so the writer can continue from one past it.
    pub last_sequence: u64,
    /// Bytes of complete, valid frames. The log is truncated here.
    pub valid_bytes: u64,
    /// Whether a partial frame was found and discarded.
    pub truncated_tail: bool,
}

/// Replay a log file into a memtable.
///
/// A missing log is not an error: it means nothing was written since the last checkpoint.
///
/// # Errors
/// [`CorruptionError`] if a complete frame failed its checksum or was structurally invalid, or
/// any storage error.
pub fn replay_into(
    storage: &dyn Storage,
    path: &DbPath,
    id_kind: IdKind,
    memtable: &mut Memtable,
) -> Result<ReplayReport> {
    let Some(meta) = storage.metadata(path)? else {
        return Ok(ReplayReport {
            applied: 0,
            rolled_back: 0,
            last_sequence: 0,
            valid_bytes: 0,
            truncated_tail: false,
        });
    };

    let file = storage.open_file(path, OpenMode::Read)?;
    let mut bytes = vec![0u8; meta.len as usize];
    let read = file.read_at(&mut bytes, 0)?;
    bytes.truncate(read);

    let scan = wal::scan(&bytes);
    if let WalTail::Corrupt { offset, reason } = &scan.tail {
        return Err(DbError::Corruption(CorruptionError::MalformedStructure {
            path: path.clone(),
            offset: *offset,
            detail: reason.to_string(),
        }));
    }
    let truncated_tail = matches!(scan.tail, WalTail::Torn { .. });

    let committed = scan.committed();
    let applied = committed.len();
    // Frames present but not applied: everything except commit records and the frames we kept.
    let non_commit = scan
        .frames
        .iter()
        .filter(|f| !matches!(f.op, WalOp::Commit { .. }))
        .count();
    let rolled_back = non_commit - applied;
    let last_sequence = scan.frames.iter().map(|f| f.sequence).max().unwrap_or(0);

    for frame in committed {
        match &frame.op {
            WalOp::Put {
                id,
                vector,
                metadata,
                content,
            } => {
                let doc_id = DocId::from_bytes(id_kind, id)?;
                let meta = if metadata.is_empty() {
                    None
                } else {
                    Some(Metadata::decode(metadata)?)
                };
                memtable.put_bytes(doc_id, vector, meta, content.clone())?;
            }
            WalOp::Delete { id } => {
                memtable.delete(DocId::from_bytes(id_kind, id)?);
            }
            WalOp::Commit { .. } => {}
            // `WalOp` is #[non_exhaustive]: an operation this build does not understand cannot
            // be replayed, and guessing would silently diverge from what was written.
            _ => {
                return Err(DbError::Corruption(CorruptionError::MalformedStructure {
                    path: path.clone(),
                    offset: scan.valid_bytes,
                    detail: "log contains an operation this build does not understand".to_owned(),
                }))
            }
        }
    }

    Ok(ReplayReport {
        applied,
        rolled_back,
        last_sequence,
        valid_bytes: scan.valid_bytes,
        truncated_tail,
    })
}

/// Discard a torn tail, so the next append starts from a frame boundary.
///
/// # Errors
/// Any storage error.
pub fn truncate_tail(storage: &dyn Storage, path: &DbPath, valid_bytes: u64) -> Result<()> {
    let Some(meta) = storage.metadata(path)? else {
        return Ok(());
    };
    if meta.len <= valid_bytes {
        return Ok(());
    }
    let mut file = storage.open_file(path, OpenMode::ReadWrite)?;
    file.truncate(valid_bytes)?;
    file.sync_data()
}
