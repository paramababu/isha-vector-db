// The React Native API.
//
// A thin layer over the JSI host object installed at startup. It exists to give the ergonomics
// JavaScript expects — objects with methods, real Errors, typed arrays converted for you — while
// the calls themselves go straight to C++ with no serialisation.
//
// The host object is injected rather than read from `globalThis` at import time, which is what
// lets this file be tested in Node against a mock. The logic here is small but not nothing:
// closed-handle tracking, vector conversion, the rule that a released collection cannot be used.

/** An error carrying the engine's own structured code. See `docs/api/errors.md`. */
export class VdbError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'VdbError';
    /** The stable numeric code. */
    this.code = code;
  }
}

/** Distance metrics. Values match `vdb_metric_t` in `include/vdb.h`; there is no zero. */
export const Metric = { Cosine: 1, L2: 2, Dot: 3 };

/**
 * The host object the TurboModule installs.
 *
 * Read lazily, not at import time: on the New Architecture the module installs during startup,
 * and a module-level read can run first and capture `undefined` forever.
 */
function hostObject() {
  const host = globalThis.__vdb;
  if (!host) {
    throw new VdbError(
      0,
      'the vdb native module is not installed. Rebuild the app after adding the package — ' +
        'Expo Go cannot load custom native code, so a development build is required.',
    );
  }
  return host;
}

/** Wrap whatever the host throws so callers always see a `VdbError`. */
function rethrow(e) {
  if (e instanceof VdbError) return e;
  const code = typeof e?.code === 'number' ? e.code : 0;
  return new VdbError(code, e?.message ?? String(e));
}

function call(fn) {
  try {
    return fn();
  } catch (e) {
    throw rethrow(e);
  }
}

/**
 * Open or create a database.
 *
 * @param {string} path              Directory to hold it.
 * @param {object} [options]         `createIfMissing`, `readOnly`, and `host` for testing.
 */
export function open(path, options = {}) {
  const { createIfMissing = true, readOnly = false, host = hostObject() } = options;
  const handle = call(() => host.open(path, createIfMissing, readOnly));
  return new Database(host, handle);
}

/** Version information, available without opening anything. */
export function versions(host = hostObject()) {
  return {
    library: host.version,
    /** Frozen. A change breaks every compiled caller. */
    abi: host.abiVersion,
    /** Moves independently of the ABI. */
    format: host.formatVersion,
  };
}

/** An open database. */
export class Database {
  #host;
  #handle;
  #open = true;

  constructor(host, handle) {
    this.#host = host;
    this.#handle = handle;
  }

  /**
   * Create a collection, or open it if it already exists.
   *
   * @param {string} name
   * @param {number} dimension
   * @param {number} [metric] One of `Metric`.
   */
  collection(name, dimension, metric = Metric.Cosine) {
    this.#alive();
    const handle = call(() => this.#host.collection(this.#handle, name, dimension, metric));
    return new Collection(this.#host, handle);
  }

  /**
   * Close the database.
   *
   * Explicit, and not optional. A `HostObject` is destroyed by the JavaScript garbage collector,
   * which is not deterministic and may never run before the app is killed — so an unclosed
   * database can hold its lock file until the process dies, and the next launch finds it held by
   * a process that no longer exists.
   */
  close() {
    if (!this.#open) return;
    this.#open = false;
    call(() => this.#host.close(this.#handle));
  }

  /** Whether `close` has been called. */
  get isOpen() {
    return this.#open;
  }

  #alive() {
    if (!this.#open) {
      throw new VdbError(0, 'this database is closed');
    }
  }
}

/** One collection. */
export class Collection {
  #host;
  #handle;
  #live = true;

  constructor(host, handle) {
    this.#host = host;
    this.#handle = handle;
  }

  /** Insert or replace a document. Returns whether it was newly inserted. */
  upsert(id, vector) {
    this.#alive();
    return call(() => this.#host.upsert(this.#handle, id, asFloats(vector)));
  }

  /** Remove a document. Returns whether it existed. */
  delete(id) {
    this.#alive();
    return call(() => this.#host.remove(this.#handle, id));
  }

  /** Whether a document exists. */
  has(id) {
    this.#alive();
    return call(() => this.#host.contains(this.#handle, id));
  }

  /** How many documents are live. */
  count() {
    this.#alive();
    return call(() => this.#host.count(this.#handle));
  }

  /** Flush this collection's writes. */
  flush() {
    this.#alive();
    call(() => this.#host.flush(this.#handle));
  }

  /** The `k` nearest documents, as `[{id, score}]`, best first. */
  search(query, k) {
    this.#alive();
    return call(() => this.#host.search(this.#handle, asFloats(query), k));
  }

  /** Release the handle. The database stays open. */
  release() {
    if (!this.#live) return;
    this.#live = false;
    call(() => this.#host.releaseCollection(this.#handle));
  }

  #alive() {
    if (!this.#live) {
      throw new VdbError(0, 'this collection has been released');
    }
  }
}

/**
 * Present a vector to the native side without copying where possible.
 *
 * A `Float32Array` is handed over as-is, so its backing store crosses as a pointer. A plain array
 * has to be converted — which is a copy, and worth avoiding on a hot path. Anything else is
 * refused here rather than reaching C++ and being misread as a buffer.
 */
function asFloats(vector) {
  if (vector instanceof Float32Array) return vector;
  if (Array.isArray(vector)) return Float32Array.from(vector);
  throw new VdbError(0, 'a vector must be a Float32Array or an array of numbers');
}
