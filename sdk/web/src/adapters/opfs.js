// A storage adapter over the Origin Private File System.
//
// # The constraint this design exists for
//
// The engine calls storage synchronously — it is one Rust call stack from `vdb_search` down to
// `read_at`, and there is nowhere to await. OPFS can do synchronous I/O, through
// `FileSystemSyncAccessHandle`, but *obtaining* one of those handles is asynchronous, and so is
// `getFileHandle`. So a file cannot be opened at the moment the engine asks for it.
//
// The way out is to acquire the handles before the engine ever runs: this adapter opens a pool
// of sync access handles at start-up and assigns them to paths as the engine asks. Each slot
// carries a small header naming the path it currently holds, so the mapping survives a reload
// without a separate index file to keep consistent. SQLite's OPFS VFS solves the same problem
// the same way, for the same reason.
//
// # Worker only
//
// `createSyncAccessHandle()` exists only on a Worker thread — on the main thread the method is
// simply absent, and this adapter fails immediately with "is not a function". That is not a
// limitation of this code; it is the platform, and it is why the SDK asks you to run the engine
// in a dedicated Worker.
//
// # Status
//
// Verified against real OPFS in Chromium by `test/browser.html`, which writes, searches, deletes
// and then reloads the page to confirm the data survived.

const HEADER_BYTES = 512;
const MAGIC = 0x56_44_42_50; // "VDBP"
const MAX_PATH = HEADER_BYTES - 8;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/**
 * Open an OPFS-backed adapter.
 *
 * @param {string} name       Directory within the origin's private filesystem.
 * @param {number} slots      Files to pre-open. Each can hold one database file.
 */
export async function opfsAdapter(name, { slots = 64 } = {}) {
  const root = await navigator.storage.getDirectory();
  const dir = await root.getDirectoryHandle(name, { create: true });

  const pool = [];
  for (let i = 0; i < slots; i++) {
    const fileName = `s${String(i).padStart(4, '0')}`;
    const handle = await dir.getFileHandle(fileName, { create: true });
    const sync = await handle.createSyncAccessHandle();
    pool.push({ sync, path: readHeader(sync) });
  }

  // Directories are not real here. OPFS has them, but a pooled slot has no place in a tree, so
  // the adapter keeps the directory structure in memory and derives it from the paths in use.
  const dirs = new Set(['/']);
  for (const slot of pool) {
    if (slot.path) for (const d of ancestors(slot.path)) dirs.add(d);
  }

  const find = (p) => pool.find((s) => s.path === p);
  const free = () => pool.find((s) => s.path === null);

  return {
    stat(p) {
      const slot = find(p);
      if (slot) return { size: slot.sync.getSize() - HEADER_BYTES, directory: false };
      if (dirs.has(p)) return { size: 0, directory: true };
      return null;
    },

    open(p, writable) {
      let slot = find(p);
      if (!slot) {
        if (!writable) throw notFound();
        slot = free();
        if (!slot) {
          const e = new Error(`the OPFS slot pool is full (${slots} files)`);
          e.name = 'QuotaExceededError';
          throw e;
        }
        slot.path = p;
        writeHeader(slot.sync, p);
        slot.sync.truncate(HEADER_BYTES);
        for (const d of ancestors(p)) dirs.add(d);
      }
      return fileOn(slot);
    },

    removeFile(p) {
      const slot = find(p);
      if (!slot) throw notFound();
      slot.path = null;
      clearHeader(slot.sync);
      slot.sync.truncate(HEADER_BYTES);
      slot.sync.flush();
    },

    createDirAll(p) {
      for (const d of ancestors(p)) dirs.add(d);
      dirs.add(p);
    },

    removeDirAll(p) {
      const prefix = p.endsWith('/') ? p : `${p}/`;
      for (const slot of pool) {
        if (slot.path && (slot.path === p || slot.path.startsWith(prefix))) {
          slot.path = null;
          clearHeader(slot.sync);
          slot.sync.truncate(HEADER_BYTES);
        }
      }
      for (const d of [...dirs]) {
        if (d === p || d.startsWith(prefix)) dirs.delete(d);
      }
    },

    syncDir(_p) {
      // Nothing to do: there is no directory entry to make durable, because there are no real
      // directories. Slot headers are flushed with their data.
    },

    listDir(p) {
      const prefix = p === '/' ? '/' : `${p}/`;
      const seen = new Map();
      for (const slot of pool) {
        if (!slot.path || !slot.path.startsWith(prefix)) continue;
        const rest = slot.path.slice(prefix.length);
        const cut = rest.indexOf('/');
        if (cut === -1) seen.set(rest, false);
        else seen.set(rest.slice(0, cut), true);
      }
      for (const d of dirs) {
        if (d === p || !d.startsWith(prefix)) continue;
        const rest = d.slice(prefix.length);
        const cut = rest.indexOf('/');
        seen.set(cut === -1 ? rest : rest.slice(0, cut), true);
      }
      return [...seen].map(([n, directory]) => ({ name: n, directory }));
    },

    /** Release every handle. Without this the files stay locked until the page goes away. */
    close() {
      for (const slot of pool) slot.sync.close();
    },
  };
}

/** The synchronous file interface `host.js` expects, offset past the slot header. */
function fileOn(slot) {
  return {
    read(dest, offset) {
      return slot.sync.read(dest, { at: HEADER_BYTES + offset });
    },
    write(src, offset) {
      let written = 0;
      while (written < src.length) {
        written += slot.sync.write(src.subarray(written), {
          at: HEADER_BYTES + offset + written,
        });
      }
    },
    truncate(len) {
      slot.sync.truncate(HEADER_BYTES + len);
    },
    size() {
      return Math.max(0, slot.sync.getSize() - HEADER_BYTES);
    },
    flush() {
      // Best-effort, which is why the Rust side declares `durable_sync: false`.
      slot.sync.flush();
    },
    close() {
      // The slot outlives the handle: closing here would release the pool entry, and the engine
      // opens and closes the same file many times over a database's life.
    },
  };
}

function ancestors(p) {
  const out = [];
  const parts = p.split('/').filter(Boolean);
  let acc = '';
  for (let i = 0; i < parts.length - 1; i++) {
    acc += `/${parts[i]}`;
    out.push(acc);
  }
  return out;
}

function readHeader(sync) {
  if (sync.getSize() < HEADER_BYTES) return null;
  const head = new Uint8Array(HEADER_BYTES);
  sync.read(head, { at: 0 });
  const dv = new DataView(head.buffer);
  if (dv.getUint32(0, false) !== MAGIC) return null;
  const len = dv.getUint16(4, true);
  if (len === 0 || len > MAX_PATH) return null;
  return decoder.decode(head.subarray(8, 8 + len));
}

function writeHeader(sync, path) {
  const bytes = encoder.encode(path);
  if (bytes.length > MAX_PATH) {
    throw new Error(`path too long for an OPFS slot header: ${path}`);
  }
  const head = new Uint8Array(HEADER_BYTES);
  const dv = new DataView(head.buffer);
  dv.setUint32(0, MAGIC, false);
  dv.setUint16(4, bytes.length, true);
  head.set(bytes, 8);
  sync.write(head, { at: 0 });
}

function clearHeader(sync) {
  sync.write(new Uint8Array(HEADER_BYTES), { at: 0 });
}

function notFound() {
  const e = new Error('no such file');
  e.name = 'NotFoundError';
  return e;
}
