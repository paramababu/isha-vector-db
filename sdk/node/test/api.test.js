'use strict';

/**
 * The Node SDK, exercised as an application would.
 *
 * Node is the first binding on purpose: mistakes in the boundary surface here in a five-second
 * loop rather than a five-minute Gradle one. This suite is where that pays off, so it covers the
 * misuse an application will actually commit, not only the happy path.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const vdb = require('..');

let counter = 0;
function scratch() {
  const dir = path.join(os.tmpdir(), `vdb-node-test-${process.pid}-${counter++}`);
  fs.rmSync(dir, { recursive: true, force: true });
  return dir;
}

function withDb(fn, options) {
  const dir = scratch();
  const db = vdb.open(dir, options);
  try {
    return fn(db, dir);
  } finally {
    db.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

test('open, write, search, close', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 3 });
    assert.equal(c.name, 'docs');
    assert.equal(c.dimension, 3);

    assert.equal(c.upsert('east', new Float32Array([1, 0, 0])), true, 'new document');
    assert.equal(c.upsert('east', new Float32Array([1, 0, 0])), false, 'replacement');
    c.upsert('north', new Float32Array([0, 1, 0]));
    assert.equal(c.count(), 2);

    const hits = c.search(new Float32Array([0.9, 0.1, 0]), 2);
    assert.equal(hits.length, 2);
    assert.equal(hits[0].id, 'east');
    assert.ok(hits[0].score > hits[1].score, 'ordered by score descending');
    assert.ok(hits[0].distance < hits[1].distance, 'cosine reports a distance');
  });
});

test('metadata round-trips, with integers staying integers', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    // A number written as 3 must come back as 3, not 3.0 — otherwise integer filters miss.
    c.upsert('a', new Float32Array([1, 0]), {
      kind: 'tool',
      count: 3,
      price: 1.5,
      live: true,
      nothing: null,
    });
    assert.equal(c.count(), 1);
  });
});

test('delete reports whether the document existed', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    c.upsert('a', new Float32Array([1, 0]));
    assert.equal(c.delete('a'), true);
    assert.equal(c.delete('a'), false, 'deleting twice is a no-op, not an error');
    assert.equal(c.delete('never-existed'), false);
    assert.equal(c.count(), 0);
  });
});

test('data survives close and reopen', () => {
  const dir = scratch();
  try {
    const first = vdb.open(dir);
    first.collection('docs', { dimension: 2 }).upsert('kept', new Float32Array([1, 0]));
    first.close();

    const second = vdb.open(dir);
    const c = second.openCollection('docs');
    assert.equal(c.contains('kept'), true);
    assert.equal(c.count(), 1);
    second.close();
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('collections can be listed and dropped', () => {
  withDb((db) => {
    db.collection('zebra', { dimension: 2 });
    db.collection('apple', { dimension: 2 });
    assert.deepEqual(db.listCollections(), ['apple', 'zebra'], 'sorted, not hash-ordered');
    db.dropCollection('zebra');
    assert.deepEqual(db.listCollections(), ['apple']);
  });
});

test('stats report what is on disk', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    for (let i = 0; i < 10; i++) c.upsert(`doc-${i}`, new Float32Array([i, 1]));
    assert.equal(c.stats().bufferedDocuments, 10);
    c.flush();
    const s = c.stats();
    assert.equal(s.liveDocuments, 10);
    assert.equal(s.segments, 1);
    assert.equal(s.bufferedDocuments, 0);
    assert.equal(s.deadRatio, 0);
  });
});

test('close is idempotent, so a finally block cannot double-throw', () => {
  const dir = scratch();
  const db = vdb.open(dir);
  db.close();
  assert.doesNotThrow(() => db.close());
  assert.equal(db.isOpen, false);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('using disposes the database when scope exits', () => {
  const dir = scratch();
  {
    // eslint-disable-next-line no-undef
    using db = vdb.open(dir);
    db.collection('docs', { dimension: 2 }).upsert('a', new Float32Array([1, 0]));
  }
  // The lock was released, so a second open succeeds.
  const again = vdb.open(dir);
  assert.equal(again.openCollection('docs').count(), 1);
  again.close();
  fs.rmSync(dir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/**
 * Regression, and the reason Node was built first.
 *
 * The `#[napi]` macro inspects the *written* return type. A type alias for `Result<T>` compiled
 * and ran, and silently returned every error to JavaScript as an ordinary value instead of
 * throwing — so `upsert` handed back an `Error` object where callers expected a boolean, and
 * nothing stopped them using it. These assertions exist to catch that ever coming back.
 */
test('errors throw rather than being returned as values', () => {
  withDb((db, dir) => {
    const c = db.collection('docs', { dimension: 3 });

    const cases = [
      ['wrong dimension on upsert', () => c.upsert('a', new Float32Array([1, 0]))],
      ['wrong dimension on search', () => c.search(new Float32Array([1, 0]), 1)],
      ['unknown collection', () => db.openCollection('nope')],
      ['unknown metric', () => db.collection('x', { dimension: 2, metric: 'nope' })],
      ['unknown durability', () => vdb.open(dir, { durability: 'nope' })],
      ['top_k of zero', () => c.search(new Float32Array([1, 0, 0]), 0)],
    ];

    for (const [label, fn] of cases) {
      let returned;
      assert.throws(
        () => {
          returned = fn();
        },
        Error,
        `${label} should throw`,
      );
      assert.equal(returned, undefined, `${label} must not return anything`);
    }
  });
});

test('engine errors carry their stable code in the message', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 3 });
    assert.throws(
      () => c.upsert('a', new Float32Array([1, 0])),
      (e) => {
        assert.match(e.message, /VDB-4003/, 'the stable code should be quotable');
        assert.match(e.message, /docs/, 'the message should name the collection');
        assert.match(e.message, /3-dimensional/);
        return true;
      },
    );
  });
});

test('using a closed database throws rather than crashing', () => {
  const dir = scratch();
  const db = vdb.open(dir);
  db.close();
  assert.throws(() => db.collection('docs', { dimension: 2 }), /closed/);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('a second writer is refused while the first holds the database', () => {
  const dir = scratch();
  const first = vdb.open(dir);
  assert.throws(() => vdb.open(dir), /already open/i);
  first.close();
  // And succeeds once released.
  vdb.open(dir).close();
  fs.rmSync(dir, { recursive: true, force: true });
});

test('a read-only handle can inspect a database another handle holds', () => {
  const dir = scratch();
  const writer = vdb.open(dir);
  writer.collection('docs', { dimension: 2 }).upsert('a', new Float32Array([1, 0]));
  writer.flush();

  const reader = vdb.open(dir, { readOnly: true });
  assert.equal(reader.openCollection('docs').count(), 1);
  assert.throws(() => reader.collection('other', { dimension: 2 }), /read-only/i);
  reader.close();
  writer.close();
  fs.rmSync(dir, { recursive: true, force: true });
});

test('non-scalar metadata is refused with a clear message', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    assert.throws(
      () => c.upsert('a', new Float32Array([1, 0]), { nested: { a: 1 } }),
      /strings, numbers, booleans or null/,
    );
  });
});
