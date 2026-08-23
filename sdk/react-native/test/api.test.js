// The JavaScript layer, against a mock host object.
//
// The real host object is C++ installed by a TurboModule and needs a running app. What is tested
// here is everything above it: closed-handle tracking, vector conversion, error wrapping, and the
// rule that a released collection cannot be used. Those are decisions this file makes, not the
// native layer's, and they would otherwise be discovered on a device.
//
// The native half is covered separately: `scripts/test-react-native.sh` compiles `vdb_bridge.cpp`
// and runs it against the real engine. Between the two, the only untested code is the value
// conversion in `vdb_jsi.cpp`.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { open, versions, Metric, VdbError, Database, Collection } from '../src/index.js';

/// A stand-in for the JSI host object, recording what it was asked to do.
function mockHost(overrides = {}) {
  const calls = [];
  const host = {
    version: '0.0.1',
    abiVersion: 1,
    formatVersion: 2,
    liveHandles: 0,
    open: (path, create, readOnly) => {
      calls.push(['open', path, create, readOnly]);
      return 1;
    },
    close: (h) => calls.push(['close', h]),
    collection: (db, name, dimension, metric) => {
      calls.push(['collection', db, name, dimension, metric]);
      return 2;
    },
    releaseCollection: (h) => calls.push(['releaseCollection', h]),
    upsert: (h, id, vector) => {
      calls.push(['upsert', h, id, vector]);
      return true;
    },
    remove: (h, id) => {
      calls.push(['remove', h, id]);
      return true;
    },
    contains: (h, id) => {
      calls.push(['contains', h, id]);
      return id === 'present';
    },
    count: (h) => {
      calls.push(['count', h]);
      return 7;
    },
    flush: (h) => calls.push(['flush', h]),
    search: (h, query, k) => {
      calls.push(['search', h, query, k]);
      return [{ id: 'a', score: 0.9 }];
    },
    ...overrides,
  };
  host.calls = calls;
  return host;
}

test('reports versions', () => {
  const v = versions(mockHost());
  assert.equal(v.abi, 1);
  assert.equal(v.format, 2);
  assert.equal(v.library, '0.0.1');
});

test('opens and drives a collection', () => {
  const host = mockHost();
  const db = open('/tmp/db', { host });
  assert.ok(db instanceof Database);

  const docs = db.collection('docs', 4);
  assert.ok(docs instanceof Collection);
  assert.deepEqual(host.calls[1], ['collection', 1, 'docs', 4, Metric.Cosine]);

  assert.equal(docs.upsert('a', [1, 2, 3, 4]), true);
  assert.equal(docs.count(), 7);
  assert.equal(docs.has('present'), true);
  assert.equal(docs.has('absent'), false);
  assert.equal(docs.delete('a'), true);
  docs.flush();
  assert.deepEqual(docs.search([1, 2, 3, 4], 1), [{ id: 'a', score: 0.9 }]);
});

test('a Float32Array is passed through without copying', () => {
  const host = mockHost();
  const docs = open('/tmp/db', { host }).collection('docs', 3);
  const vector = new Float32Array([1, 2, 3]);
  docs.upsert('a', vector);
  const passed = host.calls.at(-1)[3];
  assert.equal(passed, vector, 'the same object must reach the native side');
});

test('a plain array is converted', () => {
  const host = mockHost();
  const docs = open('/tmp/db', { host }).collection('docs', 3);
  docs.upsert('a', [1, 2, 3]);
  const passed = host.calls.at(-1)[3];
  assert.ok(passed instanceof Float32Array);
  assert.deepEqual([...passed], [1, 2, 3]);
});

test('anything else is refused before it reaches C++', () => {
  const host = mockHost();
  const docs = open('/tmp/db', { host }).collection('docs', 3);
  // A string reaching the native side would be read as a buffer, which is exactly the kind of
  // mistake that must not get past JavaScript.
  assert.throws(() => docs.upsert('a', 'not a vector'), VdbError);
  assert.throws(() => docs.upsert('a', { length: 3 }), VdbError);
});

test('a closed database refuses further use', () => {
  const host = mockHost();
  const db = open('/tmp/db', { host });
  assert.equal(db.isOpen, true);
  db.close();
  assert.equal(db.isOpen, false);
  assert.throws(() => db.collection('docs', 3), (e) => {
    assert.ok(e instanceof VdbError);
    assert.match(e.message, /closed/);
    return true;
  });
});

test('closing twice is harmless from JavaScript', () => {
  const host = mockHost();
  const db = open('/tmp/db', { host });
  db.close();
  db.close();
  // The native side treats a double close as an error; the JS layer absorbs the second one so a
  // cleanup path in a `finally` does not have to guard.
  assert.equal(host.calls.filter((c) => c[0] === 'close').length, 1);
});

test('a released collection refuses further use', () => {
  const host = mockHost();
  const docs = open('/tmp/db', { host }).collection('docs', 3);
  docs.release();
  assert.throws(() => docs.count(), (e) => {
    assert.match(e.message, /released/);
    return true;
  });
  docs.release();
  assert.equal(host.calls.filter((c) => c[0] === 'releaseCollection').length, 1);
});

test('the engine\'s structured code survives the trip', () => {
  const host = mockHost({
    upsert: () => {
      const e = new Error('[VDB-4003] collection "docs" stores 3-dimensional vectors, got 2');
      e.code = 4003;
      throw e;
    },
  });
  const docs = open('/tmp/db', { host }).collection('docs', 3);
  assert.throws(() => docs.upsert('a', [1, 2]), (e) => {
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
  // On the New Architecture the TurboModule installs during startup, and a module-level read can
  // run first and capture undefined forever.
  const host = mockHost();
  globalThis.__vdb = host;
  try {
    const db = open('/tmp/db');
    assert.ok(db instanceof Database);
  } finally {
    delete globalThis.__vdb;
  }
});
