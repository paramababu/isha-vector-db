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
  const dir = path.join(os.tmpdir(), `isha-vector-db-node-test-${process.pid}-${counter++}`);
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

test('metadata is actually stored, with integers staying integers', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    c.upsert('a', new Float32Array([1, 0]), {
      kind: 'tool',
      count: 3,
      price: 1.5,
      live: true,
      nothing: null,
    });
    c.upsert('b', new Float32Array([0, 1]));

    // Asserted through filters, because that is the only way to observe stored metadata from
    // here. An earlier version of this test checked only `count()`, which would have passed
    // just as happily had the metadata been silently dropped — and for a while it was.
    const q = new Float32Array([1, 0]);
    const ids = (f) => c.search(q, 10, f).map((h) => h.id);
    assert.deepEqual(ids({ kind: 'tool' }), ['a']);
    assert.deepEqual(ids({ live: true }), ['a']);
    assert.deepEqual(ids({ price: 1.5 }), ['a']);
    // A number written as 3 must come back as an integer 3, or integer comparisons would miss.
    assert.deepEqual(ids({ count: 3 }), ['a']);
    assert.deepEqual(ids({ count: { $gte: 3, $lte: 3 } }), ['a']);
    // An explicit null is present-and-null, and the document without metadata has neither.
    assert.deepEqual(ids({ nothing: { $exists: true } }), ['a']);
    assert.deepEqual(ids({ kind: { $exists: false } }), ['b']);
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

// Calls the disposer rather than writing `using db = ...`.
//
// The `using` syntax needs Node 22, and this file has to parse on every version the package
// supports — a syntax error is not a failing test, it is a file that never runs at all, which is
// how Node 18 and 20 reported *zero* results here rather than one failure.
//
// `Symbol.dispose` is the whole mechanism `using` invokes, so calling it directly tests the same
// contract on every supported version.
test('the disposer releases the lock, which is what `using` calls', () => {
  const dir = scratch();
  {
    const db = vdb.open(dir);
    db.collection('docs', { dimension: 2 }).upsert('a', new Float32Array([1, 0]));
    assert.equal(typeof db[Symbol.dispose], 'function', '`using` needs this to exist');
    db[Symbol.dispose]();
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

// ---------------------------------------------------------------------------
// filters
// ---------------------------------------------------------------------------

/** Three documents at decreasing similarity to [1, 0], so filtering is visible separately from ranking. */
function corpus(db) {
  const c = db.collection('docs', { dimension: 2 });
  c.upsert('hammer', new Float32Array([1, 0]), { category: 'tools', price: 25, sale: true });
  c.upsert('saw', new Float32Array([0.95, 0.31]), { category: 'tools', price: 75 });
  c.upsert('ball', new Float32Array([0.7, 0.7]), { category: 'toys' });
  return c;
}

const ids = (hits) => hits.map((h) => h.id);

test('a bare value means equality', () => {
  withDb((db) => {
    const c = corpus(db);
    assert.deepEqual(ids(c.search(new Float32Array([1, 0]), 10)), ['hammer', 'saw', 'ball']);
    assert.deepEqual(
      ids(c.search(new Float32Array([1, 0]), 10, { category: 'tools' })),
      ['hammer', 'saw'],
    );
  });
});

test('several keys in one object mean conjunction', () => {
  withDb((db) => {
    const c = corpus(db);
    const hits = c.search(new Float32Array([1, 0]), 10, {
      category: 'tools',
      price: { $lt: 50 },
    });
    assert.deepEqual(ids(hits), ['hammer']);
  });
});

test('comparison operators', () => {
  withDb((db) => {
    const c = corpus(db);
    const q = new Float32Array([1, 0]);
    assert.deepEqual(ids(c.search(q, 10, { price: { $gt: 50 } })), ['saw']);
    assert.deepEqual(ids(c.search(q, 10, { price: { $gte: 25, $lte: 75 } })), ['hammer', 'saw']);
    assert.deepEqual(ids(c.search(q, 10, { category: { $ne: 'tools' } })), ['ball']);
    assert.deepEqual(ids(c.search(q, 10, { category: { $in: ['toys', 'games'] } })), ['ball']);
    assert.deepEqual(ids(c.search(q, 10, { category: { $nin: ['toys'] } })), ['hammer', 'saw']);
    assert.deepEqual(ids(c.search(q, 10, { category: { $startsWith: 'too' } })), ['hammer', 'saw']);
  });
});

test('$and, $or and $not compose', () => {
  withDb((db) => {
    const c = corpus(db);
    const q = new Float32Array([1, 0]);
    assert.deepEqual(
      ids(c.search(q, 10, { $or: [{ category: 'toys' }, { price: { $gt: 50 } }] })),
      ['saw', 'ball'],
    );
    assert.deepEqual(ids(c.search(q, 10, { $not: { category: 'tools' } })), ['ball']);
    // Three levels deep, in one object.
    const nested = {
      $or: [
        { $and: [{ category: 'tools' }, { $or: [{ price: { $lt: 50 } }, { sale: true }] }] },
        { category: 'toys' },
      ],
    };
    assert.deepEqual(ids(c.search(q, 10, nested)), ['hammer', 'ball']);
  });
});

test('absent fields behave as documented', () => {
  withDb((db) => {
    const c = corpus(db);
    const q = new Float32Array([1, 0]);
    // "ball" has no price.
    assert.deepEqual(ids(c.search(q, 10, { price: { $exists: true } })), ['hammer', 'saw']);
    assert.deepEqual(ids(c.search(q, 10, { price: { $exists: false } })), ['ball']);
    // $ne is the exact negation of equality, so it matches the absent field too.
    assert.deepEqual(ids(c.search(q, 10, { price: { $ne: 25 } })), ['saw', 'ball']);
  });
});

test('an empty filter matches everything and topK counts matches', () => {
  withDb((db) => {
    const c = corpus(db);
    const q = new Float32Array([1, 0]);
    assert.equal(c.search(q, 10, {}).length, 3);
    assert.equal(c.search(q, 10, undefined).length, 3);
    // Only one document matches, and asking for two returns the one.
    assert.deepEqual(ids(c.search(q, 2, { category: 'toys' })), ['ball']);
  });
});

test('type mismatches are false rather than errors', () => {
  withDb((db) => {
    const c = corpus(db);
    const q = new Float32Array([1, 0]);
    assert.deepEqual(ids(c.search(q, 10, { category: 1 })), []);
    assert.deepEqual(ids(c.search(q, 10, { category: { $gt: 1 } })), []);
  });
});

test('malformed filters are rejected with a message naming the problem', () => {
  withDb((db) => {
    const c = corpus(db);
    const q = new Float32Array([1, 0]);
    assert.throws(() => c.search(q, 10, { price: { $nope: 1 } }), /unknown operator/);
    assert.throws(() => c.search(q, 10, { $nope: [] }), /unknown filter operator/);
    assert.throws(() => c.search(q, 10, { $and: 'not an array' }), /array of filters/);
    // An array as a field value is ambiguous, so it is refused rather than guessed at.
    assert.throws(() => c.search(q, 10, { tags: ['a'] }), /\$contains/);
    assert.throws(() => c.search(q, 10, { name: { $startsWith: 5 } }), /takes a string/);
  });
});

test('filters work after a flush, out of a segment', () => {
  withDb((db) => {
    const c = corpus(db);
    c.flush();
    assert.deepEqual(
      ids(c.search(new Float32Array([1, 0]), 10, { category: 'tools' })),
      ['hammer', 'saw'],
    );
  });
});


// ---------------------------------------------------------------------------
// maintenance
// ---------------------------------------------------------------------------

test('stats, compaction and verification', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    for (let i = 0; i < 10; i++) c.upsert(`doc-${i}`, new Float32Array([i, 1]));
    // Flush first: a delete only occupies space once the row it shadows is on disk.
    c.flush();
    for (let i = 0; i < 7; i++) c.delete(`doc-${i}`);
    c.flush();

    const before = c.stats();
    assert.equal(before.liveDocuments, 3);
    assert.equal(before.totalRows, 10, 'the dead rows are still there');
    assert.ok(before.deadRatio > 0.6, `deadRatio was ${before.deadRatio}`);

    const clean = db.verify('full');
    assert.equal(clean.errors, 0);
    assert.deepEqual(clean.messages, []);
    assert.ok(clean.warnings > 0, 'seventy percent dead is worth a warning');

    assert.equal(db.compact(), 7);

    const after = c.stats();
    assert.equal(after.liveDocuments, 3, 'compaction must not lose a document');
    assert.equal(after.totalRows, 3, 'the dead rows should be gone');
    assert.equal(after.deadRatio, 0);
    assert.equal(db.verify('full').errors, 0);
    // Still searchable afterwards.
    assert.equal(c.search(new Float32Array([9, 1]), 1)[0].id, 'doc-9');
  });
});

test('compaction leaves healthy segments alone unless told otherwise', () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 2 });
    for (let i = 0; i < 10; i++) c.upsert(`doc-${i}`, new Float32Array([i, 1]));
    c.flush();
    c.delete('doc-0');
    c.flush();
    assert.equal(db.compact(), 0, 'ten percent dead is not worth rewriting');
    assert.equal(db.compact(0), 1, 'unless asked to rewrite everything');
  });
});

