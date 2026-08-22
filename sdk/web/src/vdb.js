// The web SDK: the same database, reached through WebAssembly.
//
// This drives `include/vdb.h` — the identical C ABI the iOS, Android and Node SDKs use. There is
// no WebAssembly-specific database interface, only a WebAssembly-specific way of calling the one
// that already existed, which is the whole point of ADR-0009.

import { createHost } from './host.js';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Distance metrics. Values match `vdb_metric_t` in `include/vdb.h`; there is no zero. */
export const Metric = { Cosine: 1, L2: 2, Dot: 3 };

/** Durability levels, matching `vdb_durability_t`. `Batch` is the engine's own default. */
export const Durability = { Full: 1, Batch: 2, Relaxed: 3 };

/** Error codes this SDK reasons about. The full list is in `docs/api/errors.md`. */
const ERR = { COLLECTION_ALREADY_EXISTS: 4001 };

/** An error carrying the engine's own structured code. */
export class VdbError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'VdbError';
    /** The stable numeric code from `docs/api/errors.md`. */
    this.code = code;
  }
}

/**
 * Load the engine.
 *
 * @param {BufferSource|Response|Promise<Response>} source  The `.wasm` module.
 * @param {object} adapter  Synchronous storage; see `adapters/`.
 */
export async function load(source, adapter) {
  let instance;
  const host = createHost(adapter, () => instance.exports.memory);

  const imports = { vdb_host: host.imports };
  const bytes = source instanceof Uint8Array || source instanceof ArrayBuffer;
  const result = bytes
    ? await WebAssembly.instantiate(source, imports)
    : await WebAssembly.instantiateStreaming(source, imports);
  instance = result.instance ?? result;

  return new Vdb(instance, host);
}

/** A loaded engine. One instance owns one linear memory and one set of open databases. */
class Vdb {
  #x;      // exports
  #host;

  constructor(instance, host) {
    this.#x = instance.exports;
    this.#host = host;
  }

  /** The library version string. */
  get version() {
    return this.#cstr(this.#x.vdb_version());
  }

  /** The frozen C ABI version. */
  get abiVersion() {
    return this.#x.vdb_abi_version();
  }

  /** The on-disk format this build writes. */
  get formatVersion() {
    return this.#x.vdb_format_version();
  }

