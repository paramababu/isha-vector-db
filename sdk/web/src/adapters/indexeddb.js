// A storage adapter over IndexedDB, for browsers where OPFS is unavailable.
//
// # Why this exists
//
// OPFS is the primary backend and is better in every way. This one is here because "it doesn't
// work in Safari private browsing" is otherwise a permanent support burden — IndexedDB is
// available essentially everywhere OPFS is not, including older Safari and private modes where
// the origin private filesystem is restricted.
//
// # The shape of the problem
//
// The engine calls storage synchronously. IndexedDB is asynchronous with no synchronous escape
// hatch at all — unlike OPFS, which at least offers sync access handles inside a Worker. There
// is no arrangement of pools or handles that makes an async key-value store answer a synchronous
// read.
//
// So the whole database is resident in memory, and IndexedDB is where it is *kept*, not where it
// is read from. `open` loads every block; reads and writes hit memory; dirty blocks are written
// back in the background and on demand. That is the design the architecture document specifies:
// a block device with a write-back cache, 64 KiB blocks keyed by file and block number, so a
// small change to a large segment rewrites one block rather than the whole file.
//
// # What that costs
//
// **Durability is weaker than OPFS, which is already weaker than a real filesystem.** A write
// the engine considers flushed is in memory and queued; if the tab dies before the write-back
// completes, it is gone. `flush()` returns a promise that resolves when the queue has drained,
// and the adapter also drains on `pagehide` and when the page becomes hidden, which covers the
// ways a tab normally goes away. It does not cover a crash.
//
// **The database must fit in memory.** Every block is resident. For the on-device corpora this
// engine targets that is the normal case, but it is a real ceiling, and `bytesResident()` reports
// where you are against it.

const BLOCK_BYTES = 64 * 1024;
const BLOCKS = 'blocks';
const FILES = 'files';

/**
 * Open an IndexedDB-backed adapter.
 *
 * @param {string} name    Database name within the origin.
 * @param {object} options `autoFlushMs` debounces write-back; 0 disables it, leaving `flush()`.
 */
