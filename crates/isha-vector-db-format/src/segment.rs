//! Segment files: the four blocks that hold a collection's data.
//!
//! A segment is immutable once written, apart from its tombstone bitmap. That immutability is
//! what makes readers lock-free: a reader scanning a segment cannot race a writer, because
//! writers only ever create new segments.
//!
//! ```text
//! NNNNNN.vec   fixed-stride vectors     row r at DATA_OFFSET + r * stride
//! NNNNNN.dir   row directory            ids, metadata locations, cached norms
//! NNNNNN.meta  metadata and content     addressed by (offset, len) from the directory
//! NNNNNN.del   live bitmap              the only mutable part of a segment
//! ```
//!
//! # Why four files and not one
//!
//! `.vec` must contain nothing but floats, at a fixed stride, starting at an aligned offset, so
//! a brute-force scan is a straight sequential read with no per-row decoding. Interleaving
//! metadata would either break the stride or add an indirection to the hottest loop in the
//! engine.
//!
//! Isolating `.del` means deleting one document rewrites a few kilobytes rather than the whole
//! segment. On a backend that reports `prefers_few_large_files` (OPFS), the four are packed into
//! one file with a section footer; every reader here works from `(bytes, offset, len)` and does
//! not care which arrangement produced them.

use crate::block::{decode_block, encode_block, open_block, verify_block, TRAILER_LEN};
use crate::cursor::{Reader, Writer};
use crate::error::{FormatError, MalformedKind, Result};
use crate::header::{FileHeader, FileKind, HEADER_LEN};
use crate::value::Value;

/// Byte offset at which vector data begins in a `.vec` file.
///
/// 64 rather than 32 so the float array starts on a cache line. On a memory-mapped file the
/// mapping itself is page-aligned, so an offset of 64 lands 64-byte aligned in memory, which is
/// what the SIMD kernels want.
pub const VECTOR_DATA_OFFSET: usize = 64;

/// Bytes per directory entry.
pub const DIR_ENTRY_LEN: usize = 24;

/// Fixed part of the directory payload, before the entries.
const DIR_PREFIX_LEN: usize = 16;

// ---------------------------------------------------------------------------
// .vec — the vector block
// ---------------------------------------------------------------------------

/// Builds a `.vec` file.
#[derive(Debug)]
pub struct VectorBlockWriter {
    dimension: u32,
    stride: usize,
    rows: u32,
    data: Vec<u8>,
}

impl VectorBlockWriter {
    /// Start a block for vectors of `dimension` components at `stride` bytes each.
    ///
    /// # Errors
    /// [`MalformedKind::ZeroNotAllowed`] for a zero dimension or stride.
    pub fn new(dimension: u32, stride: usize) -> Result<Self> {
        if dimension == 0 {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::ZeroNotAllowed { field: "dimension" },
            });
        }
        if stride == 0 {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::ZeroNotAllowed { field: "stride" },
            });
        }
        Ok(Self {
            dimension,
            stride,
            rows: 0,
            data: Vec::new(),
        })
    }

    /// Append one row's raw bytes.
    ///
    /// # Errors
    /// [`MalformedKind::Inconsistent`] if the row is not exactly `stride` bytes.
    pub fn push_row(&mut self, row: &[u8]) -> Result<u32> {
        if row.len() != self.stride {
            return Err(FormatError::Malformed {
                offset: self.data.len() as u64,
                kind: MalformedKind::Inconsistent {
                    field: "row stride",
                },
            });
        }
        let index = self.rows;
        self.data.extend_from_slice(row);
        self.rows = self.rows.saturating_add(1);
        Ok(index)
    }

    /// Append one row of `f32` components.
    ///
    /// # Errors
    /// [`MalformedKind::Inconsistent`] for the wrong dimension, or
    /// [`MalformedKind::NonFiniteFloat`] for a NaN or infinite component.
    pub fn push_f32(&mut self, values: &[f32]) -> Result<u32> {
        if values.len() != self.dimension as usize {
            return Err(FormatError::Malformed {
                offset: self.data.len() as u64,
                kind: MalformedKind::Inconsistent { field: "dimension" },
            });
        }
        // Rejected here rather than downstream: one NaN component poisons every distance
        // computed against that row, and the resulting "search returns nothing sensible" is
        // very hard to trace back to the insert that caused it.
        if let Some(pos) = values.iter().position(|v| !v.is_finite()) {
            return Err(FormatError::Malformed {
                offset: pos as u64,
                kind: MalformedKind::NonFiniteFloat,
            });
        }
        let mut row = Vec::with_capacity(self.stride);
        for v in values {
            row.extend_from_slice(&v.to_le_bytes());
        }
        self.push_row(&row)
    }

    /// Rows written so far.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Finish the file.
    pub fn finish(self) -> Vec<u8> {
        let mut w = Writer::with_capacity(VECTOR_DATA_OFFSET + self.data.len() + TRAILER_LEN);
        // payload_len counts the padding as well as the data, so the header alone describes the
        // whole extent of the file.
        let payload_len = (VECTOR_DATA_OFFSET - HEADER_LEN as usize) + self.data.len();
        FileHeader::new(FileKind::Vectors, payload_len as u64).write_to(&mut w);
        w.align_to(VECTOR_DATA_OFFSET);
        w.raw(&self.data);
        let crc = crate::crc32c(w.as_slice().get(HEADER_LEN as usize..).unwrap_or(&[]));
        w.u32(crc);
        w.finish()
    }
}

