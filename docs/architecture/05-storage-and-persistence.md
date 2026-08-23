# 5. Storage Architecture & Persistence Format

## 5.1 Design forces

The format is shaped by five constraints, in priority order:

1. **Crash safety on mobile.** iOS jetsam and Android LMK kill processes with no warning and no
   chance to run cleanup. "Save on close" is not a durability strategy. Recovery must be automatic
   and must never require the user to delete their data.
2. **Brute-force scan speed.** The flat index reads every live vector. It must read them as one
   contiguous, aligned `f32` run — a scan that chases pointers or decodes per-record framing is
   an order of magnitude slower for no benefit.
3. **Portability across filesystems that aren't POSIX.** OPFS has no `fsync`, questionable
   `rename`, and prefers few large files. The commit protocol must not *require* atomic rename.
4. **Flash-friendly write patterns.** Random in-place writes are the worst case for mobile flash
   and for battery. Append-only + periodic rewrite is the right shape.
5. **Migratability.** Every file must announce what it is and what version it is, in its first
   32 bytes, before we read anything else.

These lead to: **an append-only WAL + immutable columnar segments + a dual-slot manifest.**
This is a well-trodden design (it is roughly LSM without the multi-level merge), and its failure
modes are understood.

## 5.2 Directory layout

```text
<db-root>/
├── LOCK                          # advisory single-writer lock (pid + boot-unique nonce)
├── MANIFEST-A                    # slot A: whole-DB manifest (see 5.4)
├── MANIFEST-B                    # slot B: the other one
└── collections/
    └── <collection-name>/
        ├── CATALOG               # dimension, metric, dtype, index spec, id_kind, created_at
        ├── wal/
        │   ├── 000042.wal        # append-only frames since the last checkpoint
        │   └── 000043.wal
        ├── segments/
        │   ├── 000007.vec        # fixed-stride f32 block: row r at byte offset 64 + r*dim*4
        │   ├── 000007.dir        # row directory: DocId, meta offset/len, flags, norm
        │   ├── 000007.meta       # metadata + content records, length-prefixed
        │   └── 000007.del        # live/tombstone bitmap (mutable, rewritten atomically)
        └── index/
            └── flat-000012.idx   # index snapshot (flat: norms + row map; hnsw: the graph)
```

Segment files carry the same numeric id so they can be found without consulting the manifest —
useful for the `verify`/`repair` CLI when the manifest itself is the damaged thing.

### Why four files per segment rather than one

`.vec` must be mmap-able, 64-byte aligned, and contain *nothing but* floats so a scan is a straight
memory read. Interleaving metadata into the same file would either break the stride or force an
offset table lookup per row. `.del` is the only mutable part and is tiny, so isolating it means a
delete rewrites 8 KB instead of 300 MB. On backends that report `prefers_few_large_files` (OPFS),
a `SegmentPack` mode concatenates the four into one file with a footer of section offsets; the
reader is identical because it works from `(file, offset, len)` triples either way.

## 5.3 Common file header (32 bytes, every file)

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic — `VDB1` + a 4-byte file-kind tag (`MANI`,`CATL`,`WAL\0`,`VEC\0`,`DIR\0`,`META`,`DEL\0`,`IDX\0`) |
| 8 | 2 | `format_version` (u16, LE) — currently `1` |
| 10 | 2 | `flags` (bit 0: compressed, bit 1: encrypted — both 0 in v1) |
| 12 | 4 | `header_len` (u32) — allows additive header growth without a version bump |
| 16 | 8 | `payload_len` (u64) |
| 24 | 4 | `header_crc32c` (u32) over bytes 0..24 |
| 28 | 4 | reserved (must be zero; readers reject non-zero to keep the door open) |

Rules enforced by the decoder, and each one is a fuzz-test target:

- Unknown magic → `CorruptionError::BadMagic { expected, found, path }`.
- `format_version > CURRENT` → `UnsupportedFormatVersion` (**never** attempt a best-effort read).
- **Never allocate based on a length field before checking it against the actual file length.**
  This is the single most common way a corrupt-file parser turns into an OOM crash. Every
  length-prefixed read goes through one helper, `read_bounded(len, remaining)`.

