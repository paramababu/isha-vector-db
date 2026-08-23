// The host half of the WebAssembly boundary.
//
// `crates/isha-vector-db-storage-web` declares a set of imports and calls them for every file operation.
// This module implements them, translating pointers into linear memory and delegating the actual
// storage to an *adapter* — OPFS in a browser, node:fs under test. Keeping the marshalling in one
// place means the tricky part (pointer arithmetic, the listing encoding, the short-read contract)
// is written once and tested once, and an adapter only has to move bytes.
//
// Nothing here may throw. An exception unwinding from JavaScript into WebAssembly aborts the
// module and takes the open database with it, so every call is wrapped and every failure becomes
// one of the negative codes the Rust side understands.

/** Error codes, mirroring `crates/isha-vector-db-storage-web/src/host.rs`. */
export const CODE = {
  NOT_FOUND: -1,
  ALREADY_EXISTS: -2,
  PERMISSION_DENIED: -3,
  IO: -4,
  INVALID_PATH: -5,
  LOCKED: -6,
  QUOTA_EXCEEDED: -7,
  BAD_HANDLE: -8,
  BUFFER_TOO_SMALL: -9,
};

/** Open modes, mirroring the Rust `mode` module. */
export const MODE = { READ: 0, READ_WRITE: 1, CREATE: 2, CREATE_NEW: 3 };

/**
 * Build the import object for an isha-vector-db WebAssembly instance.
 *
 * @param {object} adapter    Synchronous storage. See `adapters/` for the two implementations.
 * @param {() => WebAssembly.Memory} memory  Read lazily: instantiation needs the imports before
 *   the instance exists, so the memory cannot be captured up front.
 */