/// Read-only view over a `.vec` file.
///
/// Opening does not verify the payload checksum — see [`open_block`] — because that would mean
/// reading the entire file, which for a large collection is the whole database.
#[derive(Debug, Clone, Copy)]
pub struct VectorBlock<'a> {
    data: &'a [u8],
    stride: usize,
    rows: u32,
}

impl<'a> VectorBlock<'a> {
    /// Open a `.vec` file whose rows are `stride` bytes each.
    ///
    /// # Errors
    /// Any [`FormatError`]; in particular [`MalformedKind::Inconsistent`] if the data length is
    /// not a whole number of rows.
    pub fn open(bytes: &'a [u8], stride: usize) -> Result<Self> {
        if stride == 0 {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::ZeroNotAllowed { field: "stride" },
            });
        }
        let payload = open_block(bytes, FileKind::Vectors)?;
        let pad = VECTOR_DATA_OFFSET - HEADER_LEN as usize;
        let data = payload.get(pad..).ok_or(FormatError::Truncated {
            offset: HEADER_LEN.into(),
            needed: pad as u64,
            available: payload.len() as u64,
        })?;
        if data.len() % stride != 0 {
            return Err(FormatError::Malformed {
                offset: VECTOR_DATA_OFFSET as u64,
                kind: MalformedKind::Inconsistent {
                    field: "vector data length",
                },
            });
        }
        let rows = u32::try_from(data.len() / stride).map_err(|_| FormatError::Malformed {
            offset: 0,
            kind: MalformedKind::Inconsistent { field: "row count" },
        })?;
        Ok(Self { data, stride, rows })
    }

    /// Verify the block's checksum. Called by `verify(Checksums)`, not on open.
    ///
    /// # Errors
    /// [`FormatError::ChecksumMismatch`].
    pub fn verify(bytes: &[u8]) -> Result<()> {
        verify_block(bytes, FileKind::Vectors)
    }

    /// Rows in the block.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Whether the block holds no rows.
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Raw bytes of one row, or `None` if the index is out of range.
    ///
    /// Bytes rather than `&[f32]`: converting without a copy needs a pointer cast, and this
    /// crate forbids `unsafe`. The index crate, where `unsafe` is permitted and audited, does
    /// the aligned cast for its SIMD kernels.
    pub fn row(&self, index: u32) -> Option<&'a [u8]> {
        let start = (index as usize).checked_mul(self.stride)?;
        let end = start.checked_add(self.stride)?;
        self.data.get(start..end)
    }

    /// One row decoded to `f32`, for the scalar reference path and for tests.
    pub fn row_f32(&self, index: u32) -> Option<Vec<f32>> {
        let row = self.row(index)?;
        Some(
            row.chunks_exact(4)
                .filter_map(|c| <[u8; 4]>::try_from(c).ok())
                .map(f32::from_le_bytes)
                .collect(),
        )
    }

    /// The whole data region, for a sequential scan.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
}

// ---------------------------------------------------------------------------
// .dir — the row directory
// ---------------------------------------------------------------------------

/// One row's directory entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowEntry {
    /// Offset of this row's record within the `.meta` file.
    pub meta_offset: u64,
    /// Length of that record; zero when the row has neither metadata nor content.
    pub meta_len: u32,
    /// Cached reciprocal of the vector's L2 norm, so cosine is a dot product and two
    /// multiplies. Zero for a zero-length vector.
    pub inv_norm: f32,
    /// Offset of the id within the directory's id heap.
    pub id_offset: u32,
    /// Length of the id in bytes.
    pub id_len: u16,
    /// Reserved per-row flags.
    pub flags: u16,
}

/// Builds a `.dir` file.
#[derive(Debug, Default)]
pub struct DirectoryWriter {
    entries: Vec<RowEntry>,
    heap: Vec<u8>,
}