test('maintenance arguments are validated', () => {
  withDb((db) => {
    assert.throws(() => db.compact(2), /between 0 and 1/);
    assert.throws(() => db.compact(-1), /between 0 and 1/);
    assert.throws(() => db.verify('nope'), /unknown verify level/);
  });
});

// ---------------------------------------------------------------------------
// error codes
// ---------------------------------------------------------------------------

test("a failure carries the engine's numeric code, not napi's status string", () => {
  withDb((db) => {
    const c = db.collection('docs', { dimension: 3 });
    assert.throws(
      () => c.upsert('bad', new Float32Array([1, 2])),
      (e) => {
        // Without the wrapper this is the string 'GenericFailure', and Node is then the only
        // binding where a caller cannot branch on the code and has to match on English.
        assert.equal(typeof e.code, 'number', `code was ${JSON.stringify(e.code)}`);
        assert.equal(e.code, 4003);
        assert.match(e.message, /3-dimensional/);
        return true;
      },
    );
  });
});

test('the code is attached to collection methods too, not only to open', () => {
  withDb((db) => {
    assert.throws(
      () => db.collection('zero', { dimension: 0 }),
      (e) => {
        assert.equal(typeof e.code, 'number');
        assert.ok(e.code > 0);
        return true;
      },
    );
  });
});
