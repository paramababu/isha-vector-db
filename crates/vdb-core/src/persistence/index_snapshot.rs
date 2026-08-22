//! Storing an index's snapshot on disk.
//!
//! See [`IndexSnapshots`] for why this is allowed to be as casual as it is: a snapshot is a
//! cache of something the segments already contain, so every failure mode here — a missing file,
//! a truncated one, a checksum that does not match, a payload from a build that structured
//! things differently — resolves to "there is no snapshot", and the index rebuilds.

use std::sync::Arc;

use vdb_format::{encode_block, open_block, FileKind};

use crate::error::Result;
use crate::index::IndexSnapshots;
use crate::persistence::layout;
use crate::storage::{OpenMode, Storage};

/// One collection's snapshot slot, backed by storage.
#[derive(Debug)]
pub struct StoredSnapshots {
    storage: Arc<dyn Storage>,
    collection: String,
    kind: &'static str,
}

impl StoredSnapshots {
    /// The slot for `collection`'s index of the given `kind`.
    ///
    /// One file per kind, overwritten in place. There is no generation number and no rename
    /// dance, because there is nothing to protect: a half-written snapshot fails its checksum
    /// and is discarded, which is the same outcome as not having one.
    pub fn new(storage: Arc<dyn Storage>, collection: &str, kind: &'static str) -> Self {
        Self {
            storage,
            collection: collection.to_owned(),
            kind,
        }
    }
}

impl IndexSnapshots for StoredSnapshots {
    fn load(&self) -> Result<Option<Vec<u8>>> {
        let Ok(path) = layout::index_file(&self.collection, self.kind, 0) else {
            return Ok(None);
        };
        let Ok(file) = self.storage.open_file(&path, OpenMode::Read) else {
            return Ok(None);
        };
        let len = match file.len() {
            Ok(n) if n > 0 => n,
            _ => return Ok(None),
        };
        // A snapshot far larger than expected is refused rather than allocated: it is a cache,
        // and no cache is worth an out-of-memory abort.
        const MAX: u64 = 1 << 32;
        if len > MAX {
            return Ok(None);
        }
        let mut bytes = vec![0u8; len as usize];
        if file.read_exact_at(&mut bytes, 0, &path).is_err() {
            return Ok(None);
        }
        // Verified here rather than trusted, because everything downstream indexes into it.
        match open_block(&bytes, FileKind::Index) {
            Ok(payload) => Ok(Some(payload.to_vec())),
            Err(_) => Ok(None),
        }
    }

    fn store(&self, bytes: &[u8]) -> Result<()> {
        let dir = layout::index_dir(&self.collection)?;
        self.storage.create_dir_all(&dir)?;
        let path = layout::index_file(&self.collection, self.kind, 0)?;
        let framed = encode_block(FileKind::Index, bytes);
        let mut file = self.storage.open_file(&path, OpenMode::Create)?;
        // Truncate first: a new snapshot shorter than the old one would otherwise leave a tail
        // of stale bytes, and the length in the header would disagree with the file.
        file.truncate(0)?;
        file.write_at(&framed, 0)?;
        file.sync_data()?;
        Ok(())
    }
}