impl DirectoryWriter {
    /// An empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a row.
    ///
    /// # Errors
    /// [`MalformedKind::Inconsistent`] if the id is longer than `u16::MAX` or the heap would
    /// overflow `u32`.
    pub fn push(
        &mut self,
        id: &[u8],
        meta_offset: u64,
        meta_len: u32,
        inv_norm: f32,
    ) -> Result<u32> {
        let id_len = u16::try_from(id.len()).map_err(|_| FormatError::Malformed {
            offset: self.heap.len() as u64,
            kind: MalformedKind::Inconsistent { field: "id length" },
        })?;
        let id_offset = u32::try_from(self.heap.len()).map_err(|_| FormatError::Malformed {
            offset: self.heap.len() as u64,
            kind: MalformedKind::Inconsistent {
                field: "id heap size",
            },
        })?;
        if !inv_norm.is_finite() {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::NonFiniteFloat,
            });
        }
        self.heap.extend_from_slice(id);
        let index = u32::try_from(self.entries.len()).map_err(|_| FormatError::Malformed {
            offset: 0,
            kind: MalformedKind::Inconsistent { field: "row count" },
        })?;
        self.entries.push(RowEntry {
            meta_offset,
            meta_len,
            inv_norm,
            id_offset,
            id_len,
            flags: 0,
        });
        Ok(index)
    }

    /// Rows written so far.
    pub fn rows(&self) -> usize {
        self.entries.len()
    }

    /// Finish the file.
    pub fn finish(self) -> Vec<u8> {
        let row_count = self.entries.len() as u32;
        let heap_offset = (DIR_PREFIX_LEN + self.entries.len() * DIR_ENTRY_LEN) as u32;
        let mut w = Writer::with_capacity(heap_offset as usize + self.heap.len());
        w.u32(row_count).u32(heap_offset).reserved(8);
        for e in &self.entries {
            w.u64(e.meta_offset)
                .u32(e.meta_len)
                .f32(e.inv_norm)
                .u32(e.id_offset)
                .u16(e.id_len)
                .u16(e.flags);
        }
        w.raw(&self.heap);
        encode_block(FileKind::Directory, w.as_slice())
    }
}

/// Read-only view over a `.dir` file.
#[derive(Debug, Clone, Copy)]
pub struct Directory<'a> {
    entries: &'a [u8],
    heap: &'a [u8],
    rows: u32,
}

impl<'a> Directory<'a> {
    /// Open and fully validate a `.dir` file.
    ///
    /// The directory is small — 24 bytes per row — so its checksum *is* verified on open, and
    /// every entry's id range is bounds-checked here. That costs a linear pass once and means
    /// [`Directory::id`] can never fail afterwards, which removes an error path from the hot
    /// read loop.
    ///
    /// # Errors
    /// Any [`FormatError`].
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        let payload = decode_block(bytes, FileKind::Directory)?;
        let mut r = Reader::new(payload);
        let rows = r.u32()?;
        let heap_offset = r.u32()?;
        r.reserved(8)?;

        let expected_heap = DIR_PREFIX_LEN
            .checked_add((rows as usize).checked_mul(DIR_ENTRY_LEN).ok_or(
                FormatError::Malformed {
                    offset: 0,
                    kind: MalformedKind::Inconsistent { field: "row count" },
                },
            )?)
            .ok_or(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::Inconsistent { field: "row count" },
            })?;
        if heap_offset as usize != expected_heap {
            return Err(FormatError::Malformed {
                offset: 4,
                kind: MalformedKind::Inconsistent {
                    field: "heap_offset",
                },
            });
        }
        let entries =
            payload
                .get(DIR_PREFIX_LEN..expected_heap)
                .ok_or(FormatError::LengthExceedsInput {
                    offset: 0,
                    claimed: expected_heap as u64,
                    available: payload.len() as u64,
                })?;
        let heap = payload.get(expected_heap..).ok_or(FormatError::Truncated {
            offset: expected_heap as u64,
            needed: expected_heap as u64,
            available: payload.len() as u64,
        })?;

        let dir = Self {
            entries,
            heap,
            rows,
        };
        for i in 0..rows {
            let entry = dir.entry(i).ok_or(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::Inconsistent {
                    field: "entry table",
                },
            })?;
            let start = entry.id_offset as usize;
            let end = start
                .checked_add(entry.id_len as usize)
                .ok_or(FormatError::Malformed {
                    offset: 0,
                    kind: MalformedKind::Inconsistent { field: "id range" },
                })?;
            if end > heap.len() {
                return Err(FormatError::LengthExceedsInput {
                    offset: (DIR_PREFIX_LEN + i as usize * DIR_ENTRY_LEN) as u64,
                    claimed: end as u64,
                    available: heap.len() as u64,
                });
            }
        }
        Ok(dir)
    }

    /// Rows in the directory.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Whether the directory holds no rows.
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// One row's entry, or `None` if the index is out of range.
    pub fn entry(&self, index: u32) -> Option<RowEntry> {
        let start = (index as usize).checked_mul(DIR_ENTRY_LEN)?;
        let slice = self.entries.get(start..start.checked_add(DIR_ENTRY_LEN)?)?;
        let mut r = Reader::new(slice);
        Some(RowEntry {
            meta_offset: r.u64().ok()?,
            meta_len: r.u32().ok()?,
            // A corrupt non-finite norm would silently poison every cosine score, so it is
            // rejected here rather than propagated.
            inv_norm: r.f32().ok()?,
            id_offset: r.u32().ok()?,
            id_len: r.u16().ok()?,
            flags: r.u16().ok()?,
        })
    }

    /// One row's id. Always succeeds for an in-range index, because [`Directory::open`]
    /// validated every range.
    pub fn id(&self, index: u32) -> Option<&'a [u8]> {
        let entry = self.entry(index)?;
        let start = entry.id_offset as usize;
        self.heap
            .get(start..start.checked_add(entry.id_len as usize)?)
    }
}