export async function indexedDbAdapter(name, { autoFlushMs = 250 } = {}) {
  const db = await openDatabase(name);

  // Everything, up front. A synchronous read cannot wait for a cursor.
  const files = new Map(); // path -> { size, directory }
  const blocks = new Map(); // "path n" -> Uint8Array
  await readAll(db, FILES, (key, value) => files.set(key, value));
  await readAll(db, BLOCKS, (key, value) => blocks.set(key, value));

  const dirtyBlocks = new Set();
  const dirtyFiles = new Set();
  const deletedBlocks = new Set();
  const deletedFiles = new Set();

  let timer = null;
  let inFlight = null;

  function schedule() {
    if (autoFlushMs <= 0 || timer !== null) return;
    timer = setTimeout(() => {
      timer = null;
      void flush();
    }, autoFlushMs);
  }

  const touchFile = (path) => {
    dirtyFiles.add(path);
    deletedFiles.delete(path);
    schedule();
  };
  const touchBlock = (key) => {
    dirtyBlocks.add(key);
    deletedBlocks.delete(key);
    schedule();
  };

  /** Drain everything pending. Resolves when the browser has acknowledged the writes. */
  async function flush() {
    // Serialised: two overlapping transactions could write the same block out of order.
    while (inFlight) await inFlight;
    if (
      dirtyBlocks.size === 0 &&
      dirtyFiles.size === 0 &&
      deletedBlocks.size === 0 &&
      deletedFiles.size === 0
    ) {
      return;
    }
    // Snapshotted before the transaction, so writes arriving while it runs are not lost: they
    // stay in the sets and are picked up by the next drain.
    const blockKeys = [...dirtyBlocks];
    const fileKeys = [...dirtyFiles];
    const blockGone = [...deletedBlocks];
    const fileGone = [...deletedFiles];
    dirtyBlocks.clear();
    dirtyFiles.clear();
    deletedBlocks.clear();
    deletedFiles.clear();

    inFlight = (async () => {
      const tx = db.transaction([BLOCKS, FILES], 'readwrite');
      const blockStore = tx.objectStore(BLOCKS);
      const fileStore = tx.objectStore(FILES);
      for (const key of blockKeys) {
        const value = blocks.get(key);
        if (value) blockStore.put(value, key);
      }
      for (const key of fileKeys) {
        const value = files.get(key);
        if (value) fileStore.put(value, key);
      }
      for (const key of blockGone) blockStore.delete(key);
      for (const key of fileGone) fileStore.delete(key);
      await settled(tx);
    })();
    try {
      await inFlight;
    } finally {
      inFlight = null;
    }
  }

  // The tab going away is the realistic loss window, and these are the events that precede it.
  // `pagehide` fires where `beforeunload` is unreliable, notably on iOS.
  if (typeof addEventListener === 'function') {
    const drain = () => void flush();
    addEventListener('pagehide', drain);
    addEventListener('visibilitychange', () => {
      if (typeof document !== 'undefined' && document.visibilityState === 'hidden') drain();
    });
  }

  const blockKey = (path, n) => `${path} ${n}`;

  const readInto = (path, dest, offset, size) => {
    let written = 0;
    while (written < dest.length) {
      const at = offset + written;
      if (at >= size) break;
      const n = Math.floor(at / BLOCK_BYTES);
      const within = at % BLOCK_BYTES;
      const block = blocks.get(blockKey(path, n));
      const available = Math.min(BLOCK_BYTES - within, size - at, dest.length - written);
      if (block) {
        // A block shorter than the read is not corruption: a partially written block is stored
        // at its written length, and the rest of it reads as the zeroes it logically holds.
        const slice = block.subarray(within, within + available);
        dest.set(slice, written);
        if (slice.length < available) dest.fill(0, written + slice.length, written + available);
      } else {
        // A hole. Writing past the end zero-fills, so an absent block is zeroes.
        dest.fill(0, written, written + available);
      }
      written += available;
    }
    return written;
  };

  const writeFrom = (path, src, offset) => {
    let read = 0;
    while (read < src.length) {
      const at = offset + read;
      const n = Math.floor(at / BLOCK_BYTES);
      const within = at % BLOCK_BYTES;
      const take = Math.min(BLOCK_BYTES - within, src.length - read);
      const key = blockKey(path, n);
      const existing = blocks.get(key);
      const needed = within + take;
      let block;
      if (existing && existing.length >= needed) {
        block = existing;
      } else {
        block = new Uint8Array(Math.max(needed, existing ? existing.length : 0));
        if (existing) block.set(existing);
      }
      block.set(src.subarray(read, read + take), within);
      blocks.set(key, block);
      touchBlock(key);
      read += take;
    }
  };

  const dropBlocksFrom = (path, size) => {
    const first = Math.ceil(size / BLOCK_BYTES);
    for (const key of [...blocks.keys()]) {
      if (!key.startsWith(`${path} `)) continue;
      const n = Number(key.slice(path.length + 1));
      if (n >= first) {
        blocks.delete(key);
        dirtyBlocks.delete(key);
        deletedBlocks.add(key);
      }
    }
  };

  return {
    stat(p) {
      const entry = files.get(p);
      return entry ? { size: entry.size, directory: entry.directory } : null;
    },

    open(p, writable) {
      let entry = files.get(p);
      if (!entry) {
        if (!writable) {
          const e = new Error('no such file');
          e.name = 'NotFoundError';
          throw e;
        }
        entry = { size: 0, directory: false };
        files.set(p, entry);
        touchFile(p);
      }
      return {
        read(dest, offset) {
          return readInto(p, dest, offset, files.get(p)?.size ?? 0);
        },
        write(src, offset) {
          writeFrom(p, src, offset);
          const current = files.get(p);
          const end = offset + src.length;
          if (current && end > current.size) {
            current.size = end;
            touchFile(p);
          }
        },
        truncate(len) {
          const current = files.get(p);
          if (!current) return;
          current.size = len;
          dropBlocksFrom(p, len);
          touchFile(p);
        },
        size() {
          return files.get(p)?.size ?? 0;
        },
        flush() {
          // Best-effort by construction: the bytes are in memory and queued. The Rust backend
          // declares `durable_sync: false`, which is honest about exactly this.
          schedule();
        },
        close() {},
      };
    },

    removeFile(p) {
      files.delete(p);
      dirtyFiles.delete(p);
      deletedFiles.add(p);
      dropBlocksFrom(p, 0);
      schedule();
    },

    createDirAll(p) {
      const parts = p.split('/').filter(Boolean);
      let acc = '';
      for (const part of parts) {
        acc += `/${part}`;
        if (!files.has(acc)) {
          files.set(acc, { size: 0, directory: true });
          touchFile(acc);
        }
      }
    },

    removeDirAll(p) {
      const prefix = `${p}/`;
      for (const key of [...files.keys()]) {
        if (key === p || key.startsWith(prefix)) {
          files.delete(key);
          dirtyFiles.delete(key);
          deletedFiles.add(key);
          dropBlocksFrom(key, 0);
        }
      }
      schedule();
    },

    syncDir(_p) {},

    listDir(p) {
      const prefix = p === '/' ? '/' : `${p}/`;
      const seen = new Map();
      for (const [key, entry] of files) {
        if (!key.startsWith(prefix) || key === p) continue;
        const rest = key.slice(prefix.length);
        const cut = rest.indexOf('/');
        if (cut === -1) seen.set(rest, entry.directory);
        else seen.set(rest.slice(0, cut), true);
      }
      return [...seen].map(([n, directory]) => ({ name: n, directory }));
    },

    /** Drain the write-back queue. The only way to know the data has actually been stored. */
    flush,

    /** How much of the database is resident, which is all of it. */
    bytesResident() {
      let total = 0;
      for (const block of blocks.values()) total += block.length;
      return total;
    },

    /** Drain and release the connection. */
    async close() {
      await flush();
      db.close();
    },
  };
}

function openDatabase(name) {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(name, 1);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(BLOCKS)) db.createObjectStore(BLOCKS);
      if (!db.objectStoreNames.contains(FILES)) db.createObjectStore(FILES);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
    request.onblocked = () => reject(new Error('another tab is holding this database open'));
  });
}

function readAll(db, store, onEntry) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(store, 'readonly');
    const cursor = tx.objectStore(store).openCursor();
    cursor.onsuccess = () => {
      const c = cursor.result;
      if (!c) {
        resolve();
        return;
      }
      onEntry(c.key, c.value);
      c.continue();
    };
    cursor.onerror = () => reject(cursor.error);
  });
}

function settled(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error ?? new Error('transaction aborted'));
  });
}