## 5.4 The manifest: dual-slot, not rename

The manifest is the root of the tree; if it is lost or torn, the database is lost. Two fixed-size
slots (`MANIFEST-A`, `MANIFEST-B`) alternate. Each contains `sequence: u64`, the body, and a CRC.
On open: read both, discard any with a bad CRC or bad header, take the one with the higher
sequence. On commit: write to the slot *not* currently in use, `sync_data`, done.

This gives atomic root updates using nothing but "write bytes at an offset and flush", so it works
identically on POSIX, Windows, and OPFS. Atomic `rename` is used as an optimization for whole-file
replacement (segments, index snapshots) where the capability is advertised, and a
write-temp + fsync + `.commit` marker protocol is used where it isn't.

Manifest body: format version, db uuid, creation/modification time (injected clock), the collection
list, and for each collection its live segment ids, `.del` generation, current index snapshot id,
last-applied WAL sequence, and per-collection counters.

## 5.5 Write path

```text
insert/upsert/delete
  → validate (dimension, id length, metadata limits, collection open, not read-only)
  → serialize into one WAL frame  [len | seq | op | payload | crc32c]
  → append to wal/NNNNNN.wal
  → sync_data()  (policy-dependent, see 5.7)
  → apply to the in-memory memtable (vectors in a contiguous arena + id map + metadata)
  → return

when memtable_bytes > flush_threshold  (or flush()/close()):
  → write segments/NNNNNN.{vec,dir,meta,del}   (immutable once written)
  → sync each
  → commit new manifest slot (sequence + 1)
  → sync
  → delete the now-checkpointed WAL files
```

An abrupt kill at *any* point in that sequence leaves either the old consistent state (manifest not
yet advanced — the new segment files are orphans, cleaned up on next open) or the new consistent
state. There is no intermediate visible state. This property is what the fault-injection test suite
(§8.3) exists to verify at every single I/O operation index.

Deletes are tombstones: a bit cleared in the in-memory live set, a WAL frame, and on flush a
rewritten `.del`. Space is reclaimed only by `compact()`, which rewrites segments whose dead ratio
exceeds a threshold and then commits a manifest that drops the old ones.

## 5.6 Recovery

On open, in order:

1. Acquire `LOCK`. A stale lock (same pid, different boot nonce, or a pid that no longer exists) is
   reclaimed with a warning in `VerifyReport`; a live lock is `DatabaseAlreadyOpen`.
2. Read manifests, pick the highest valid sequence. Both slots invalid → `CorruptionError::
   NoValidManifest` with the path and both CRCs. This is where a future `repair` command
   reconstructs a manifest by scanning `segments/` — designed for, not implemented in v1.
