// The JavaScript layer, against a mock host object.
//
// The real host object is C++ installed by the native module and needs a running app. What is
// tested here is everything above it: argument validation, closed-handle tracking, vector
// conversion, batch packing, error wrapping, and the rule that a released collection cannot be
// used. Those are decisions this file's subject makes, not the native layer's, and they would
// otherwise be discovered on a device.
//
// The native half is covered separately — `scripts/test-react-native.sh` compiles and runs
// `vdb_bridge.cpp` against the real engine, and compiles `vdb_jsi.cpp` against React Native's
// own headers. The filter and metadata compilers have their own file, `filter.test.js`.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { open, versions, VdbError, Database, Collection } from '../src/index.js';
import { mockHost, lastCall } from './mock-host.js';

/** A database and a collection over a fresh mock, since almost every test wants both. */
function fixture(overrides) {
  const host = mockHost(overrides);
  const db = open('/tmp/db', { host });
  return { host, db, docs: db.collection('docs', { dimension: 4 }) };
}

test('reports versions', () => {
  const v = versions(mockHost());
  assert.equal(v.abi, 1);
  assert.equal(v.format, 2);
  assert.equal(v.library, '0.0.1');
});

test('opens and drives a collection', () => {
  const { host, db, docs } = fixture();
  assert.ok(db instanceof Database);
  assert.ok(docs instanceof Collection);

  // 'cosine' is VDB_METRIC_COSINE, and 'batch' is VDB_DURABILITY_BATCH.
  assert.deepEqual(lastCall(host, 'open'), ['open', '/tmp/db', true, false, 2]);
  assert.deepEqual(lastCall(host, 'collection'), ['collection', 1, 'docs', 4, 1]);
  assert.equal(docs.name, 'docs');
  assert.equal(docs.dimension, 4);

  assert.equal(docs.upsert('a', [1, 2, 3, 4]), true);
  assert.equal(docs.count(), 7);
  assert.equal(docs.contains('present'), true);
  assert.equal(docs.contains('absent'), false);
  assert.equal(docs.delete('a'), true);
  docs.flush();
  assert.deepEqual(docs.search([1, 2, 3, 4], 1), [{ id: 'a', score: 0.9 }]);
});

test('the metric and durability names map to the C ABI values', () => {
  // These numbers are the C ABI's, and a binding that got one wrong would build a collection
  // with the wrong metric and return results that look plausible and are wrong.
  for (const [name, value] of [['cosine', 1], ['l2', 2], ['dot', 3]]) {
    const host = mockHost();
    open('/tmp/db', { host }).collection('c', { dimension: 2, metric: name });
    assert.equal(lastCall(host, 'collection')[4], value, `metric ${name}`);
  }
  for (const [name, value] of [['full', 1], ['batch', 2], ['relaxed', 3]]) {
    const host = mockHost();
    open('/tmp/db', { host, durability: name });
    assert.equal(lastCall(host, 'open')[4], value, `durability ${name}`);
  }
  for (const [name, value] of [['quick', 1], ['checksums', 2], ['full', 3]]) {
    const host = mockHost();
    open('/tmp/db', { host }).verify(name);
    assert.equal(lastCall(host, 'verify')[2], value, `verify level ${name}`);
  }
});

test('an unknown metric, durability or level names the alternatives', () => {
  const host = mockHost();
  assert.throws(() => open('/tmp/db', { host, durability: 'fsync' }), (e) => {
    assert.ok(e instanceof VdbError);
    assert.match(e.message, /'full', 'batch', 'relaxed'/);
    return true;
  });
  const db = open('/tmp/db', { host });
  assert.throws(() => db.collection('c', { dimension: 2, metric: 'euclidean' }), /'cosine'/);
  assert.throws(() => db.verify('paranoid'), /'quick'/);
});

test('a collection needs a dimension, and says so', () => {
  const { db } = fixture();
  // The old API took it positionally. Someone porting will write this, and the message has to
  // tell them what to write instead.
  assert.throws(() => db.collection('docs', 4), (e) => {
    assert.match(e.message, /collection\(name, \{ dimension \}\)/);
    return true;
  });
  assert.throws(() => db.collection('docs'), /dimension/);
});

test('a Float32Array is passed through without copying', () => {
  const { host, docs } = fixture();
  const vector = new Float32Array([1, 2, 3, 4]);
  docs.upsert('a', vector);
  assert.equal(lastCall(host, 'upsert')[3], vector, 'the same object must reach the native side');
});

