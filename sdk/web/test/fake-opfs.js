// A stand-in for the small part of OPFS this SDK uses.
//
// It implements `navigator.storage.getDirectory()` and `FileSystemSyncAccessHandle` over plain
// buffers, following the specified semantics: `read` and `write` take `{ at }`, `read` returns a
// short count at end of file, `truncate` grows with zeroes, and handles are exclusive.
//
// This does not make the OPFS adapter "tested in a browser" — a real implementation can differ in
// ways a hand-written double will agree with by construction. What it does test is everything the
// adapter itself decides: slot assignment, the header format, path-to-slot recovery after a
// reload, the virtual directory tree, and the listing. Those are the parts most likely to be
// wrong, and they are wrong in the same way whoever runs it.

class FakeSyncAccessHandle {
  constructor(file) {
    this.file = file;
    this.closed = false;
  }

  read(dest, { at = 0 } = {}) {
    this.#live();
    const available = Math.max(0, this.file.data.length - at);
    const n = Math.min(dest.length, available);
    dest.set(this.file.data.subarray(at, at + n));
    return n;
  }

  write(src, { at = 0 } = {}) {
    this.#live();
    const end = at + src.length;
    if (end > this.file.data.length) {
      const grown = new Uint8Array(end);
      grown.set(this.file.data);
      this.file.data = grown;
    }
    this.file.data.set(src, at);
    return src.length;
  }

  truncate(len) {
    this.#live();
    const next = new Uint8Array(len);
    next.set(this.file.data.subarray(0, Math.min(len, this.file.data.length)));
    this.file.data = next;
  }

  getSize() {
    this.#live();
    return this.file.data.length;
  }

  flush() {
    this.#live();
  }

  close() {
    this.closed = true;
    this.file.locked = false;
  }

  #live() {
    if (this.closed) throw new Error('handle is closed');
  }
}

class FakeFileHandle {
  constructor(file) {
    this.file = file;
  }

  async createSyncAccessHandle() {
    if (this.file.locked) {
      const e = new Error('already open');
      e.name = 'NoModificationAllowedError';
      throw e;
    }
    this.file.locked = true;
    return new FakeSyncAccessHandle(this.file);
  }
}

class FakeDirectoryHandle {
  constructor(store, prefix) {
    this.store = store;
    this.prefix = prefix;
  }

  async getDirectoryHandle(name, { create = false } = {}) {
    const key = `${this.prefix}${name}/`;
    if (!this.store.dirs.has(key)) {
      if (!create) throw Object.assign(new Error('no dir'), { name: 'NotFoundError' });
      this.store.dirs.add(key);
    }
    return new FakeDirectoryHandle(this.store, key);
  }

  async getFileHandle(name, { create = false } = {}) {
    const key = this.prefix + name;
    let file = this.store.files.get(key);
    if (!file) {
      if (!create) throw Object.assign(new Error('no file'), { name: 'NotFoundError' });
      file = { data: new Uint8Array(0), locked: false };
      this.store.files.set(key, file);
    }
    return new FakeFileHandle(file);
  }
}

/**
 * Install a fake OPFS on `globalThis.navigator`.
 *
 * Returns the backing store, so a test can drop every handle and mount the same bytes again —
 * which is how "does the slot mapping survive a reload?" is asked.
 */
export function installFakeOpfs(store = { files: new Map(), dirs: new Set() }) {
  const directory = new FakeDirectoryHandle(store, '/');
  // Node defines `navigator` as a getter-only global, so it cannot simply be assigned.
  Object.defineProperty(globalThis, 'navigator', {
    value: { storage: { getDirectory: async () => directory } },
    configurable: true,
    writable: true,
  });
  return store;
}