3. Verify that every segment referenced by the manifest exists and has a valid header. Missing or
   bad → `CorruptionError::MissingSegment` (fail loud; do not silently drop the user's data).
4. Delete orphan segment files not referenced by the manifest (they are aborted flushes).
5. Replay WAL frames with `sequence > manifest.last_applied`, stopping at the first frame with a
   bad CRC or truncated length. **A torn tail is expected, not corruption** — a process killed
   mid-append leaves a partial frame. The tail is truncated and the WAL continues from there.
   A bad CRC in the *middle* (a valid frame after an invalid one) is real corruption and is
   reported.
6. Rebuild the id→RowId map and load or rebuild the index snapshot.

`VerifyLevel`: `Quick` (headers + manifest), `Checksums` (every block CRC), `Full` (checksums +
index/data cross-consistency + id-uniqueness). `Full` is what the CLI and the test suite run.

## 5.7 Durability policy

| Mode | fsync behaviour | Loses on power failure | Use |
|---|---|---|---|
| `Full` | every write op | nothing | financial-grade; slow on mobile flash |
| `Batch` (**default**) | on `write_batch` commit, `flush()`, `close()`, and every N ms of writes | the last unbatched writes | the right default for app workloads |
| `Relaxed` | only on `flush()`/`close()` | recent writes | bulk import; document it as such |

In all three modes a **process crash** (as opposed to power loss) loses nothing, because the WAL
bytes are already in the OS page cache. Only power loss or a kernel panic can lose `Batch`/`Relaxed`
writes. That distinction matters enormously on mobile, where process death is routine and power
loss is rare — and it is why `Batch` is the default rather than `Full`.

If `capabilities().durable_sync == false` (browser), the effective mode is reported as degraded in
`DatabaseStats` rather than silently pretending.

## 5.8 Sizing

Per document: `dim*4` bytes of vector + ~`24 + id_len` bytes of directory + metadata size. At
768 dims: 3,072 B/vector, so 100k documents ≈ 307 MB of vectors. Metadata is typically 100–500 B.
The `.del` bitmap is `n/8` bytes. Index overhead: flat adds 4 B/row (cached inverse norm); HNSW
will add roughly `M * 2 * 4` bytes per row.

**Memory:** v1 keeps the id→RowId map in RAM: roughly `48 + id_len` bytes per document (~10 MB at
100k docs with short string ids, ~5 MB with `IdKind::U64`). This is fine to a few million documents
and *not* fine at 50 million; the mitigation (an on-disk sorted id table with binary search over an
mmap) is a v0.4 item, and the limit is documented rather than discovered by users.

Vectors themselves are mmap'd read-only on backends that support it and otherwise read in chunks
into a reusable buffer, so the working set is not required to fit in RAM.

## 5.9 Storage implementations

- **`isha-vector-db-storage-memory`** — `BTreeMap<DbPath, Vec<u8>>`. The reference implementation and the
  substrate for fault injection. `capabilities`: everything true except `mmap`. Its most important
  job is making the entire persistence test suite runnable with no filesystem, so it runs in
  milliseconds and identically on every CI runner.
- **`isha-vector-db-storage-os`** — `std::fs` with positional I/O (`pread`/`pwrite`), `memmap2` behind a
  feature flag, `fs2`-style advisory locks. Handles the platform quirks: `fsync` on the *directory*
  after rename on POSIX; `FlushFileBuffers` and rename-replace semantics on Windows; `F_FULLFSYNC`
  on macOS/iOS (plain `fsync` on Darwin does **not** guarantee the drive flushed its cache).
- **`vdb-storage-opfs`** — wasm32 only, `FileSystemSyncAccessHandle` (worker-only). No mmap, no
  real fsync (`flush()` is best-effort), so `durable_sync: false` and `prefers_few_large_files:
  true`. See §9.6.

## 5.10 Format versioning and migration

- `format_version` is a single u16 for the whole database, recorded in the manifest and in every
  file header. Mixed versions inside one database are illegal and detected at open.
- The engine declares `MIN_READABLE`, `CURRENT`, and refuses anything outside `[MIN_READABLE,
  CURRENT]` with a clear error naming both versions and pointing at the migration command.
- **Additive changes** (a new optional field appended within `header_len`, a new metadata value
  tag, a new index kind) do not bump the version; readers ignore what they don't know **only** in
  regions explicitly declared extensible. Everywhere else, unknown = error.
- **Breaking changes** bump the version and require a `Migration` implementation:

```rust
pub trait Migration {
    fn from(&self) -> u16;
    fn to(&self) -> u16;
    fn describe(&self) -> &str;
    fn estimate(&self, src: &dyn Storage, root: &DbPath) -> Result<MigrationEstimate>;
    fn run(&self, src: &dyn Storage, root: &DbPath, dst: &DbPath, p: &mut dyn Progress) -> Result<()>;
}
```

`MigrationManager` chains registered migrations (1→2→3) to reach `CURRENT`. Migration **never
mutates in place**: it writes a complete new database directory, verifies it at `VerifyLevel::Full`,
then swaps and keeps the original as `<root>.v<N>.bak` until the caller confirms. On a phone with
limited free space, `estimate()` lets the SDK check space first and fail with
`InsufficientStorage { required, available }` instead of half-migrating.

Migration is opt-in (`allow_format_migration: false` by default) because silently rewriting a
user's database on app launch — possibly taking minutes, possibly on battery — must be the app
developer's decision, not ours.

Golden files for every released format version live in `testdata/`, and a CI job opens each with
the current build. **Any change to committed golden bytes must be justified in the PR description**
— that check is what makes rule 9 ("every storage format change must be versioned") real rather
than aspirational.