export function createHost(adapter, memory, onPanic = null) {
  let handles = new Map();
  let nextHandle = 1;
  const locks = new Set();

  // A fresh view every time. `memory.buffer` is detached and replaced whenever linear memory
  // grows, and a cached view would silently read the old, dead buffer — a bug that shows up only
  // once the database is big enough to trigger a growth.
  const u8 = () => new Uint8Array(memory().buffer);
  const view = () => new DataView(memory().buffer);

  const decoder = new TextDecoder('utf-8', { fatal: true });

  const readString = (ptr, len) => decoder.decode(u8().subarray(ptr, ptr + len));

  /** Wrap a host call so no exception can cross back into WebAssembly. */
  const guard = (fn, onError = CODE.IO) => (...args) => {
    try {
      const rc = fn(...args);
      return rc === undefined ? 0 : rc;
    } catch (e) {
      return classify(e, onError);
    }
  };

  const classify = (e, fallback) => {
    const name = e && e.name;
    if (name === 'NotFoundError') return CODE.NOT_FOUND;
    if (name === 'QuotaExceededError') return CODE.QUOTA_EXCEEDED;
    if (name === 'NoModificationAllowedError') return CODE.LOCKED;
    if (name === 'TypeMismatchError') return CODE.PERMISSION_DENIED;
    if (e && e.code === 'ENOENT') return CODE.NOT_FOUND;
    if (e && e.code === 'EEXIST') return CODE.ALREADY_EXISTS;
    if (e && e.code === 'EACCES' || e && e.code === 'EPERM') return CODE.PERMISSION_DENIED;
    if (e && e.code === 'EISDIR') return CODE.PERMISSION_DENIED;
    if (e && e.vdbCode !== undefined) return e.vdbCode;
    return fallback;
  };

  const file = (handle) => {
    const f = handles.get(handle);
    if (f === undefined) {
      const err = new Error('unknown handle');
      err.vdbCode = CODE.BAD_HANDLE;
      throw err;
    }
    return f;
  };

  return {
    /** Release every handle. Called when the instance is torn down. */
    dispose() {
      for (const f of handles.values()) {
        try { f.close(); } catch { /* already gone */ }
      }
      handles = new Map();
      locks.clear();
    },

    imports: {
      vdb_host_open: guard((ptr, len, mode) => {
        const path = readString(ptr, len);
        const existing = adapter.stat(path);
        if (existing && existing.directory) return CODE.PERMISSION_DENIED;
        if (!existing && (mode === MODE.READ || mode === MODE.READ_WRITE)) return CODE.NOT_FOUND;
        if (existing && mode === MODE.CREATE_NEW) return CODE.ALREADY_EXISTS;
        const f = adapter.open(path, mode !== MODE.READ);
        const handle = nextHandle++;
        handles.set(handle, f);
        return handle;
      }),

      vdb_host_read: guard((handle, ptr, len, offset) => {
        // The Rust side treats a short read as end-of-file, not an error, so the adapter is
        // allowed to return fewer bytes than asked for and must never pad.
        const dest = u8().subarray(ptr, ptr + len);
        return file(handle).read(dest, offset);
      }),

      vdb_host_write: guard((handle, ptr, len, offset) => {
        const src = u8().subarray(ptr, ptr + len);
        file(handle).write(src, offset);
        return 0;
      }),

      vdb_host_truncate: guard((handle, len) => {
        file(handle).truncate(len);
        return 0;
      }),

      // Returns a size as a double, so failure is a negative number rather than a code.
      vdb_host_size: (handle) => {
        try {
          return file(handle).size();
        } catch (e) {
          return classify(e, CODE.IO);
        }
      },

      vdb_host_sync: guard((handle) => {
        file(handle).flush();
        return 0;
      }),

      vdb_host_close: guard((handle) => {
        const f = handles.get(handle);
        if (f === undefined) return CODE.BAD_HANDLE;
        handles.delete(handle);
        f.close();
        return 0;
      }),

      vdb_host_remove_file: guard((ptr, len) => {
        const path = readString(ptr, len);
        const st = adapter.stat(path);
        if (!st) return CODE.NOT_FOUND;
        if (st.directory) return CODE.PERMISSION_DENIED;
        adapter.removeFile(path);
        return 0;
      }),

      vdb_host_create_dir_all: guard((ptr, len) => {
        adapter.createDirAll(readString(ptr, len));
        return 0;
      }),

      vdb_host_remove_dir_all: guard((ptr, len) => {
        const path = readString(ptr, len);
        if (!adapter.stat(path)) return CODE.NOT_FOUND;
        adapter.removeDirAll(path);
        return 0;
      }),

      vdb_host_sync_dir: guard((ptr, len) => {
        const path = readString(ptr, len);
        const st = adapter.stat(path);
        if (!st) return CODE.NOT_FOUND;
        if (!st.directory) return CODE.PERMISSION_DENIED;
        adapter.syncDir(path);
        return 0;
      }),

      vdb_host_metadata: guard((ptr, len, outLen, outKind) => {
        const st = adapter.stat(readString(ptr, len));
        if (!st) return CODE.NOT_FOUND;
        const dv = view();
        dv.setFloat64(outLen, st.directory ? 0 : st.size, true);
        dv.setUint32(outKind, st.directory ? 1 : 0, true);
        return 0;
      }),

      vdb_host_list_dir: guard((ptr, len, buf, cap) => {
        const path = readString(ptr, len);
        const st = adapter.stat(path);
        if (!st) return CODE.NOT_FOUND;
        if (!st.directory) return CODE.NOT_FOUND;
        // `kind_byte name "\n"` per entry. A name containing a newline would be ambiguous, and
        // the Rust side would mis-split it, so it is refused here rather than corrupted.
        const parts = [];
        let total = 0;
        for (const entry of adapter.listDir(path)) {
          if (entry.name.includes('\n')) return CODE.IO;
          const bytes = new TextEncoder().encode((entry.directory ? 'd' : 'f') + entry.name + '\n');
          parts.push(bytes);
          total += bytes.length;
        }
        if (total > cap) return CODE.BUFFER_TOO_SMALL;
        const dest = u8();
        let at = buf;
        for (const p of parts) {
          dest.set(p, at);
          at += p.length;
        }
        return total;
      }),

      // Diagnostics, not error handling: a panic in the engine is a bug, and this is how the
      // message survives `panic = "abort"` instead of arriving as "RuntimeError: unreachable".
      vdb_host_panic: (ptr, len) => {
        try {
          const message = readString(ptr, len);
          if (onPanic) onPanic(message);
          else console.error('[vdb] panic:', message);
        } catch {
          // Never throw from here: the module is already dying.
        }
      },

      // wasm32-unknown-unknown has no clock of its own; `SystemTime::now()` panics there.
      vdb_host_now_ms: () => Date.now(),

      vdb_host_lock: guard((ptr, len) => {
        const path = readString(ptr, len);
        if (locks.has(path)) return CODE.LOCKED;
        locks.add(path);
        return 0;
      }),

      vdb_host_unlock: guard((ptr, len) => {
        locks.delete(readString(ptr, len));
        return 0;
      }),
    },
  };
}
