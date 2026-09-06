// The React Native API.
//
// A thin layer over the JSI host object installed at startup. It exists to give the ergonomics
// JavaScript expects — objects with methods, real Errors, query-object filters, typed arrays
// converted for you — while the calls themselves go straight to C++ with no serialisation.
//
// # This mirrors the Node SDK, and that is not a coincidence
//
// `sdk/node/index.d.ts` is the canonical shape of the JavaScript API, and this file matches it:
// same names, same argument order, same defaults, same error codes. A developer moving a query
// from a Node service to a React Native app should not have to rewrite it, and semantic
// divergence between SDKs is the fastest way to make a cross-platform library untrustworthy.
//
// Where the two genuinely differ, they differ because the platform does: a collection here holds
// a native handle that must be released, because React Native's garbage collector will not do it
// for you at any predictable moment.
//
// # Why the logic is here and not in C++
//
// The host object is injected rather than read from `globalThis` at import time, which is what
// lets this file be tested in Node against a mock. Everything above the JSI boundary — argument
// validation, the filter and metadata compilers, closed-handle tracking, vector conversion — is
// therefore covered by `npm test` on a development machine rather than only on a device.

import { VdbError } from './error.js';
import { compileFilter, compileMetadata } from './filter.js';

export { VdbError };

/** Similarity metrics, mapped to `vdb_metric_t`. */
const METRICS = { cosine: 1, l2: 2, dot: 3 };

/** Durability modes, mapped to `vdb_durability_t`. */
const DURABILITY = { full: 1, batch: 2, relaxed: 3 };

/** Verification levels, mapped to `vdb_verify_t`. */
const VERIFY = { quick: 1, checksums: 2, full: 3 };

/**
 * The host object the native module installs.
 *
 * Read lazily, not at import time: on the New Architecture the module installs during startup,
 * and a module-level read can run first and capture `undefined` forever.
 */