// ---------------------------------------------------------------------------
// .meta — metadata and content records
// ---------------------------------------------------------------------------

/// One row's metadata and optional content.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MetaRecord {
    /// The metadata map, or `None` when the document has none.
    pub metadata: Option<Value>,
    /// Optional opaque payload, such as the source text the vector was derived from.
    pub content: Option<Vec<u8>>,
}

impl MetaRecord {
    /// Whether this record holds nothing, in which case the directory stores a zero length and
    /// no bytes are written at all.
    pub fn is_empty(&self) -> bool {
        self.metadata.is_none() && self.content.is_none()
    }

    /// Encode the record.
    ///
    /// # Errors
    /// Any [`FormatError`] from encoding the metadata value.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        let mut flags = 0u8;
        if self.metadata.is_some() {
            flags |= 1;
        }
        if self.content.is_some() {
            flags |= 2;
        }
        w.u8(flags);
        if let Some(v) = &self.metadata {
            let encoded = v.encode()?;
            w.blob(&encoded);
        }
        if let Some(c) = &self.content {
            w.blob(c);
        }
        Ok(w.finish())
    }

    /// Decode a record from a `(offset, len)` slice of the metadata file.
    ///
    /// # Errors
    /// Any [`FormatError`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::default());
        }
        let mut r = Reader::new(bytes);
        let flags = r.u8()?;
        if flags & !0b11 != 0 {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::UnknownFlags(u16::from(flags)),
            });
        }
        let metadata = if flags & 1 != 0 {
            Some(Value::decode(r.blob()?)?)
        } else {
            None
        };
        let content = if flags & 2 != 0 {
            Some(r.blob()?.to_vec())
        } else {
            None
        };
        r.expect_end("meta record")?;
        Ok(Self { metadata, content })
    }
}

/// Builds a `.meta` file, returning each record's `(offset, len)` for the directory.
#[derive(Debug, Default)]
pub struct MetaWriter {
    payload: Vec<u8>,
}

impl MetaWriter {
    /// An empty metadata file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a record, returning where it landed.
    ///
    /// An empty record occupies no bytes: most documents in a vector database have modest
    /// metadata and some have none, and paying a byte per empty record across a million rows is
    /// avoidable.
    ///
    /// # Errors
    /// Any [`FormatError`] from encoding.
    pub fn push(&mut self, record: &MetaRecord) -> Result<(u64, u32)> {
        if record.is_empty() {
            return Ok((self.payload.len() as u64, 0));
        }
        let encoded = record.encode()?;
        let offset = self.payload.len() as u64;
        let len = u32::try_from(encoded.len()).map_err(|_| FormatError::Malformed {
            offset,
            kind: MalformedKind::Inconsistent {
                field: "record length",
            },
        })?;
        self.payload.extend_from_slice(&encoded);
        Ok((offset, len))
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    /// Finish the file.
    pub fn finish(self) -> Vec<u8> {
        encode_block(FileKind::Metadata, &self.payload)
    }
}

/// Read-only view over a `.meta` file.
#[derive(Debug, Clone, Copy)]
pub struct MetaBlock<'a> {
    payload: &'a [u8],
}

impl<'a> MetaBlock<'a> {
    /// Open a `.meta` file without verifying its checksum; it can be large.
    ///
    /// # Errors
    /// Any [`FormatError`] from the header.
    pub fn open(bytes: &'a [u8]) -> Result<Self> {
        Ok(Self {
            payload: open_block(bytes, FileKind::Metadata)?,
        })
    }

    /// Wrap an already-extracted payload.
    ///
    /// A caller that reads a segment once and then serves many rows from it — which is every
    /// filtered scan — should not re-validate the file header per row.
    pub fn from_payload(payload: &'a [u8]) -> Self {
        Self { payload }
    }