test('a plain array is converted', () => {
  const { host, docs } = fixture();
  docs.upsert('a', [1, 2, 3, 4]);
  const passed = lastCall(host, 'upsert')[3];
  assert.ok(passed instanceof Float32Array);
  assert.deepEqual([...passed], [1, 2, 3, 4]);
});

test('anything else is refused before it reaches C++', () => {
  const { docs } = fixture();
  // A string reaching the native side would be read as a buffer, which is exactly the kind of
  // mistake that must not get past JavaScript.
  assert.throws(() => docs.upsert('a', 'not a vector'), VdbError);
  assert.throws(() => docs.upsert('a', { length: 3 }), VdbError);
  assert.throws(() => docs.search([1, 2, 3, 4], 0), /positive integer/);
  assert.throws(() => docs.search([1, 2, 3, 4], 1.5), /positive integer/);
});

test('metadata is compiled and handed over with the document', () => {
  const { host, docs } = fixture();
  docs.upsert('a', [1, 2, 3, 4], { category: 'tools', price: 25, stocked: true, note: null });
  const fields = lastCall(host, 'upsert')[4];
  assert.deepEqual(
    fields.map((f) => [f.key, f.kind]),
    // 1 string, 2 i64, 4 bool, 5 null — the kinds in cpp/vdb_bridge.h.
    [['category', 1], ['price', 2], ['stocked', 4], ['note', 5]],
  );
  // A document with no metadata costs an empty list, and the bridge turns that into no
  // allocation at all.
  docs.upsert('b', [1, 2, 3, 4]);
  assert.deepEqual(lastCall(host, 'upsert')[4], []);
});

test('a batch packs every vector into one buffer', () => {
  const { host, docs } = fixture();
  const inserted = docs.upsertMany([
    { id: 'a', vector: [1, 0, 0, 0] },
    { id: 'b', vector: new Float32Array([0, 1, 0, 0]) },
    { id: 'c', vector: [0, 0, 1, 0] },
  ]);
  assert.equal(inserted, 3);

  const [, , ids, vectors, dimension, metadata] = lastCall(host, 'upsertMany');
  assert.deepEqual(ids, ['a', 'b', 'c']);
  assert.equal(dimension, 4);
  assert.ok(vectors instanceof Float32Array);
  assert.equal(vectors.length, 12, 'one contiguous block of count * dimension');
  assert.deepEqual([...vectors], [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0], 'packed in order');
  assert.deepEqual(metadata, [], 'a batch with no metadata sends none, not three empty lists');
});

test('a batch carrying metadata sends one entry per document', () => {
  const { host, docs } = fixture();
  docs.upsertMany([
    { id: 'a', vector: [1, 0, 0, 0], metadata: { n: 1 } },
    // Only some documents carry metadata, but the bridge takes all or none — so the ones
    // without must still get an entry, or the indices would not line up.
    { id: 'b', vector: [0, 1, 0, 0] },
  ]);
  const metadata = lastCall(host, 'upsertMany')[5];
  assert.equal(metadata.length, 2);
  assert.deepEqual(metadata[0].map((f) => f.key), ['n']);
  assert.deepEqual(metadata[1], []);
});

test('a batch of mismatched vectors is refused rather than silently misaligned', () => {
  const { docs } = fixture();
  // The failure this prevents is the worst kind: a short vector would shift every subsequent
  // document's floats by a few positions and store nonsense that looks like data.
  assert.throws(
    () =>
      docs.upsertMany([
        { id: 'a', vector: [1, 0, 0, 0] },
        { id: 'b', vector: [0, 1, 0] },
      ]),
    (e) => {
      assert.match(e.message, /document 1 \(b\) has 3 dimensions but the batch started with 4/);
      return true;
    },
  );
  assert.throws(() => docs.upsertMany([{ id: 7, vector: [1, 0, 0, 0] }]), /an id must be a string/);
  assert.throws(() => docs.upsertMany('not an array'), /an array of/);
  // An empty first vector would make the stride zero and pack every document on top of the last.
  assert.throws(() => docs.upsertMany([{ id: 'a', vector: [] }]), /cannot be empty/);
  assert.throws(() => docs.upsertMany([null]), /must be a Float32Array/);
});

test('an empty batch does nothing rather than crossing to C++', () => {
  const { host, docs } = fixture();
  assert.equal(docs.upsertMany([]), 0);
  assert.equal(host.calls.filter((c) => c[0] === 'upsertMany').length, 0);
});