  /** Open or create a database rooted at `path` within the adapter's storage. */
  open(path, { createIfMissing = true, readOnly = false, durability = Durability.Batch } = {}) {
    return this.#call((err) => {
      const out = this.#scratch(4);
      const [p, plen] = this.#str(path);
      try {
        const rc = this.#x.vdb_open(
          p, plen, createIfMissing ? 1 : 0, readOnly ? 1 : 0, durability, out.ptr, err,
        );
        this.#check(rc, err);
        return new Database(this, this.#u32(out.ptr));
      } finally {
        this.#free(p, plen);
        out.free();
      }
    });
  }

  /** Release every handle this instance holds. */
  dispose() {
    this.#host.dispose();
  }

  // ---- the marshalling primitives every call above is built from ----

  /**
   * Run `fn` with an error out-parameter, freeing it however the call ends.
   *
   * Every function in the C ABI reports failure through this out-parameter rather than a global,
   * so every call site needs the same allocate/check/free dance. Doing it in one place is what
   * keeps the bindings below readable, and means an early return cannot leak the error object.
   */
  #call(fn) {
    const err = this.#scratch(4);
    try {
      this.#view().setUint32(err.ptr, 0, true);
      return fn(err.ptr);
    } finally {
      const e = this.#u32(err.ptr);
      if (e !== 0) this.#x.vdb_error_free(e);
      err.free();
    }
  }

  /** Copy a JS string into linear memory as UTF-8. Returns `[ptr, len]`. */
  #str(s) {
    const bytes = encoder.encode(s);
    if (bytes.length === 0) return [0, 0];
    const ptr = this.#x.vdb_wasm_alloc(bytes.length);
    if (ptr === 0) throw new VdbError(0, 'out of memory in the WebAssembly instance');
    this.#bytes().set(bytes, ptr);
    return [ptr, bytes.length];
  }

  /** Copy a vector into linear memory. Returns a block the caller must free. */
  #floats(values) {
    const f = values instanceof Float32Array ? values : new Float32Array(values);
    const ptr = this.#x.vdb_wasm_alloc(f.byteLength);
    if (ptr === 0) throw new VdbError(0, 'out of memory in the WebAssembly instance');
    this.#bytes().set(new Uint8Array(f.buffer, f.byteOffset, f.byteLength), ptr);
    return { ptr, len: f.length, free: () => this.#x.vdb_wasm_free(ptr, f.byteLength) };
  }

  #free(ptr, len) {
    if (ptr !== 0) this.#x.vdb_wasm_free(ptr, len);
  }

  /** A scratch block for out-parameters. */
  #scratch(len) {
    const ptr = this.#x.vdb_wasm_alloc(len);
    if (ptr === 0) throw new VdbError(0, 'out of memory in the WebAssembly instance');
    return { ptr, free: () => this.#x.vdb_wasm_free(ptr, len) };
  }

  // Views are built per call: linear memory is replaced wholesale when it grows, and a cached
  // view would keep reading a detached buffer.
  #bytes() { return new Uint8Array(this.#x.memory.buffer); }
  #view() { return new DataView(this.#x.memory.buffer); }

  #u32(ptr) { return this.#view().getUint32(ptr, true); }

  /** Read a NUL-terminated string the engine owns. */
  #cstr(ptr) {
    if (ptr === 0) return '';
    const mem = this.#bytes();
    let end = ptr;
    while (mem[end] !== 0) end++;
    return decoder.decode(mem.subarray(ptr, end));
  }

  /**
   * Turn a non-zero return code into a structured error.
   *
   * The message comes from the engine, not from this file: the whole point of the structured
   * error model is that "collection \"docs\" stores 3-dimensional vectors, got 2" reaches the
   * developer intact, rather than being flattened into "search failed".
   */
  #check(rc, errPtr) {
    if (rc === 0) return;
    const e = this.#u32(errPtr);
    const code = e !== 0 ? this.#x.vdb_error_code(e) : rc;
    const message = e !== 0 ? this.#cstr(this.#x.vdb_error_message(e)) : `vdb error ${rc}`;
    throw new VdbError(code, message);
  }

  // Internal access for `Database` and `Collection`, which are thin wrappers over the ABI.
  get _x() { return this.#x; }
  _call(fn) { return this.#call(fn); }
  _str(s) { return this.#str(s); }
  _floats(v) { return this.#floats(v); }
  _free(p, n) { return this.#free(p, n); }
  _scratch(n) { return this.#scratch(n); }
  _u32(p) { return this.#u32(p); }
  _bytes() { return this.#bytes(); }
  _view() { return this.#view(); }
  _cstr(p) { return this.#cstr(p); }
  _check(rc, e) { return this.#check(rc, e); }
}

/** An open database. */
class Database {
  constructor(vdb, handle) {
    this._v = vdb;
    this._h = handle;
  }

  /**
   * Create a collection, or open it if it already exists.
   *
   * Creating first and falling back is deliberate: asking "does it exist?" and then acting would
   * be two decisions with a gap between them, and the engine already answers this atomically.
   */
  collection(name, dimension, metric = Metric.Cosine) {
    return this._v._call((err) => {
      const out = this._v._scratch(4);
      const [n, nlen] = this._v._str(name);
      try {
        let rc = this._v._x.vdb_collection_create(
          this._h, n, nlen, dimension, metric, 0, out.ptr, err,
        );
        if (rc !== 0) {
          const e = this._v._u32(err);
          const code = e !== 0 ? this._v._x.vdb_error_code(e) : 0;
          // Fall back *only* when the collection is already there. Retrying on any failure
          // would replace a precise diagnosis ("dimension must be non-zero") with a misleading
          // one ("collection not found") — which is exactly what happened the first time this
          // was written, and cost an hour.
          if (code !== ERR.COLLECTION_ALREADY_EXISTS) {
            this._v._check(rc, err);
          }
          if (e !== 0) this._v._x.vdb_error_free(e);
          this._v._view().setUint32(err, 0, true);
          rc = this._v._x.vdb_collection_open(this._h, n, nlen, out.ptr, err);
        }
        this._v._check(rc, err);
        return new Collection(this._v, this._v._u32(out.ptr));
      } finally {
        this._v._free(n, nlen);
        out.free();
      }
    });
  }

  /** Flush every collection to storage. */
  flush() {
    this._v._call((err) => this._v._check(this._v._x.vdb_flush(this._h, err), err));
  }

  /** Close the database. Further use of the handle is an error. */
  close() {
    this._v._call((err) => this._v._check(this._v._x.vdb_close(this._h, err), err));
  }
}

/** One collection. */
class Collection {
  constructor(vdb, handle) {
    this._v = vdb;
    this._h = handle;
  }

  /** Insert or replace a document. Returns whether it was newly inserted. */
  upsert(id, vector) {
    return this._v._call((err) => {
      const [i, ilen] = this._v._str(id);
      const v = this._v._floats(vector);
      const inserted = this._v._scratch(4);
      try {
        this._v._check(
          this._v._x.vdb_upsert(this._h, i, ilen, v.ptr, v.len, 0, inserted.ptr, err),
          err,
        );
        return this._v._bytes()[inserted.ptr] !== 0;
      } finally {
        inserted.free();
        v.free();
        this._v._free(i, ilen);
      }
    });
  }

  /** How many documents are live. */
  count() {
    return this._v._call((err) => {
      const out = this._v._scratch(8);
      try {
        this._v._check(this._v._x.vdb_collection_count(this._h, out.ptr, err), err);
        return Number(this._v._view().getBigUint64(out.ptr, true));
      } finally {
        out.free();
      }
    });
  }

  /** Whether a document exists. */
  has(id) {
    return this._v._call((err) => {
      const [i, ilen] = this._v._str(id);
      const out = this._v._scratch(4);
      try {
        this._v._check(this._v._x.vdb_contains(this._h, i, ilen, out.ptr, err), err);
        return this._v._bytes()[out.ptr] !== 0;
      } finally {
        out.free();
        this._v._free(i, ilen);
      }
    });
  }

  /** Remove a document. Returns whether it existed. */
  delete(id) {
    return this._v._call((err) => {
      const [i, ilen] = this._v._str(id);
      const out = this._v._scratch(4);
      try {
        this._v._check(this._v._x.vdb_delete(this._h, i, ilen, out.ptr, err), err);
        return this._v._bytes()[out.ptr] !== 0;
      } finally {
        out.free();
        this._v._free(i, ilen);
      }
    });
  }

  /** Flush this collection's writes. */
  flush() {
    this._v._call((err) =>
      this._v._check(this._v._x.vdb_collection_flush(this._h, err), err));
  }

  /** The `k` nearest documents to `query`, as `[{id, score}]`, best first. */
  search(query, k) {
    return this._v._call((err) => {
      const q = this._v._floats(query);
      const out = this._v._scratch(4);
      try {
        this._v._check(this._v._x.vdb_search(this._h, q.ptr, q.len, k, out.ptr, err), err);
        const results = this._v._u32(out.ptr);
        try {
          return this.#collect(results);
        } finally {
          this._v._x.vdb_results_free(results);
        }
      } finally {
        out.free();
        q.free();
      }
    });
  }

  /** Read a results handle into plain objects before it is freed. */
  #collect(results) {
    const n = this._v._x.vdb_results_len(results);
    const lenOut = this._v._scratch(4);
    try {
      const hits = [];
      for (let i = 0; i < n; i++) {
        const ptr = this._v._x.vdb_results_id(results, i, lenOut.ptr);
        const len = this._v._u32(lenOut.ptr);
        hits.push({
          id: decoder.decode(this._v._bytes().slice(ptr, ptr + len)),
          score: this._v._x.vdb_results_score(results, i),
        });
      }
      return hits;
    } finally {
      lenOut.free();
    }
  }
}