    /// The raw payload, for a caller that wants to hold it directly.
    pub fn payload(&self) -> &'a [u8] {
        self.payload
    }

    /// Verify the file's checksum.
    ///
    /// # Errors
    /// [`FormatError::ChecksumMismatch`].
    pub fn verify(bytes: &[u8]) -> Result<()> {
        verify_block(bytes, FileKind::Metadata)
    }

    /// The encoded metadata map for a row, without decoding it.
    ///
    /// What a filtered scan wants: the bytes, so only the fields the filter names get decoded.
    ///
    /// # Errors
    /// [`FormatError::LengthExceedsInput`] if the entry points outside the file.
    pub fn metadata_bytes(&self, entry: &RowEntry) -> Result<Option<&'a [u8]>> {
        if entry.meta_len == 0 {
            return Ok(None);
        }
        let slice = self.slice_for(entry)?;
        let mut r = Reader::new(slice);
        let flags = r.u8()?;
        if flags & !0b11 != 0 {
            return Err(FormatError::Malformed {
                offset: entry.meta_offset,
                kind: MalformedKind::UnknownFlags(u16::from(flags)),
            });
        }
        if flags & 1 == 0 {
            return Ok(None);
        }
        Ok(Some(r.blob()?))
    }

    /// The record's raw bytes within the block.
    fn slice_for(&self, entry: &RowEntry) -> Result<&'a [u8]> {
        let start =
            usize::try_from(entry.meta_offset).map_err(|_| FormatError::LengthExceedsInput {
                offset: entry.meta_offset,
                claimed: entry.meta_offset,
                available: self.payload.len() as u64,
            })?;
        let end =
            start
                .checked_add(entry.meta_len as usize)
                .ok_or(FormatError::LengthExceedsInput {
                    offset: entry.meta_offset,
                    claimed: u64::from(entry.meta_len),
                    available: self.payload.len() as u64,
                })?;
        self.payload
            .get(start..end)
            .ok_or(FormatError::LengthExceedsInput {
                offset: entry.meta_offset,
                claimed: end as u64,
                available: self.payload.len() as u64,
            })
    }

    /// Read the record a directory entry points at.
    ///
    /// # Errors
    /// [`FormatError::LengthExceedsInput`] if the entry points outside the file, which means
    /// the directory and the metadata file disagree.
    pub fn record(&self, entry: &RowEntry) -> Result<MetaRecord> {
        if entry.meta_len == 0 {
            return Ok(MetaRecord::default());
        }
        let start =
            usize::try_from(entry.meta_offset).map_err(|_| FormatError::LengthExceedsInput {
                offset: entry.meta_offset,
                claimed: entry.meta_offset,
                available: self.payload.len() as u64,
            })?;
        let end =
            start
                .checked_add(entry.meta_len as usize)
                .ok_or(FormatError::LengthExceedsInput {
                    offset: entry.meta_offset,
                    claimed: u64::from(entry.meta_len),
                    available: self.payload.len() as u64,
                })?;
        let slice = self
            .payload
            .get(start..end)
            .ok_or(FormatError::LengthExceedsInput {
                offset: entry.meta_offset,
                claimed: end as u64,
                available: self.payload.len() as u64,
            })?;
        MetaRecord::decode(slice)
    }
}

// ---------------------------------------------------------------------------
// .del — the tombstone bitmap
// ---------------------------------------------------------------------------

/// A segment's live-row bitmap: bit `r` set means row `r` is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstones {
    /// Rows the bitmap covers.
    pub rows: u32,
    /// Bumped on every rewrite, so a stale `.del` left by an interrupted write is detectable
    /// against the generation the manifest recorded.
    pub generation: u32,
    /// Packed bits, 64 rows per word, little-endian on disk.
    pub words: Vec<u64>,
}

impl Tombstones {
    /// All rows live.
    pub fn all_live(rows: u32, generation: u32) -> Self {
        let mut words = vec![u64::MAX; (rows as usize).div_ceil(64)];
        let used = rows % 64;
        if used != 0 {
            if let Some(last) = words.last_mut() {
                *last &= (1u64 << used) - 1;
            }
        }
        Self {
            rows,
            generation,
            words,
        }
    }