test('maintenance reaches the database, not the collection', () => {
  const { host, db, docs } = fixture();
  assert.deepEqual(docs.stats(), {
    liveDocuments: 7,
    totalRows: 9,
    segments: 1,
    bufferedDocuments: 0,
    deadRatio: 0.22,
    dimension: 4,
  });
  assert.equal(db.compact(0.5), 2);
  assert.deepEqual(lastCall(host, 'compact'), ['compact', 1, 0.5]);
  assert.equal(db.compact(), 2);
  assert.equal(lastCall(host, 'compact')[2], 0.2, 'the default dead ratio');
  assert.throws(() => db.compact(2), /between 0 and 1/);

  assert.deepEqual(db.verify(), { errors: 0, warnings: 1 });
  assert.equal(lastCall(host, 'verify')[2], 2, "'checksums' is the default level");

  db.flush();
  assert.deepEqual(lastCall(host, 'flushDatabase'), ['flushDatabase', 1]);
});

test('opening a collection is not the same as creating one', () => {
  const { host, db } = fixture();
  const existing = db.openCollection('docs');
  assert.deepEqual(lastCall(host, 'openCollection'), ['openCollection', 1, 'docs']);
  // Nothing here knows the dimension, because nothing here chose it.
  assert.equal(existing.dimension, undefined);

  db.dropCollection('docs');
  assert.deepEqual(lastCall(host, 'dropCollection'), ['dropCollection', 1, 'docs']);
});

test('a closed database refuses further use', () => {
  const { db } = fixture();
  assert.equal(db.isOpen, true);
  db.close();
  assert.equal(db.isOpen, false);
  assert.throws(() => db.collection('docs', { dimension: 3 }), (e) => {
    assert.ok(e instanceof VdbError);
    assert.match(e.message, /closed/);
    return true;
  });
  // Every entry point, not only the one that happened to be tested.
  assert.throws(() => db.compact(), /closed/);
  assert.throws(() => db.verify(), /closed/);
  assert.throws(() => db.flush(), /closed/);
  assert.throws(() => db.dropCollection('docs'), /closed/);
  assert.throws(() => db.openCollection('docs'), /closed/);
});

test('closing twice is harmless from JavaScript', () => {
  const { host, db } = fixture();
  db.close();
  db.close();
  // The native side treats a double close as an error; the JS layer absorbs the second one so a
  // cleanup path in a `finally` does not have to guard.
  assert.equal(host.calls.filter((c) => c[0] === 'close').length, 1);
});

test('a released collection refuses further use', () => {
  const { host, docs } = fixture();
  docs.release();
  assert.throws(() => docs.count(), (e) => {
    assert.match(e.message, /released/);
    return true;
  });
  assert.throws(() => docs.stats(), /released/);
  assert.throws(() => docs.upsertMany([{ id: 'a', vector: [1, 2, 3, 4] }]), /released/);
  docs.release();
  assert.equal(host.calls.filter((c) => c[0] === 'releaseCollection').length, 1);
});

test('Symbol.dispose closes and releases', () => {
  const { host, db, docs } = fixture();
  docs[Symbol.dispose]();
  db[Symbol.dispose]();
  assert.equal(host.calls.filter((c) => c[0] === 'releaseCollection').length, 1);
  assert.equal(host.calls.filter((c) => c[0] === 'close').length, 1);
});

test("the engine's structured code survives the trip", () => {
  const { docs } = fixture({
    upsert: () => {
      const e = new Error('[VDB-4003] collection "docs" stores 3-dimensional vectors, got 2');
      e.code = 4003;
      throw e;
    },
  });
  assert.throws(() => docs.upsert('a', [1, 2, 3, 4]), (e) => {
    assert.ok(e instanceof VdbError, 'must be wrapped, not passed through raw');
    assert.equal(e.code, 4003);
    assert.match(e.message, /3-dimensional/);
    return true;
  });
});

test('a missing native module says what to do about it', () => {
  // The most likely first-run failure by a wide margin: Expo Go cannot load custom native code.
  const previous = globalThis.__vdb;
  delete globalThis.__vdb;
  try {
    assert.throws(() => open('/tmp/db'), (e) => {
      assert.match(e.message, /native module is not installed/);
      assert.match(e.message, /Expo Go/);
      return true;
    });
  } finally {
    if (previous !== undefined) globalThis.__vdb = previous;
  }
});

test('the host object is read lazily, not at import time', () => {
  // On the New Architecture the native module installs during startup, and a module-level read
  // can run first and capture undefined forever.
  const host = mockHost();
  globalThis.__vdb = host;
  try {
    assert.ok(open('/tmp/db') instanceof Database);
  } finally {
    delete globalThis.__vdb;
  }
});

test('a path is checked before it reaches the native side', () => {
  const host = mockHost();
  assert.throws(() => open('', { host }), /non-empty string/);
  assert.throws(() => open(undefined, { host }), /non-empty string/);
});