function hostObject() {
  const host = globalThis.__vdb;
  if (!host) {
    throw new VdbError(
      0,
      'the isha-vector-db native module is not installed. Rebuild the app after adding the package — ' +
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

/** Look one of the string enums up, naming the alternatives when it is not there. */
function choose(table, value, what) {
  const mapped = table[value];
  if (mapped === undefined) {
    throw new VdbError(
      1002,
      `unknown ${what} ${JSON.stringify(value)}; expected one of ${Object.keys(table)
        .map((k) => `'${k}'`)
        .join(', ')}`,
    );
  }
  return mapped;
}

/**
 * Open or create a database.
 *
 * @param {string} path                    Directory to hold it.
 * @param {object} [options]
 * @param {boolean} [options.createIfMissing=true]
 * @param {boolean} [options.readOnly=false]  No write lock; every mutation is refused.
 * @param {'full'|'batch'|'relaxed'} [options.durability='batch']
 * @param {object} [options.host]          The host object, for testing.
 */
export function open(path, options = {}) {
  const {
    createIfMissing = true,
    readOnly = false,
    durability = 'batch',
    host = hostObject(),
  } = options;
  if (typeof path !== 'string' || path.length === 0) {
    throw new VdbError(1002, 'a database path must be a non-empty string');
  }
  const mode = choose(DURABILITY, durability, 'durability mode');
  const handle = call(() => host.open(path, createIfMissing, readOnly, mode));
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
   * Create a collection, or open it if one exists with a matching shape.
   *
   * A mismatch — a different dimension or metric — is an error rather than a silent
   * substitution, because a differently-shaped collection returns results that look plausible
   * and are wrong.
   *
   * @param {string} name
   * @param {object} options
   * @param {number} options.dimension       Fixed for the collection's lifetime.
   * @param {'cosine'|'l2'|'dot'} [options.metric='cosine']
   */
  collection(name, options) {
    this.#alive();
    if (typeof options?.dimension !== 'number') {
      throw new VdbError(1002, "a collection needs a dimension: collection(name, { dimension })");
    }
    const metric = choose(METRICS, options.metric ?? 'cosine', 'metric');
    const handle = call(() =>
      this.#host.collection(this.#handle, name, options.dimension, metric),
    );
    return new Collection(this.#host, handle, name, options.dimension);
  }

  /**
   * Open a collection that already exists.
   *
   * Unlike `collection`, this creates nothing: a missing collection is an error, which is what
   * you want when its absence is a bug rather than a first run.
   */
  openCollection(name) {
    this.#alive();
    const handle = call(() => this.#host.openCollection(this.#handle, name));
    return new Collection(this.#host, handle, name, undefined);
  }

  /** Delete a collection and everything in it. Irreversible. */
  dropCollection(name) {
    this.#alive();
    call(() => this.#host.dropCollection(this.#handle, name));
  }

  /** Fold every collection's buffered writes into segments. */
  flush() {
    this.#alive();
    call(() => this.#host.flushDatabase(this.#handle));
  }

  /**
   * Reclaim the space held by tombstoned rows, returning how many were removed.
   *
   * Explicit rather than automatic: rewriting hundreds of megabytes is a decision about when to
   * spend I/O and battery, and your application knows more about that than the engine does —
   * when the device is charging, when the user is not waiting, when the screen is off. Use a
   * collection's `deadRatio` to decide.
   *
   * This blocks the calling thread for as long as the rewrite takes. On the JS thread that is
   * visible to the user; see the threading note in the README.
   *
   * @param {number} [minDeadRatio=0.2] How dead a segment must be before it is rewritten.
   */
  compact(minDeadRatio = 0.2) {
    this.#alive();
    if (!(minDeadRatio >= 0 && minDeadRatio <= 1)) {
      throw new VdbError(1002, 'minDeadRatio is a fraction between 0 and 1');
    }
    return call(() => this.#host.compact(this.#handle, minDeadRatio));
  }

  /**
   * Check integrity.
   *
   * Reports rather than repairs, and a damaged database is a result rather than a throw: it
   * returns counts, and only a verification that could not run at all raises. Deciding what to
   * discard is not a choice a library should make on someone's behalf.
   *
   * @param {'quick'|'checksums'|'full'} [level='checksums']
   */
  verify(level = 'checksums') {
    this.#alive();
    const mode = choose(VERIFY, level, 'verification level');
    return call(() => this.#host.verify(this.#handle, mode));
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

  /** `using db = open(path)` closes it at the end of the scope, where the runtime supports it. */
  [Symbol.dispose]() {
    this.close();
  }

  #alive() {
    if (!this.#open) {
      throw new VdbError(1002, 'this database is closed');
    }
  }
}

/** One collection. */
export class Collection {
  #host;
  #handle;
  #live = true;
  #name;
  #dimension;

  constructor(host, handle, name, dimension) {
    this.#host = host;
    this.#handle = handle;
    this.#name = name;
    this.#dimension = dimension;
  }

  get name() {
    return this.#name;
  }

  /** The dimension this collection was opened with, or `undefined` after `openCollection`. */
  get dimension() {
    return this.#dimension;
  }

  /**
   * Insert or replace a document. Returns whether it was newly inserted.
   *
   * @param {string} id
   * @param {Float32Array|number[]} vector
   * @param {Record<string, string|number|boolean|null>} [metadata]
   */
  upsert(id, vector, metadata) {
    this.#alive();
    const fields = compileMetadata(metadata);
    return call(() => this.#host.upsert(this.#handle, id, asFloats(vector), fields));
  }

  /**
   * Insert or replace many documents in one native call. Returns how many were newly inserted.
   *
   * This is what JSI is here for. Each `upsert` is a separate crossing with its own
   * `ArrayBuffer` lookup; a batch packs every vector into one contiguous buffer and crosses
   * once, which for a bulk import is the difference that matters.
   *
   * It is **not** a transaction — the C ABI has no batch write, so a failure part-way leaves the
   * documents before it written. The error says which document stopped it.
   *
   * @param {Array<{id: string, vector: Float32Array|number[], metadata?: object}>} documents
   */
  upsertMany(documents) {
    this.#alive();
    if (!Array.isArray(documents)) {
      throw new VdbError(1002, 'upsertMany takes an array of { id, vector, metadata } documents');
    }
    if (documents.length === 0) return 0;

    // Packed here rather than in C++ so the shape crossing JSI is one buffer and one string
    // array, and so this — the part that can get the stride wrong — is testable in Node.
    const first = asFloats(documents[0]?.vector);
    const dimension = first.length;
    if (dimension === 0) {
      throw new VdbError(1002, 'document 0: a vector cannot be empty');
    }
    const ids = new Array(documents.length);
    const vectors = new Float32Array(documents.length * dimension);
    const metadata = [];
    let anyMetadata = false;

    for (let i = 0; i < documents.length; i++) {
      const { id, vector, metadata: fields } = documents[i];
      if (typeof id !== 'string') {
        throw new VdbError(1002, `document ${i}: an id must be a string`);
      }
      const floats = i === 0 ? first : asFloats(vector);
      if (floats.length !== dimension) {
        throw new VdbError(
          1002,
          `document ${i} (${id}) has ${floats.length} dimensions but the batch started with ` +
            `${dimension}; every vector in a batch must be the same length`,
        );
      }
      ids[i] = id;
      vectors.set(floats, i * dimension);
      const compiled = compileMetadata(fields);
      if (compiled.length > 0) anyMetadata = true;
      metadata.push(compiled);
    }

    // All or nothing: the bridge takes metadata for every document or for none, so a batch where
    // nobody carries any does not pay for an empty list each.
    return call(() =>
      this.#host.upsertMany(this.#handle, ids, vectors, dimension, anyMetadata ? metadata : []),
    );
  }

  /** Remove a document. Returns whether it existed; removing an absent one is not an error. */
  delete(id) {
    this.#alive();
    return call(() => this.#host.remove(this.#handle, id));
  }

  /** Whether a document exists. */
  contains(id) {
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

  /**
   * The `topK` nearest documents, as `[{id, score}]`, best first.
   *
   * Ordered by score descending — always higher-is-better, whatever the metric — with ties
   * broken by ascending id.
   *
   * `topK` counts *matches*, not candidates: a filter excluding most of the collection still
   * returns up to `topK` results.
   *
   * @param {Float32Array|number[]} query
   * @param {number} topK
   * @param {object} [filter] A metadata predicate; see the README and `docs/api/filters.md`.
   */
  search(query, topK, filter) {
    this.#alive();
    if (!Number.isInteger(topK) || topK < 1) {
      throw new VdbError(1002, 'topK must be a positive integer');
    }
    const steps = compileFilter(filter);
    return call(() => this.#host.search(this.#handle, asFloats(query), topK, steps));
  }

  /** Counters for this collection, including the `deadRatio` that says whether to compact. */
  stats() {
    this.#alive();
    return call(() => this.#host.stats(this.#handle));
  }

  /** Release the handle. The database stays open. */
  release() {
    if (!this.#live) return;
    this.#live = false;
    call(() => this.#host.releaseCollection(this.#handle));
  }

  /** `using docs = db.collection(…)` releases it at the end of the scope. */
  [Symbol.dispose]() {
    this.release();
  }

  #alive() {
    if (!this.#live) {
      throw new VdbError(1002, 'this collection has been released');
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
  throw new VdbError(1002, 'a vector must be a Float32Array or an array of numbers');
}