    /// Encode a `.del` file.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(16 + self.words.len() * 8);
        w.u32(self.rows).u32(self.generation).reserved(8);
        for word in &self.words {
            w.u64(*word);
        }
        encode_block(FileKind::Deleted, w.as_slice())
    }

    /// Decode a `.del` file.
    ///
    /// # Errors
    /// Any [`FormatError`]; in particular [`MalformedKind::Inconsistent`] if the word count does
    /// not match the row count, or if bits are set beyond the last row.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let payload = decode_block(bytes, FileKind::Deleted)?;
        let mut r = Reader::new(payload);
        let rows = r.u32()?;
        let generation = r.u32()?;
        r.reserved(8)?;

        let expected_words = (rows as usize).div_ceil(64);
        if r.remaining() != expected_words * 8 {
            return Err(FormatError::Malformed {
                offset: 16,
                kind: MalformedKind::Inconsistent {
                    field: "bitmap length",
                },
            });
        }
        let mut words = Vec::with_capacity(expected_words);
        for _ in 0..expected_words {
            words.push(r.u64()?);
        }
        // Bits past the final row must be clear, or the live count derived from a popcount
        // would over-report and the collection would claim documents it does not have.
        let used = rows % 64;
        if used != 0 {
            if let Some(last) = words.last() {
                if last & !((1u64 << used) - 1) != 0 {
                    return Err(FormatError::Malformed {
                        offset: 16,
                        kind: MalformedKind::Inconsistent {
                            field: "bitmap tail",
                        },
                    });
                }
            }
        }
        r.expect_end("tombstones")?;
        Ok(Self {
            rows,
            generation,
            words,
        })
    }

    /// Whether row `index` is live.
    pub fn is_live(&self, index: u32) -> bool {
        if index >= self.rows {
            return false;
        }
        let word = (index / 64) as usize;
        match self.words.get(word) {
            Some(w) => w & (1u64 << (index % 64)) != 0,
            None => false,
        }
    }

    /// Mark a row dead, returning whether it changed.
    pub fn kill(&mut self, index: u32) -> bool {
        if index >= self.rows {
            return false;
        }
        let word = (index / 64) as usize;
        let Some(w) = self.words.get_mut(word) else {
            return false;
        };
        let mask = 1u64 << (index % 64);
        if *w & mask == 0 {
            return false;
        }
        *w &= !mask;
        true
    }

    /// Live rows.
    pub fn live_count(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // ---- .vec ----

    #[test]
    fn vector_block_round_trips() {
        let mut w = VectorBlockWriter::new(3, 12).unwrap();
        w.push_f32(&[1.0, 2.0, 3.0]).unwrap();
        w.push_f32(&[-1.5, 0.0, 2.5]).unwrap();
        assert_eq!(w.rows(), 2);
        let bytes = w.finish();

        let block = VectorBlock::open(&bytes, 12).unwrap();
        assert_eq!(block.rows(), 2);
        assert_eq!(block.row_f32(0).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(block.row_f32(1).unwrap(), vec![-1.5, 0.0, 2.5]);
        assert!(block.row(2).is_none());
        VectorBlock::verify(&bytes).unwrap();
    }

    #[test]
    fn vector_data_starts_at_a_cache_line_boundary() {
        let mut w = VectorBlockWriter::new(1, 4).unwrap();
        w.push_f32(&[1.0]).unwrap();
        let bytes = w.finish();
        assert_eq!(
            &bytes[VECTOR_DATA_OFFSET..VECTOR_DATA_OFFSET + 4],
            &1.0f32.to_le_bytes()
        );
        assert!(bytes[HEADER_LEN as usize..VECTOR_DATA_OFFSET]
            .iter()
            .all(|&b| b == 0));
    }

    #[test]
    fn an_empty_vector_block_is_valid() {
        let bytes = VectorBlockWriter::new(4, 16).unwrap().finish();
        let block = VectorBlock::open(&bytes, 16).unwrap();
        assert_eq!(block.rows(), 0);
        assert!(block.is_empty());
        assert!(block.row(0).is_none());
    }

    #[test]
    fn non_finite_components_cannot_be_written() {
        let mut w = VectorBlockWriter::new(2, 8).unwrap();
        assert!(matches!(
            w.push_f32(&[1.0, f32::NAN]),
            Err(FormatError::Malformed {
                kind: MalformedKind::NonFiniteFloat,
                offset: 1
            })
        ));
        assert!(w.push_f32(&[1.0, f32::INFINITY]).is_err());
        assert_eq!(w.rows(), 0, "a rejected row must not be partially written");
    }

    #[test]
    fn the_wrong_dimension_is_refused() {
        let mut w = VectorBlockWriter::new(3, 12).unwrap();
        assert!(w.push_f32(&[1.0, 2.0]).is_err());
        assert!(w.push_f32(&[1.0, 2.0, 3.0, 4.0]).is_err());
        assert!(w.push_row(&[0u8; 11]).is_err());
    }

    #[test]
    fn a_zero_dimension_or_stride_is_refused() {
        assert!(VectorBlockWriter::new(0, 4).is_err());
        assert!(VectorBlockWriter::new(4, 0).is_err());
        let bytes = VectorBlockWriter::new(1, 4).unwrap().finish();
        assert!(VectorBlock::open(&bytes, 0).is_err());
    }

    /// A stride that does not divide the data means the file and the catalog disagree; reading
    /// it anyway would silently return misaligned garbage as vectors.
    #[test]
    fn a_stride_mismatch_is_detected_rather_than_returning_garbage() {
        let mut w = VectorBlockWriter::new(3, 12).unwrap();
        w.push_f32(&[1.0, 2.0, 3.0]).unwrap();
        let bytes = w.finish();
        assert!(matches!(
            VectorBlock::open(&bytes, 8),
            Err(FormatError::Malformed {
                kind: MalformedKind::Inconsistent {
                    field: "vector data length"
                },
                ..
            })
        ));
    }

    #[test]
    fn vector_block_corruption_is_caught_by_verify_but_not_by_open() {
        let mut w = VectorBlockWriter::new(2, 8).unwrap();
        w.push_f32(&[1.0, 2.0]).unwrap();
        let mut bytes = w.finish();
        bytes[VECTOR_DATA_OFFSET] ^= 0xFF;
        assert!(VectorBlock::open(&bytes, 8).is_ok(), "open must stay cheap");
        assert!(VectorBlock::verify(&bytes).is_err(), "verify must catch it");
    }

    // ---- .dir ----

    #[test]
    fn directory_round_trips() {
        let mut w = DirectoryWriter::new();
        w.push(b"doc-one", 0, 10, 0.5).unwrap();
        w.push(b"", 10, 0, 1.0).unwrap();
        w.push(b"a-much-longer-document-id", 10, 42, 0.25).unwrap();
        assert_eq!(w.rows(), 3);
        let bytes = w.finish();

        let dir = Directory::open(&bytes).unwrap();
        assert_eq!(dir.rows(), 3);
        assert_eq!(dir.id(0).unwrap(), b"doc-one");
        assert_eq!(dir.id(1).unwrap(), b"");
        assert_eq!(dir.id(2).unwrap(), b"a-much-longer-document-id");
        let e = dir.entry(2).unwrap();
        assert_eq!(e.meta_offset, 10);
        assert_eq!(e.meta_len, 42);
        assert_eq!(e.inv_norm, 0.25);
        assert!(dir.entry(3).is_none());
        assert!(dir.id(3).is_none());
    }

    #[test]
    fn an_empty_directory_is_valid() {
        let bytes = DirectoryWriter::new().finish();
        let dir = Directory::open(&bytes).unwrap();
        assert_eq!(dir.rows(), 0);
        assert!(dir.is_empty());
        assert!(dir.entry(0).is_none());
    }

    #[test]
    fn a_non_finite_norm_cannot_be_written() {
        let mut w = DirectoryWriter::new();
        assert!(w.push(b"x", 0, 0, f32::NAN).is_err());
    }

    /// A corrupt id range must be caught when the directory is opened, so later reads cannot
    /// hand back bytes from an unrelated id.
    #[test]
    fn an_id_range_past_the_heap_is_rejected_on_open() {
        let mut w = DirectoryWriter::new();
        w.push(b"short", 0, 0, 1.0).unwrap();
        let mut bytes = w.finish();
        // id_len lives at entry offset 20 within the entry table.
        let id_len_at = HEADER_LEN as usize + DIR_PREFIX_LEN + 20;
        bytes[id_len_at..id_len_at + 2].copy_from_slice(&9999u16.to_le_bytes());
        let payload_len = bytes.len() - HEADER_LEN as usize - TRAILER_LEN;
        let crc = crate::crc32c(&bytes[HEADER_LEN as usize..HEADER_LEN as usize + payload_len]);
        let end = bytes.len();
        bytes[end - 4..].copy_from_slice(&crc.to_le_bytes());

        assert!(matches!(
            Directory::open(&bytes),
            Err(FormatError::LengthExceedsInput { .. })
        ));
    }

    #[test]
    fn a_row_count_that_disagrees_with_the_heap_offset_is_rejected() {
        let mut w = DirectoryWriter::new();
        w.push(b"a", 0, 0, 1.0).unwrap();
        let mut bytes = w.finish();
        let rows_at = HEADER_LEN as usize;
        bytes[rows_at..rows_at + 4].copy_from_slice(&5u32.to_le_bytes());
        let payload_len = bytes.len() - HEADER_LEN as usize - TRAILER_LEN;
        let crc = crate::crc32c(&bytes[HEADER_LEN as usize..HEADER_LEN as usize + payload_len]);
        let end = bytes.len();
        bytes[end - 4..].copy_from_slice(&crc.to_le_bytes());

        assert!(matches!(
            Directory::open(&bytes),
            Err(FormatError::Malformed {
                kind: MalformedKind::Inconsistent {
                    field: "heap_offset"
                },
                ..
            })
        ));
    }

    #[test]
    fn a_directory_claiming_a_billion_rows_is_refused_before_allocating() {
        let mut w = Writer::new();
        // A row count whose entry table alone would be 24 GB.
        let rows: u32 = 1_000_000_000;
        w.u32(rows).u32(u32::MAX).reserved(8);
        let bytes = encode_block(FileKind::Directory, w.as_slice());
        assert!(Directory::open(&bytes).is_err());
    }

    // ---- .meta ----

    #[test]
    fn meta_records_round_trip() {
        let mut meta = BTreeMap::new();
        meta.insert("category".to_owned(), Value::Str("tools".into()));
        meta.insert("price".to_owned(), Value::F64(9.99));

        let records = [
            MetaRecord::default(),
            MetaRecord {
                metadata: Some(Value::Map(meta)),
                content: None,
            },
            MetaRecord {
                metadata: None,
                content: Some(b"just the source text".to_vec()),
            },
            MetaRecord {
                metadata: Some(Value::Map(BTreeMap::new())),
                content: Some(b"both".to_vec()),
            },
        ];

        let mut w = MetaWriter::new();
        let locations: Vec<(u64, u32)> = records.iter().map(|r| w.push(r).unwrap()).collect();
        let bytes = w.finish();

        let block = MetaBlock::open(&bytes).unwrap();
        for (record, (offset, len)) in records.iter().zip(locations) {
            let entry = RowEntry {
                meta_offset: offset,
                meta_len: len,
                inv_norm: 1.0,
                id_offset: 0,
                id_len: 0,
                flags: 0,
            };
            assert_eq!(&block.record(&entry).unwrap(), record);
        }
        MetaBlock::verify(&bytes).unwrap();
    }

    #[test]
    fn an_empty_record_occupies_no_bytes() {
        let mut w = MetaWriter::new();
        let (_, len) = w.push(&MetaRecord::default()).unwrap();
        assert_eq!(len, 0);
        assert!(w.is_empty());
    }

    /// The directory and the metadata file are separate files; if they disagree, say so rather
    /// than returning whatever bytes happen to be at that offset.
    #[test]
    fn a_record_pointing_outside_the_file_is_an_error() {
        let mut w = MetaWriter::new();
        w.push(&MetaRecord {
            metadata: None,
            content: Some(b"x".to_vec()),
        })
        .unwrap();
        let bytes = w.finish();
        let block = MetaBlock::open(&bytes).unwrap();

        let entry = RowEntry {
            meta_offset: 9_000,
            meta_len: 10,
            inv_norm: 1.0,
            id_offset: 0,
            id_len: 0,
            flags: 0,
        };
        assert!(matches!(
            block.record(&entry),
            Err(FormatError::LengthExceedsInput { .. })
        ));
    }

    #[test]
    fn unknown_record_flags_are_rejected() {
        assert!(matches!(
            MetaRecord::decode(&[0b1000_0000]),
            Err(FormatError::Malformed {
                kind: MalformedKind::UnknownFlags(_),
                ..
            })
        ));
    }

    // ---- .del ----

    #[test]
    fn tombstones_round_trip() {
        let mut t = Tombstones::all_live(200, 3);
        assert_eq!(t.live_count(), 200);
        assert!(t.kill(0));
        assert!(t.kill(199));
        assert!(!t.kill(199), "killing twice reports no change");
        assert_eq!(t.live_count(), 198);

        let bytes = t.encode();
        let decoded = Tombstones::decode(&bytes).unwrap();
        assert_eq!(decoded, t);
        assert!(!decoded.is_live(0));
        assert!(decoded.is_live(1));
        assert!(!decoded.is_live(199));
        assert!(
            !decoded.is_live(200),
            "out of range reads as dead, not a panic"
        );
        assert_eq!(decoded.generation, 3);
    }

    #[test]
    fn all_live_ignores_bits_past_the_last_row() {
        for rows in [0u32, 1, 63, 64, 65, 127, 128, 129, 1000] {
            let t = Tombstones::all_live(rows, 0);
            assert_eq!(t.live_count(), rows, "rows = {rows}");
            let decoded = Tombstones::decode(&t.encode()).unwrap();
            assert_eq!(decoded.live_count(), rows);
        }
    }

    /// A corrupt tail would make the collection claim documents it does not have.
    #[test]
    fn bits_set_past_the_last_row_are_rejected() {
        let t = Tombstones {
            rows: 10,
            generation: 0,
            words: vec![u64::MAX],
        };
        assert!(matches!(
            Tombstones::decode(&t.encode()),
            Err(FormatError::Malformed {
                kind: MalformedKind::Inconsistent {
                    field: "bitmap tail"
                },
                ..
            })
        ));
    }

    #[test]
    fn a_word_count_that_disagrees_with_the_row_count_is_rejected() {
        let t = Tombstones {
            rows: 200,
            generation: 0,
            words: vec![0; 1],
        };
        assert!(matches!(
            Tombstones::decode(&t.encode()),
            Err(FormatError::Malformed {
                kind: MalformedKind::Inconsistent {
                    field: "bitmap length"
                },
                ..
            })
        ));
    }

    #[test]
    fn every_block_kind_rejects_truncation_at_every_length() {
        let mut vw = VectorBlockWriter::new(2, 8).unwrap();
        vw.push_f32(&[1.0, 2.0]).unwrap();
        let vec_bytes = vw.finish();

        let mut dw = DirectoryWriter::new();
        dw.push(b"id", 0, 0, 1.0).unwrap();
        let dir_bytes = dw.finish();

        let mut mw = MetaWriter::new();
        mw.push(&MetaRecord {
            metadata: None,
            content: Some(b"c".to_vec()),
        })
        .unwrap();
        let meta_bytes = mw.finish();

        let del_bytes = Tombstones::all_live(100, 1).encode();

        for len in 0..vec_bytes.len() {
            assert!(
                VectorBlock::open(&vec_bytes[..len], 8).is_err(),
                "vec at {len}"
            );
        }
        for len in 0..dir_bytes.len() {
            assert!(Directory::open(&dir_bytes[..len]).is_err(), "dir at {len}");
        }
        for len in 0..meta_bytes.len() {
            assert!(
                MetaBlock::open(&meta_bytes[..len]).is_err(),
                "meta at {len}"
            );
        }
        for len in 0..del_bytes.len() {
            assert!(
                Tombstones::decode(&del_bytes[..len]).is_err(),
                "del at {len}"
            );
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x0BAD_F00D_1234_5678u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 128) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = VectorBlock::open(&bytes, 8);
            if let Ok(d) = Directory::open(&bytes) {
                for i in 0..d.rows().min(64) {
                    let _ = d.id(i);
                    let _ = d.entry(i);
                }
            }
            if let Ok(m) = MetaBlock::open(&bytes) {
                let entry = RowEntry {
                    meta_offset: seed % 256,
                    meta_len: (seed % 64) as u32,
                    inv_norm: 1.0,
                    id_offset: 0,
                    id_len: 0,
                    flags: 0,
                };
                let _ = m.record(&entry);
            }
            let _ = Tombstones::decode(&bytes);
            let _ = MetaRecord::decode(&bytes);
        }
    }
}
